//! mship-bt-bridge: the mothershipOS Bluetooth bridge (root), the mship.bt 1.x contract (docs/contracts/mship.bt-1.md).
//!
//! The handle bt:hci0 is the phone's Bluetooth controller at the HCI level. The driver, firmware download and power
//! stay on the host; the VM runs its own Bluetooth stack against the controller. A VM's bt slot is virtio-console
//! port 5 (hvc5, H4 framing), which crosvm connects to /run/mothership/bt/<vm>.sock. While the VM holds the
//! handle, its packets go to an HCI_CHANNEL_USER socket on the controller and back, unchanged. That takes the
//! controller away from the host's kernel stack and BlueZ; the kernel gives it back when the socket closes.
//!
//! Control: datagrams on /run/mothership/bt-bridge.sock, one reply each: `attach <vm>` (lend the controller to the
//! VM, by its directory name), `detach <vm> [user|screen|stop|revoked|gone]`, `status`.
//!
//! A lend that arrives before the VM has connected is kept and applied when it connects. Empty slot: the guest's
//! packets are dropped and nothing answers. A guest HAL waiting for its initial HCI_Reset (the Cuttlefish HAL sends
//! it on initialize) gets it answered by the controller at attach, so a stack that is starting comes up then;
//! otherwise attach sends a Hardware Error event and the guest's stack restarts against the controller. Release:
//! HCI_Reset on the controller (drops every link), socket closed (the controller returns to BlueZ, which restores
//! its own power state), then a Hardware Error event to the guest so its stack restarts.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mship_devices::{for_session, fresh, log, poll};
use serde_json::json;

const CONF: &str = "/etc/mothership/bt-bridge.json";
const CTL: &str = "/run/mothership/bt-bridge.sock";
const SOCKS: &str = "/run/mothership/bt";
const VMS: &str = "/var/lib/mothership/vms";
const REASONS: &[&str] = &["user", "screen", "stop", "revoked", "gone"];

const AF_BLUETOOTH: i32 = 31;
const BTPROTO_HCI: i32 = 1;
const HCI_CHANNEL_USER: u16 = 1;
/// _IOW('H', 202, int)
const HCIDEVDOWN: libc::c_ulong = 0x400448ca;
const CMD: u8 = 1;
const ACL: u8 = 2;
const SCO: u8 = 3;
const EVT: u8 = 4;
const ISO: u8 = 5;
const HCI_RESET: &[u8] = &[CMD, 0x03, 0x0c, 0x00];
/// Hardware Error event, code 0
const HW_ERROR: &[u8] = &[EVT, 0x10, 0x01, 0x00];
/// bytes queued towards a guest that doesn't read; beyond that packets are dropped
const OUT_MAX: usize = 1 << 20;

/// H4 packet type -> (header length, payload length from the header)
fn h4_len(t: u8, h: &[u8]) -> Option<(usize, usize)> {
    Some(match t {
        CMD | SCO => (3, h.get(2).copied()? as usize),
        ACL => (4, u16::from_le_bytes([*h.get(2)?, *h.get(3)?]) as usize),
        ISO => (4, (u16::from_le_bytes([*h.get(2)?, *h.get(3)?]) & 0x3fff) as usize),
        _ => return None,
    })
}

/// Whole H4 packets from the front of `buf` (the rest stays); Err(type) on a bad packet type.
fn split_h4(buf: &mut Vec<u8>) -> Result<Vec<Vec<u8>>, u8> {
    let mut out = vec![];
    while let Some(&t) = buf.first() {
        if !matches!(t, CMD | ACL | SCO | ISO) {
            return Err(t);
        }
        let Some((hl, plen)) = h4_len(t, &buf[1..]) else { break };
        let n = 1 + hl + plen;
        if buf.len() < n {
            break;
        }
        out.push(buf.drain(..n).collect());
    }
    Ok(out)
}

#[repr(C)]
struct SockaddrHci {
    family: u16,
    dev: u16,
    channel: u16,
}

/// The controller as a raw HCI_CHANNEL_USER socket (taken down first: BlueZ may have it powered).
fn open_user_channel(dev: u16) -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_RAW | libc::SOCK_CLOEXEC, BTPROTO_HCI) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ioctl(fd.as_raw_fd(), HCIDEVDOWN, dev as libc::c_int) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EALREADY) {
            return Err(e);
        }
    }
    let addr = SockaddrHci { family: AF_BLUETOOTH as u16, dev, channel: HCI_CHANNEL_USER };
    for _ in 0..20 {
        // EBUSY while the kernel or BlueZ is still busy with the device
        if unsafe { libc::bind(fd.as_raw_fd(), (&addr as *const SockaddrHci).cast(), std::mem::size_of::<SockaddrHci>() as u32) } == 0 {
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
            return Ok(fd);
        }
        let e = std::io::Error::last_os_error();
        if !matches!(e.raw_os_error(), Some(libc::EBUSY) | Some(libc::EALREADY)) {
            return Err(e);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(std::io::Error::other("hci device stays busy"))
}

fn send_fd(fd: RawFd, b: &[u8]) -> bool {
    unsafe { libc::send(fd, b.as_ptr().cast(), b.len(), libc::MSG_NOSIGNAL) == b.len() as isize }
}

fn recv_fd(fd: RawFd) -> Option<Vec<u8>> {
    let mut b = vec![0u8; 65536];
    let n = unsafe { libc::recv(fd, b.as_mut_ptr().cast(), b.len(), libc::MSG_DONTWAIT) };
    (n > 0).then(|| {
        b.truncate(n as usize);
        b
    })
}

/// HCI_Reset and wait (<= 1 s) for its Command Complete: every connection and pending operation ends.
fn reset_controller(fd: RawFd) -> bool {
    if !send_fd(fd, HCI_RESET) {
        return false;
    }
    let end = Instant::now() + Duration::from_secs(1);
    while Instant::now() < end {
        if poll(&[fd], Some(end - Instant::now())).is_empty() {
            break;
        }
        if let Some(p) = recv_fd(fd)
            && p.starts_with(&[EVT, 0x0e])
            && p.get(4..6) == Some(&[0x03, 0x0c])
        {
            return true;
        }
    }
    false
}

/// One VM's bt slot: the H4 stream from its hvc5, and the controller while the VM holds it.
struct Guest {
    path: PathBuf,
    listener: UnixListener,
    conn: Option<UnixStream>,
    hci: Option<OwnedFd>,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    dropped: u64,
    /// the guest's last command while empty was HCI_Reset (its HAL is waiting)
    pending_reset: bool,
}

impl Guest {
    fn new(name: &str) -> std::io::Result<Self> {
        let path = Path::new(SOCKS).join(format!("{name}.sock"));
        fresh(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        for_session(&path, 0o660);
        Ok(Self { path, listener, conn: None, hci: None, inbuf: vec![], out: vec![], dropped: 0, pending_reset: false })
    }

    fn queue(&mut self, pkt: &[u8]) {
        if self.conn.is_none() {
            return;
        }
        if self.out.len() + pkt.len() > OUT_MAX {
            self.dropped += 1;
            return;
        }
        self.out.extend_from_slice(pkt);
        self.flush();
    }

    /// false: the guest is gone
    fn flush(&mut self) -> bool {
        let Some(c) = self.conn.as_mut() else { return true };
        while !self.out.is_empty() {
            match c.write(&self.out) {
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }
        true
    }
}

struct Bridge {
    hci_dev: u16,
    guests: BTreeMap<String, Guest>,
    /// VMs the handle is lent to (applied when the VM connects)
    lent: BTreeSet<String>,
}

impl Bridge {
    /// one bt socket per VM directory
    fn scan(&mut self) {
        let names: BTreeSet<String> = std::fs::read_dir(VMS)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let new: Vec<String> = names.iter().filter(|n| !self.guests.contains_key(*n)).cloned().collect();
        for n in &new {
            match Guest::new(n) {
                Ok(g) => {
                    self.guests.insert(n.clone(), g);
                }
                Err(e) => log(format!("{n}: {e}")),
            }
        }
        let gone: Vec<String> = self.guests.keys().filter(|n| !names.contains(*n)).cloned().collect();
        for n in gone {
            self.disconnect(&n);
            if let Some(g) = self.guests.remove(&n) {
                let _ = std::fs::remove_file(g.path);
            }
        }
    }

    fn accept(&mut self, name: &str) {
        let Some(Ok((conn, _))) = self.guests.get(name).map(|g| g.listener.accept()) else { return };
        if self.guests[name].conn.is_some() {
            log(format!("{name}: new connection replaces the old one"));
            self.disconnect(name);
        }
        let _ = conn.set_nonblocking(true);
        let g = self.guests.get_mut(name).unwrap();
        g.conn = Some(conn);
        g.inbuf.clear();
        g.out.clear();
        g.pending_reset = false;
        log(format!("{name}: guest connected"));
        if self.lent.contains(name) {
            self.attach(name);
        }
    }

    fn disconnect(&mut self, name: &str) {
        self.release(name, "stop");
        if let Some(g) = self.guests.get_mut(name)
            && g.conn.take().is_some()
        {
            log(format!("{name}: guest disconnected"));
        }
    }

    fn on_guest(&mut self, name: &str) {
        let g = self.guests.get_mut(name).unwrap();
        if !g.flush() {
            return self.disconnect(name);
        }
        let mut b = vec![0u8; 65536];
        let n = match g.conn.as_mut().map(|c| c.read(&mut b)) {
            Some(Ok(n)) => n,
            Some(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            _ => 0,
        };
        if n == 0 {
            return self.disconnect(name);
        }
        g.inbuf.extend_from_slice(&b[..n]);
        let pkts = match split_h4(&mut g.inbuf) {
            Ok(p) => p,
            Err(t) => {
                log(format!("{name}: bad H4 packet type {t:#x} from the guest, stream reset"));
                g.inbuf.clear();
                vec![]
            }
        };
        for pkt in pkts {
            let g = self.guests.get_mut(name).unwrap();
            match g.hci.as_ref() {
                Some(h) => {
                    if !send_fd(h.as_raw_fd(), &pkt) {
                        log(format!("{name}: controller write failed ({})", std::io::Error::last_os_error()));
                        self.release(name, "gone");
                    }
                }
                // empty slot: dropped, nothing answers
                None if pkt[0] == CMD => g.pending_reset = pkt == HCI_RESET,
                None => {}
            }
        }
    }

    /// fresh on attach: the guest's stack restarts and initializes the controller itself
    fn attach(&mut self, name: &str) -> bool {
        let Some(g) = self.guests.get(name) else { return false };
        if g.hci.is_some() || g.conn.is_none() {
            return true;
        }
        let holders: Vec<String> = self.guests.iter().filter(|(_, g)| g.hci.is_some()).map(|(n, _)| n.clone()).collect();
        for h in holders {
            self.release(&h, "revoked");
        }
        let hci = match open_user_channel(self.hci_dev) {
            Ok(h) => h,
            Err(e) => {
                log(format!("{name}: hci{}: {e}", self.hci_dev));
                return false;
            }
        };
        let g = self.guests.get_mut(name).unwrap();
        if g.pending_reset {
            // its Reset Complete finishes the guest HAL's initialization
            g.pending_reset = false;
            send_fd(hci.as_raw_fd(), HCI_RESET);
        } else {
            g.queue(HW_ERROR);
        }
        g.hci = Some(hci);
        log(format!("{name}: attached hci{}", self.hci_dev));
        true
    }

    /// abort, never complete: links dropped by the reset, in-flight packets lost, the guest's stack restarts
    fn release(&mut self, name: &str, reason: &str) {
        let Some(g) = self.guests.get_mut(name) else { return };
        let Some(hci) = g.hci.take() else { return };
        reset_controller(hci.as_raw_fd());
        drop(hci);
        g.queue(HW_ERROR);
        log(format!("{name}: released ({reason}); hci{} back to the host", self.hci_dev));
    }

    fn on_hci(&mut self, name: &str) {
        let g = self.guests.get_mut(name).unwrap();
        let Some(fd) = g.hci.as_ref().map(|h| h.as_raw_fd()) else { return };
        let mut b = vec![0u8; 65536];
        let n = unsafe { libc::recv(fd, b.as_mut_ptr().cast(), b.len(), libc::MSG_DONTWAIT) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
            return;
        }
        if n <= 0 {
            if n < 0 {
                log(format!("{name}: controller read failed ({})", std::io::Error::last_os_error()));
            }
            return self.release(name, "gone");
        }
        g.queue(&b[..n as usize]);
    }

    fn control(&mut self, msg: &str) -> String {
        self.scan();
        let w: Vec<&str> = msg.split_whitespace().collect();
        match w.as_slice() {
            ["attach", vm, ..] if self.guests.contains_key(*vm) => {
                let others: Vec<String> = self.lent.iter().filter(|n| n != vm).cloned().collect();
                for o in others {
                    // one owner
                    self.lent.remove(&o);
                    self.release(&o, "revoked");
                }
                self.lent.insert(vm.to_string());
                if self.attach(vm) { "ok".into() } else { "error: controller unavailable".into() }
            }
            ["detach", vm, rest @ ..] => {
                let reason = rest.first().filter(|r| REASONS.contains(r)).copied().unwrap_or("user");
                self.lent.remove(*vm);
                self.release(vm, reason);
                "ok".into()
            }
            ["status", ..] => json!({
                "hci": format!("hci{}", self.hci_dev),
                "lent": self.lent,
                "vms": self.guests.iter().filter(|(n, g)| g.conn.is_some() || self.lent.contains(*n))
                    .map(|(n, g)| (n.clone(), json!({"connected": g.conn.is_some(), "attached": g.hci.is_some(), "dropped": g.dropped})))
                    .collect::<serde_json::Map<_, _>>(),
            })
            .to_string(),
            _ => "error: attach <vm> | detach <vm> [reason] | status".into(),
        }
    }
}

fn serve() -> std::io::Result<()> {
    let hci_dev = std::fs::read_to_string(CONF)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|c| c["hci"].as_u64())
        .unwrap_or(0) as u16;
    std::fs::create_dir_all(SOCKS)?;
    for_session(Path::new(SOCKS), 0o750);
    let mut b = Bridge { hci_dev, guests: BTreeMap::new(), lent: BTreeSet::new() };
    b.scan();
    fresh(Path::new(CTL));
    let ctl = UnixDatagram::bind(CTL)?;
    for_session(Path::new(CTL), 0o660);
    log(format!("serving hci{hci_dev} for {} VMs in {SOCKS}", b.guests.len()));
    let mut last = Instant::now();
    loop {
        // (fd, what): 0 control, 1 listener, 2 guest stream, 3 controller
        let mut fds: Vec<(RawFd, u8, String)> = vec![(ctl.as_raw_fd(), 0, String::new())];
        for (n, g) in &b.guests {
            fds.push((g.listener.as_raw_fd(), 1, n.clone()));
            if let Some(c) = &g.conn {
                fds.push((c.as_raw_fd(), 2, n.clone()));
            }
            if let Some(h) = &g.hci {
                fds.push((h.as_raw_fd(), 3, n.clone()));
            }
        }
        // a guest with queued output: poll its writability too, briefly
        let backlog = b.guests.values().any(|g| !g.out.is_empty());
        let raw: Vec<RawFd> = fds.iter().map(|f| f.0).collect();
        let ready = poll(&raw, Some(if backlog { Duration::from_millis(20) } else { Duration::from_secs(10) }));
        for (fd, what, name) in &fds {
            if !ready.contains(fd) && !(*what == 2 && backlog) {
                continue;
            }
            match what {
                0 => {
                    let mut m = [0u8; 512];
                    if let Ok((n, peer)) = ctl.recv_from(&mut m) {
                        let reply = b.control(&String::from_utf8_lossy(&m[..n]));
                        if let Some(p) = peer.as_pathname() {
                            let _ = ctl.send_to(reply.as_bytes(), p);
                        }
                    }
                }
                1 => b.accept(name),
                2 if b.guests.get(name).is_some_and(|g| g.conn.is_some()) => b.on_guest(name),
                3 => b.on_hci(name),
                _ => {}
            }
        }
        if last.elapsed() >= Duration::from_secs(10) {
            // new or removed VM directories
            b.scan();
            last = Instant::now();
        }
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: mship-bt-bridge (control: {CTL})");
        return std::process::ExitCode::from(2);
    }
    if let Err(e) = serve() {
        eprintln!("mship-bt-bridge: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h4_framing() {
        let mut b = vec![CMD, 0x03, 0x0c, 0x00, ACL, 0x01, 0x20, 0x02, 0x00, 0xaa, 0xbb, CMD, 0x01];
        assert_eq!(split_h4(&mut b).unwrap(), vec![HCI_RESET.to_vec(), vec![ACL, 0x01, 0x20, 0x02, 0x00, 0xaa, 0xbb]]);
        assert_eq!(b, [CMD, 0x01]); // incomplete: waits for the rest
        let mut bad = vec![0x09, 0x00];
        assert_eq!(split_h4(&mut bad), Err(0x09));
        let mut iso = vec![ISO, 0, 0, 0x01, 0x40, 0xff]; // length masked to 14 bits
        assert_eq!(split_h4(&mut iso).unwrap().len(), 1);
    }
}
