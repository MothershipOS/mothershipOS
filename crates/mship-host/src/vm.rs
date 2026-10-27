//! VMs on the phone: their directories, whether they run, and stopping them cleanly.

use std::path::{Path, PathBuf};

use crate::util::{Result, alive, fail, load_value, rd, run, sleep_s};

pub const VMS: &str = "/var/lib/mothership/vms";
const CROSVM: &str = "/opt/mship-gpu/bin/crosvm";

/// A VM directory under /var/lib/mothership/vms (by name or path).
pub fn vm_dir(vm: &str) -> Result<PathBuf> {
    let p = if vm.contains('/') { PathBuf::from(vm) } else { Path::new(VMS).join(vm) };
    let p = p.canonicalize().unwrap_or(p);
    if p.parent() != Some(Path::new(VMS)) || !p.is_dir() {
        return fail("not a VM directory");
    }
    Ok(p)
}

/// The names of the VMs on the phone (directories with a vm.json).
pub fn names() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(VMS)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("vm.json").exists())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    v.sort();
    v
}

/// The VMM's pid (vm.json VMs: the launcher's run.pid), if it runs.
pub fn pid(d: &Path) -> Option<i32> {
    rd(d.join("run.pid"), "").parse().ok().filter(|p| alive(*p))
}

pub fn running(d: &Path) -> bool {
    pid(d).is_some()
}

pub fn running_dirs() -> Vec<PathBuf> {
    names().into_iter().map(|n| Path::new(VMS).join(n)).filter(|d| running(d)).collect()
}

fn gone(pid: i32, seconds: u32) -> bool {
    for _ in 0..seconds {
        if !alive(pid) {
            return true;
        }
        sleep_s(1.0);
    }
    !alive(pid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stopped {
    pub stopped: bool,
    /// it had to be killed
    pub killed: bool,
}

const STOPPED: Stopped = Stopped { stopped: true, killed: false };
const KILLED: Stopped = Stopped { stopped: true, killed: true };

/// Stop a VM cleanly (the caller wakes it first if it sleeps): Linux guests with their power button (`crosvm
/// powerbtn`: the guest's power key; it shuts down), Android over adb; stopped if they do not go, killed last.
pub fn stop(d: &Path) -> Result<Stopped> {
    let conf = load_value(d.join("vm.json")).unwrap_or_default();
    let platform = conf["platform"]["id"].as_str().unwrap_or("cuttlefish-arm64");
    Ok(if platform.starts_with("cuttlefish") { stop_android(d, &conf) } else { stop_linux(d) })
}

fn crosvm(cmd: &str, d: &Path) {
    run("timeout", &["10", CROSVM, cmd, &d.join("crosvm.sock").to_string_lossy()]);
}

/// Press the power button, then stop the VMM, then kill it.
fn stop_linux(d: &Path) -> Stopped {
    let Some(pid) = pid(d) else { return STOPPED };
    crosvm("powerbtn", d);
    if gone(pid, 60) {
        return STOPPED;
    }
    crosvm("stop", d);
    if gone(pid, 10) {
        return STOPPED;
    }
    unsafe { libc::kill(pid, libc::SIGTERM) };
    KILLED
}

fn stop_android(d: &Path, conf: &serde_json::Value) -> Stopped {
    let Some(pid) = pid(d) else { return STOPPED };
    let serial = format!("vsock:{}:5555", conf["cid"].as_u64().unwrap_or(3));
    run("timeout", &["10", "adb", "connect", &serial]);
    run("timeout", &["15", "adb", "-s", &serial, "shell", "reboot", "-p"]);
    if gone(pid, 60) {
        return STOPPED;
    }
    unsafe { libc::kill(pid, libc::SIGTERM) };
    KILLED
}
