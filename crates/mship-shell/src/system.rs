//! The phone's performance (live), the raw power votes, about.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use mship_core::api::{Op, PowerState, Topic, VoteState};
use mship_core::status::Resources;

use crate::ui::{Metric, Rows, button, esc, fmt_rate, group, mhz, page, row};

use crate::{S, VMS};

// ------------------------------------------------------------------ sampling
/// per-CPU (busy, total) jiffies from /proc/stat; index 0 is all CPUs together
pub fn cpu_times() -> Vec<(u64, u64)> {
    std::fs::read_to_string("/proc/stat")
        .unwrap_or_default()
        .lines()
        .take_while(|l| l.starts_with("cpu"))
        .map(|l| {
            let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let total: u64 = v.iter().sum();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0); // idle + iowait
            (total - idle, total)
        })
        .collect()
}

/// /proc/meminfo in kB
pub fn meminfo() -> HashMap<String, u64> {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.to_string(), v.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}

/// (rx, tx) bytes summed over the phone's uplinks (VM traffic is NATed out through them)
fn net_bytes() -> (u64, u64) {
    let mut t = (0, 0);
    let wifi = mship_core::device::wifi_iface();
    for l in std::fs::read_to_string("/proc/net/dev").unwrap_or_default().lines() {
        let Some((name, rest)) = l.split_once(':') else { continue };
        if Some(name.trim()) == wifi.as_deref() || name.trim() == "usb0" {
            let f: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            t.0 += f.first().copied().unwrap_or(0);
            t.1 += f.get(8).copied().unwrap_or(0);
        }
    }
    t
}

/// (read, written) bytes of the whole UFS disks (sdX, not partitions)
fn disk_bytes() -> (u64, u64) {
    let mut t = (0, 0);
    for l in std::fs::read_to_string("/proc/diskstats").unwrap_or_default().lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() > 9 && f[2].len() == 3 && f[2].starts_with("sd") {
            t.0 += f[5].parse::<u64>().unwrap_or(0) * 512;
            t.1 += f[9].parse::<u64>().unwrap_or(0) * 512;
        }
    }
    t
}

/// pid -> (comm, utime + stime)
fn proc_ticks() -> HashMap<u32, (String, u64)> {
    let mut r = HashMap::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|p| p.parse::<u32>().ok()) else { continue };
        let Ok(s) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        let (Some(a), Some(b)) = (s.find('('), s.rfind(')')) else { continue };
        let f: Vec<&str> = s[b + 2..].split_whitespace().collect();
        let t = f.get(11).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        r.insert(pid, (s[a + 1..b].to_string(), t));
    }
    r
}

/// highest temperature (°C) per group: cpu, gpu, video, battery
fn thermal() -> BTreeMap<&'static str, f64> {
    let mut t = BTreeMap::new();
    for e in std::fs::read_dir("/sys/class/thermal").into_iter().flatten().flatten() {
        if !e.file_name().to_string_lossy().starts_with("thermal_zone") {
            continue;
        }
        let kind = std::fs::read_to_string(e.path().join("type")).unwrap_or_default();
        let Some(c) = std::fs::read_to_string(e.path().join("temp")).ok().and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v / 1000.0) else { continue };
        if let Some(g) = ["cpu", "gpu", "video", "battery"].into_iter().find(|k| kind.starts_with(k))
            && c > -40.0
            && c < 150.0
        {
            let e = t.entry(g).or_insert(c);
            *e = e.max(c);
        }
    }
    t
}

fn rd_i64(path: &str) -> i64 {
    std::fs::read_to_string(path).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// The CPU clusters as (cpus, short name): the cores' names, the fastest single core of three or more clusters "prime".
fn clusters() -> Vec<(Vec<u32>, String)> {
    let c = mship_core::device::clusters();
    let n = c.len();
    c.into_iter().enumerate().map(|(i, c)| {
        let prime = n >= 3 && i == n - 1 && c.cpus.len() <= 2;
        (c.cpus, if prime { "prime".into() } else { c.core })
    }).collect()
}

struct Sample {
    cpu: Vec<(u64, u64)>,
    t: Instant,
    net: (u64, u64),
    dsk: (u64, u64),
    procs: HashMap<u32, (String, u64)>,
    res: Option<Resources>,
}

pub fn resources_page(s: &S) -> adw::NavigationPage {
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let total_mb = meminfo().get("MemTotal").copied().unwrap_or(1) as f64 / 1024.0;
    let m_cpu = Metric::new("CPU", (0.36, 0.6, 1.0), Some(100.0));
    let m_mem = Metric::new("Memory", (0.3, 0.8, 0.45), Some(total_mb));
    let m_gpu = Metric::new(&format!("GPU ({})", mship_core::device::gpu_name()), (0.75, 0.5, 1.0), Some(100.0));
    let m_pwr = Metric::new("Battery power", (1.0, 0.8, 0.25), None);
    let m_net = Metric::new("Network (Wi-Fi + USB)", (1.0, 0.55, 0.25), None);
    let m_dsk = Metric::new("Storage", (0.3, 0.8, 0.8), None);
    // per-core load: a column (bar + percentage) per CPU, grouped by cluster
    let cores = gtk::Box::builder().spacing(6).margin_start(12).margin_end(12).margin_top(10).margin_bottom(10).homogeneous(true).build();
    let mut bars = vec![];
    let clusters = clusters();
    for (cpus, name) in &clusters {
        for (k, _) in cpus.iter().enumerate() {
            let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
            let lb = gtk::LevelBar::builder().orientation(gtk::Orientation::Vertical).inverted(true).min_value(0.0).max_value(100.0).height_request(56).halign(gtk::Align::Center).build();
            for o in ["low", "high", "full"] {
                lb.remove_offset_value(Some(o));
            }
            let pct = gtk::Label::builder().label("").css_classes(["caption"]).build();
            col.append(&lb);
            col.append(&pct);
            col.append(&gtk::Label::builder().label(if k == 0 { name.as_str() } else { "" }).css_classes(["caption", "dim"]).build());
            cores.append(&col);
            bars.push((lb, pct));
        }
    }
    let r_cores = adw::PreferencesRow::builder().activatable(false).child(&cores).build();
    let r_temp = row("Temperatures", "…", None, false);
    let r_venus = row("Video decoder (Venus)", "…", None, false);
    // the CPU mode, then the live numbers
    let g_mode = group("", "");
    let r_mode = row("CPU mode", "Cool / Balanced / Performance, clocks per core cluster", Some("power-profile-performance-symbolic"), true);
    let me = Rc::downgrade(s);
    r_mode.connect_activated(move |_| {
        if let Some(s) = me.upgrade() {
            s.push(&crate::settings::cpu_page(&s));
        }
    });
    g_mode.add(&r_mode);
    let g_sys = group("Now", "Updated every 2 s; graphs show the last 2 minutes. Each VM's own use is on its page.");
    g_sys.add(&m_cpu.row);
    g_sys.add(&r_cores);
    g_sys.add(&m_mem.row);
    g_sys.add(&m_gpu.row);
    g_sys.add(&m_pwr.row);
    g_sys.add(&r_temp);
    g_sys.add(&m_net.row);
    g_sys.add(&m_dsk.row);
    g_sys.add(&r_venus);
    let g_top = group("Top processes", "% of one core");
    let top_rows: Vec<adw::ActionRow> = (0..6).map(|_| row("…", "", None, false)).collect();
    for r in &top_rows {
        g_top.add(r);
    }
    let st = Rc::new(RefCell::new(Sample { cpu: cpu_times(), t: Instant::now(), net: net_bytes(), dsk: disk_bytes(), procs: proc_ticks(), res: None }));

    let on_res = {
        let (st, r_venus, m_gpu) = (st.clone(), r_venus.clone(), m_gpu.clone());
        Rc::new(move |r: Resources| {
            let prev = st.borrow_mut().res.replace(r.clone());
            if r.venus_hz != 0 {
                r_venus.set_subtitle(&format!("core clock {:.0} MHz", r.venus_hz as f64 / 1e6));
            }
            let Some(prev) = prev else { return };
            let dt = (r.t - prev.t).max(1e-3);
            let gpu = (100.0 * (r.gpu_ns.saturating_sub(prev.gpu_ns)) as f64 / (dt * 1e9)).min(100.0);
            let freq = mship_core::device::gpu_devfreq().map_or(0, |g| rd_i64(&g.join("cur_freq").to_string_lossy())) as f64 / 1e6;
            m_gpu.set(&format!("{gpu:.0} %"), gpu, Some(&format!("{freq:.0} MHz")));
        })
    };
    let ask_res = {
        let (on_res, me) = (on_res.clone(), Rc::downgrade(s));
        move || {
            if let Some(s) = me.upgrade() {
                let on_res = on_res.clone();
                s.broker.get(Op::Resources, move |r: Result<Resources, String>| {
                    if let Ok(r) = r {
                        on_res(r);
                    }
                });
            }
        }
    };
    let sample = {
        let ask_res = ask_res.clone();
        move || {
            let mut stb = st.borrow_mut();
            let now = Instant::now();
            let dt = now.duration_since(stb.t).as_secs_f64().max(1e-3);
            let (cpu, net, dsk, procs) = (cpu_times(), net_bytes(), disk_bytes(), proc_ticks());
            let pc: Vec<f64> = cpu.iter().zip(&stb.cpu).map(|(c, p)| 100.0 * c.0.saturating_sub(p.0) as f64 / (c.1.saturating_sub(p.1)).max(1) as f64).collect();
            let clocks: Vec<String> = clusters.iter().map(|(cpus, name)| format!("{name} {}", mhz(rd_i64(&format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq", cpus.first().copied().unwrap_or(0))) as u64))).collect();
            if let Some(all) = pc.first() {
                m_cpu.set(&format!("{all:.0} %"), *all, Some(&clocks.join(" · ")));
            }
            for ((lb, lab), v) in bars.iter().zip(pc.iter().skip(1)) {
                lb.set_value(*v);
                lab.set_label(&format!("{v:.0}"));
            }
            let m = meminfo();
            let g = |k: &str| m.get(k).copied().unwrap_or(0) as f64;
            let used = (g("MemTotal") - g("MemAvailable")) / 1024.0;
            let swap = (g("SwapTotal") - g("SwapFree")) / 1024.0;
            // VM RAM is shared memory, which the kernel counts as cache: show what is available instead
            m_mem.set(&format!("{:.1} of {:.1} GB", used / 1024.0, total_mb / 1024.0), used,
                      Some(&format!("available {:.1} GB · swap {:.1} of {:.1} GB", g("MemAvailable") / 1048576.0, swap / 1024.0, g("SwapTotal") / 1048576.0)));
            let batt = mship_core::device::power_supply("Battery").unwrap_or_default();
            let w = rd_i64(&batt.join("power_now").to_string_lossy()).abs() as f64 / 1e6;
            let cur = rd_i64(&batt.join("current_now").to_string_lossy());
            m_pwr.set(&format!("{w:.1} W"), w, Some(if cur > 0 { "charging" } else { "from the battery" }));
            let t = thermal();
            let names = [("cpu", "CPU"), ("gpu", "GPU"), ("video", "video"), ("battery", "battery")];
            r_temp.set_subtitle(&names.iter().filter_map(|(k, n)| t.get(k).map(|v| format!("{n} {v:.0} °C"))).collect::<Vec<_>>().join(" · "));
            let (rx, tx) = (net.0.saturating_sub(stb.net.0) as f64 / dt, net.1.saturating_sub(stb.net.1) as f64 / dt);
            m_net.set(&fmt_rate(rx + tx), rx + tx, Some(&format!("↓ {} · ↑ {}", fmt_rate(rx), fmt_rate(tx))));
            let (rd, wr) = (dsk.0.saturating_sub(stb.dsk.0) as f64 / dt, dsk.1.saturating_sub(stb.dsk.1) as f64 / dt);
            m_dsk.set(&fmt_rate(rd + wr), rd + wr, Some(&format!("read {} · write {}", fmt_rate(rd), fmt_rate(wr))));
            let mut busy: Vec<(f64, String, u32)> = procs
                .iter()
                .filter_map(|(p, (comm, tk))| stb.procs.get(p).map(|o| (100.0 * tk.saturating_sub(o.1) as f64 / tck / dt, comm.clone(), *p)))
                .collect();
            busy.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (r, (c, comm, p)) in top_rows.iter().zip(busy) {
                r.set_title(&esc(&comm));
                r.set_subtitle(&format!("{c:.0} % · pid {p}"));
            }
            stb.cpu = cpu;
            stb.t = now;
            stb.net = net;
            stb.dsk = dsk;
            stb.procs = procs;
            drop(stb);
            ask_res();
        }
    };
    let p = page("Performance", &[&g_mode, &g_sys, &g_top], None);
    ask_res();
    s.live(&p.page, 2, sample);
    p.page
}

// ------------------------------------------------------------------ power votes (diagnostic)
fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn word<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

/// mothershipd's power table, raw: the verdict, floors, and every participant's fields as reported.
fn power_text(p: Option<&PowerState>) -> String {
    let Some(p) = p else { return "mothershipd: NOT CONNECTED (not running, or restarting)".into() };
    let now = now_s();
    let mut out = vec![format!("sleep       {}", if p.sleep.allowed { "ALLOWED" } else { "BLOCKED" })];
    out.extend(p.sleep.blockers.iter().map(|b| format!("  blocker   {b}")));
    let floors: Vec<String> = p.floors.iter().map(|(k, v)| format!("{}={}", word(k), word(v))).collect();
    out.push(format!("floors      {}", if floors.is_empty() { "-".into() } else { floors.join(" ") }));
    out.push(format!("applied     {}", p.applied.iter().map(|(k, v)| format!("{k}={}", v.map_or("-".into(), |v| v.to_string()))).collect::<Vec<_>>().join(" ")));
    let g = &p.guard;
    let caps: Vec<String> = g.caps.iter().map(|(k, v)| format!("{k}={v}")).collect();
    out.push(format!("cpu guard   limit={} hottest={} throttled={} caps={}", g.limit.map_or("None".into(), |l| l.to_string()), g.hottest, g.throttled,
                     if caps.is_empty() { "-".into() } else { caps.join(",") }));
    for (n, e) in &p.participants {
        let age = e.updated.map_or("-".into(), |u| format!("{:.0}s", now - u));
        let demand: Vec<String> = e.demand.iter().map(|(k, v)| format!("{}={}", word(k), word(v))).collect();
        out.push(String::new());
        out.push(format!("[{n}]{}", e.name.as_ref().filter(|x| *x != n).map_or(String::new(), |x| format!("  {x}"))));
        out.push(format!("  state      {}", word(&e.state)));
        out.push(format!("  may_sleep  {}", e.may_sleep));
        out.push(format!("  demand     {}", if demand.is_empty() { "-".into() } else { demand.join(" ") }));
        out.push(format!("  source     {}   seq {}   updated {age} ago", e.source, e.seq));
        out.push(format!("  reason     {}", if e.reason.is_empty() { "-" } else { &e.reason }));
    }
    out.join("\n")
}

pub fn power_page(s: &S) -> adw::NavigationPage {
    let text = gtk::Label::builder().xalign(0.0).yalign(0.0).selectable(true).wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["monospace"]).margin_start(14).margin_end(14).margin_top(10).margin_bottom(10).build();
    let g_raw = group("mship.power", "mothershipd power state, live");
    g_raw.add(&adw::PreferencesRow::builder().activatable(false).child(&text).build());
    let act = Rows::new(group("VM actions", ""));
    let upd = {
        let (me, act) = (Rc::downgrade(s), act.clone());
        move || {
            let Some(s) = me.upgrade() else { return };
            let p: Option<PowerState> = s.broker.state(Topic::Power).and_then(|v| serde_json::from_value(v).ok());
            text.set_label(&power_text(p.as_ref()));
            act.clear();
            for (n, e) in p.map(|p| p.participants).unwrap_or_default() {
                if n == "mothershipOS" || !std::path::Path::new(VMS).join(&n).is_dir() {
                    continue;
                }
                let asleep = e.state == VoteState::Suspended;
                let r = row(&n, &word(&e.state), None, false);
                let me = Rc::downgrade(&s);
                r.add_suffix(&button(if asleep { "Wake" } else { "Sleep now" }, None, move || {
                    if let Some(s) = me.upgrade() {
                        s.vm_power(&n, asleep);
                    }
                }));
                act.add(&r);
            }
        }
    };
    upd();
    let p = page("Power votes", &[&g_raw, &act.group], None);
    let upd = Rc::new(upd);
    let u2 = upd.clone();
    s.follow(&p.page, &[Topic::Power], move || u2()); // changes at once; the timer keeps the ages moving
    s.live(&p.page, 2, move || upd());
    p.page
}

// ------------------------------------------------------------------ about
pub fn about_page(_s: &S) -> adw::NavigationPage {
    let g = group("", "");
    let mut un: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut un) };
    let cstr = |b: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned();
    let m = meminfo();
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statvfs(c"/".as_ptr(), &mut st) };
    let up: f64 = std::fs::read_to_string("/proc/uptime").ok().and_then(|u| u.split_whitespace().next()?.parse().ok()).unwrap_or(0.0);
    let gb = |blocks: u64| blocks * st.f_frsize / (1 << 30);
    for (t, sub) in [
        ("Device", std::fs::read_to_string("/sys/firmware/devicetree/base/model").map(|m| m.trim_end_matches('\0').to_string()).unwrap_or_else(|_| "unknown".into())),
        ("Kernel", format!("{} ({}), {}", cstr(&un.release), cstr(&un.machine),
                           if std::path::Path::new("/dev/kvm").exists() { "KVM at EL2" } else { "no KVM (EL1): no VMs" })),
        ("Memory", format!("{} MB free of {} MB", m.get("MemAvailable").copied().unwrap_or(0) / 1024, m.get("MemTotal").copied().unwrap_or(0) / 1024)),
        ("Storage", format!("{} GB free of {} GB", gb(st.f_bavail), gb(st.f_blocks))),
        ("Uptime", format!("{} h {} min", (up / 3600.0) as u64, (up % 3600.0 / 60.0) as u64)),
        ("mothershipOS", mship_core::RELEASE.to_string()),
    ] {
        g.add(&row(t, &sub, None, false));
    }
    // diagnostics
    let g_diag = group("", "");
    let r_votes = row("Power votes", "diagnostic: the raw mship.power table", Some("utilities-terminal-symbolic"), true);
    let me = Rc::downgrade(_s);
    r_votes.connect_activated(move |_| {
        if let Some(s) = me.upgrade() {
            s.push(&power_page(&s));
        }
    });
    g_diag.add(&r_votes);
    page("About", &[&g, &g_diag], None).page
}
