//! mship-usb: the mothershipOS USB devices (root), the mship.usb 1.x contract (docs/contracts/mship.usb-1.md).
//!
//! The handle usb:host is every USB device plugged into the phone. While a VM holds it, each device (except hubs;
//! the devices behind a hub go one by one) is attached to the VM's virtual xHCI controller: crosvm gets the
//! device's usbdevfs node (opened here, as root) and the guest drives it with its own drivers. Devices plugged in
//! while lent are attached on arrival. Release: each device is detached from the VM (the guest sees it unplugged),
//! then reset, so the host's drivers bind to it again from a fresh state.
//!
//! Control: datagrams on /run/mothership/usb.sock, one reply each: `attach <vm>` (lend the USB devices to the VM,
//! by its directory name), `detach <vm> [user|screen|stop|revoked|gone]`, `status`.
//!
//! Configuration: /etc/mothership/usb.json {"exclude": ["<port path>" | "<vid>:<pid>", ...]}: devices the host
//! always keeps. VMM: crosvm (control socket <vm dir>/crosvm.sock, `crosvm usb attach`).

use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use mship_devices::{for_session, fresh, log, poll, rd, read_uevent, uevents};
use serde::Serialize;
use serde_json::json;

const CONF: &str = "/etc/mothership/usb.json";
const CTL: &str = "/run/mothership/usb.sock";
/// survives a service restart (not a reboot)
const OWNER: &str = "/run/mothership/usb-owner";
const VMS: &str = "/var/lib/mothership/vms";
const SYS: &str = "/sys/bus/usb/devices";
const REASONS: &[&str] = &["user", "screen", "stop", "revoked", "gone"];
/// _IO('U', 20)
const USBDEVFS_RESET: libc::c_ulong = 0x5514;
const RESCAN: Duration = Duration::from_secs(5);
/// before a failed attach is tried again
const RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Dev {
    bus: u32,
    dev: u32,
    vid: String,
    pid: String,
    product: String,
    hub: bool,
}

/// a device by port path ("1-1.2"; root hubs are usbN, interfaces have ':')
fn is_device(n: &str) -> bool {
    n.split_once('-').is_some_and(|(b, p)| {
        !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit()) && !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit() || c == b'.')
    })
}

/// port path -> device; only devices that are fully enumerated
fn devices() -> BTreeMap<String, Dev> {
    let attr = |n: &str, k: &str| rd(Path::new(SYS).join(n).join(k));
    std::fs::read_dir(SYS)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            if !is_device(&n) || attr(&n, "devnum").is_empty() {
                return None;
            }
            let product = [attr(&n, "product"), attr(&n, "manufacturer")].into_iter().find(|s| !s.is_empty()).unwrap_or_else(|| "?".into());
            Some((n.clone(), Dev {
                bus: attr(&n, "busnum").parse().unwrap_or(0),
                dev: attr(&n, "devnum").parse().unwrap_or(0),
                vid: attr(&n, "idVendor"),
                pid: attr(&n, "idProduct"),
                product,
                hub: attr(&n, "bDeviceClass") == "09",
            }))
        })
        .collect()
}

fn node(d: &Dev) -> String {
    format!("/dev/bus/usb/{:03}/{:03}", d.bus, d.dev)
}

/// fresh state, and the host's drivers bind again (usb_reset_device)
fn reset(d: &Dev) -> bool {
    let Ok(f) = std::fs::OpenOptions::new().write(true).open(node(d)) else { return false };
    if unsafe { libc::ioctl(f.as_raw_fd(), USBDEVFS_RESET, 0) } != 0 {
        log(format!("reset {}: {}", node(d), std::io::Error::last_os_error()));
        return false;
    }
    true
}

/// interface -> its driver
fn drivers(path: &str) -> BTreeMap<String, Option<String>> {
    std::fs::read_dir(Path::new(SYS).join(path))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("{path}:")))
        .map(|e| {
            let drv = std::fs::read_link(e.path().join("driver")).ok().and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()));
            (e.file_name().to_string_lossy().into_owned(), drv)
        })
        .collect()
}

/// host default after a release: once crosvm has let go of the device (its usbfs claims are gone), reset it and
/// let the host's drivers probe every interface that has none
fn give_back(path: &str, d: &Dev) {
    for _ in 0..30 {
        if !drivers(path).values().any(|v| v.as_deref() == Some("usbfs")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    reset(d);
    for (i, drv) in drivers(path) {
        if drv.is_none() {
            let _ = std::fs::write("/sys/bus/usb/drivers_probe", &i);
        }
    }
}

fn crosvm_bin() -> String {
    std::env::var("MSHIP_CROSVM").unwrap_or_else(|_| "/opt/mship-gpu/bin/crosvm".into())
}

fn crosvm(args: &[&str]) -> (bool, String) {
    match Command::new("timeout").arg("10").arg(crosvm_bin()).args(["--log-level=error", "usb"]).args(args).output() {
        Ok(o) => (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim().to_string()),
        Err(e) => (false, e.to_string()),
    }
}

/// crosvm's answer to `usb attach`: "ok <port>"
fn attached_port(out: &str) -> Option<u32> {
    let w: Vec<&str> = out.split_whitespace().collect();
    w.windows(2).find(|p| p[0] == "ok").and_then(|p| p[1].parse().ok())
}

/// crosvm's `usb list`: "devices <port> <vid> <pid> ..." -> [(port, vid, pid)]
fn parse_list(out: &str) -> Vec<(u32, String, String)> {
    let w: Vec<&str> = out.split_whitespace().collect();
    if w.first() != Some(&"devices") {
        return vec![];
    }
    w[1..].as_chunks::<3>().0.iter().filter_map(|c| Some((c[0].parse().ok()?, c[1].to_string(), c[2].to_string()))).collect()
}

/// a running crosvm VM's control socket
fn vmm(vm: Option<&str>) -> Option<String> {
    let s = Path::new(VMS).join(vm?).join("crosvm.sock");
    s.exists().then(|| s.to_string_lossy().into_owned())
}

struct Attached {
    d: Dev,
    port: u32,
    vm: String,
}

struct Service {
    exclude: Vec<String>,
    /// VM the devices are lent to (applied while its crosvm socket is there)
    owner: Option<String>,
    attached: BTreeMap<String, Attached>,
    /// (port path, devnum, vm) -> a failed attach: retried after RETRY
    failed: BTreeMap<(String, u32, String), Instant>,
    /// why the devices of a VM that is no longer the owner go back (the last detach's reason)
    revoked: &'static str,
}

impl Service {
    fn set_owner(&mut self, vm: Option<&str>) {
        self.owner = vm.map(String::from);
        let tmp = format!("{OWNER}.tmp");
        if std::fs::write(&tmp, vm.unwrap_or("")).is_ok() {
            let _ = std::fs::rename(&tmp, OWNER);
        }
    }

    /// after a restart: adopt what running VMs still have from us (the owner's), detach the rest
    fn recover(&mut self) {
        let now = devices();
        let mut vms: Vec<String> = std::fs::read_dir(VMS).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        vms.sort();
        for vm in vms {
            let Some(sock) = vmm(Some(&vm)) else { continue };
            let (ok, out) = crosvm(&["list", &sock]);
            if !ok {
                continue;
            }
            for (port, vid, pid) in parse_list(&out) {
                // crosvm lists vendor:product only: match it to a device not adopted yet
                let path = now.iter().find(|(p, d)| (d.vid == vid && d.pid == pid) && !self.attached.contains_key(*p)).map(|(p, _)| p.clone());
                match path {
                    Some(p) if self.owner.as_deref() == Some(vm.as_str()) => {
                        log(format!("{p}: still attached to {vm} ({port})"));
                        self.attached.insert(p.clone(), Attached { d: now[&p].clone(), port, vm: vm.clone() });
                    }
                    p => {
                        crosvm(&["detach", &port.to_string(), &sock]);
                        log(format!("{}: detached from {vm} ({port}, not lent)", p.clone().unwrap_or(format!("{vid}:{pid}"))));
                        if let Some(p) = p {
                            give_back(&p, &now[&p]);
                        }
                    }
                }
            }
        }
    }

    fn lendable(&self, path: &str, d: &Dev) -> bool {
        !d.hub && !self.exclude.iter().any(|e| e == path || *e == format!("{}:{}", d.vid, d.pid))
    }

    fn attach(&mut self, path: &str, d: &Dev) -> bool {
        let Some(owner) = self.owner.clone() else { return false };
        let Some(sock) = vmm(Some(&owner)) else { return false }; // VM not running (yet): attached when it is
        reset(d); // fresh on attach: no host state carries over
        let (ok, out) = crosvm(&["attach", &format!("1:1:{}:{}", d.bus, d.dev), &node(d), &sock]);
        match attached_port(&out).filter(|_| ok) {
            Some(port) => {
                log(format!("{path} {}:{} {}: attached to {owner} ({port})", d.vid, d.pid, d.product));
                self.attached.insert(path.into(), Attached { d: d.clone(), port, vm: owner });
                true
            }
            None => {
                log(format!("{path} {}:{} {}: attach to {owner} failed: {out}", d.vid, d.pid, d.product));
                self.failed.insert((path.into(), d.dev, owner), Instant::now());
                false
            }
        }
    }

    fn release(&mut self, path: &str, reason: &str, present: bool) {
        let Some(a) = self.attached.remove(path) else { return };
        if let Some(sock) = vmm(Some(&a.vm)) {
            let (ok, out) = crosvm(&["detach", &a.port.to_string(), &sock]); // the guest sees an unplug
            if !ok {
                log(format!("{path}: detach from {}: {out}", a.vm));
            }
        }
        if present {
            give_back(path, &a.d); // host default: the host's drivers again
        }
        log(format!("{path} {}:{} {}: released ({reason})", a.d.vid, a.d.pid, a.d.product));
    }

    /// attach what the owner should have, release what is gone or not the owner's
    fn sync(&mut self) {
        let now = devices();
        let held: Vec<(String, u32, String)> = self.attached.iter().map(|(p, a)| (p.clone(), a.d.dev, a.vm.clone())).collect();
        for (path, dev, vm) in held {
            match now.get(&path) {
                // unplugged (or re-enumerated: a new device)
                d if d.is_none_or(|d| d.dev != dev) => self.release(&path, "gone", d.is_some()),
                _ if Some(&vm) != self.owner.as_ref() => self.release(&path, self.revoked, true),
                _ if vmm(Some(&vm)).is_none() => self.release(&path, "stop", true), // the VM stopped
                _ => {}
            }
        }
        self.revoked = "revoked";
        if let Some(o) = self.owner.clone()
            && vmm(Some(&o)).is_none()
        {
            // the owner is not running (it stopped): its next start gets the devices when mothershipd lends them
            // again, not while it boots (its xHCI is not ready then, and a refused attach can wedge crosvm's USB)
            log(format!("{o} is not running: the devices stay on the host"));
            self.set_owner(None);
        }
        let Some(owner) = self.owner.clone() else { return };
        for (path, d) in &now {
            let retry = self.failed.get(&(path.clone(), d.dev, owner.clone())).is_none_or(|t| t.elapsed() > RETRY);
            if !self.attached.contains_key(path) && self.lendable(path, d) && retry {
                self.attach(path, d);
            }
        }
    }

    fn control(&mut self, msg: &str) -> String {
        let w: Vec<&str> = msg.split_whitespace().collect();
        let valid = |vm: &str| vm.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c)) && Path::new(VMS).join(vm).is_dir();
        match w.as_slice() {
            ["attach", vm, ..] if valid(vm) => {
                if vmm(Some(vm)).is_none() {
                    return format!("error: {vm} is not running");
                }
                if self.owner.as_deref() != Some(*vm) {
                    self.set_owner(Some(vm)); // one owner: another VM's devices are released by sync
                }
                self.failed.clear();
                "ok".into() // attached by the sync after this reply (crosvm takes a while per device)
            }
            ["detach", vm, rest @ ..] => {
                if self.owner.as_deref() == Some(*vm) {
                    self.set_owner(None);
                    // released by the sync after this reply
                    self.revoked = REASONS.iter().find(|r| rest.first() == Some(*r)).copied().unwrap_or("user");
                }
                "ok".into()
            }
            ["status", ..] => json!({
                "owner": self.owner,
                "devices": devices().into_iter().map(|(p, d)| {
                    let mut v = serde_json::to_value(&d).unwrap();
                    v["attached"] = json!(self.attached.contains_key(&p));
                    (p, v)
                }).collect::<serde_json::Map<_, _>>(),
            })
            .to_string(),
            _ => "error: attach <vm> | detach <vm> [reason] | status".into(),
        }
    }
}

fn serve() -> std::io::Result<()> {
    let exclude = std::fs::read_to_string(CONF)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|c| serde_json::from_value(c["exclude"].clone()).ok())
        .unwrap_or_default();
    let owner = Some(rd(OWNER)).filter(|o| !o.is_empty());
    let mut s = Service { exclude, owner, attached: BTreeMap::new(), failed: BTreeMap::new(), revoked: "revoked" };
    s.recover();
    let ue = uevents().map_err(|e| log(format!("no uevents ({e}), rescanning only"))).ok();
    fresh(Path::new(CTL));
    let ctl = UnixDatagram::bind(CTL)?;
    for_session(Path::new(CTL), 0o660);
    log(format!("serving {CTL}; {} USB devices, lent to {}", devices().len(), s.owner.as_deref().unwrap_or("nobody")));
    loop {
        let mut fds = vec![ctl.as_raw_fd()];
        fds.extend(ue.as_ref().map(|u| u.as_raw_fd()));
        let ready = poll(&fds, Some(RESCAN));
        if ready.contains(&ctl.as_raw_fd()) {
            let mut m = [0u8; 512];
            if let Ok((n, peer)) = ctl.recv_from(&mut m) {
                let reply = s.control(&String::from_utf8_lossy(&m[..n]));
                if let Some(p) = peer.as_pathname() {
                    let _ = ctl.send_to(reply.as_bytes(), p);
                }
            }
        }
        if let Some(u) = ue.as_ref().filter(|u| ready.contains(&u.as_raw_fd())) {
            let mut usb = false;
            while let Some(m) = read_uevent(u.as_raw_fd()) {
                let has = |k: &[u8]| m.split(|b| *b == 0).any(|f| f == k);
                usb |= has(b"SUBSYSTEM=usb") && has(b"DEVTYPE=usb_device");
            }
            if usb {
                std::thread::sleep(Duration::from_millis(200)); // let the new device finish enumerating
            }
        }
        s.sync();
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: mship-usb (control: {CTL})");
        return std::process::ExitCode::from(2);
    }
    if let Err(e) = serve() {
        eprintln!("mship-usb: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_answers() {
        assert!(is_device("1-1") && is_device("2-1.4.2"));
        assert!(!is_device("usb1") && !is_device("1-1:1.0") && !is_device("1-"));
        assert_eq!(attached_port("ok 3"), Some(3));
        assert_eq!(attached_port("error: no free port"), None);
        assert_eq!(parse_list("devices 1 046d c52b 2 0bda 8153"), vec![(1, "046d".into(), "c52b".into()), (2, "0bda".into(), "8153".into())]);
        assert!(parse_list("no devices").is_empty());
    }
}
