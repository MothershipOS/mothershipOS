//! Power: mship.power 1.0 (docs/contracts/mship.power-1.md).
//!
//! Every participant states what it wants; the host decides. Participants: "mothershipOS" (built here from the
//! phone's screen, USB dev link, external display, charging) and every running VM. A VM's vote comes from its guest
//! adapter over vsock if it has one (host CID 2, port 7720; 4-byte big-endian length + UTF-8 JSON, as mship.wifi),
//! identified by the peer CID (vm.json "cid"); otherwise the host builds it from what it sees of the VM: its
//! display (on, or blanked: the presenter reports `vm.display`) and its video decoder sessions (crosvm has the
//! decoder open). A running VM without either is "unknown" and blocks sleep.
//!
//! Every pass (2 s): sleep is allowed when every participant is fresh and either sleeping/suspended or not active
//! with may_sleep; floors (the highest level asked for) go to CPU scaling_min_freq and GPU devfreq min_freq, below
//! the caps the host sets (CPU mode, CPU guard); codec is recorded only. A VM's own display output follows its vote.
//! The phone suspends (suspend-to-idle) once its screen has been off for the sleep delay and sleep is allowed; it
//! powers off at critical battery.
//!
//! VM sleep: a guest suspends itself (its kernel's PSCI SYSTEM_SUSPEND: crosvm parks it and asks for vm.sleep;
//! or "sleep" over a guest adapter's channel), or the host puts it to sleep (vm.sleep). The VM's vCPUs are frozen (crosvm suspend), its display output
//! switched off, its USB devices taken back (mship-usb) and their input devices grabbed; the first key, button or
//! motion, or vm.wake, hands them back and resumes it. A frozen VM counts as suspended.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use mship_core::api::{Level, PowerState, Resource, SleepVerdict, Vote, VoteRequest, VoteState, POWER_VSOCK_PORT};
use mship_core::status::GuardState;
use mship_host::util::{Result, fail, load_value, rd, rd_u64, run, wr};
use mship_host::{power as hpower, vm};
use serde_json::{Value, json};

const CPUFREQ: &str = "/sys/devices/system/cpu/cpufreq";
const GPU: &str = "/sys/class/devfreq/3d00000.gpu";
const UDC: &str = "/sys/class/udc/a600000.usb/state";
const BATT: &str = "/sys/class/power_supply/battery";
const CROSVM: &str = "/opt/mship-gpu/bin/crosvm";
const USB_CTL: &str = "/run/mothership/usb.sock";
const VERSIONS: &[&str] = &["1.0"];
/// s without a set/heartbeat from a guest: its vote no longer counts
const STALE: f64 = 150.0;
const MAX_MSG: u32 = 64 * 1024;
const CRITICAL_PCT: i64 = 2;
/// _IOW('E', 0x90, int)
const EVIOCGRAB: libc::c_ulong = 0x40044590;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
pub const HOST: &str = "mothershipOS";

fn now() -> f64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

// ---------------------------------------------------------------- pure policy (tested)
/// Does this participant agree to a physical host suspend now?
pub fn asleep(v: &Vote) -> bool {
    matches!(v.state, VoteState::Sleeping | VoteState::Suspended)
        || (!matches!(v.state, VoteState::Active | VoteState::Unknown) && v.may_sleep)
}

fn state_name(s: VoteState) -> String {
    serde_json::to_value(s).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

/// (blockers, floors) for a participant table.
pub fn decide(table: &BTreeMap<String, Vote>) -> (Vec<String>, BTreeMap<Resource, Level>) {
    let blockers = table
        .iter()
        .filter(|(_, v)| !asleep(v))
        .map(|(n, v)| {
            let who = v.name.clone().unwrap_or_else(|| n.clone());
            if n == HOST { format!("{who}: {}", v.reason) } else { format!("{who}: {}", state_name(v.state)) }
        })
        .collect();
    let mut floors = BTreeMap::new();
    for v in table.values() {
        for (r, l) in &v.demand {
            let e = floors.entry(*r).or_insert(*l);
            *e = (*e).max(*l);
        }
    }
    (blockers, floors)
}

/// The frequency for a named level within a table, never above `top` (the caps the host set).
pub fn freq_floor(freqs: &[u64], top: u64, level: Level) -> u64 {
    let mut t: Vec<u64> = freqs.iter().copied().filter(|f| *f <= top).collect();
    if t.is_empty() {
        t = freqs.iter().take(1).copied().collect();
    }
    match level {
        Level::Low => t[0],
        Level::Balanced => t[t.len() / 3],
        Level::High => t[t.len() - 1],
    }
}

fn vote_from(r: VoteRequest, source: &str) -> Vote {
    let mut reason = r.reason;
    reason.truncate(200);
    Vote { state: r.state, may_sleep: r.may_sleep, demand: r.demand, reason, seq: r.seq, updated: Some(now()),
           source: source.into(), name: None }
}

// ---------------------------------------------------------------- guest framing
fn send(w: &mut impl Write, v: &Value) -> std::io::Result<()> {
    let b = serde_json::to_vec(v).map_err(std::io::Error::other)?;
    w.write_all(&(b.len() as u32).to_be_bytes())?;
    w.write_all(&b)
}

fn recv(r: &mut impl Read) -> std::io::Result<Value> {
    let mut n = [0u8; 4];
    r.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n);
    if n > MAX_MSG {
        return Err(std::io::Error::other("message too large"));
    }
    let mut b = vec![0u8; n as usize];
    r.read_exact(&mut b)?;
    serde_json::from_slice(&b).map_err(std::io::Error::other)
}

// ---------------------------------------------------------------- system access
/// {vm dir name: vm.json} of the running VMs
fn running_vms() -> BTreeMap<String, Value> {
    vm::names()
        .into_iter()
        .filter(|n| vm::running(&Path::new(vm::VMS).join(n)))
        .map(|n| {
            let c = load_value(Path::new(vm::VMS).join(&n).join("vm.json")).unwrap_or_default();
            (n, c)
        })
        .collect()
}

fn vm_for_cid(cid: u32) -> Option<String> {
    vm::names().into_iter().find(|n| {
        load_value(Path::new(vm::VMS).join(n).join("vm.json")).is_some_and(|c| c["cid"].as_u64() == Some(cid as u64))
    })
}

/// The phone's own vote: what mothershipOS itself needs.
fn host_vote() -> Vote {
    let conf = hpower::power_conf();
    let screen_on = hpower::screen_on();
    let mut hold = vec![];
    if !conf.sleep {
        hold.push("sleep disabled in Settings");
    }
    if screen_on {
        hold.push("screen on");
    }
    // suspended, the phone stops answering the monitor's USB PD and a monitor can wedge (2026-10-19)
    let dp = std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten().any(|e| {
        e.file_name().to_string_lossy().contains("-DP-") && rd(e.path().join("status"), "") == "connected"
    });
    if dp {
        hold.push("external display connected");
    }
    // the charge limit is enforced by the driver's poll loop, which does not run while suspended
    let end: u32 = rd(Path::new(BATT).join("charge_control_end_threshold"), "100").parse().unwrap_or(100);
    if end < 100 && rd(Path::new(BATT).join("status"), "") == "Charging" {
        hold.push("charging to the charge limit");
    }
    if conf.stay_awake_usb && rd(UDC, "") == "configured" {
        hold.push("USB dev link");
    }
    Vote {
        state: if screen_on { VoteState::Active } else { VoteState::ScreenOff },
        may_sleep: hold.is_empty(),
        demand: BTreeMap::new(),
        reason: if hold.is_empty() { "idle".into() } else { hold.join(", ") },
        seq: 0,
        updated: Some(now()),
        source: "host".into(),
        name: None,
    }
}

fn table_of(path: impl AsRef<Path>) -> Vec<u64> {
    let mut t: Vec<u64> = rd(path, "").split_whitespace().filter_map(|f| f.parse().ok()).collect();
    t.sort();
    t
}

fn apply_floors(floors: &BTreeMap<Resource, Level>) -> BTreeMap<String, Option<u64>> {
    let mut applied = BTreeMap::new();
    let lvl = floors.get(&Resource::Cpu).copied().unwrap_or(Level::Low);
    for p in mship_host::cpu::policies() {
        let b = Path::new(CPUFREQ).join(&p);
        let freqs = table_of(b.join("scaling_available_frequencies"));
        if freqs.is_empty() {
            continue;
        }
        let top = rd(b.join("scaling_max_freq"), "").parse().unwrap_or(freqs[freqs.len() - 1]); // CPU mode + guard caps
        let want = freq_floor(&freqs, top, lvl);
        if rd_u64(b.join("scaling_min_freq")) != want {
            let _ = wr(b.join("scaling_min_freq"), want);
        }
        applied.insert(format!("cpu.{p}"), Some(want));
    }
    let freqs = table_of(Path::new(GPU).join("available_frequencies"));
    if !freqs.is_empty() {
        let top = rd(Path::new(GPU).join("max_freq"), "").parse().unwrap_or(freqs[freqs.len() - 1]);
        let want = freq_floor(&freqs, top, floors.get(&Resource::Gpu).copied().unwrap_or(Level::Low));
        if rd_u64(Path::new(GPU).join("min_freq")) != want {
            let _ = wr(Path::new(GPU).join("min_freq"), want);
        }
        applied.insert("gpu".into(), Some(want));
    }
    applied.insert("codec".into(), None); // no control yet: Venus clocks follow the decode sessions' declared load
    applied
}

fn usb_ctl(cmd: &str) -> String {
    mship_core::ctl::request(USB_CTL, cmd, Duration::from_secs(10)).unwrap_or_else(|e| format!("error: {e}"))
}

/// Switch a VM's own display output (its mship-vm-display presenter's DRM lease, e.g. the USB-C monitor):
/// SIGUSR1 turns its CRTC off (the monitor loses the signal), SIGUSR2 on again with the last frame.
pub fn vm_display(vm: &str, on: bool) {
    let sock = Path::new(vm::VMS).join(vm).join("display.sock");
    let sock = sock.to_string_lossy();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|p| p.parse::<i32>().ok()) else { continue };
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else { continue };
        let argv: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
        if argv[0].ends_with(b"mship-presenter") && argv.iter().any(|a| *a == sock.as_bytes()) {
            unsafe { libc::kill(pid, if on { libc::SIGUSR2 } else { libc::SIGUSR1 }) };
        }
    }
}

/// The vote of a VM without a guest adapter, from what the host sees: its display, and its video decoder sessions
/// (crosvm has the decoder's /dev/video* open while the guest decodes).
fn seen_vote(vm: &str, display_on: bool, name: Option<String>) -> Vote {
    let decoding = decoder_open(vm);
    let demand = if decoding { BTreeMap::from([(Resource::Codec, Level::High)]) } else { BTreeMap::new() };
    let (state, reason) = match (display_on, decoding) {
        (true, _) => (VoteState::Active, "display on"),
        (false, true) => (VoteState::ScreenOff, "display blanked, decoding video"),
        (false, false) => (VoteState::ScreenOff, "display blanked"),
    };
    Vote { state, may_sleep: !display_on && !decoding, demand, reason: reason.into(), seq: 0, updated: Some(now()),
           source: "host".into(), name }
}

/// The VM's crosvm (the process with its control socket on the command line) has a video device open.
fn decoder_open(vm: &str) -> bool {
    let sock = Path::new(vm::VMS).join(vm).join("crosvm.sock");
    let sock = sock.to_string_lossy();
    std::fs::read_dir("/proc").into_iter().flatten().flatten().any(|e| {
        std::fs::read(e.path().join("cmdline")).is_ok_and(|raw| raw.split(|b| *b == 0).any(|a| a == sock.as_bytes()))
            && std::fs::read_dir(e.path().join("fd")).into_iter().flatten().flatten().any(|fd| {
                std::fs::read_link(fd.path()).is_ok_and(|l| l.to_string_lossy().starts_with("/dev/video"))
            })
    })
}

fn crosvm(cmd: &str, vm: &str) -> bool {
    run("timeout", &["20", CROSVM, cmd, &Path::new(vm::VMS).join(vm).join("crosvm.sock").to_string_lossy()]).ok()
}

/// Event devices of USB input hardware (the devices the host took back from sleeping VMs).
fn usb_inputs() -> Vec<String> {
    std::fs::read_dir("/sys/class/input")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
        .filter(|e| std::fs::canonicalize(e.path()).is_ok_and(|p| p.to_string_lossy().contains("/usb")))
        .map(|e| format!("/dev/input/{}", e.file_name().to_string_lossy()))
        .collect()
}

fn battery_critical() -> bool {
    let cap: Option<i64> = rd(Path::new(BATT).join("capacity"), "").parse().ok();
    cap.is_some_and(|c| c <= CRITICAL_PCT) && rd(Path::new(BATT).join("status"), "") == "Discharging"
}

// ---------------------------------------------------------------- the domain
struct Frozen {
    since: f64,
    /// it held the USB devices
    usb: bool,
    /// the guest's suspend call, answered "resumed" after the wake
    waiter: Option<vsock::VsockStream>,
}

#[derive(Default)]
struct Inner {
    /// participant -> its vote (VMs keyed by VM dir name)
    entries: BTreeMap<String, Vote>,
    /// VM -> open guest connections (agent, and the sleep helper while asleep)
    sessions: HashMap<String, BTreeSet<u64>>,
    next_session: u64,
    frozen: BTreeMap<String, Frozen>,
    /// VM -> its display output is switched off
    display_off: BTreeMap<String, bool>,
    /// VM -> its guest's display is on, as its presenter sees it (VMs without a guest adapter)
    screens: BTreeMap<String, bool>,
}

pub struct Power {
    inner: Mutex<Inner>,
    state: Mutex<PowerState>,
    /// the phone's screen went off at
    off_since: Mutex<Option<Instant>>,
    last_log: Mutex<String>,
    /// the daemon's latest sleep line (Settings › Display shows it)
    pub last_decision: Mutex<String>,
    publish: Box<dyn Fn(&PowerState) + Send + Sync>,
}

impl Power {
    pub fn new(publish: impl Fn(&PowerState) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            state: Mutex::new(PowerState::default()),
            off_since: Mutex::new(None),
            last_log: Mutex::new(String::new()),
            last_decision: Mutex::new(String::new()),
            publish: Box::new(publish),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let p = self.clone();
        std::thread::Builder::new().name("power-vsock".into()).spawn(move || p.vsock_loop()).unwrap();
        let p = self.clone();
        std::thread::Builder::new().name("power-wake".into()).spawn(move || p.wake_loop()).unwrap();
    }

    pub fn state(&self) -> PowerState {
        self.state.lock().unwrap().clone()
    }

    pub fn is_asleep(&self, vm: &str) -> bool {
        self.inner.lock().unwrap().frozen.contains_key(vm)
    }

    // ------------------------------------------------ table
    fn table(&self) -> BTreeMap<String, Vote> {
        let t = now();
        let mut out = BTreeMap::from([(HOST.to_string(), host_vote())]);
        let running = running_vms();
        let mut i = self.inner.lock().unwrap();
        i.frozen.retain(|n, _| running.contains_key(n)); // stopped while asleep
        i.screens.retain(|n, _| running.contains_key(n));
        for (n, conf) in &running {
            let name = Some(conf["name"].as_str().unwrap_or(n).to_string());
            let v = if let Some(f) = i.frozen.get(n) {
                Vote { state: VoteState::Suspended, may_sleep: true, demand: BTreeMap::new(),
                       reason: "asleep (vCPUs frozen); wakes on a key or the mouse, or Wake".into(), seq: 0,
                       updated: Some(f.since), source: "host".into(), name }
            } else {
                match i.entries.get(n) {
                    Some(e) if t - e.updated.unwrap_or(0.0) <= STALE => Vote { name, ..e.clone() },
                    None if let Some(&on) = i.screens.get(n) => seen_vote(n, on, name),
                    e => Vote {
                        state: VoteState::Unknown, may_sleep: false, demand: BTreeMap::new(),
                        reason: match e {
                            None => "starting (no display yet)".into(),
                            Some(e) => format!("no update for {:.0} s", t - e.updated.unwrap_or(0.0)),
                        },
                        seq: 0, updated: e.and_then(|e| e.updated), source: "none".into(), name,
                    },
                }
            };
            out.insert(n.clone(), v);
        }
        // host-side agents count while their vote is fresh
        for (n, e) in &i.entries {
            if e.source == "host-agent" && !out.contains_key(n) && t - e.updated.unwrap_or(0.0) <= STALE {
                out.insert(n.clone(), e.clone());
            }
        }
        out
    }

    /// A VM's own display output follows its vote: off while its screen is off or it sleeps, on otherwise; VMs
    /// without a vote are left alone.
    fn follow_displays(&self, table: &BTreeMap<String, Vote>) {
        let mut i = self.inner.lock().unwrap();
        for (n, v) in table {
            if n == HOST || v.source == "none" || !Path::new(vm::VMS).join(n).is_dir() {
                continue;
            }
            let off = matches!(v.state, VoteState::ScreenOff | VoteState::Sleeping | VoteState::Suspended);
            if i.display_off.get(n) != Some(&off) {
                i.display_off.insert(n.clone(), off);
                vm_display(n, !off);
                println!("{n}: display {} ({})", if off { "off" } else { "on" }, state_name(v.state));
            }
        }
        i.display_off.retain(|n, _| table.contains_key(n));
    }

    // ------------------------------------------------ the 2 s pass
    pub fn tick(&self, guard: &GuardState) {
        if battery_critical() {
            println!("battery at {}% and discharging: powering off", rd(Path::new(BATT).join("capacity"), "?"));
            run("systemctl", &["poweroff"]);
            return;
        }
        let table = self.table();
        let (blockers, floors) = decide(&table);
        self.follow_displays(&table);
        let applied = apply_floors(&floors);
        let st = PowerState {
            contract: "mship.power".into(),
            version: VERSIONS[VERSIONS.len() - 1].into(),
            sleep: SleepVerdict { allowed: blockers.is_empty(), blockers: blockers.clone() },
            floors: floors.clone(),
            applied,
            participants: table,
            guard: guard.clone(),
        };
        *self.state.lock().unwrap() = st.clone();
        (self.publish)(&st);
        let line = format!(
            "sleep {}{}",
            if blockers.is_empty() { "allowed".to_string() } else { format!("blocked: {}", blockers.join("; ")) },
            if floors.is_empty() { String::new() } else { format!(" · floors {}", serde_json::to_string(&floors).unwrap()) }
        );
        let mut last = self.last_log.lock().unwrap();
        if *last != line {
            println!("{line}");
            *self.last_decision.lock().unwrap() = line.clone();
            *last = line;
        }
        drop(last);
        self.maybe_suspend(blockers.is_empty());
    }

    /// Suspend-to-idle once the screen has been off for the sleep delay and every participant agrees; after every
    /// wake that did not turn the screen on (RTC, volume key, Wi-Fi) the delay starts again.
    fn maybe_suspend(&self, allowed: bool) {
        let mut off = self.off_since.lock().unwrap();
        if hpower::screen_on() {
            *off = None;
            return;
        }
        let since = *off.get_or_insert_with(Instant::now);
        if !allowed || since.elapsed() < Duration::from_secs(hpower::power_conf().sleep_delay) {
            return;
        }
        println!("suspend");
        *self.last_decision.lock().unwrap() = "suspend".into();
        let t = now();
        run("systemctl", &["suspend"]);
        std::thread::sleep(Duration::from_secs(3)); // systemctl returns at once; this runs again after the resume
        println!("resumed after {:.0} s", now() - t);
        *off = Some(Instant::now());
    }

    // ------------------------------------------------ API
    /// A VM's display as its presenter sees it (the vote of VMs without a guest adapter).
    pub fn set_display(&self, vm: &str, on: bool) {
        if self.inner.lock().unwrap().screens.insert(vm.into(), on) != Some(on) {
            println!("{vm}: guest display {}", if on { "on" } else { "blanked" });
        }
    }

    pub fn set_vote(&self, participant: &str, r: VoteRequest) {
        self.inner.lock().unwrap().entries.insert(participant.into(), vote_from(r, "host-agent"));
    }

    // ------------------------------------------------ guest channel (vsock)
    fn guest_session(self: &Arc<Self>, mut conn: vsock::VsockStream, vm: String, sid: u64) {
        let mut hello = false;
        let result = (|| -> std::io::Result<()> {
            loop {
                let m = recv(&mut conn)?;
                let op = m["op"].as_str().unwrap_or_default().to_string();
                let rid = m["id"].clone();
                let reply = |status: i32, extra: Value| {
                    let mut r = json!({"op": "result", "id": rid, "status": status});
                    if let (Some(r), Some(e)) = (r.as_object_mut(), extra.as_object()) {
                        r.extend(e.clone());
                    }
                    r
                };
                match op.as_str() {
                    "hello" => {
                        let ok = m["contract"] == "mship.power"
                            && m["versions"].as_array().is_some_and(|v| v.iter().any(|x| VERSIONS.contains(&x.as_str().unwrap_or(""))));
                        if !ok {
                            send(&mut conn, &reply(-71, json!({"error": format!("supported: {VERSIONS:?}")})))?;
                            return Ok(());
                        }
                        hello = true;
                        send(&mut conn, &reply(0, json!({"version": VERSIONS[0], "states": ["active", "idle", "screen_off", "sleeping", "suspended"],
                                                          "levels": ["low", "balanced", "high"], "resources": ["cpu", "gpu", "codec"],
                                                          "heartbeat_s": 60, "stale_s": STALE})))?;
                        println!("{vm}: hello {}", VERSIONS[0]);
                    }
                    _ if !hello => send(&mut conn, &reply(-71, json!({"error": "hello first"})))?,
                    "set" => {
                        let r: VoteRequest = match serde_json::from_value(m.clone()) {
                            Ok(r) => r,
                            Err(e) => {
                                send(&mut conn, &reply(-22, json!({"error": e.to_string()})))?;
                                continue;
                            }
                        };
                        let v = vote_from(r, "guest");
                        let old = {
                            let mut i = self.inner.lock().unwrap();
                            let old = i.entries.get(&vm).cloned();
                            if old.as_ref().is_some_and(|o| o.seq > v.seq) {
                                drop(i);
                                send(&mut conn, &reply(-116, json!({"error": "old seq"})))?; // stale or reordered
                                continue;
                            }
                            i.entries.insert(vm.clone(), v.clone());
                            old
                        };
                        if old.is_none_or(|o| (o.state, o.may_sleep, &o.demand) != (v.state, v.may_sleep, &v.demand)) {
                            println!("{vm}: {}{}{} ({})", state_name(v.state), if v.may_sleep { " may sleep" } else { "" },
                                     if v.demand.is_empty() { String::new() } else { format!(" {}", serde_json::to_string(&v.demand).unwrap()) },
                                     v.reason);
                        }
                        send(&mut conn, &reply(0, json!({})))?;
                    }
                    "sleep" => {
                        // the guest's own suspend: freeze it, answer "resumed" after the wake
                        let waiter = conn.try_clone().ok();
                        match self.freeze(&vm, waiter) {
                            Ok(()) => send(&mut conn, &reply(0, json!({})))?,
                            Err(e) => send(&mut conn, &reply(-16, json!({"error": e.0})))?,
                        }
                    }
                    "get_state" => {
                        let st = self.state();
                        send(&mut conn, &reply(0, json!({"sleep_allowed": st.sleep.allowed, "floors": st.floors})))?;
                    }
                    other => send(&mut conn, &reply(-95, json!({"error": format!("unknown op {other}")})))?,
                }
            }
        })();
        if let Err(e) = result {
            println!("{vm}: disconnected ({})", e.kind());
        }
        let mut i = self.inner.lock().unwrap();
        let last = i.sessions.get_mut(&vm).map(|s| {
            s.remove(&sid);
            s.is_empty()
        });
        if last == Some(true) {
            // no connection left: no vote, it blocks
            i.sessions.remove(&vm);
            i.entries.remove(&vm);
        }
    }

    fn vsock_loop(self: Arc<Self>) {
        let l = match vsock::VsockListener::bind_with_cid_port(libc::VMADDR_CID_ANY, POWER_VSOCK_PORT) {
            Ok(l) => l,
            Err(e) => {
                println!("mship.power: cannot listen on vsock port {POWER_VSOCK_PORT}: {e}");
                return;
            }
        };
        println!("mship.power: listening on vsock port {POWER_VSOCK_PORT}");
        for conn in l.incoming().flatten() {
            let cid = conn.peer_addr().map(|a| a.cid()).unwrap_or(0);
            let Some(vm) = vm_for_cid(cid) else {
                println!("cid {cid}: not a configured VM, closing");
                continue;
            };
            let sid = {
                let mut i = self.inner.lock().unwrap();
                i.next_session += 1;
                let sid = i.next_session;
                i.sessions.entry(vm.clone()).or_default().insert(sid);
                sid
            };
            let p = self.clone();
            std::thread::spawn(move || p.guest_session(conn, vm, sid));
        }
    }

    // ------------------------------------------------ VM sleep (host-side freeze)
    /// Put a running crosvm VM to sleep.
    pub fn freeze(&self, vm_name: &str, waiter: Option<vsock::VsockStream>) -> Result<()> {
        if self.inner.lock().unwrap().frozen.contains_key(vm_name) {
            return Ok(());
        }
        let d = Path::new(vm::VMS).join(vm_name);
        if !vm::running(&d) || !d.join("crosvm.sock").exists() {
            return fail("not a running crosvm VM");
        }
        self.inner.lock().unwrap().frozen.insert(vm_name.into(), Frozen { since: now(), usb: false, waiter });
        let owner = serde_json::from_str::<Value>(&usb_ctl("status")).ok().and_then(|v| v["owner"].as_str().map(String::from));
        if owner.as_deref() == Some(vm_name) {
            // take the keyboard/mouse back: the host watches them for the wake-up
            let taken = usb_ctl(&format!("detach {vm_name} user")).starts_with("ok");
            if let Some(f) = self.inner.lock().unwrap().frozen.get_mut(vm_name) {
                f.usb = taken;
            }
        }
        if !crosvm("suspend", vm_name) {
            self.thaw(vm_name, "freeze failed");
            return fail("crosvm suspend failed");
        }
        vm_display(vm_name, false);
        let mut i = self.inner.lock().unwrap();
        i.display_off.insert(vm_name.into(), true);
        let usb = i.frozen.get(vm_name).is_some_and(|f| f.usb);
        println!("{vm_name}: asleep{}", if usb { " (USB devices back on the host)" } else { "" });
        Ok(())
    }

    pub fn thaw(&self, vm_name: &str, why: &str) -> bool {
        let Some(f) = self.inner.lock().unwrap().frozen.remove(vm_name) else { return false };
        vm_display(vm_name, true);
        // the next pass switches it off again if the guest is still blanked
        self.inner.lock().unwrap().display_off.insert(vm_name.into(), false);
        let ok = crosvm("resume", vm_name);
        if f.usb {
            usb_ctl(&format!("attach {vm_name}"));
        }
        if let Some(mut w) = f.waiter {
            // the guest's suspend call returns: its resume path runs (GNOME shows the lock screen)
            let _ = send(&mut w, &json!({"op": "resumed", "id": 0, "reason": why}));
        }
        // keep its pre-sleep vote (it could not refresh it while frozen)
        if let Some(e) = self.inner.lock().unwrap().entries.get_mut(vm_name) {
            e.updated = Some(now());
        }
        println!("{vm_name}: woken ({why}){}", if ok { "" } else { ", crosvm resume FAILED" });
        ok
    }

    /// While a VM sleeps with its USB devices on the host: grab their input devices (sway does not see the wake-up
    /// keystroke) and wake the VMs on the first key, button or motion.
    fn wake_loop(self: Arc<Self>) {
        let mut fds: BTreeMap<String, i32> = BTreeMap::new();
        let close_all = |fds: &mut BTreeMap<String, i32>| {
            for fd in fds.values() {
                unsafe { libc::close(*fd) };
            }
            fds.clear();
        };
        loop {
            let sleepers: Vec<String> =
                self.inner.lock().unwrap().frozen.iter().filter(|(_, f)| f.usb).map(|(n, _)| n.clone()).collect();
            if sleepers.is_empty() {
                close_all(&mut fds);
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            for dev in usb_inputs() {
                if fds.contains_key(&dev) {
                    continue;
                }
                let c = std::ffi::CString::new(dev.clone()).unwrap();
                let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
                if fd >= 0 {
                    unsafe { libc::ioctl(fd, EVIOCGRAB, 1) };
                    fds.insert(dev, fd);
                }
            }
            if fds.is_empty() {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            let mut pfds: Vec<libc::pollfd> = fds.values().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
            unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 500) };
            let mut woke = false;
            let mut dead = vec![];
            for p in pfds.iter().filter(|p| p.revents != 0) {
                let mut buf = [0u8; 24 * 64];
                let n = unsafe { libc::read(p.fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    dead.push(p.fd); // unplugged
                    continue;
                }
                // struct input_event (64-bit): timeval (16), type u16, code u16, value i32
                for ev in buf[..n as usize].as_chunks::<24>().0 {
                    let typ = u16::from_ne_bytes([ev[16], ev[17]]);
                    let val = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
                    if (typ == EV_KEY && val == 1) || typ == EV_REL || typ == EV_ABS {
                        woke = true;
                    }
                }
            }
            fds.retain(|_, fd| {
                let d = dead.contains(fd);
                if d {
                    unsafe { libc::close(*fd) };
                }
                !d
            });
            if woke {
                close_all(&mut fds); // release before the devices go back to the VM
                for v in &sleepers {
                    self.thaw(v, "input");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote(state: VoteState, may_sleep: bool, demand: &[(Resource, Level)]) -> Vote {
        Vote { state, may_sleep, demand: demand.iter().copied().collect(), reason: state_name(state), seq: 0,
               updated: None, source: "guest".into(), name: None }
    }

    #[test]
    fn all_agree() {
        let t = BTreeMap::from([(HOST.to_string(), vote(VoteState::ScreenOff, true, &[])),
                                ("vm".to_string(), vote(VoteState::Suspended, true, &[]))]);
        assert_eq!(decide(&t), (vec![], BTreeMap::new()));
    }

    #[test]
    fn unknown_vm_blocks() {
        let mut v = vote(VoteState::Unknown, false, &[]);
        v.name = Some("Ubuntu".into());
        let t = BTreeMap::from([(HOST.to_string(), vote(VoteState::ScreenOff, true, &[])), ("vm".to_string(), v)]);
        assert_eq!(decide(&t).0, ["Ubuntu: unknown"]);
    }

    #[test]
    fn screen_off_needs_may_sleep() {
        assert!(!asleep(&vote(VoteState::ScreenOff, false, &[])));
        assert!(asleep(&vote(VoteState::Idle, true, &[])));
        assert!(!asleep(&vote(VoteState::Active, true, &[]))); // in use: never, whatever it says
        assert!(asleep(&vote(VoteState::Sleeping, false, &[])));
    }

    #[test]
    fn highest_floor_wins() {
        let t = BTreeMap::from([
            ("a".to_string(), vote(VoteState::Active, false, &[(Resource::Cpu, Level::Balanced)])),
            ("b".to_string(), vote(VoteState::Active, false, &[(Resource::Cpu, Level::High), (Resource::Codec, Level::Low)])),
        ]);
        assert_eq!(decide(&t).1, BTreeMap::from([(Resource::Cpu, Level::High), (Resource::Codec, Level::Low)]));
    }

    #[test]
    fn floor_respects_cap() {
        let t = [300, 600, 900, 1200, 1500, 1800];
        assert_eq!(freq_floor(&t, 1800, Level::Low), 300);
        assert_eq!(freq_floor(&t, 1800, Level::Balanced), 900);
        assert_eq!(freq_floor(&t, 1200, Level::High), 1200); // never above the cap
    }

    #[test]
    fn framing() {
        let mut buf = vec![];
        send(&mut buf, &json!({"op": "hello"})).unwrap();
        assert_eq!(&buf[..4], &(14u32).to_be_bytes()); // {"op":"hello"}
        assert_eq!(recv(&mut buf.as_slice()).unwrap(), json!({"op": "hello"}));
    }
}
