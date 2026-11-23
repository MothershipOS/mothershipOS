//! Hardware handover: lend hardware to VMs, take it back.
//!
//! What there is comes from the handle registry (`handles`): every assignable group has one assigned owner
//! (mothershipOS or a VM) in /var/lib/mothership/config/hardware.json (a VM's page › Hardware), and is lent the way its
//! handles say: with the screen (`while_shown`: they move with the shown VM) or while the VM runs. A group whose
//! handles have a control socket is lent through it (`attach <vm> [arg]`, `detach <vm> <reason> [arg]`); handles
//! every VM gets (`every_vm`) need no handover.
//!
//! mothershipd lends the session's groups itself: display = panel + touchscreen + power key. VMs with
//! "display_mode": "lease" (crosvm + mship-vm-display) get the panel itself: sway leases the panel's connector to the
//! VM's presenter (`swaymsg lease_output <connector> on`) and gets it back on take. Other VMs: their window fullscreen
//! at scale one. Either way the host idle timeout is off while a VM has the display, and touch goes to the VM (its
//! input bridge, `<vm dir>/input.ctl`). The buttons group is the volume keys, internet the shared internet
//! (mship-net), display-ext the USB-C external display, leased to the VM's presenter (sway itself never uses it).

use std::collections::BTreeMap;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mship_core::api::HandoverState;
use mship_core::handle::Lend;
use mship_host::util::{Result, fail, load_value};
use mship_host::vm;

use crate::handles::Registry;
use crate::session;

/// the panel's connector and the session's scale on it (mship_core::device)
fn out() -> String {
    mship_core::device::panel_output()
}

fn session_scale() -> String {
    mship_core::device::panel().map_or(2.0, |p| p.scale).to_string()
}

/// display-ext: leased to a VM, never sway's
fn ext_out() -> String {
    mship_core::device::output("external")
}
const ASSOC: &str = "/var/lib/mothership/config/hardware.json";
/// a repeated toggle (key bounce, re-announced key state) is ignored
const DEBOUNCE: Duration = Duration::from_millis(1500);
/// private: who holds what, for mothershipd's own restart only
const RECOVER: &str = "/run/mothership/handover.recover";

fn conf(d: &Path) -> serde_json::Value {
    load_value(d.join("vm.json")).unwrap_or_default()
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
    reg: Arc<Registry>,
    publish: Box<dyn Fn(HandoverState) + Send + Sync>,
}

/// who holds a group now ("host" | VM dir)
fn holder(i: &Inner, item: &str) -> String {
    i.st.get(item).cloned().unwrap_or_else(|| "host".into())
}

impl Handover {
    pub fn new(reg: Arc<Registry>, publish: impl Fn(HandoverState) + Send + Sync + 'static) -> Self {
        let mut st: BTreeMap<String, String> = reg.groups().into_iter().map(|g| (g.name, "host".to_string())).collect();
        if let Some(old) = load_value(RECOVER).and_then(|v| serde_json::from_value::<BTreeMap<String, String>>(v).ok()) {
            for (i, d) in old {
                if d == "host" || vm::running(Path::new(&d)) {
                    st.insert(i, d);
                }
            }
        }
        Self { inner: Mutex::new(Inner { st, resume: None, changed_at: None }), reg, publish: Box::new(publish) }
    }

    fn all(&self) -> Vec<String> {
        self.reg.groups().into_iter().map(|g| g.name).collect()
    }

    // ------------------------------------------------ the assignment map (a setting: kept in a file)
    /// {group: "host" | VM name}, for every group there is now (an assignment of a group that is gone is kept in
    /// the file for when it comes back)
    pub fn assoc(&self) -> BTreeMap<String, String> {
        let a: BTreeMap<String, String> =
            load_value(ASSOC).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        self.all().into_iter().map(|g| { let o = a.get(&g).cloned().unwrap_or_else(|| "host".into()); (g, o) }).collect()
    }

    /// The registry changed or device services (re)started: hardware is hot-plugged. Every group assigned to a
    /// running VM is lent now if it may be (screen groups only to the shown VM): one whose service just appeared, and
    /// again one whose service restarted (the new process doesn't know it was lent).
    pub fn handles_changed(&self, started: &[String]) {
        let mut i = self.inner.lock().unwrap();
        let all = self.all();
        for g in &all {
            i.st.entry(g.clone()).or_insert_with(|| "host".into());
        }
        // a group whose service went away is held by nobody (it comes back to its assigned VM with the service)
        for (g, h) in i.st.iter_mut() {
            if !all.contains(g) && *h != "host" {
                println!("handover: {g} unplugged from {} (its service is gone)", base(h));
                *h = "host".into();
            }
        }
        let shown = holder(&i, "display");
        let mut lend: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (g, owner) in self.assoc() {
            if owner == "host" {
                continue;
            }
            let d = Path::new(vm::VMS).join(&owner);
            let d = d.canonicalize().unwrap_or(d).to_string_lossy().into_owned();
            let Some(group) = self.reg.group(&g) else { continue };
            if !vm::running(Path::new(&d)) || (group.lend == Lend::WhileShown && shown != d) {
                continue;
            }
            let restarted = self.reg.services(&g).iter().any(|s| started.contains(s));
            if holder(&i, &g) != d || restarted {
                lend.entry(d).or_default().push(g);
            }
        }
        for (d, groups) in lend {
            println!("handover: hot-plugging {} into {}", groups.join(", "), base(&d));
            self.give_locked(&mut i, Path::new(&d), Some(groups));
        }
        self.done(&mut i);
    }

    fn assoc_save(&self, new: &BTreeMap<String, String>) -> Result<()> {
        let mut a: BTreeMap<String, String> = load_value(ASSOC).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        a.extend(new.clone());
        let tmp = format!("{ASSOC}.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&a).unwrap())?;
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
            session::sway(&["lease_output", &out(), "on"]); // the VM's presenter takes the panel itself
        } else {
            session::sway(&["workspace", "number", "2"]);
            session::sway(&["output", &out(), "scale", "1"]);
            session::sway(&["[workspace=\"^2$\"]", "fullscreen", "enable"]);
        }
        idle(false); // the VM is in charge of its screen; the host would blank it under a grabbed touchscreen
    }

    fn show_host(prev: Option<&str>) {
        if prev.is_some_and(leased) {
            session::sway(&["lease_output", &out(), "off"]); // revoke: the panel comes back to sway as a new output
            for _ in 0..30 {
                if session::outputs().is_some_and(|o| o.iter().any(|n| *n == out())) {
                    break; // sway has it again: configure it
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        session::sway(&["output", &out(), "scale", &session_scale()]);
        session::sway(&["workspace", "number", "1"]);
        idle(true);
    }

    // ------------------------------------------------ transitions
    fn take_locked(&self, i: &mut Inner, items: &[&str]) -> BTreeMap<String, String> {
        let prev = holder(i, "display");
        let items: Vec<&str> = items.iter().copied().filter(|it| holder(i, it) != "host").collect();
        for item in &items {
            let holder = holder(i, item);
            match *item {
                "display-ext" => {
                    session::sway(&["lease_output", &ext_out(), "off"]); // revokes only this output's lease
                }
                g => {
                    for (sock, arg) in self.reg.controls(g) {
                        bridge(format!("detach {} user {}", base(&holder), arg.unwrap_or_default()).trim_end(), &sock);
                    }
                }
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
        if !items.is_empty() {
            println!("handover: took {}", items.join(", "));
        }
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
            let holder = holder(i, item);
            let item = item.as_str();
            if self.reg.group(item).is_none() {
                continue; // its service isn't running (the assignment stays for when it is)
            }
            if item == "display-ext" {
                if holder != "host" && holder != d {
                    session::sway(&["lease_output", &ext_out(), "off"]);
                }
                session::sway(&["lease_output", &ext_out(), "on"]); // the VM's presenter takes it
                i.st.insert(item.into(), d.clone());
                continue;
            }
            let controls = self.reg.controls(item);
            if !controls.is_empty() {
                let mut lent = false;
                for (sock, arg) in controls {
                    let arg = arg.unwrap_or_default();
                    if holder != "host" && holder != d {
                        bridge(format!("detach {} revoked {arg}", base(&holder)).trim_end(), &sock);
                    }
                    lent |= bridge(format!("attach {name} {arg}").trim_end(), &sock);
                }
                if lent {
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
        let Some(group) = self.reg.group(item) else {
            return fail(format!("no hardware group {item:?} (there are: {})", self.all().join(", ")));
        };
        let mut i = self.inner.lock().unwrap();
        self.assoc_save(&BTreeMap::from([(item.to_string(), owner.to_string())]))?;
        if holder(&i, item) != "host" && base(&holder(&i, item)) != owner {
            self.take_locked(&mut i, &[item]);
        }
        let d = Path::new(vm::VMS).join(owner);
        let shown = holder(&i, "display") == d.canonicalize().unwrap_or(d.clone()).to_string_lossy();
        if owner != "host" && vm::running(&d) && (group.lend != Lend::WhileShown || shown) {
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

    /// Home (holding the assist key): take the screen back, or lend it to the running VM.
    pub fn toggle(&self) -> BTreeMap<String, String> {
        let mut i = self.inner.lock().unwrap();
        if i.changed_at.is_some_and(|t| t.elapsed() < DEBOUNCE) {
            return i.st.clone(); // key bounce, re-announced key state
        }
        if holder(&i, "display") != "host" {
            let screen = self.reg.screen_groups();
            return self.take_locked(&mut i, &screen.iter().map(String::as_str).collect::<Vec<_>>());
        }
        match vm::running_dirs().first() {
            Some(d) => self.give_locked(&mut i, d, None),
            None => {
                let all = self.all();
                self.take_locked(&mut i, &all.iter().map(String::as_str).collect::<Vec<_>>())
            }
        }
    }

    /// The screen's groups back to mothershipOS.
    pub fn take_screen(&self) -> BTreeMap<String, String> {
        let screen = self.reg.screen_groups();
        self.take(&screen.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// Everything (or the named groups) back to mothershipOS.
    pub fn take_named(&self, items: &[String]) -> BTreeMap<String, String> {
        let all = self.all();
        let items: Vec<&str> = all.iter().map(String::as_str).filter(|g| items.is_empty() || items.iter().any(|x| x == g)).collect();
        self.take(&items)
    }

    /// The power key: pause/resume the VM's touch while the panel is off.
    pub fn screen(&self, on: bool) -> BTreeMap<String, String> {
        let mut i = self.inner.lock().unwrap();
        let shown = holder(&i, "display");
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
        let running_groups: Vec<String> = self.reg.groups().into_iter().filter(|g| g.lend == Lend::WhileRunning).map(|g| g.name).collect();
        let gone: Vec<&str> = running_groups
            .iter()
            .map(String::as_str)
            .filter(|item| holder(&i, item) != "host" && !vm::running(Path::new(&holder(&i, item))))
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
