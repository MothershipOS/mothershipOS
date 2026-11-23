//! mship-fingerprint: the phone's fingerprint reader (root), the mship.fingerprint 1.x contract
//! (docs/contracts/mship.fingerprint-1.md).
//!
//! The sensor, the matcher and the templates live in the phone's secure world (an FPC Trusted Application under
//! QSEE; the port's driver `/dev/fpc_tee` relays its command buffers and the finger interrupt). This service runs
//! the sensor on behalf of the VM holding the handle `fingerprint`, keeps that VM's template sets (one per guest user
//! group, `<state>/<vm>/<gid>.db`, encrypted by the TA) and authorizes its enrolments through the phone's Gatekeeper
//! (`gk`). The VMs connect over vsock (host port 9700): one stream per guest, each message a big-endian u32 length
//! and UTF-8 JSON. Guests not holding the handle see an empty slot.
//!
//! Control: datagrams on /run/mothership/fingerprint.sock, one reply each: `attach <vm>`, `detach <vm> [reason]`,
//! `status`. Port configuration: /etc/mothership/fingerprint.json `{"device": "/dev/fpc_tee", "sensor_type":
//! "side"}` (the service runs only where it exists).

mod gk;
mod tee;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mship_core::handle::{Direction, Handle};
use mship_devices::{for_session, fresh, log, poll, publish_handles};
use serde_json::{Value, json};

use gk::Credential;
use tee::Tee;

const CONF: &str = "/etc/mothership/fingerprint.json";
const CTL: &str = "/run/mothership/fingerprint.sock";
/// the TA instance this boot's service initialised (sensor + bio init happen once per instance)
const TA_MARK: &str = "/run/mothership/fingerprint.ta";
const STATE: &str = "/var/lib/mothership/fingerprint";
const VMS: &str = "/var/lib/mothership/vms";
const CONTRACT: &str = "mship.fingerprint";
const VERSIONS: &[&str] = &["1.0"];
const PORT: u32 = 9700;
/// templates per group (the vendor HAL's limit)
const MAX_TEMPLATES: u32 = 5;
/// an enrolment challenge is good for this long
const CHALLENGE_TTL: Duration = Duration::from_secs(600);
/// one message from a guest at most
const MSG_MAX: usize = 64 * 1024;
const REASONS: &[&str] = &["user", "screen", "stop", "revoked", "gone"];

fn vm_cid(vm: &str) -> Option<u32> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(Path::new(VMS).join(vm).join("vm.json")).ok()?).ok()?;
    Some(v["cid"].as_u64().unwrap_or(3) as u32)
}

/// A name for files: the VM's directory name, nothing else.
fn safe_name(vm: &str) -> bool {
    !vm.is_empty() && vm.len() <= 64 && vm.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') && !vm.starts_with('.')
}

/// The VM's Gatekeeper uid (stable per VM name; "MS" + 16 bits).
fn gk_uid(vm: &str) -> u32 {
    let h = vm.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    0x4d53_0000 | (h & 0xffff)
}

struct Guest {
    stream: vsock::VsockStream,
    inbuf: Vec<u8>,
    out: Vec<u8>,
}

impl Guest {
    fn send(&mut self, v: &Value) {
        let b = v.to_string();
        if self.out.len() + b.len() > 1 << 20 {
            return;
        }
        self.out.extend((b.len() as u32).to_be_bytes());
        self.out.extend_from_slice(b.as_bytes());
        self.flush();
    }

    /// false: gone
    fn flush(&mut self) -> bool {
        while !self.out.is_empty() {
            match self.stream.write(&self.out) {
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

    /// Whole messages that arrived. Err: gone or not the protocol.
    fn read(&mut self) -> Result<Vec<Value>, ()> {
        let mut b = [0u8; 8192];
        loop {
            match self.stream.read(&mut b) {
                Ok(0) => return Err(()),
                Ok(n) => self.inbuf.extend_from_slice(&b[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return Err(()),
            }
        }
        let mut msgs = vec![];
        while self.inbuf.len() >= 4 {
            let n = u32::from_be_bytes(self.inbuf[..4].try_into().unwrap()) as usize;
            if n > MSG_MAX {
                return Err(());
            }
            if self.inbuf.len() < 4 + n {
                break;
            }
            let m: Vec<u8> = self.inbuf.drain(..4 + n).skip(4).collect();
            msgs.push(serde_json::from_slice(&m).map_err(|_| ())?);
        }
        Ok(msgs)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Enroll,
    Authenticate,
    Detect,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stage {
    /// arm finger-down detection
    Arm,
    /// waiting for a touch
    Touch,
    /// arm finger-up detection
    LiftArm,
    /// waiting for the finger to lift (until)
    Lift(Instant),
}

struct Active {
    kind: Kind,
    stage: Stage,
    deadline: Option<Instant>,
    /// consecutive failed TA calls (a TA that keeps failing ends the operation)
    errors: u32,
}

/// The sensor side once it came up: the TA, the template set loaded, the challenge given out.
struct Hw {
    tee: Tee,
    key_shared: bool,
    store_gen: u32,
}

struct Fp {
    conf: Value,
    hw: Option<Hw>,
    hw_err: String,
    listener: vsock::VsockListener,
    guests: BTreeMap<u32, Guest>,
    /// the VM holding the handle and its CID
    lent: Option<(String, u32)>,
    /// the holder's active group in the TA
    group: Option<u32>,
    challenge: Option<(u64, Instant)>,
    active: Option<Active>,
}

impl Fp {
    // ------------------------------------------------------------------ bring-up

    fn hw_up(conf: &Value) -> Result<Hw, String> {
        let dev = conf["device"].as_str().unwrap_or("/dev/fpc_tee");
        let tee = Tee::open(dev)?;
        log(format!("TA up (app id {})", tee.app_id));
        let state = Path::new(STATE);
        // before the Keymaster app runs: it caches its files for the rest of the boot
        match gk::store_restore(&tee, state) {
            Ok(true) => log("keymaster files restored"),
            Ok(false) => log("keymaster files kept (the app already ran this boot)"),
            Err(e) => log(format!("keymaster files: {e}")),
        }
        let info = tee.build_info()?;
        let build: Value = serde_json::from_str(&info).unwrap_or_default();
        log(format!("TA build {} {}", build["date"].as_str().unwrap_or("?"), build["FPC_BUILD_TYPE"].as_str().unwrap_or("")));
        // Once per TA instance: the TA doesn't free a previous init (a third bio init in one boot failed with
        // -9, out of memory). The instance = app id + the device node's creation (a module reload makes a new one).
        let dev_path = conf["device"].as_str().unwrap_or("/dev/fpc_tee");
        let ctime = std::fs::metadata(dev_path).map(|m| std::os::unix::fs::MetadataExt::ctime(&m)).unwrap_or(0);
        let instance = format!("{} {ctime}", tee.app_id);
        if std::fs::read_to_string(TA_MARK).ok().as_deref() == Some(instance.as_str()) {
            log("TA already initialised (service restart)");
        } else {
            // a failed sensor init leaves the TA unable to take bio commands (it crashes): stop here
            match tee.sensor(0)? {
                0 => {}
                s => return Err(format!("sensor init: {s}")),
            }
            tee.sensor(5)?;
            match tee.bio(0, &[])?.0 {
                0 => {}
                s => return Err(format!("bio init: {s} ({})", tee::fpc_err(s))),
            }
            let _ = std::fs::write(TA_MARK, &instance);
        }
        tee.bio(7, &[])?; // an empty template set until a VM picks its group
        let mut hw = Hw { tee, key_shared: false, store_gen: 0 };
        match gk::share_token_key(&hw.tee) {
            Ok(()) => hw.key_shared = true,
            Err(e) => log(format!("enrolment unavailable for now: {e}")),
        }
        hw.save_store();
        Ok(hw)
    }

    fn handle(&self) -> Handle {
        Handle::new("fingerprint:0", CONTRACT, "Fingerprint reader", "auth-fingerprint-symbolic", Direction::In, "fingerprint")
            .control(CTL, None)
            .caps(json!({
                "sensor_type": self.conf["sensor_type"].as_str().unwrap_or("other"),
                // where the reader is, in panel pixels {x, y, radius} (port configuration; guests place hints there)
                "location": self.conf.get("location").cloned().unwrap_or(Value::Null),
                "vendor": "fpc",
                "max_templates": MAX_TEMPLATES,
                "hw_auth_token": false,
            }))
            .present(self.hw.is_some())
    }

    // ------------------------------------------------------------------ guests

    fn accept(&mut self) {
        while let Ok((stream, addr)) = self.listener.accept() {
            let cid = addr.cid();
            let _ = stream.set_nonblocking(true);
            self.guests.insert(cid, Guest { stream, inbuf: vec![], out: vec![] });
            log(format!("cid {cid}: connected{}", if self.holder(cid) { "" } else { " (slot empty)" }));
            let st = self.state_event(cid);
            self.send(cid, &st);
        }
    }

    fn holder(&self, cid: u32) -> bool {
        self.lent.as_ref().is_some_and(|(_, c)| *c == cid)
    }

    fn state_event(&self, cid: u32) -> Value {
        json!({"type": "event", "event": "state", "attached": self.holder(cid) && self.hw.is_some()})
    }

    fn send(&mut self, cid: u32, v: &Value) {
        if let Some(g) = self.guests.get_mut(&cid) {
            g.send(v);
        }
    }

    /// An event for the holder.
    fn event(&mut self, ev: &str, mut fields: Value) {
        let Some((_, cid)) = self.lent.clone() else { return };
        fields["type"] = json!("event");
        fields["event"] = json!(ev);
        self.send(cid, &fields);
    }

    fn on_guest(&mut self, cid: u32) {
        let Some(g) = self.guests.get_mut(&cid) else { return };
        match g.read() {
            Ok(msgs) => {
                for m in msgs {
                    let id = m["id"].clone();
                    let mut r = match self.request(cid, &m) {
                        Ok(v) => v,
                        Err(e) => json!({"ok": false, "error": e}),
                    };
                    if r.get("ok").is_none() {
                        r["ok"] = json!(true);
                    }
                    r["type"] = json!("response");
                    r["id"] = id;
                    self.send(cid, &r);
                }
            }
            Err(()) => {
                self.guests.remove(&cid);
                log(format!("cid {cid}: disconnected"));
                if self.holder(cid) {
                    self.stop(None);
                    self.group = None;
                }
            }
        }
    }

    fn request(&mut self, cid: u32, m: &Value) -> Result<Value, String> {
        let op = m["op"].as_str().unwrap_or("");
        if op == "hello" {
            let theirs: Vec<&str> = m["versions"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let both = VERSIONS.iter().find(|v| theirs.contains(v));
            return Ok(json!({"version": both, "attached": self.holder(cid) && self.hw.is_some(), "caps": self.handle().caps}));
        }
        if !self.holder(cid) {
            return Err("not_attached".into());
        }
        let vm = self.lent.clone().map(|(v, _)| v).unwrap_or_default();
        if self.hw.is_none() {
            return Err("hw_unavailable".into());
        }
        match op {
            "set_active_group" => {
                let gid = m["gid"].as_u64().filter(|g| *g <= u32::MAX as u64).ok_or("gid")? as u32;
                self.stop(None);
                self.load_group(&vm, gid)?;
                Ok(json!({"authenticator_id": self.tee()?.db_id()?}))
            }
            "enumerate" => {
                self.group.ok_or("no_group")?;
                Ok(json!({"fids": self.tee()?.finger_ids(MAX_TEMPLATES)?}))
            }
            "authenticator_id" => {
                self.group.ok_or("no_group")?;
                Ok(json!({"authenticator_id": self.tee()?.db_id()?}))
            }
            "challenge" => {
                let ch = self.tee()?.enrol_challenge()?;
                self.challenge = Some((ch, Instant::now()));
                Ok(json!({"challenge": ch}))
            }
            "revoke_challenge" => {
                if self.challenge.is_some_and(|(c, _)| Some(c) == m["challenge"].as_u64()) {
                    self.challenge = None;
                }
                Ok(json!({}))
            }
            "enroll" => self.enroll(&vm, m),
            "authenticate" => {
                self.group.ok_or("no_group")?;
                // opens the TA's authentication session (as the vendor HAL); the result it signs is read after a
                // match (get_auth_result) to close it
                let ch = m["challenge"].as_u64().unwrap_or(0);
                let s = self.tee()?.cmd(3, 1, &[ch as u32, (ch >> 32) as u32])?.0;
                if s != 0 {
                    log(format!("set auth challenge: {s} ({})", tee::fpc_err(s)));
                }
                self.start(Kind::Authenticate, None);
                Ok(json!({}))
            }
            "detect" => {
                self.start(Kind::Detect, None);
                Ok(json!({}))
            }
            "cancel" => {
                self.stop(Some("canceled"));
                Ok(json!({}))
            }
            "remove" => self.remove(&vm, m),
            _ => Err(format!("unknown op {op:?}")),
        }
    }

    fn tee(&self) -> Result<&Tee, String> {
        self.hw.as_ref().map(|h| &h.tee).ok_or("hw_unavailable".into())
    }

    fn db_path(vm: &str, gid: u32) -> PathBuf {
        Path::new(STATE).join(vm).join(format!("{gid}.db"))
    }

    /// The group's templates into the TA, then the group (the vendor HAL's set_active_group order).
    fn load_group(&mut self, vm: &str, gid: u32) -> Result<(), String> {
        let path = Self::db_path(vm, gid);
        self.group = None;
        let tee = self.hw.as_ref().map(|h| &h.tee).ok_or("hw_unavailable")?;
        match std::fs::read(&path) {
            Ok(db) if !db.is_empty() => {
                if let Err(e) = tee.db_write(&db) {
                    log(format!("{vm}: group {gid}: {e}; templates dropped"));
                    let _ = std::fs::rename(&path, path.with_extension("db.bad"));
                    tee.bio(7, &[])?;
                }
            }
            _ => {
                tee.bio(7, &[])?;
            }
        }
        match tee.bio(0xa, &[gid])?.0 {
            0 => {}
            s => return Err(format!("set group: {s}")),
        }
        log(format!("{vm}: group {gid} active ({} templates)", tee.finger_ids(MAX_TEMPLATES).map(|f| f.len()).unwrap_or(0)));
        self.group = Some(gid);
        Ok(())
    }

    fn save_group(&self, vm: &str) -> Result<(), String> {
        let gid = self.group.ok_or("no_group")?;
        let tee = self.tee()?;
        match tee.db_read() {
            Ok(db) => gk::write_private(&Self::db_path(vm, gid), &db),
            // no fingers left: nothing to keep
            Err(_) if tee.finger_ids(MAX_TEMPLATES).is_ok_and(|f| f.is_empty()) => {
                let _ = std::fs::remove_file(Self::db_path(vm, gid));
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn enroll(&mut self, vm: &str, m: &Value) -> Result<Value, String> {
        self.group.ok_or("no_group")?;
        let hat = &m["hat"];
        let (ch, at) = self.challenge.ok_or("no_challenge")?;
        if at.elapsed() > CHALLENGE_TTL || hat["challenge"].as_u64() != Some(ch) || hat["user_id"].as_u64().unwrap_or(0) == 0 {
            return Err("bad_token".into());
        }
        self.stop(None);
        if self.tee()?.finger_ids(MAX_TEMPLATES)?.len() as u32 >= MAX_TEMPLATES {
            return Err("no_space".into());
        }
        let hw = self.hw.as_mut().ok_or("hw_unavailable")?;
        // a leftover authentication session (a match whose result was never read) makes the TA refuse enrolment
        // (FPC_ERROR_STATE): read and drop any pending result first; nothing pending is an error, ignored
        let mut res = vec![0u8; 12 + 0x45];
        res[..4].copy_from_slice(&3u32.to_le_bytes());
        res[4..8].copy_from_slice(&4u32.to_le_bytes());
        res[8..12].copy_from_slice(&0x45u32.to_le_bytes());
        let _ = hw.tee.tac(&mut res);
        res.fill(0);
        if !hw.key_shared {
            gk::share_token_key(&hw.tee)?;
            hw.key_shared = true;
        }
        let mut cred = Credential::load(&hw.tee, &Path::new(STATE).join(vm), gk_uid(vm))?;
        let mut token = cred.token(&hw.tee, ch);
        hw.save_store();
        let token_bytes = token.as_mut().map_err(|e| e.clone())?;
        let s = hw.tee.auth_blob(3, token_bytes);
        token_bytes.fill(0);
        match s? {
            0 => {}
            s => return Err(format!("authorize enrol: {s}")),
        }
        match self.tee()?.bio(1, &[])?.0 {
            0 => {}
            s => return Err(format!("begin enrol: {s}")),
        }
        // the challenge stays valid until the guest revokes it (or it expires): Android's Settings starts an
        // enrolment on its find-the-sensor screen, cancels it, then enrols for real with the same token
        let timeout = m["timeout_s"].as_u64().unwrap_or(60).clamp(10, 600);
        self.start(Kind::Enroll, Some(Instant::now() + Duration::from_secs(timeout)));
        log(format!("{vm}: enrolling"));
        Ok(json!({}))
    }

    fn remove(&mut self, vm: &str, m: &Value) -> Result<Value, String> {
        self.group.ok_or("no_group")?;
        self.stop(None);
        let tee = self.tee()?;
        let mut fids: Vec<u32> = m["fids"].as_array().map(|a| a.iter().filter_map(|v| v.as_u64()).map(|v| v as u32).collect()).unwrap_or_default();
        if fids.is_empty() {
            fids = tee.finger_ids(MAX_TEMPLATES)?;
        }
        let mut removed = vec![];
        for fid in fids {
            match tee.bio(9, &[fid])?.0 {
                0 => removed.push(fid),
                s => log(format!("{vm}: remove {fid:#x}: {s}")),
            }
        }
        self.save_group(vm)?;
        Ok(json!({"removed": removed, "authenticator_id": self.tee()?.db_id()?}))
    }

    // ------------------------------------------------------------------ the sensor

    /// A new operation replaces a running one without an error event: the guest asked for it.
    fn start(&mut self, kind: Kind, deadline: Option<Instant>) {
        self.stop(None);
        self.active = Some(Active { kind, stage: Stage::Arm, deadline, errors: 0 });
    }

    /// End the operation (an error event with `reason` to the holder) and put the sensor to sleep.
    fn stop(&mut self, reason: Option<&str>) {
        if self.active.take().is_none() {
            return;
        }
        if let Some(hw) = &self.hw {
            let _ = hw.tee.sensor(5);
        }
        if let Some(r) = reason {
            self.event("error", json!({"code": r}));
        }
    }

    fn acquired(&mut self, info: &str) {
        self.event("acquired", json!({"info": info}));
    }

    /// One step of the operation; true if it waited (the sensor paced the loop).
    fn step(&mut self) -> bool {
        let Some(a) = self.active.as_ref() else { return false };
        let (kind, stage) = (a.kind, a.stage);
        if a.deadline.is_some_and(|d| Instant::now() > d) {
            self.stop(Some("timeout"));
            return false;
        }
        let Some(hw) = self.hw.as_ref() else { return false };
        let tee = &hw.tee;
        let next = match stage {
            Stage::Arm => {
                tee.wait_irq(1); // a stale level
                match tee.sensor(3) {
                    Ok(0) => Stage::Touch,
                    r => return self.failed(&format!("arm: {r:?}")),
                }
            }
            Stage::Touch => {
                if !tee.wait_irq(50) {
                    return true;
                }
                match tee.sensor(4) {
                    Ok(0) => return self.captured(kind),
                    Ok(1) => {
                        self.acquired("insufficient");
                        Stage::LiftArm
                    }
                    Ok(7) => {
                        self.acquired("partial");
                        Stage::LiftArm
                    }
                    Ok(3) => {
                        std::thread::sleep(Duration::from_millis(20));
                        Stage::Arm // no stable finger yet
                    }
                    Ok(s) if s > 0 => Stage::Arm,
                    r => return self.failed(&format!("capture: {r:?}")),
                }
            }
            Stage::LiftArm => {
                tee.wait_irq(1);
                match tee.sensor(2) {
                    Ok(0) => Stage::Lift(Instant::now() + Duration::from_secs(10)),
                    _ => Stage::Arm,
                }
            }
            Stage::Lift(until) => {
                if !tee.wait_irq(50) && Instant::now() < until {
                    return true;
                }
                let _ = tee.sensor(5);
                Stage::Arm
            }
        };
        if let Some(a) = self.active.as_mut() {
            a.stage = next;
        }
        false
    }

    fn failed(&mut self, what: &str) -> bool {
        log(format!("sensor: {what}"));
        let Some(a) = self.active.as_mut() else { return false };
        a.errors += 1;
        a.stage = Stage::Arm;
        if a.errors >= 3 {
            self.stop(Some("unable_to_process"));
        }
        false
    }

    fn captured(&mut self, kind: Kind) -> bool {
        let vm = self.lent.clone().map(|(v, _)| v).unwrap_or_default();
        let Some(tee) = self.hw.as_ref().map(|h| &h.tee) else { return false };
        let mut next = Stage::LiftArm;
        match kind {
            Kind::Detect => {
                self.event("interaction", json!({}));
                self.stop(None);
                return false;
            }
            Kind::Authenticate => {
                let r = tee.bio(4, &[]);
                if let Ok((0, b)) = &r {
                    // as the vendor HAL after an identify: read (and drop: its key is not the guest's) the signed
                    // result of a match, which ends the TA's authentication state, then let the TA adapt the
                    // template and keep the DB if it did (without these the TA answers FPC_ERROR_STATE to the next
                    // identify and enrolment)
                    if u32::from_le_bytes(b[8..12].try_into().unwrap()) != 0 {
                        let mut res = vec![0u8; 12 + 0x45];
                        res[..4].copy_from_slice(&3u32.to_le_bytes());
                        res[4..8].copy_from_slice(&4u32.to_le_bytes());
                        res[8..12].copy_from_slice(&0x45u32.to_le_bytes());
                        if let Ok(s) = tee.tac(&mut res) && s != 0 {
                            log(format!("auth result: {s} ({})", tee::fpc_err(s)));
                        }
                        res.fill(0);
                    }
                    match tee.bio(5, &[]) {
                        Ok((0, u)) if u32::from_le_bytes(u[8..12].try_into().unwrap()) != 0 => {
                            if let Err(e) = self.save_group(&vm) {
                                log(format!("{vm}: store updated templates: {e}"));
                            }
                        }
                        Ok((0, _)) => {}
                        Ok((s, _)) => log(format!("update template: {s} ({})", tee::fpc_err(s))),
                        Err(e) => log(format!("update template: {e}")),
                    }
                }
                match r {
                    Ok((0, b)) if u32::from_le_bytes(b[8..12].try_into().unwrap()) != 0 => {
                        let fid = u32::from_le_bytes(b[8..12].try_into().unwrap());
                        self.acquired("good");
                        self.event("authenticated", json!({"fid": fid}));
                        log(format!("{vm}: match"));
                        self.stop(None);
                        return false;
                    }
                    Ok((s, _)) => {
                        if s != 0 {
                            log(format!("identify: {s} ({})", tee::fpc_err(s)));
                        }
                        self.acquired("good");
                        self.event("authenticated", json!({"fid": 0}));
                    }
                    Err(e) => return self.failed(&e),
                }
            }
            Kind::Enroll => {
                let r = tee.bio(2, &[]);
                let (s, b) = match r {
                    Ok(x) => x,
                    Err(e) => return self.failed(&e),
                };
                let remaining = u32::from_le_bytes(b[8..12].try_into().unwrap());
                match s {
                    0 => return self.enrolled(&vm),
                    6 | 8 => {
                        self.acquired("good");
                        self.event("enroll_progress", json!({"fid": 0, "remaining": remaining.max(1)}));
                    }
                    7 => self.acquired("partial"),
                    9 => self.acquired("dirty"),
                    10 => self.acquired("duplicate"),
                    s if s < 0 => {
                        log(format!("enrol: {s} ({})", tee::fpc_err(s)));
                        self.stop(Some("unable_to_process"));
                        return false;
                    }
                    _ => next = Stage::Arm,
                }
            }
        }
        if let Some(a) = self.active.as_mut() {
            a.stage = next;
            a.errors = 0;
        }
        false
    }

    fn enrolled(&mut self, vm: &str) -> bool {
        let Some(tee) = self.hw.as_ref().map(|h| &h.tee) else { return false };
        let (s, b) = match tee.bio(3, &[]) {
            Ok(x) => x,
            Err(e) => {
                log(format!("end enrol: {e}"));
                self.stop(Some("unable_to_process"));
                return false;
            }
        };
        let fid = u32::from_le_bytes(b[8..12].try_into().unwrap());
        if s != 0 || fid == 0 {
            log(format!("end enrol: {s} ({}), id {fid:#x}", tee::fpc_err(s)));
            self.stop(Some("unable_to_process"));
            return false;
        }
        if let Err(e) = self.save_group(vm) {
            // as the vendor HAL: a template that can't be stored is not kept
            log(format!("{vm}: store templates: {e}"));
            if let Some(tee) = self.hw.as_ref().map(|h| &h.tee) {
                let _ = tee.bio(9, &[fid]);
            }
            self.stop(Some("unable_to_process"));
            return false;
        }
        let auth_id = self.tee().and_then(|t| t.db_id()).unwrap_or(0);
        log(format!("{vm}: enrolled {fid:#x}"));
        self.acquired("good");
        self.event("enrolled", json!({"fid": fid, "authenticator_id": auth_id}));
        self.active = None;
        if let Some(hw) = &self.hw {
            let _ = hw.tee.sensor(5);
        }
        false
    }

    // ------------------------------------------------------------------ the handle

    fn attach(&mut self, vm: &str) -> Result<(), String> {
        if !safe_name(vm) {
            return Err(format!("bad VM name {vm:?}"));
        }
        let cid = vm_cid(vm).ok_or(format!("{vm}: no such VM"))?;
        if self.lent.as_ref().is_some_and(|(v, _)| v != vm) {
            let old = self.lent.clone().unwrap().0;
            self.release(&old, "revoked");
        }
        self.lent = Some((vm.into(), cid));
        self.group = None;
        self.challenge = None;
        if let Some(hw) = &self.hw {
            let _ = hw.tee.bio(7, &[]);
        }
        log(format!("{vm}: attached (cid {cid})"));
        let st = self.state_event(cid);
        self.send(cid, &st);
        Ok(())
    }

    fn release(&mut self, vm: &str, reason: &str) {
        if self.lent.as_ref().is_none_or(|(v, _)| v != vm) {
            return;
        }
        self.stop(Some("canceled"));
        let cid = self.lent.take().map(|(_, c)| c).unwrap();
        self.group = None;
        self.challenge = None;
        if let Some(hw) = &self.hw {
            let _ = hw.tee.bio(7, &[]); // no VM's templates stay loaded
        }
        log(format!("{vm}: detached ({reason})"));
        let st = self.state_event(cid);
        self.send(cid, &st);
    }

    fn control(&mut self, msg: &str) -> String {
        let words: Vec<&str> = msg.split_whitespace().collect();
        match words.as_slice() {
            ["attach", vm, ..] => match self.attach(vm) {
                Ok(()) => "ok".into(),
                Err(e) => format!("error: {e}"),
            },
            ["detach", vm, rest @ ..] => {
                let reason = rest.first().copied().filter(|r| REASONS.contains(r)).unwrap_or("user");
                self.release(vm, reason);
                "ok".into()
            }
            ["status"] => json!({
                "hw": self.hw.is_some(),
                "error": self.hw_err,
                "lent": self.lent.as_ref().map(|(v, _)| v),
                "group": self.group,
                "active": self.active.as_ref().map(|a| format!("{:?}", a.kind)),
                "guests": self.guests.keys().collect::<Vec<_>>(),
            }).to_string(),
            _ => "error: attach <vm> | detach <vm> [reason] | status".into(),
        }
    }
}

impl Hw {
    fn save_store(&mut self) {
        match gk::store_save(&self.tee, Path::new(STATE), self.store_gen) {
            Ok(g) => self.store_gen = g,
            Err(e) => log(format!("keymaster files: {e}")),
        }
    }
}

fn serve() -> Result<(), String> {
    let conf: Value = std::fs::read_to_string(CONF).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let _ = std::fs::create_dir_all(STATE);
    let _ = std::fs::set_permissions(STATE, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let (hw, hw_err) = match Fp::hw_up(&conf) {
        Ok(hw) => (Some(hw), String::new()),
        Err(e) => {
            log(format!("fingerprint reader unavailable: {e}"));
            (None, e)
        }
    };
    let listener = vsock::VsockListener::bind_with_cid_port(libc::VMADDR_CID_ANY, PORT).map_err(|e| format!("vsock {PORT}: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    fresh(Path::new(CTL));
    let ctl = UnixDatagram::bind(CTL).map_err(|e| format!("{CTL}: {e}"))?;
    for_session(Path::new(CTL), 0o660);
    let mut fp = Fp { conf, hw, hw_err, listener, guests: BTreeMap::new(), lent: None, group: None, challenge: None, active: None };
    publish_handles("fingerprint", &[fp.handle()]);
    log(format!("vsock port {PORT}, control {CTL}"));

    loop {
        let waited = fp.step();
        let mut fds = vec![ctl.as_raw_fd(), fp.listener.as_raw_fd()];
        let guests: Vec<(i32, u32)> = fp.guests.iter().map(|(c, g)| (g.stream.as_raw_fd(), *c)).collect();
        fds.extend(guests.iter().map(|(fd, _)| *fd));
        let timeout = if fp.active.is_some() { Some(Duration::from_millis(if waited { 0 } else { 5 })) } else { None };
        let ready = poll(&fds, timeout);
        if ready.contains(&ctl.as_raw_fd()) {
            let mut m = [0u8; 512];
            if let Ok((n, from)) = ctl.recv_from(&mut m) {
                let reply = fp.control(&String::from_utf8_lossy(&m[..n]));
                if let Some(p) = from.as_pathname() {
                    let _ = ctl.send_to(reply.as_bytes(), p);
                }
            }
        }
        if ready.contains(&fp.listener.as_raw_fd()) {
            fp.accept();
        }
        for (fd, cid) in guests {
            if ready.contains(&fd) {
                fp.on_guest(cid);
            }
        }
        fp.guests.retain(|_, g| g.flush());
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: mship-fingerprint (control: {CTL}, guests: vsock port {PORT})");
        return std::process::ExitCode::from(2);
    }
    match serve() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            log(e);
            std::process::ExitCode::FAILURE
        }
    }
}
