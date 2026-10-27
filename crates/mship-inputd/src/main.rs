//! mship-inputd: the VM input bridge (user mship). It lends the phone's touchscreen and buttons to a crosvm VM.
//!
//! `mship-inputd <vm dir> <touch evdev> <key evdev>[,<key evdev>...] [<power key evdev>]`
//!
//! On the nio, volume up and the assist key are gpio-keys, volume down is the PMIC's pm8941_resin. The key devices
//! are never grabbed (gpio-keys also carries the assist key, and grabbing/releasing it re-announces key state to
//! sway, which retriggered the assist binding in a loop): volume keys are read and, while the buttons are lent,
//! forwarded; sway leaves volume keys unbound, so the host does nothing with them.
//!
//! crosvm connects to <vm dir>/touch.sock (`--input multi-touch[path=...]`) and <vm dir>/keys.sock
//! (`--input keyboard[path=...]`); events go out as virtio_input_event (le16 type, le16 code, le32 value). The
//! power key (pm8941_pwrkey, a device of its own) belongs to the display: while the VM has the display it is grabbed
//! and forwarded, so Android sleeps/wakes itself (its screen state then switches the panel); lending the display
//! also sends KEY_WAKEUP so the VM is awake when it is shown.
//!
//! Nothing reaches the VM until mothershipd lends a device, via datagrams on <vm dir>/input.ctl:
//! - `display on|off`: touchscreen grabbed for / released from the VM
//! - `buttons on|off`: volume keys forwarded to / no longer forwarded to the VM
//! - `pause|resume`: screen off: drop touches (no pocket input) without changing ownership
//!
//! A device that goes away (the power key's driver is rebound by mship-pwrkey-rearm; any driver reprobe) is found
//! again by its name (the new event node) and gets its grab back; until then it is left out (a dead fd would wake
//! the loop for ever).
//!
//! Holding both volume buttons for MSHIP_HANDOVER_HOLD seconds (1) while the VM has the display takes the display
//! and buttons back to mothershipOS (a lent Wi-Fi stays with the VM), whether or not the buttons are lent.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use mship_core::api::Op;

/// _IOW('E', 0x90, int)
const EVIOCGRAB: libc::c_ulong = 0x40044590;
/// struct input_event (64-bit): timeval (16), type u16, code u16, value i32
const EV_SIZE: usize = 24;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const VOLDOWN: u16 = 114;
const VOLUP: u16 = 115;
const KEY_WAKEUP: u16 = 143;

type Ev = (u16, u16, i32);

fn log(msg: &str) {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    println!("{:02}:{:02}:{:02} {msg}", t / 3600 % 24, t / 60 % 60, t % 60);
}

/// The device name of an event node (sysfs), e.g. "pm8941_pwrkey".
fn dev_name(path: &str) -> Option<String> {
    let node = Path::new(path).file_name()?.to_string_lossy().into_owned();
    Some(std::fs::read_to_string(format!("/sys/class/input/{node}/device/name")).ok()?.trim().to_string())
}

/// The event node of the device called `name`, now.
fn find(name: &str) -> Option<String> {
    let mut found: Vec<String> = std::fs::read_dir("/sys/class/input")
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let dev = std::fs::read_to_string(e.path().join("device/name")).ok()?;
            (n.starts_with("event") && dev.trim() == name).then(|| format!("/dev/input/{n}"))
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

fn open_node(path: &str) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path)
}

/// An evdev node, grabbed only while it is lent to the VM.
struct Dev {
    /// None while the device is gone
    f: Option<std::fs::File>,
    /// its device name: how it is found again
    name: String,
    grabbed: bool,
}

impl Dev {
    fn open(path: &str) -> std::io::Result<Self> {
        let f = open_node(path)?;
        Ok(Self { f: Some(f), name: dev_name(path).unwrap_or_default(), grabbed: false })
    }

    fn alive(&self) -> bool {
        self.f.is_some()
    }

    /// The device went away (its driver was rebound): drop it until it is back.
    fn lost(&mut self) {
        if self.f.take().is_some() {
            log(&format!("{} went away; waiting for it", self.name));
        }
        self.grabbed = false;
    }

    /// Find it again by name; true once it is open.
    fn reopen(&mut self) -> bool {
        if self.f.is_none()
            && !self.name.is_empty()
            && let Some(path) = find(&self.name)
            && let Ok(f) = open_node(&path)
        {
            log(&format!("{} is back as {path}", self.name));
            self.f = Some(f);
        }
        self.f.is_some()
    }

    /// false while another process (a previous VM's bridge) still holds the device, or while it is gone
    fn grab(&mut self, on: bool) -> bool {
        let Some(f) = self.f.as_ref() else { return !on };
        if on != self.grabbed {
            if unsafe { libc::ioctl(f.as_raw_fd(), EVIOCGRAB, on as libc::c_int) } != 0 {
                log(&format!("grab {on} failed: {}; retrying", std::io::Error::last_os_error()));
                return false;
            }
            self.grabbed = on;
        }
        true
    }

    fn read(&mut self) -> Vec<Ev> {
        let Some(f) = self.f.as_ref() else { return vec![] };
        let mut b = [0u8; EV_SIZE * 64];
        let n = unsafe { libc::read(f.as_raw_fd(), b.as_mut_ptr().cast(), b.len()) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENODEV) {
            self.lost();
        }
        if n <= 0 {
            return vec![];
        }
        b[..n as usize]
            .as_chunks::<EV_SIZE>().0.iter()
            .map(|e| (u16::from_ne_bytes([e[16], e[17]]), u16::from_ne_bytes([e[18], e[19]]), i32::from_ne_bytes([e[20], e[21], e[22], e[23]])))
            .collect()
    }

    /// -1 while gone (poll skips it)
    fn fd(&self) -> RawFd {
        self.f.as_ref().map_or(-1, |f| f.as_raw_fd())
    }
}

fn listener(d: &Path, name: &str) -> std::io::Result<UnixListener> {
    let p = d.join(name);
    let _ = std::fs::remove_file(&p);
    UnixListener::bind(p)
}

fn encode(events: &[Ev]) -> Vec<u8> {
    events.iter().flat_map(|(t, c, v)| [t.to_le_bytes().as_slice(), &c.to_le_bytes(), &v.to_le_bytes()].concat()).collect()
}

#[derive(Debug, Default)]
struct State {
    display: bool,
    buttons: bool,
    paused: bool,
    retry: bool,
}

struct Bridge {
    touch: Dev,
    keys: Vec<Dev>,
    power: Option<Dev>,
    /// crosvm's connections: touch, keys
    conn: [Option<UnixStream>; 2],
    st: State,
    /// volume key -> pressed at
    held: BTreeMap<u16, Instant>,
    /// the display was lent at the last apply: lending it (not a re-grab) wakes the VM
    shown: bool,
}

const TOUCH: usize = 0;
const KEYS: usize = 1;
const KIND: [&str; 2] = ["touch", "keys"];

impl Bridge {
    fn send(&mut self, kind: usize, events: &[Ev]) {
        let Some(c) = self.conn[kind].as_mut() else { return };
        if events.is_empty() {
            return;
        }
        if c.write_all(&encode(events)).is_err() {
            log(&format!("crosvm {} connection lost", KIND[kind]));
            self.conn[kind] = None;
        }
    }

    fn apply(&mut self) {
        let mut back = self.touch.reopen();
        for k in &mut self.keys {
            back = k.reopen() && back;
        }
        if let Some(p) = self.power.as_mut() {
            back = p.reopen() && back;
        }
        let lent = self.st.display && !self.st.paused;
        let mut ok = self.touch.grab(lent) && back;
        let woke = lent && !self.shown && self.power.is_some();
        self.shown = self.st.display;
        if let Some(p) = self.power.as_mut() {
            ok = p.grab(self.st.display) && ok; // also while paused: the VM wakes its own screen
        }
        if woke {
            self.send(KEYS, &[(EV_KEY, KEY_WAKEUP, 1), (EV_SYN, 0, 0), (EV_KEY, KEY_WAKEUP, 0), (EV_SYN, 0, 0)]);
        }
        self.st.retry = !ok;
        if !self.st.buttons && !self.held.is_empty() {
            // release keys the VM still thinks are down
            let mut up: Vec<Ev> = self.held.keys().map(|k| (EV_KEY, *k, 0)).collect();
            up.push((EV_SYN, 0, 0));
            self.send(KEYS, &up);
        }
    }

    fn control(&mut self, msg: &str) {
        let w: Vec<&str> = msg.split_whitespace().collect();
        match w.as_slice() {
            ["display", v] => self.st.display = *v == "on",
            ["buttons", v] => self.st.buttons = *v == "on",
            ["pause"] => self.st.paused = true,
            ["resume"] => self.st.paused = false,
            _ => {}
        }
        self.apply();
        log(&format!("state {:?}", self.st));
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: mship-inputd <vm dir> <touch evdev> <key evdev>[,...] [<power key evdev>]");
        std::process::exit(2);
    }
    let d = Path::new(&a[1]).to_path_buf();
    let hold = Duration::from_secs_f64(std::env::var("MSHIP_HANDOVER_HOLD").ok().and_then(|h| h.parse().ok()).unwrap_or(1.0));
    let open = |p: &str| Dev::open(p).unwrap_or_else(|e| {
        eprintln!("mship-inputd: {p}: {e}");
        std::process::exit(1);
    });
    let socks = [listener(&d, "touch.sock").unwrap(), listener(&d, "keys.sock").unwrap()];
    let _ = std::fs::remove_file(d.join("input.ctl"));
    let ctl = UnixDatagram::bind(d.join("input.ctl")).unwrap();
    let mut b = Bridge {
        touch: open(&a[2]),
        keys: a[3].split(',').filter(|k| !k.is_empty()).map(open).collect(),
        power: a.get(4).filter(|p| !p.is_empty()).map(|p| open(p)),
        conn: [None, None],
        st: State::default(),
        held: BTreeMap::new(),
        shown: false,
    };
    let mut combo_fired = false;
    log(&format!("ready: touch {}, keys {}, power {}, hold {}s", a[2], a[3], a.get(4).map_or("-", |s| s), hold.as_secs_f64()));
    loop {
        let mut fds = vec![ctl.as_raw_fd(), b.touch.fd()];
        fds.extend(b.keys.iter().map(Dev::fd));
        fds.extend(b.power.iter().map(Dev::fd));
        let listening: Vec<usize> = (0..2).filter(|k| b.conn[*k].is_none()).collect();
        fds.extend(listening.iter().map(|k| socks[*k].as_raw_fd()));
        let mut pfds: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        let timeout = if b.held.len() == 2 && !combo_fired { 100 } else if b.st.retry { 500 } else { -1 };
        unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout) };
        let ready = |fd: RawFd| fd >= 0 && pfds.iter().any(|p| p.fd == fd && p.revents != 0);
        // a device that went away: POLLERR/POLLHUP/POLLNVAL for ever, until it is dropped
        let gone = |fd: RawFd| fd >= 0 && pfds.iter().any(|p| p.fd == fd && p.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0);
        if gone(b.touch.fd()) {
            b.touch.lost();
        }
        for k in &mut b.keys {
            if gone(k.fd()) {
                k.lost();
            }
        }
        if let Some(p) = b.power.as_mut().filter(|p| gone(p.fd())) {
            p.lost();
        }
        if !b.touch.alive() || b.keys.iter().any(|k| !k.alive()) || b.power.as_ref().is_some_and(|p| !p.alive()) {
            b.st.retry = true;
        }
        if b.st.retry {
            b.apply();
        }
        for k in listening {
            if ready(socks[k].as_raw_fd())
                && let Ok((c, _)) = socks[k].accept()
            {
                b.conn[k] = Some(c);
                log(&format!("crosvm connected ({})", KIND[k]));
            }
        }
        if ready(ctl.as_raw_fd()) {
            let mut m = [0u8; 256];
            if let Ok(n) = ctl.recv(&mut m) {
                b.control(&String::from_utf8_lossy(&m[..n]));
            }
        }
        if ready(b.touch.fd()) {
            let ev = b.touch.read();
            if b.touch.grabbed {
                let ev: Vec<Ev> = ev.into_iter().filter(|e| matches!(e.0, EV_SYN | EV_KEY | EV_ABS)).collect();
                b.send(TOUCH, &ev);
            }
        }
        if let Some(p) = b.power.as_mut().filter(|p| ready(p.fd())) {
            let ev: Vec<Ev> = p.read().into_iter().filter(|e| matches!(e.0, EV_SYN | EV_KEY)).collect();
            if p.grabbed {
                b.send(KEYS, &ev);
            }
        }
        for i in 0..b.keys.len() {
            if !ready(b.keys[i].fd()) {
                continue;
            }
            let mut out = vec![];
            for (t, code, val) in b.keys[i].read() {
                if t == EV_KEY && (code == VOLUP || code == VOLDOWN) {
                    if val == 1 {
                        b.held.insert(code, Instant::now());
                    } else if val == 0 {
                        b.held.remove(&code);
                        if b.held.is_empty() {
                            combo_fired = false;
                        }
                    }
                    if !combo_fired {
                        out.push((t, code, val));
                    }
                }
            }
            if b.st.buttons && !out.is_empty() {
                out.push((EV_SYN, 0, 0));
                b.send(KEYS, &out);
            }
        }
        // both volume buttons held: the screen back to mothershipOS
        if b.held.len() == 2 && !combo_fired && b.st.display && b.held.values().max().is_some_and(|t| t.elapsed() >= hold) {
            combo_fired = true;
            if b.st.buttons {
                b.send(KEYS, &[(EV_KEY, VOLUP, 0), (EV_KEY, VOLDOWN, 0), (EV_SYN, 0, 0)]);
            }
            log("volume buttons held: returning hardware to mothershipOS");
            // its own thread: mothershipd answers after telling this bridge (input.ctl)
            std::thread::spawn(|| {
                if let Err(e) = mship_core::client::call(Op::HandoverTake { items: vec!["display".into(), "buttons".into()] }) {
                    log(&format!("mothershipd: {e}"));
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtio_input_event() {
        assert_eq!(encode(&[(EV_KEY, VOLUP, 1)]), [1, 0, 115, 0, 1, 0, 0, 0]);
    }
}
