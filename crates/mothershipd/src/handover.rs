//! Hardware handover: lend hardware to VMs, take it back.
//!
//! Every piece of hardware has one assigned owner (mothershipOS or a VM) in /var/lib/mothership/config/hardware.json
//! (Settings › Hardware). Assigned hardware is lent automatically: screen hardware (display, buttons) while the VM
//! is shown, network hardware (wifi-2g, wifi-5g, internet), bluetooth, usb, cellular, cameras and display-ext while it
//! runs.
//!
//! display = panel + touchscreen. VMs with "display_mode": "lease" (crosvm + mship-vm-display) get the panel
//! itself: sway leases the DSI connector to the VM's presenter (`swaymsg lease_output DSI-1 on`) and gets it back
//! on take. Other VMs: their window fullscreen at scale 1. Either way the host idle timeout is off while a VM has
//! the display, and touch goes to the VM (its input bridge, `<vm dir>/input.ctl`). buttons = volume keys.
//! wifi-2g / wifi-5g = one Wi-Fi band (mship.wifi, mship-wifi-bridge). internet = shared internet (the phone's
//! uplink NATed to the VM). bluetooth (mship.bt, mship-bt-bridge), usb (mship.usb, mship-usb), cellular (mship.modem,
//! mship-modem: the modem with its SIM, calls, SMS and mobile data), cameras (mship.camera, mship-camera:
//! every camera). display-ext = the
//! USB-C external display (DP-1), leased to the VM's presenter; sway itself never uses it.

use std::collections::BTreeMap;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mship_core::api::{HandoverState, ITEMS, SCREEN_ITEMS};
use mship_host::util::{Result, fail, load_value};
use mship_host::{vm, wifi};

use crate::session;

const OUT: &str = "DSI-1";
const SESSION_SCALE: &str = "2";
/// display-ext: leased to a VM, never sway's
const EXT_OUT: &str = "DP-1";
/// closed when their VM stops
const NETWORK: &[&str] = &["internet", "display-ext", "bluetooth", "usb", "wifi-2g", "wifi-5g", "cellular", "cameras"];
const BANDS: &[(&str, &str)] = &[("wifi-2g", "2g"), ("wifi-5g", "5g")];
const ASSOC: &str = "/var/lib/mothership/config/hardware.json";
const WIFI_BRIDGE: &str = "/run/mothership/wifi-bridge.sock";
const BRIDGES: &[(&str, &str)] =
    &[("bluetooth", "/run/mothership/bt-bridge.sock"), ("usb", "/run/mothership/usb.sock"), ("cellular", "/run/mothership/modem.sock"),
      ("cameras", "/run/mothership/camera.sock")];
/// a repeated toggle (key bounce, re-announced key state) is ignored
const DEBOUNCE: Duration = Duration::from_millis(1500);
/// private: who holds what, for mothershipd's own restart only
const RECOVER: &str = "/run/mothership/handover.recover";

fn conf(d: &Path) -> serde_json::Value {
    load_value(d.join("vm.json")).unwrap_or_default()
}

fn cid(d: &str) -> u64 {
    conf(Path::new(d))["cid"].as_u64().unwrap_or(0)
}

fn leased(d: &str) -> bool {
    conf(Path::new(d))["display_mode"].as_str() == Some("lease")
}

fn base(d: &str) -> String {
    Path::new(d).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// A device service's control socket (datagram, one reply): true on "ok".
pub fn bridge(cmd: &str, path: &str) -> bool {
    match mship_core::ctl::request(path, cmd, Duration::from_secs(5)) {
        Ok(r) => r.starts_with("ok") || {
            println!("{path}: {cmd}: {r}");
            false
        },
        Err(e) => {
            println!("{path}: {cmd}: {e}");
            false
        }
    }
}

/// The VM's input bridge (mship-vm-inputd): `<vm dir>/input.ctl`.
fn ctl(d: &str, msg: &str) -> bool {
    let path = Path::new(d).join("input.ctl");
    path.exists() && UnixDatagram::unbound().is_ok_and(|s| s.send_to(msg.as_bytes(), &path).is_ok())
}

/// The host's idle timeout (swayidle, in the session): off while a VM has the display.
pub fn idle(on: bool) {
    session::pkill("swayidle");
    if on {
        let _ = session::spawn(&["/usr/local/bin/mship", "idle"], &[], None);
    }
}

struct Inner {
    /// who holds each item now: "host" | VM dir
    st: BTreeMap<String, String>,
    /// a leased VM's display, taken back for the power key's screen off
    resume: Option<String>,
    changed_at: Option<Instant>,
}

pub struct Handover {
    inner: Mutex<Inner>,
    publish: Box<dyn Fn(HandoverState) + Send + Sync>,
}

impl Handover {
    pub fn new(publish: impl Fn(HandoverState) + Send + Sync + 'static) -> Self {
        let mut st: BTreeMap<String, String> = ITEMS.iter().map(|i| (i.to_string(), "host".to_string())).collect();
        if let Some(old) = load_value(RECOVER).and_then(|v| serde_json::from_value::<BTreeMap<String, String>>(v).ok()) {
            for (i, d) in old {
                if st.contains_key(&i) && (d == "host" || vm::running(Path::new(&d))) {
                    st.insert(i, d);
                }
            }
        }
        Self { inner: Mutex::new(Inner { st, resume: None, changed_at: None }), publish: Box::new(publish) }
    }

    // ------------------------------------------------ the assignment map (a setting: kept in a file)
    /// {item: "host" | VM name}
    pub fn assoc(&self) -> BTreeMap<String, String> {
        let a: BTreeMap<String, String> =
            load_value(ASSOC).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        ITEMS.iter().map(|i| (i.to_string(), a.get(*i).cloned().unwrap_or_else(|| "host".into()))).collect()
    }

    fn assoc_save(&self, a: &BTreeMap<String, String>) -> Result<()> {
        let tmp = format!("{ASSOC}.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(a).unwrap())?;
        let (uid, gid) = session::user();
        std::os::unix::fs::chown(&tmp, Some(uid), Some(gid))?;
        std::fs::rename(&tmp, ASSOC)?;
        Ok(())
    }

    pub fn holders(&self) -> BTreeMap<String, String> {
        self.inner.lock().unwrap().st.clone()
    }

    pub fn state(&self) -> HandoverState {
        HandoverState { holders: self.holders(), assoc: self.assoc() }
    }

    fn done(&self, i: &mut Inner) -> BTreeMap<String, String> {
        i.changed_at = Some(Instant::now());
        // survives a mothershipd restart while VMs hold hardware
        let tmp = format!("{RECOVER}.tmp");
        if std::fs::write(&tmp, serde_json::to_string(&i.st).unwrap()).is_ok() {
            let _ = mship_host::util::set_mode(&tmp, 0o600);
            let _ = std::fs::rename(&tmp, RECOVER);
        }
        (self.publish)(HandoverState { holders: i.st.clone(), assoc: self.assoc() });
        i.st.clone()
    }

    // ------------------------------------------------ screen
    fn show_vm(d: &str) {
        crate::screen::panel(true);
        if leased(d) {
            session::sway(&["lease_output", OUT, "on"]); // the VM's presenter takes the panel itself
        } else {
            session::sway(&["workspace", "number", "2"]);
            session::sway(&["output", OUT, "scale", "1"]);
            session::sway(&["[workspace=\"^2$\"]", "fullscreen", "enable"]);
        }
        idle(false); // the VM is in charge of its screen; the host would blank it under a grabbed touchscreen
    }

    fn show_host(prev: Option<&str>) {
        if prev.is_some_and(leased) {
            session::sway(&["lease_output", OUT, "off"]); // revoke: the panel comes back to sway as a new output
            for _ in 0..30 {
                if session::outputs().is_some_and(|o| o.iter().any(|n| n == OUT)) {
                    break; // sway has it again: configure it
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        session::sway(&["output", OUT, "scale", SESSION_SCALE]);
        session::sway(&["workspace", "number", "1"]);
        idle(true);
    }

    // ------------------------------------------------ transitions
    fn take_locked(&self, i: &mut Inner, items: &[&str]) -> BTreeMap<String, String> {
        let prev = i.st["display"].clone();
        for item in items {
            let holder = i.st[*item].clone();
            if holder == "host" {
                continue;
            }
            match *item {
                "internet" => {
                    let _ = wifi::owner_set("wifi", "host");
                }
                b if BANDS.iter().any(|(n, _)| *n == b) => {
                    bridge(&format!("detach {} user", cid(&holder)), WIFI_BRIDGE);
                }
                b if BRIDGES.iter().any(|(n, _)| *n == b) => {
                    let path = BRIDGES.iter().find(|(n, _)| *n == b).unwrap().1;
                    bridge(&format!("detach {} user", base(&holder)), path);
                }
                "display-ext" => {
                    session::sway(&["lease_output", EXT_OUT, "off"]); // revokes only this output's lease
                }
                _ => {}
            }
            ctl(&holder, &format!("{item} off"));
            if *item == "display" {
                ctl(&holder, "resume");
            }
            i.st.insert(item.to_string(), "host".into());
        }
        if items.contains(&"display") {
            Self::show_host((prev != "host").then_some(prev.as_str()));
        }
        println!("handover: took {}", items.join(", "));
        self.done(i)
    }

    pub fn take(&self, items: &[&str]) -> BTreeMap<String, String> {
        let mut i = self.inner.lock().unwrap();
        self.take_locked(&mut i, items)
    }

    fn give_locked(&self, i: &mut Inner, d: &Path, items: Option<Vec<String>>) -> BTreeMap<String, String> {
        let d = d.canonicalize().unwrap_or_else(|_| d.to_path_buf()).to_string_lossy().into_owned();
        let name = base(&d);
        let items: Vec<String> = items.unwrap_or_else(|| {
            self.assoc().into_iter().filter(|(_, owner)| *owner == name).map(|(i, _)| i).collect()
        });
        for item in &items {
            let holder = i.st[item].clone();
            let item = item.as_str();
            if item == "internet" {
                if holder != d && wifi::owner_set("wifi", &format!("vm:{d}")).is_ok() {
                    i.st.insert(item.into(), d.clone()); // replaces any other VM's NAT
                }
                continue;
            }
            if let Some((_, band)) = BANDS.iter().find(|(n, _)| *n == item) {
                if holder != "host" && holder != d {
                    bridge(&format!("detach {} revoked", cid(&holder)), WIFI_BRIDGE);
                }
                let c = cid(&d);
                if c != 0 && bridge(&format!("attach {c} {band}"), WIFI_BRIDGE) {
                    i.st.insert(item.into(), d.clone()); // kept until the VM's driver connects
                }
                continue;
            }
            if item == "display-ext" {
                if holder != "host" && holder != d {
                    session::sway(&["lease_output", EXT_OUT, "off"]);
                }
                session::sway(&["lease_output", EXT_OUT, "on"]); // the VM's presenter (DP-1) takes it
                i.st.insert(item.into(), d.clone());
                continue;
            }
            if let Some((_, path)) = BRIDGES.iter().find(|(n, _)| *n == item) {
                if holder != "host" && holder != d {
                    bridge(&format!("detach {} revoked", base(&holder)), path);
                }
                if bridge(&format!("attach {name}"), path) {
                    i.st.insert(item.into(), d.clone()); // kept until the VM is up
                }
                continue;
            }
            if holder != "host" && holder != d {
                ctl(&holder, &format!("{item} off"));
            }
            if ctl(&d, &format!("{item} on")) || item == "display" {
                i.st.insert(item.into(), d.clone());
            }
        }
        if items.iter().any(|i| i == "display") {
            Self::show_vm(&d);
        }
        println!("handover: gave {} to {name}", if items.is_empty() { "nothing".into() } else { items.join(", ") });
        self.done(i)
    }

    pub fn give(&self, d: &Path, items: Option<Vec<String>>) -> BTreeMap<String, String> {
        let mut i = self.inner.lock().unwrap();
        self.give_locked(&mut i, d, items)
    }

    /// New owner for one piece of hardware; lent right away if that VM is running (and shown, for screen hardware).
    pub fn assign(&self, item: &str, owner: &str) -> Result<BTreeMap<String, String>> {
        if !ITEMS.contains(&item) {
            return fail(format!("item must be one of {}", ITEMS.join(", ")));
        }
        let mut i = self.inner.lock().unwrap();
        let mut a = self.assoc();
        a.insert(item.into(), owner.into());
        self.assoc_save(&a)?;
        if i.st[item] != "host" && base(&i.st[item]) != owner {
            self.take_locked(&mut i, &[item]);
        }
        let d = Path::new(vm::VMS).join(owner);
        let shown = i.st["display"] == d.canonicalize().unwrap_or(d.clone()).to_string_lossy();
        if owner != "host" && vm::running(&d) && (!SCREEN_ITEMS.contains(&item) || shown) {
            self.give_locked(&mut i, &d, Some(vec![item.into()]));
        }
        Ok(self.done(&mut i))
    }

    /// A VM stops: back to mothershipOS, only what it holds (other VMs keep theirs).
    pub fn release(&self, d: &Path) -> BTreeMap<String, String> {
        let d = d.canonicalize().unwrap_or_else(|_| d.to_path_buf()).to_string_lossy().into_owned();
        let mut i = self.inner.lock().unwrap();
        let held: Vec<String> = i.st.iter().filter(|(_, o)| **o == d).map(|(k, _)| k.clone()).collect();
        if held.is_empty() {
            return i.st.clone();
        }
        let held: Vec<&str> = held.iter().map(String::as_str).collect();
        self.take_locked(&mut i, &held)
    }

    /// The assist key: take the screen back, or lend it to the running VM.
    pub fn toggle(&self) -> BTreeMap<String, String> {
        let mut i = self.inner.lock().unwrap();
        if i.changed_at.is_some_and(|t| t.elapsed() < DEBOUNCE) {
            return i.st.clone(); // key bounce, re-announced key state
        }
        if i.st["display"] != "host" {
            return self.take_locked(&mut i, SCREEN_ITEMS);
        }
        match vm::running_dirs().first() {
            Some(d) => self.give_locked(&mut i, d, None),
            None => self.take_locked(&mut i, ITEMS),
        }
    }

    /// The power key: pause/resume the VM's touch while the panel is off.
    pub fn screen(&self, on: bool) -> BTreeMap<String, String> {
        let mut i = self.inner.lock().unwrap();
        let shown = i.st["display"].clone();
        if !on && shown != "host" && leased(&shown) {
            // sway can only switch the panel off while it has it: take it back, re-lend on screen on
            i.resume = Some(shown);
            return self.take_locked(&mut i, &["display"]);
        }
        if !on && shown != "host" {
            ctl(&shown, "pause");
        } else if on && i.resume.is_some() {
            let d = PathBuf::from(i.resume.take().unwrap());
            if vm::running(&d) {
                return self.give_locked(&mut i, &d, Some(vec!["display".into()]));
            }
        } else if on && shown != "host" {
            ctl(&shown, "resume");
        }
        i.st.clone()
    }

    /// VMs that stopped without giving their hardware back: close network hardware now, forget the rest.
    pub fn tick(&self) {
        let mut i = self.inner.lock().unwrap();
        let gone: Vec<&str> = NETWORK
            .iter()
            .copied()
            .filter(|item| i.st[*item] != "host" && !vm::running(Path::new(&i.st[*item])))
            .collect();
        if !gone.is_empty() {
            self.take_locked(&mut i, &gone);
        }
        let stale: Vec<String> = i
            .st
            .iter()
            .filter(|(_, d)| *d != "host" && !vm::running(Path::new(d)))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &stale {
            i.st.insert(k.clone(), "host".into());
        }
        if !stale.is_empty() {
            self.done(&mut i);
        }
    }
}
