//! Resources, the raw power votes, hardware owners, VM memory, a VM's settings, about.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use mship_core::api::{Op, PowerState, Topic, VoteState};
use mship_core::status::Resources;
use serde_json::Value;

use crate::ui::{Metric, Rows, button, esc, fmt_rate, group, mhz, page, row, strings};
use crate::{HW, HW_SCREEN, S, VMS, base, vm_conf, vm_name};

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
    for l in std::fs::read_to_string("/proc/net/dev").unwrap_or_default().lines() {
        let Some((name, rest)) = l.split_once(':') else { continue };
        if ["wlp1s0", "usb0"].contains(&name.trim()) {
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

const CORES: &[(usize, usize, &str)] = &[(0, 4, "A55"), (4, 7, "A77"), (7, 8, "prime")];

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
    let m_gpu = Metric::new("GPU (Adreno 650)", (0.75, 0.5, 1.0), Some(100.0));
    let m_pwr = Metric::new("Battery power", (1.0, 0.8, 0.25), None);
    let m_net = Metric::new("Network (Wi-Fi + USB)", (1.0, 0.55, 0.25), None);
    let m_dsk = Metric::new("Storage", (0.3, 0.8, 0.8), None);
    // per-core load: a column (bar + percentage) per CPU, grouped by cluster
    let cores = gtk::Box::builder().spacing(6).margin_start(12).margin_end(12).margin_top(10).margin_bottom(10).homogeneous(true).build();
    let mut bars = vec![];
    for (first, last, name) in CORES {
        for i in *first..*last {
            let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
            let lb = gtk::LevelBar::builder().orientation(gtk::Orientation::Vertical).inverted(true).min_value(0.0).max_value(100.0).height_request(56).halign(gtk::Align::Center).build();
            for o in ["low", "high", "full"] {
                lb.remove_offset_value(Some(o));
            }
            let pct = gtk::Label::builder().label("").css_classes(["caption"]).build();
            col.append(&lb);
            col.append(&pct);
            col.append(&gtk::Label::builder().label(if i == *first { *name } else { "" }).css_classes(["caption", "dim"]).build());
            cores.append(&col);
            bars.push((lb, pct));
        }
    }
    let r_cores = adw::PreferencesRow::builder().activatable(false).child(&cores).build();
    let r_temp = row("Temperatures", "…", None, false);
    let r_venus = row("Video decoder (Venus)", "…", None, false);
    let g_sys = group("Phone", "Updated every 2 s; graphs show the last 2 minutes");
    g_sys.add(&m_cpu.row);
    g_sys.add(&r_cores);
    g_sys.add(&m_mem.row);
    g_sys.add(&m_gpu.row);
    g_sys.add(&m_pwr.row);
    g_sys.add(&r_temp);
    g_sys.add(&m_net.row);
    g_sys.add(&m_dsk.row);
    g_sys.add(&r_venus);
    let vms = Rows::new(group("Virtual machines", "CPU in % of one core, as top shows it"));
    let g_top = group("Top processes", "% of one core");
    let top_rows: Vec<adw::ActionRow> = (0..6).map(|_| row("…", "", None, false)).collect();
    for r in &top_rows {
        g_top.add(r);
    }
    let st = Rc::new(RefCell::new(Sample { cpu: cpu_times(), t: Instant::now(), net: net_bytes(), dsk: disk_bytes(), procs: proc_ticks(), res: None }));

    let on_res = {
        let (st, vms, r_venus, m_gpu) = (st.clone(), vms.clone(), r_venus.clone(), m_gpu.clone());
        Rc::new(move |r: Resources| {
            let prev = st.borrow_mut().res.replace(r.clone());
            vms.clear();
            if r.vms.is_empty() {
                vms.add(&row("No VM is running", "", None, false));
            }
            if r.venus_hz != 0 {
                r_venus.set_subtitle(&format!("core clock {:.0} MHz", r.venus_hz as f64 / 1e6));
            }
            let Some(prev) = prev else { return };
            let dt = (r.t - prev.t).max(1e-3);
            let gpu = (100.0 * (r.gpu_ns.saturating_sub(prev.gpu_ns)) as f64 / (dt * 1e9)).min(100.0);
            let freq = rd_i64("/sys/class/devfreq/3d00000.gpu/cur_freq") as f64 / 1e6;
            m_gpu.set(&format!("{gpu:.0} %"), gpu, Some(&format!("{freq:.0} MHz")));
            for v in &r.vms {
                let Some(o) = prev.vms.iter().find(|o| o.dir == v.dir) else { continue };
                let cpu = 100.0 * v.ticks.saturating_sub(o.ticks) as f64 / tck / dt;
                let g = (100.0 * v.gpu_ns.saturating_sub(o.gpu_ns) as f64 / (dt * 1e9)).min(100.0);
                vms.add(&row(&v.name, &format!("CPU {cpu:.0} % · RAM {:.1} GB · GPU {g:.0} %", v.pss_kb as f64 / 1048576.0), Some("computer-symbolic"), false));
            }
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
            let clocks: Vec<String> = CORES.iter().map(|(first, _, name)| format!("{name} {}", mhz(rd_i64(&format!("/sys/devices/system/cpu/cpu{first}/cpufreq/scaling_cur_freq")) as u64))).collect();
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
            let w = rd_i64("/sys/class/power_supply/battery/power_now").abs() as f64 / 1e6;
            let cur = rd_i64("/sys/class/power_supply/battery/current_now");
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
    let p = page("Resources", &[&g_sys, &vms.group, &g_top], None);
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

// ------------------------------------------------------------------ a VM's settings
pub fn vm_page(s: &S, d: &str) -> adw::NavigationPage {
    let path = format!("{VMS}/{d}");
    let g_now = group("Now", "");
    let r_state = row("State", "…", Some("computer-symbolic"), false);
    g_now.add(&r_state);
    let (me, dd) = (Rc::downgrade(s), d.to_string());
    let give = button("Give now", Some("suggested-action"), move || {
        let Some(s) = me.upgrade() else { return };
        let (me2, dd2) = (Rc::downgrade(&s), dd.clone());
        s.ask(Op::HandoverGive { vm: dd.clone(), items: vec![] }, move |_| {
            if let Some(s) = me2.upgrade() {
                s.toast(&format!("{} has its hardware", vm_name(&dd2)));
            }
        });
    });
    let me = Rc::downgrade(s);
    let take = button("Take back", None, move || {
        let Some(s) = me.upgrade() else { return };
        let me2 = Rc::downgrade(&s);
        s.ask(Op::HandoverTake { items: vec![] }, move |_| {
            if let Some(s) = me2.upgrade() {
                s.toast("Hardware is back with mothershipOS");
            }
        });
    });
    let r_act = row("Hardware", "lend its assigned devices now, or take everything back", None, false);
    r_act.add_suffix(&take);
    r_act.add_suffix(&give);
    g_now.add(&r_act);
    let g = group("Assigned hardware", "");
    let assoc = s.handover().assoc;
    let assigned: Vec<_> = HW.iter().filter(|(k, _, _)| assoc.get(*k).map(String::as_str) == Some(d)).collect();
    for (k, t, i) in &assigned {
        g.add(&row(t, if HW_SCREEN.contains(k) { "while shown" } else { "while running" }, Some(i), false));
    }
    if assigned.is_empty() {
        g.add(&row("Nothing assigned", "everything stays with mothershipOS", None, false));
    }
    let r_edit = row("Change assignments", "Settings › Hardware: one owner per device", Some("drive-multidisk-symbolic"), true);
    let me = Rc::downgrade(s);
    r_edit.connect_activated(move |_| {
        if let Some(s) = me.upgrade() {
            s.push(&hw_page(&s));
        }
    });
    g.add(&r_edit);
    let upd = {
        let (me, d) = (Rc::downgrade(s), d.to_string());
        move || {
            let Some(s) = me.upgrade() else { return };
            let run = s.vm_info(&d).is_some_and(|v| v.running);
            let own = s.handover().holders;
            let held: Vec<&str> = HW.iter().filter(|(k, _, _)| own.get(*k) == Some(&path)).map(|(_, t, _)| *t).collect();
            r_state.set_subtitle(&format!("{}{}", if run { "running" } else { "stopped" },
                                          if held.is_empty() { String::new() } else { format!(" · has: {}", held.join(", ").to_lowercase()) }));
            give.set_sensitive(run);
            take.set_sensitive(!held.is_empty());
        }
    };
    upd();
    let title = vm_conf(d)["name"].as_str().map(String::from).unwrap_or(d.to_string());
    let p = page(&title, &[&g_now, &g], None);
    s.follow(&p.page, &[Topic::Vms, Topic::Handover], upd);
    p.page
}

// ------------------------------------------------------------------ hardware assignment
const HW_OTHER: &[(&str, &str, &str)] = &[
    ("Speaker & microphones", "mothershipOS", "audio-speakers-symbolic"),
    ("Camera (OV64B)", "mothershipOS", "camera-photo-symbolic"),
    ("USB-C port to a PC", "mothershipOS (dev link)", "computer-symbolic"),
    ("GPU (Adreno 650)", "shared by every VM", "video-display-symbolic"),
    ("Battery status", "shared by every VM, read-only", "battery-good-symbolic"),
    ("Modem, sensors", "mothershipOS", "network-cellular-symbolic"),
];

pub fn hw_page(s: &S) -> adw::NavigationPage {
    let vms: Vec<String> = s.vm_list().into_iter().map(|v| v.vm).collect();
    let mut owners = vec!["host".to_string()];
    owners.extend(vms.iter().cloned());
    let mut labels = vec!["mothershipOS".to_string()];
    labels.extend(vms.iter().map(|d| vm_name(d)));
    let g = group("Owner", "Each device has one owner. A VM gets its devices automatically: the display and buttons while it is shown, Wi-Fi bands and shared internet while it runs. Holding both volume buttons brings the screen back to mothershipOS.");
    let assoc = s.handover().assoc;
    let mut rows: Vec<(&str, adw::ComboRow)> = vec![];
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    for (key, title, icon) in HW {
        let cr = adw::ComboRow::builder().title(*title).model(&strings(&label_refs)).build();
        cr.add_prefix(&gtk::Image::from_icon_name(icon));
        let cur = assoc.get(*key).cloned().unwrap_or_else(|| "host".into());
        cr.set_selected(owners.iter().position(|o| *o == cur).unwrap_or(0) as u32);
        let (me, owners, labels) = (Rc::downgrade(s), owners.clone(), labels.clone());
        cr.connect_selected_notify(move |w| {
            let Some(s) = me.upgrade() else { return };
            let i = w.selected() as usize;
            let (me2, label) = (Rc::downgrade(&s), labels[i].clone());
            s.ask(Op::HandoverAssign { item: key.to_string(), owner: owners[i].clone() }, move |_| {
                if let Some(s) = me2.upgrade() {
                    s.toast(&format!("{title} → {label}"));
                }
            });
        });
        g.add(&cr);
        rows.push((key, cr));
    }
    let g2 = group("Not lendable yet", "");
    for (t, sub, icon) in HW_OTHER {
        g2.add(&row(t, sub, Some(icon), false));
    }
    let upd = {
        let me = Rc::downgrade(s);
        move || {
            let Some(s) = me.upgrade() else { return };
            let h = s.handover();
            for (key, r) in &rows {
                let holder = h.holders.get(*key).cloned().unwrap_or_else(|| "host".into());
                let owner = h.assoc.get(*key).cloned().unwrap_or_else(|| "host".into());
                let text = if holder != "host" {
                    format!("in use by {}", vm_name(base(&holder)))
                } else if owner != "host" {
                    format!("{} · lent while it {}", vm_name(&owner), if HW_SCREEN.contains(key) { "is shown" } else { "runs" })
                } else {
                    "mothershipOS".into()
                };
                r.set_subtitle(&esc(&text));
            }
        }
    };
    upd();
    let p = page("Hardware", &[&g, &g2], None);
    s.follow(&p.page, &[Topic::Handover], upd);
    p.page
}

// ------------------------------------------------------------------ VM memory
/// A VM's RAM limit (MB): vm.json resources.memory_mb, None = the whole pool
fn vm_mem(d: &str) -> Option<u64> {
    vm_conf(d)["resources"]["memory_mb"].as_u64()
}

/// resources.memory_mb into the VM's vm.json (None removes it), keeping everything else (atomic replace)
fn vm_set_mem(d: &str, mb: Option<u64>) -> std::io::Result<()> {
    let path = format!("{VMS}/{d}/vm.json");
    let mut conf = vm_conf(d);
    if !conf.is_object() {
        conf = Value::Object(Default::default());
    }
    if !conf["resources"].is_object() {
        conf["resources"] = Value::Object(Default::default());
    }
    let r = conf["resources"].as_object_mut().unwrap();
    match mb {
        Some(mb) => r.insert("memory_mb".into(), mb.into()),
        None => r.remove("memory_mb"),
    };
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&conf).unwrap_or_default() + "\n")?;
    std::fs::rename(tmp, path)
}

pub fn vm_mem_page(s: &S) -> adw::NavigationPage {
    let total = meminfo().get("MemTotal").copied().unwrap_or(0) / 1024;
    let pool = mship_core::pool::vm_size_mb(total, None);
    let vms: Vec<String> = s.vm_list().into_iter().filter(|v| v.manifest).map(|v| v.vm).collect();
    let g = group("Memory and CPUs", &format!("VMs share the phone: each may use all {} CPUs and up to {:.1} GB of RAM \
        (the phone's {:.1} GB less what mothershipOS needs), taking what it uses as it runs. VMs that are awake share \
        alike, whichever one is shown; a VM whose screen is off gets less CPU time and gives back memory first. A \
        limit caps one VM's RAM; it applies the next time the VM starts.",
        mship_core::pool::host_cpus().len(), pool as f64 / 1024.0, total as f64 / 1024.0));
    for d in &vms {
        let limit = vm_mem(d);
        let ex = adw::ExpanderRow::new();
        ex.set_title(&esc(&vm_name(d)));
        ex.set_show_enable_switch(true);
        ex.set_enable_expansion(limit.is_some());
        ex.set_expanded(limit.is_some());
        let running = s.vm_info(d).is_some_and(|v| v.running);
        let sub = move |on: bool| format!("{}{}", if on { "Limited" } else { "Shares the pool" },
                                          if running { " · running: restart it to apply" } else { "" });
        ex.set_subtitle(&sub(limit.is_some()));
        ex.add_prefix(&gtk::Image::from_icon_name(if d == "pixelos" { "phone-symbolic" } else { "computer-symbolic" }));
        let min = mship_core::pool::GUEST_MIN_MB as f64;
        let adj = gtk::Adjustment::new(limit.unwrap_or(pool) as f64, min, pool.max(min as u64) as f64, 256.0, 1024.0, 0.0);
        let sp = adw::SpinRow::new(Some(&adj), 256.0, 0);
        sp.set_title("Limit");
        sp.set_subtitle("MB");
        ex.add_row(&sp);
        let save = {
            let (me, d, ex, sp) = (Rc::downgrade(s), d.clone(), ex.clone(), sp.clone());
            Rc::new(move || {
                let on = ex.enables_expansion();
                ex.set_subtitle(&sub(on));
                if let (Err(e), Some(s)) = (vm_set_mem(&d, on.then(|| sp.value() as u64)), me.upgrade()) {
                    s.toast(&format!("Could not save: {e}"));
                }
            })
        };
        let f = save.clone();
        ex.connect_enable_expansion_notify(move |_| f());
        sp.connect_value_notify(move |_| save());
        g.add(&ex);
    }
    if vms.is_empty() {
        g.add(&row("No virtual machines", "", None, false));
    }
    page("Virtual machines", &[&g], None).page
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
        ("Device", "Motorola nio (Snapdragon 870) — mothershipOS host".to_string()),
        ("Kernel", format!("{} ({}), KVM at EL2", cstr(&un.release), cstr(&un.machine))),
        ("Memory", format!("{} MB free of {} MB", m.get("MemAvailable").copied().unwrap_or(0) / 1024, m.get("MemTotal").copied().unwrap_or(0) / 1024)),
        ("Storage", format!("{} GB free of {} GB", gb(st.f_bavail), gb(st.f_blocks))),
        ("Uptime", format!("{} h {} min", (up / 3600.0) as u64, (up % 3600.0 / 60.0) as u64)),
        ("mothershipOS", mship_core::RELEASE.to_string()),
    ] {
        g.add(&row(t, &sub, None, false));
    }
    page("About", &[&g], None).page
}
