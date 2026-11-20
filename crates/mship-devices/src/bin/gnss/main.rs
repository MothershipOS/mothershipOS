//! mship-gnss: the phone's GNSS receiver (root), the mship.gnss 1.x contract (docs/contracts/mship.gnss-1.md).
//!
//! The receiver is the engine in the modem (QMI LOC over QRTR, `loc`). This service keeps one client on it, unlocks
//! it, gives it the time and predicted orbits (`xtra`), and turns its reports into the contract's stream: standard
//! NMEA 0183 epochs plus `$PMSHG` control sentences (`nmea`). Clients:
//! - **VMs**: a VM whose manifest has a `mship.gnss` slot gets /run/mothership/gnss/<vm>.sock, which crosvm connects
//!   its slot's console to (cuttlefish-arm64: hvc6). Only the VM holding the handle `location` gets epochs; the
//!   others' slots are empty.
//! - **the host**: clients of /run/mothership/gnss/host.sock (same protocol) get epochs while no VM holds it.
//! The receiver runs only while the clients that get epochs asked for it (`$PMSHG,START`), at the shortest interval
//! any of them asked for.
//!
//! Control: datagrams on /run/mothership/gnss.sock, one reply each: `attach <vm>`, `detach <vm> [reason]`,
//! `status`. Configuration (optional): /etc/mothership/gnss.json `{"xtra": true}` (false: never download
//! predicted orbits).

mod loc;
mod nmea;
mod xtra;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mship_devices::qrtr::{PORT_CTRL, Qrtr, ctrl, ctrl_packet};
use mship_core::handle::{Direction, Handle};
use mship_devices::{for_session, fresh, log, poll, publish_handles};
use serde_json::{Value, json};

use loc::{Engine, Event};
use nmea::{Command, Epoch, Fix, Sat, System};

const CONF: &str = "/etc/mothership/gnss.json";
const CTL: &str = "/run/mothership/gnss.sock";
const SOCKS: &str = "/run/mothership/gnss";
const HOST_SOCK: &str = "host.sock";
const VMS: &str = "/var/lib/mothership/vms";
const XTRA_FILE: &str = "/var/lib/mothership/gnss/xtra.bin";
const REASONS: &[&str] = &["user", "screen", "stop", "revoked", "gone"];
const VERSIONS: &[&str] = &["1.0"];
const CONTRACT: &str = "mship.gnss";
/// interval bounds a client may ask for
const INTERVAL: (u32, u32) = (1000, 60_000);
/// a client that reads nothing loses epochs beyond this
const OUT_MAX: usize = 64 * 1024;
/// no final fix this long after an epoch was due: an epoch without one (the satellites still reported)
const GRACE: Duration = Duration::from_millis(500);

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The host clock for the engine: (UTC ms, uncertainty ms). Synchronized by NTP: the kernel's error estimate;
/// otherwise mship-timesync's HTTP Date (seconds) on the RTC offset restored at boot. None before 2026.
fn host_time() -> Option<(u64, u32)> {
    let ms = now_ms();
    if ms < 1_767_225_600_000 {
        return None;
    }
    let mut t: libc::timex = unsafe { std::mem::zeroed() };
    let state = unsafe { libc::adjtimex(&mut t) };
    let unc = if state >= 0 && state != libc::TIME_ERROR { (t.maxerror / 1000).clamp(20, 5000) as u32 } else { 5000 };
    Some((ms as u64, unc))
}

/// One client connection: a VM's slot console or a host client.
struct Peer {
    conn: UnixStream,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    /// the interval it asked for with START (None: stopped)
    want: Option<u32>,
    dropped: u64,
}

impl Peer {
    fn new(conn: UnixStream) -> Self {
        let _ = conn.set_nonblocking(true);
        Self { conn, inbuf: vec![], out: vec![], want: None, dropped: 0 }
    }

    fn send(&mut self, lines: &[String]) {
        let n: usize = lines.iter().map(String::len).sum();
        if self.out.len() + n > OUT_MAX {
            self.dropped += 1;
            return;
        }
        for l in lines {
            self.out.extend_from_slice(l.as_bytes());
        }
        self.flush();
    }

    /// false: gone
    fn flush(&mut self) -> bool {
        while !self.out.is_empty() {
            match self.conn.write(&self.out) {
                Ok(0) => return false,
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }
        true
    }

    /// Commands that arrived. Err: gone.
    fn read(&mut self) -> Result<Vec<Command>, ()> {
        let mut b = [0u8; 4096];
        loop {
            match self.conn.read(&mut b) {
                Ok(0) => return Err(()),
                Ok(n) => self.inbuf.extend_from_slice(&b[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return Err(()),
            }
        }
        let mut cmds = vec![];
        while let Some(i) = self.inbuf.iter().position(|c| *c == b'\n' || *c == b'\r') {
            let line: Vec<u8> = self.inbuf.drain(..=i).collect();
            cmds.extend(nmea::command(&String::from_utf8_lossy(&line)));
        }
        if self.inbuf.len() > 4096 {
            self.inbuf.clear(); // no line end: not the protocol
        }
        Ok(cmds)
    }

    /// The reply to a command; true if what it wants changed.
    fn on(&mut self, cmd: Command) -> bool {
        let before = self.want;
        match cmd {
            Command::Hello(v) => {
                let both = VERSIONS.iter().find(|ours| v.iter().any(|x| x == *ours));
                self.send(&[nmea::hello(both.copied())]);
            }
            Command::Start(ms) => self.want = Some(ms.clamp(INTERVAL.0, INTERVAL.1)),
            Command::Stop => self.want = None,
        }
        self.want != before
    }
}

/// One VM's slot.
struct Slot {
    path: PathBuf,
    listener: UnixListener,
    peer: Option<Peer>,
}

fn listen(path: &Path) -> std::io::Result<UnixListener> {
    fresh(path);
    let l = UnixListener::bind(path)?;
    l.set_nonblocking(true)?;
    for_session(path, 0o660);
    Ok(l)
}

/// A VM's last START/STOP, kept over a restart of this service (the slot's console stays connected meanwhile).
/// Removed when the guest disconnects: a guest that connects to a running service is a new boot.
fn want_file(vm: &str) -> PathBuf {
    Path::new(SOCKS).join(format!("{vm}.want"))
}

/// VM directories whose manifest has a mship.gnss slot.
fn gnss_vms() -> BTreeSet<String> {
    std::fs::read_dir(VMS).into_iter().flatten().flatten()
        .filter(|e| {
            let m: Value = std::fs::read_to_string(e.path().join("vm.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            m["slots"].as_object().is_some_and(|s| s.values().any(|x| x["contract"] == CONTRACT))
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

struct Gnss {
    ns: Qrtr,
    /// (node, port) of the LOC service
    loc_at: Option<(u32, u32)>,
    engine: Option<Engine>,
    engine_err: String,
    /// the session's interval while one runs
    running: Option<u32>,
    engine_on: bool,
    slots: BTreeMap<String, Slot>,
    host_listener: UnixListener,
    host: Vec<Peer>,
    /// the VM holding the handle
    lent: Option<String>,
    /// what the engine reported last (cleared on every change of holder: fresh on attach)
    sats: Vec<Sat>,
    used: BTreeSet<(System, u16)>,
    fix: Option<(Instant, Fix)>,
    epochs: u64,
    /// when an epoch without a fix is due
    due: Option<Instant>,
    xtra: xtra::Xtra,
    xtra_on: bool,
    xtra_max: (u32, u32),
    xtra_servers: Vec<String>,
    xtra_injected: Option<SystemTime>,
}

impl Gnss {
    // ------------------------------------------------ the engine
    fn on_ns(&mut self) {
        while let Ok(Some((_, port, data))) = self.ns.recv() {
            if port != PORT_CTRL {
                continue;
            }
            let Some((cmd, service, _, node, port)) = ctrl_packet(&data) else { continue };
            match cmd {
                ctrl::NEW_SERVER if service == loc::SERVICE && self.loc_at.is_none() => {
                    self.loc_at = Some((node, port));
                    self.engine_up();
                }
                ctrl::DEL_SERVER if service == loc::SERVICE && self.loc_at == Some((node, port)) => self.engine_down(),
                ctrl::BYE if self.loc_at.is_some_and(|(n, _)| n == service) => self.engine_down(),
                _ => {}
            }
        }
    }

    fn engine_up(&mut self) {
        let Some((node, port)) = self.loc_at else { return };
        match Engine::open(node, port) {
            Ok(e) => {
                log(format!("engine up (QRTR node {node}), unlocked"));
                self.engine = Some(e);
                self.engine_err.clear();
                self.xtra_injected = None;
                self.inject_time();
                self.xtra_check(false);
                self.update();
            }
            Err(e) => {
                log(format!("engine: {e}"));
                self.engine_err = e;
            }
        }
    }

    fn engine_down(&mut self) {
        if self.loc_at.take().is_some() {
            log("engine gone");
        }
        self.engine = None;
        self.running = None;
        self.engine_on = false;
        self.clear();
    }

    fn inject_time(&mut self) {
        let (Some(e), Some((ms, unc))) = (self.engine.as_mut(), host_time()) else { return };
        if let Err(err) = e.inject_time(ms, unc) {
            log(format!("time: {err}"));
        }
    }

    /// Predicted orbits: a kept file young enough goes in now (once per engine start or download); a missing or
    /// old one is downloaded (`force`: the engine asked).
    fn xtra_check(&mut self, force: bool) {
        if !self.xtra_on || self.engine.is_none() {
            return;
        }
        let age = self.xtra.age();
        if let Some((data, age)) = self.xtra.cached().filter(|(_, a)| *a < xtra::EXPIRED)
            && (self.xtra_injected.is_none() || force && age < xtra::REFRESH)
        {
            self.inject_xtra(&data, age);
        }
        if age.is_none_or(|a| a >= xtra::REFRESH) && !self.xtra.busy() {
            if self.xtra_servers.is_empty() {
                match self.engine.as_mut().unwrap().xtra_source() {
                    Ok((servers, max_file, max_part)) => {
                        self.xtra_servers = servers;
                        self.xtra_max = (max_file, max_part);
                    }
                    Err(e) => log(format!("xtra: source: {e} (default servers)")),
                }
            }
            self.xtra.fetch(self.xtra_servers.clone(), self.xtra_max.0);
        }
    }

    fn inject_xtra(&mut self, data: &[u8], age: Duration) {
        let Some(e) = self.engine.as_mut() else { return };
        match e.inject_xtra(data, self.xtra_max.1.max(1024)) {
            Ok(()) => {
                log(format!("xtra: {} bytes injected ({} h old)", data.len(), age.as_secs() / 3600));
                self.xtra_injected = Some(SystemTime::now());
            }
            Err(err) => log(format!("xtra: {err}")),
        }
    }

    /// Run the receiver while its current clients want it, at the shortest interval asked.
    fn update(&mut self) {
        let want = self.consumers_want();
        if want == self.running {
            return;
        }
        let Some(e) = self.engine.as_mut() else { return };
        let r = match want {
            Some(ms) => e.start(ms),
            None => e.stop(),
        };
        match r {
            Ok(()) => {
                log(match want {
                    Some(ms) => format!("receiver on ({ms} ms){}", self.lent.as_ref().map(|v| format!(" for {v}")).unwrap_or_default()),
                    None => "receiver off".into(),
                });
                if want.is_some() && self.running.is_none() {
                    self.inject_time();
                }
                self.running = want;
                self.due = want.map(|ms| Instant::now() + Duration::from_millis(ms as u64) + GRACE);
            }
            Err(err) => log(format!("receiver: {err}")),
        }
    }

    fn consumers_want(&self) -> Option<u32> {
        match &self.lent {
            Some(vm) => self.slots.get(vm).and_then(|s| s.peer.as_ref()).and_then(|p| p.want),
            None => self.host.iter().filter_map(|p| p.want).min(),
        }
    }

    fn on_engine(&mut self) {
        let Some(e) = self.engine.as_mut() else { return };
        let events = match e.events() {
            Ok(ev) => ev,
            Err(_) => return self.engine_down(),
        };
        for ev in events {
            match ev {
                Event::Position(fix, utc, used) => {
                    self.used = used.into_iter().collect();
                    self.fix = Some((Instant::now(), fix));
                    self.epoch(true, utc);
                }
                Event::Sats(s) => self.sats = s,
                Event::TimeRequest => self.inject_time(),
                Event::XtraRequest => self.xtra_check(true),
                Event::Engine(on) => self.engine_on = on,
            }
        }
    }

    /// Send an epoch to the current clients: the newest fix if it is fresh, the satellites in view.
    fn epoch(&mut self, report: bool, utc: Option<i64>) {
        let Some(ms) = self.running else {
            self.due = None;
            return;
        };
        let interval = Duration::from_millis(ms as u64);
        let fix = self.fix.as_ref().filter(|(t, _)| t.elapsed() < interval + GRACE).map(|(_, f)| f.clone());
        let sats = self.sats.iter().cloned().map(|mut s| {
            s.used = fix.is_some() && self.used.contains(&(s.system, s.id));
            s
        }).collect();
        let lines = nmea::epoch(&Epoch { utc_ms: utc.filter(|_| fix.is_some()).unwrap_or_else(now_ms), fix, sats });
        self.epochs += 1;
        // the next one: a fix's report is awaited a little past the interval, otherwise the interval holds
        self.due = Some(Instant::now() + interval + if report { GRACE } else { Duration::ZERO });
        match self.lent.clone() {
            Some(vm) => {
                if let Some(p) = self.slots.get_mut(&vm).and_then(|s| s.peer.as_mut()).filter(|p| p.want.is_some()) {
                    p.send(&lines);
                }
            }
            None => {
                for p in self.host.iter_mut().filter(|p| p.want.is_some()) {
                    p.send(&lines);
                }
            }
        }
    }

    fn clear(&mut self) {
        self.sats.clear();
        self.used.clear();
        self.fix = None;
    }

    // ------------------------------------------------ clients
    fn scan(&mut self) {
        let names = gnss_vms();
        let new: Vec<String> = names.iter().filter(|n| !self.slots.contains_key(*n)).cloned().collect();
        for n in new {
            let path = Path::new(SOCKS).join(format!("{n}.sock"));
            match listen(&path) {
                Ok(listener) => {
                    self.slots.insert(n, Slot { path, listener, peer: None });
                }
                Err(e) => log(format!("{n}: {e}")),
            }
        }
        let gone: Vec<String> = self.slots.keys().filter(|n| !names.contains(*n)).cloned().collect();
        for n in gone {
            self.release(&n, "gone");
            if let Some(s) = self.slots.remove(&n) {
                let _ = std::fs::remove_file(s.path);
            }
        }
    }

    fn accept_vm(&mut self, name: &str) {
        let Some(Ok((conn, _))) = self.slots.get(name).map(|s| s.listener.accept()) else { return };
        let mut p = Peer::new(conn);
        // what the guest asked for before this service (re)started: its HAL only says it again when it starts over
        p.want = std::fs::read_to_string(want_file(name)).ok().and_then(|w| w.trim().parse().ok());
        let attached = self.lent.as_deref() == Some(name);
        p.send(&[nmea::state(attached, None)]);
        let s = self.slots.get_mut(name).unwrap();
        if s.peer.replace(p).is_some() {
            log(format!("{name}: new connection replaces the old one"));
        }
        log(format!("{name}: guest connected"));
        self.update();
    }

    fn on_vm(&mut self, name: &str) {
        let Some(p) = self.slots.get_mut(name).and_then(|s| s.peer.as_mut()) else { return };
        let cmds = if p.flush() { p.read() } else { Err(()) };
        match cmds {
            Ok(cmds) => {
                let changed = cmds.into_iter().fold(false, |c, cmd| p.on(cmd) | c);
                if changed {
                    let _ = std::fs::write(want_file(name), p.want.map(|w| w.to_string()).unwrap_or_default());
                }
                if changed && self.lent.as_deref() == Some(name) {
                    self.update();
                }
            }
            Err(()) => {
                self.slots.get_mut(name).unwrap().peer = None;
                let _ = std::fs::remove_file(want_file(name));
                log(format!("{name}: guest disconnected"));
                self.update();
            }
        }
    }

    fn accept_host(&mut self) {
        while let Ok((conn, _)) = self.host_listener.accept() {
            let mut p = Peer::new(conn);
            p.send(&[nmea::state(self.lent.is_none(), self.lent.as_ref().map(|_| "revoked"))]);
            self.host.push(p);
        }
    }

    fn on_host(&mut self, ready: &[RawFd]) {
        let mut changed = false;
        self.host.retain_mut(|p| {
            if !ready.contains(&p.conn.as_raw_fd()) && p.out.is_empty() {
                return true;
            }
            match if p.flush() { p.read() } else { Err(()) } {
                Ok(cmds) => {
                    changed |= cmds.into_iter().fold(false, |c, cmd| p.on(cmd) | c);
                    true
                }
                Err(()) => {
                    changed |= p.want.is_some();
                    false
                }
            }
        });
        if changed && self.lent.is_none() {
            self.update();
        }
    }

    // ------------------------------------------------ the handle
    /// Fresh on attach: nothing from before reaches the VM; the receiver follows what the VM asked for.
    fn attach(&mut self, vm: &str) -> Result<(), String> {
        self.scan();
        if !self.slots.contains_key(vm) {
            return Err(format!("{vm}: no {CONTRACT} slot in its vm.json"));
        }
        if self.lent.as_deref() == Some(vm) {
            return Ok(());
        }
        if let Some(old) = self.lent.clone() {
            self.release(&old, "revoked");
        }
        for p in &mut self.host {
            p.send(&[nmea::state(false, Some("revoked"))]);
        }
        self.clear();
        self.lent = Some(vm.into());
        if let Some(p) = self.slots.get_mut(vm).and_then(|s| s.peer.as_mut()) {
            p.send(&[nmea::state(true, None)]);
        }
        log(format!("{vm}: attached"));
        self.update();
        Ok(())
    }

    /// Immediate: the VM's epochs stop, it is told the slot is empty; the host's clients get the receiver back.
    fn release(&mut self, vm: &str, reason: &str) {
        if self.lent.as_deref() != Some(vm) {
            return;
        }
        self.lent = None;
        self.clear();
        if let Some(p) = self.slots.get_mut(vm).and_then(|s| s.peer.as_mut()) {
            p.send(&[nmea::state(false, Some(reason))]);
        }
        for p in &mut self.host {
            p.send(&[nmea::state(true, None)]);
        }
        log(format!("{vm}: released ({reason}); the receiver is the host's again"));
        self.update();
    }

    fn control(&mut self, msg: &str) -> String {
        let w: Vec<&str> = msg.split_whitespace().collect();
        match w.as_slice() {
            ["attach", vm, ..] => match self.attach(vm) {
                Ok(()) => "ok".into(),
                Err(e) => format!("error: {e}"),
            },
            ["detach", vm, rest @ ..] => {
                let reason = rest.first().filter(|r| REASONS.contains(r)).copied().unwrap_or("user");
                self.release(vm, reason);
                "ok".into()
            }
            ["status", ..] => {
                let fix = self.fix.as_ref().map(|(t, f)| json!({"age_s": t.elapsed().as_secs(), "accuracy_m": f.h_acc}));
                json!({
                    "engine": self.loc_at.map(|(n, _)| format!("qrtr node {n}")),
                    "engine_error": (!self.engine_err.is_empty()).then_some(&self.engine_err),
                    "lent": self.lent,
                    "receiver": self.running.map(|ms| json!({"interval_ms": ms, "engine_on": self.engine_on})),
                    "satellites": {"in_view": self.sats.len(), "tracked": self.sats.iter().filter(|s| s.cn0.is_some()).count(), "used": self.used.len()},
                    "fix": fix,
                    "epochs": self.epochs,
                    "xtra": {"enabled": self.xtra_on, "age_h": self.xtra.age().map(|a| a.as_secs() / 3600), "injected": self.xtra_injected.is_some(), "downloading": self.xtra.busy()},
                    "vms": self.slots.iter().map(|(n, s)| (n.clone(), json!({"connected": s.peer.is_some(), "wants": s.peer.as_ref().and_then(|p| p.want),
                        "dropped": s.peer.as_ref().map(|p| p.dropped)}))).collect::<serde_json::Map<_, _>>(),
                    "host_clients": self.host.len(),
                })
                .to_string()
            }
            _ => "error: attach <vm> | detach <vm> [reason] | status".into(),
        }
    }
}

fn serve() -> std::io::Result<()> {
    let conf: Value = std::fs::read_to_string(CONF).ok().and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
    std::fs::create_dir_all(SOCKS)?;
    for_session(Path::new(SOCKS), 0o750);
    let ns = Qrtr::open()?;
    ns.lookup(loc::SERVICE)?;
    let mut g = Gnss {
        ns, loc_at: None, engine: None, engine_err: String::new(), running: None, engine_on: false, slots: BTreeMap::new(),
        host_listener: listen(&Path::new(SOCKS).join(HOST_SOCK))?, host: vec![], lent: None, sats: vec![], used: BTreeSet::new(),
        fix: None, epochs: 0, due: None, xtra: xtra::Xtra::new(XTRA_FILE.into()), xtra_on: conf["xtra"].as_bool().unwrap_or(true),
        xtra_max: (0, 0), xtra_servers: vec![], xtra_injected: None,
    };
    g.scan();
    fresh(Path::new(CTL));
    let ctl = UnixDatagram::bind(CTL)?;
    for_session(Path::new(CTL), 0o660);
    publish_handles("gnss", &[Handle::new("gnss:0", CONTRACT, "Location (GPS)", "find-location-symbolic", Direction::In, "location")
        .caps(json!({"constellations": ["gps", "glonass", "galileo", "beidou", "qzss", "sbas"], "min_interval_ms": INTERVAL.0, "assistance": ["xtra", "time"]}))
        .control(CTL, None)]);
    log(format!("serving {} VM slots and the host in {SOCKS}", g.slots.len()));
    let mut last_scan = Instant::now();
    loop {
        // (fd, what): 0 control, 1 name service, 2 engine, 3 VM listener, 4 VM stream, 5 host listener, 6 host stream
        let mut fds: Vec<(RawFd, u8, String)> = vec![(ctl.as_raw_fd(), 0, String::new()), (g.ns.fd(), 1, String::new()),
                                                     (g.host_listener.as_raw_fd(), 5, String::new())];
        fds.extend(g.engine.as_ref().map(|e| (e.fd(), 2, String::new())));
        for (n, s) in &g.slots {
            fds.push((s.listener.as_raw_fd(), 3, n.clone()));
            fds.extend(s.peer.as_ref().map(|p| (p.conn.as_raw_fd(), 4, n.clone())));
        }
        fds.extend(g.host.iter().map(|p| (p.conn.as_raw_fd(), 6, String::new())));
        let backlog = g.slots.values().filter_map(|s| s.peer.as_ref()).chain(&g.host).any(|p| !p.out.is_empty());
        let wait = match (backlog, g.due, g.xtra.busy()) {
            (true, _, _) => Duration::from_millis(20),
            (_, Some(d), _) => d.saturating_duration_since(Instant::now()).max(Duration::from_millis(10)),
            (_, _, true) => Duration::from_secs(1),
            _ => Duration::from_secs(10),
        };
        let raw: Vec<RawFd> = fds.iter().map(|f| f.0).collect();
        let ready = poll(&raw, Some(wait));
        let mut host_ready = false;
        for (fd, what, name) in &fds {
            let io = ready.contains(fd) || backlog && matches!(what, 4 | 6);
            if !io {
                continue;
            }
            match what {
                0 => {
                    let mut m = [0u8; 512];
                    if let Ok((n, peer)) = ctl.recv_from(&mut m) {
                        let reply = g.control(&String::from_utf8_lossy(&m[..n]));
                        if let Some(p) = peer.as_pathname() {
                            let _ = ctl.send_to(reply.as_bytes(), p);
                        }
                    }
                }
                1 => g.on_ns(),
                2 => g.on_engine(),
                3 => g.accept_vm(name),
                4 => g.on_vm(name),
                5 => g.accept_host(),
                6 => host_ready = true,
                _ => {}
            }
        }
        if host_ready || backlog {
            g.on_host(&ready);
        }
        if g.due.is_some_and(|d| Instant::now() >= d) {
            g.epoch(false, None); // no final fix in time: an epoch without one
        }
        if let Some(r) = g.xtra.done() {
            match r {
                Ok(data) => {
                    log(format!("xtra: downloaded {} bytes", data.len()));
                    g.inject_xtra(&data, Duration::ZERO);
                }
                Err(e) => log(format!("xtra: download: {e}")),
            }
        }
        if last_scan.elapsed() >= Duration::from_secs(10) {
            g.scan();
            last_scan = Instant::now();
        }
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: mship-gnss (control: {CTL}, clients: {SOCKS}/{HOST_SOCK})");
        return std::process::ExitCode::from(2);
    }
    if let Err(e) = serve() {
        eprintln!("mship-gnss: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
