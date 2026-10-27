//! VM lifecycle: the VMs on the phone, starting them (their launcher, as the session user in the session's
//! environment), stopping them (cleanly: woken first if asleep, then the guest's own power-off) and showing them.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mship_core::api::{SCREEN_ITEMS, ShowResult, VmInfo, VmList};
use mship_host::util::{Result, fail, load_value};
use mship_host::vm;

use crate::handover::Handover;
use crate::pool::Pool;
use crate::power::Power;
use crate::session;

/// runs a VM from its vm.json (vm.json "launcher" overrides it, for development)
const VM_LAUNCHER: &str = "/usr/local/bin/mship-launch";
const VM_NAMES: &[(&str, &str)] = &[("ubuntu-touch-2004", "Ubuntu Touch 20.04"), ("ubuntu-touch-2404", "Ubuntu Touch 24.04"),
                                    ("ubuntu-touch", "Ubuntu Touch 26.04 (dev)")];
/// "starting" is shown this long (or until the VM runs)
const STARTING: Duration = Duration::from_secs(20);

pub struct Vms {
    busy: Mutex<BTreeMap<String, String>>,
    state: Mutex<VmList>,
    power: Arc<Power>,
    handover: Arc<Handover>,
    pool: Arc<Pool>,
    publish: Box<dyn Fn(&VmList) + Send + Sync>,
}

impl Vms {
    pub fn new(power: Arc<Power>, handover: Arc<Handover>, pool: Arc<Pool>,
               publish: impl Fn(&VmList) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { busy: Mutex::default(), state: Mutex::default(), power, handover, pool, publish: Box::new(publish) })
    }

    fn scan(&self) -> VmList {
        let busy = self.busy.lock().unwrap().clone();
        let vms = vm::names()
            .into_iter()
            .map(|name| {
                let d = Path::new(vm::VMS).join(&name);
                let conf = load_value(d.join("vm.json"));
                let c = conf.clone().unwrap_or_default();
                let fallback = VM_NAMES.iter().find(|(n, _)| *n == name).map(|(_, t)| t.to_string()).unwrap_or_else(|| name.clone());
                VmInfo {
                    name: c["name"].as_str().map(String::from).unwrap_or(fallback),
                    manifest: conf.is_some(),
                    running: vm::running(&d),
                    busy: busy.get(&name).cloned(),
                    display_mode: c["display_mode"].as_str().map(String::from),
                    output: c["output"].as_str().unwrap_or("DSI-1").into(),
                    workspace: c["workspace"].as_u64().unwrap_or(2) as u32,
                    memory_mb: c["resources"]["memory_mb"].as_u64(),
                    vm: name,
                }
            })
            .collect();
        VmList { vms }
    }

    pub fn list(&self) -> VmList {
        self.state.lock().unwrap().clone()
    }

    /// Publish when a VM started, stopped or changed.
    pub fn tick(&self) {
        let st = self.scan();
        let mut cur = self.state.lock().unwrap();
        if *cur != st {
            *cur = st.clone();
            drop(cur);
            (self.publish)(&st);
        }
    }

    fn set_busy(self: &Arc<Self>, vm: &str, what: Option<&str>) {
        let mut b = self.busy.lock().unwrap();
        match what {
            Some(w) => b.insert(vm.into(), w.into()),
            None => b.remove(vm),
        };
        drop(b);
        self.tick();
    }

    pub fn start(self: &Arc<Self>, name: &str) -> Result<()> {
        let d = vm::vm_dir(name)?;
        if vm::running(&d) {
            return fail(format!("{name} is already running"));
        }
        let c = load_value(d.join("vm.json")).unwrap_or_default();
        let launcher = c["launcher"].as_str().unwrap_or(VM_LAUNCHER).to_string();
        let log = d.join(c["log"].as_str().unwrap_or("qemu.log"));
        if std::fs::metadata(&log).is_ok_and(|m| m.len() > 0) {
            // keep the previous run's log
            let s = log.to_string_lossy();
            let prev = s.strip_suffix(".log").map(|b| format!("{b}.prev.log")).unwrap_or_else(|| format!("{s}.prev"));
            let _ = std::fs::rename(&log, prev);
        }
        let dir = d.to_string_lossy().into_owned();
        // in its own cgroup: the pool weighs its CPU share
        session::spawn_in(&[&launcher], &[("MSHIP_VM_DIR", &dir)], Some(&log), self.pool.cgroup(name).as_deref())?;
        println!("{name}: starting ({launcher})");
        self.set_busy(name, Some("starting"));
        let me = self.clone();
        let name = name.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(STARTING);
            me.set_busy(&name, None);
        });
        Ok(())
    }

    pub fn stop(self: &Arc<Self>, name: &str) -> Result<()> {
        let d = vm::vm_dir(name)?;
        if !vm::running(&d) {
            return fail(format!("{name} is not running"));
        }
        self.set_busy(name, Some("stopping"));
        let me = self.clone();
        let name = name.to_string();
        std::thread::spawn(move || {
            me.power.thaw(&name, "stop"); // a sleeping VM cannot shut down (no-op if awake)
            match vm::stop(&d) {
                Ok(s) => println!("{name}: stopped{}", if s.killed { " (killed)" } else { "" }),
                Err(e) => println!("{name}: stop failed: {e}"),
            }
            me.handover.release(&d);
            me.set_busy(&name, None);
        });
        Ok(())
    }

    /// Stop every running VM and wait for them (power off / reboot).
    pub fn stop_all(&self) {
        for d in vm::running_dirs() {
            let name = d.file_name().unwrap().to_string_lossy().into_owned();
            self.power.thaw(&name, "power off");
            let _ = vm::stop(&d);
        }
    }

    /// Bring the VM to the front (a message when there is nothing to switch).
    pub fn show(&self, name: &str) -> Result<ShowResult> {
        let d = vm::vm_dir(name)?;
        let conf = load_value(d.join("vm.json"));
        let c = conf.clone().unwrap_or_default();
        if conf.is_some() && c["output"].as_str().unwrap_or("DSI-1") != "DSI-1" {
            // its screen is another display (External display in Settings › Hardware): the panel stays as it is
            return Ok(ShowResult { message: Some(format!("{} is on the external display", c["name"].as_str().unwrap_or(name))) });
        }
        if c["display_mode"].as_str() == Some("window") {
            // a window in the host's compositor, on its workspace (sway config: 3 = the external display if any)
            let ws = c["workspace"].as_u64().unwrap_or(2).to_string();
            if ws == "3" && session::outputs().is_some_and(|o| o.iter().any(|n| n == "DP-1")) {
                session::sway(&["workspace", "number", &ws]); // the panel stays with whoever has it
                return Ok(ShowResult::default());
            }
            self.handover.take(SCREEN_ITEMS); // on the panel: a leased VM gives it back
            session::sway(&["workspace", "number", &ws]);
            session::sway(&["output", "DSI-1", "scale", "1"]);
        } else if conf.is_some() {
            self.handover.give(&d, None); // everything assigned to the VM (Settings › Hardware)
        } else {
            session::sway(&["workspace", "2"]);
            session::sway(&["output", "DSI-1", "scale", "1"]);
        }
        Ok(ShowResult::default())
    }
}
