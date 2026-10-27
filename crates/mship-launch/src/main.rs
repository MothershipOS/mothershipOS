//! mship-launch: run a VM from its directory (`mship-launch <vm dir>`, or $MSHIP_VM_DIR). mothershipd starts it as
//! the session user in the session's environment (vm.json "launcher"); it is the VM's process (run.pid) until the
//! VM ends.
//!
//! By the manifest's platform: cuttlefish-arm64 ([`android`]: PixelOS on the panel) or efi-virt-arm64 ([`linux`]:
//! a desktop Linux on the external display). Both run crosvm with the VM's presenter (mship-presenter: the display
//! through a DRM lease), lend the VM its assigned hardware once crosvm's window is up, and give it back on the way
//! out (other VMs keep theirs).

mod android;
mod linux;
mod seed;

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mship_core::api::Op;
use serde_json::Value;

pub const PRESENTER: &str = "/usr/local/bin/mship-presenter";
pub const INPUTD: &str = "/usr/local/bin/mship-inputd";
pub const CROSVM: &str = "/opt/mship-gpu/bin/crosvm";
pub const GPU_LIBS: &str = "/opt/mship-gpu/lib";

pub type Res<T> = Result<T, String>;

pub fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// An environment override (MSHIP_VM_*), else the default.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.into())
}

/// The event device whose name contains `name`.
pub fn evdev(name: &str) -> Option<String> {
    let mut found: Vec<String> = std::fs::read_dir("/sys/class/input")
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let dev = std::fs::read_to_string(e.path().join("device/name")).ok()?;
            (n.starts_with("event") && dev.contains(name)).then(|| format!("/dev/input/{n}"))
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

/// The VM's camera slot (mship.camera): mship-camera's nodes for it, in camera order (the guest sees them as V4L2
/// capture devices through virtio-media, in this order).
pub fn camera_nodes(vm: &str) -> Vec<String> {
    let mut links: Vec<PathBuf> = std::fs::read_dir(Path::new("/run/mothership/camera").join(vm))
        .into_iter().flatten().flatten().map(|e| e.path()).collect();
    links.sort();
    links.iter().filter_map(|l| std::fs::canonicalize(l).ok()).map(|p| p.to_string_lossy().into_owned()).collect()
}

/// The host Venus decoder (by name: its /dev/videoN index changes between boots).
pub fn venus_decoder() -> Option<String> {
    std::fs::read_dir("/sys/class/video4linux").ok()?.flatten().find_map(|v| {
        let name = std::fs::read_to_string(v.path().join("name")).ok()?;
        (name.trim() == "qcom-venus-decoder").then(|| format!("/dev/{}", v.file_name().to_string_lossy()))
    })
}

pub fn wait_for(path: &Path, tries: u32) -> bool {
    for _ in 0..tries {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    path.exists()
}

/// A log file for a helper: the previous run's kept as <name>.prev.log.
pub fn log_file(d: &Path, name: &str) -> Res<File> {
    let p = d.join(format!("{name}.log"));
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 0) {
        let _ = std::fs::rename(&p, d.join(format!("{name}.prev.log")));
    }
    File::create(&p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Follow a file's new lines (tail -F) until `stop`.
pub fn follow(path: PathBuf, stop: Arc<AtomicBool>, mut f: impl FnMut(&str) + Send + 'static) {
    std::thread::spawn(move || {
        let mut r: Option<BufReader<File>> = None;
        let mut line = String::new();
        while !stop.load(Ordering::Relaxed) {
            if r.is_none() {
                r = File::open(&path).ok().map(|mut f| {
                    let _ = f.seek(SeekFrom::End(0));
                    BufReader::new(f)
                });
            }
            match r.as_mut().map(|r| r.read_line(&mut line)) {
                Some(Ok(n)) if n > 0 => {
                    if line.ends_with('\n') {
                        f(line.trim_end());
                        line.clear();
                    }
                }
                Some(Ok(_)) => std::thread::sleep(Duration::from_millis(200)),
                _ => {
                    r = None;
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    });
}

/// mothershipd, best effort (the VM runs without it, only without its hardware).
pub fn daemon(op: Op) -> Option<Value> {
    match mship_core::client::call(op) {
        Ok(v) => Some(v),
        Err(e) => {
            println!("mothershipd: {e}");
            None
        }
    }
}

pub fn sway(cmd: &str) {
    if let Err(e) = mship_core::sway::Sway::session().and_then(|mut s| s.command(cmd)) {
        println!("sway: {e}");
    }
}

/// crosvm and its children inherit this. Its GPU device keeps an eventfd per fence and a dma-buf per exported buffer:
/// an Android desktop needs well over the usual soft limit of 1024 (at the limit the renderer can't export a buffer,
/// the guest's mapping fails and Mesa takes SurfaceFlinger down with it). The hard limit stays the cap.
fn raise_file_limit() {
    let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) == 0 && l.rlim_cur < l.rlim_max {
            l.rlim_cur = l.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &l);
        }
    }
}

pub fn signal(pid: u32, sig: i32) -> bool {
    unsafe { libc::kill(pid as i32, sig) == 0 }
}

/// The VM's helper processes, stopped when the VM ends.
#[derive(Default)]
pub struct Helpers(Mutex<Vec<(String, Child)>>);

impl Helpers {
    pub fn spawn(&self, name: &str, cmd: &mut Command) -> Res<u32> {
        let c = cmd.stdin(Stdio::null()).spawn().map_err(|e| format!("{name}: {e}"))?;
        let pid = c.id();
        self.0.lock().unwrap().push((name.into(), c));
        Ok(pid)
    }

    /// SIGTERM `name` and wait for it (at most `wait`).
    pub fn stop(&self, name: &str, wait: Duration) {
        let mut v = self.0.lock().unwrap();
        for (_, c) in v.iter_mut().filter(|(n, _)| n == name) {
            signal(c.id(), libc::SIGTERM);
            let t = std::time::Instant::now();
            while t.elapsed() < wait {
                if c.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
        v.retain(|(n, _)| n != name);
    }

    pub fn stop_all(&self) {
        let names: Vec<String> = self.0.lock().unwrap().iter().map(|(n, _)| n.clone()).collect();
        for n in names {
            self.stop(&n, Duration::from_secs(1));
        }
    }
}

pub type Cleanup = Box<dyn FnOnce(&Vm) + Send>;

/// What every VM run shares.
pub struct Vm {
    pub dir: PathBuf,
    pub name: String,
    pub conf: Value,
    pub helpers: Helpers,
    /// set when the VM ends: helper threads stop
    pub stop: Arc<AtomicBool>,
    cleaned: AtomicBool,
    /// the profile's own cleanup, before the hardware is given back
    pub before_release: Mutex<Option<Cleanup>>,
}

impl Vm {
    /// crosvm's CPU, memory and balloon arguments (mship_core::pool): vCPUs mirroring the host's CPUs, the pool's RAM
    /// (resources.cpus / resources.memory_mb cap them; MSHIP_VM_MEM overrides the RAM), part of it outside the
    /// balloon at boot. mothershipd lends the rest through the balloon (crosvm.sock); page reporting returns what
    /// the guest frees.
    pub fn pool_args(&self) -> Vec<String> {
        use mship_core::pool;
        let cap = |k: &str| self.conf["resources"][k].as_u64();
        let size = std::env::var("MSHIP_VM_MEM").ok().and_then(|m| m.parse().ok())
            .unwrap_or_else(|| pool::vm_size_mb(pool::host_total_mb(), cap("memory_mb")));
        let mut a = pool::crosvm_cpu_args(&pool::host_cpus(), cap("cpus").map(|c| c as usize));
        a.extend([
            "--mem".into(), format!("size={size}"),
            "--init-mem".into(), pool::vm_boot_mb(size).to_string(),
            "--balloon-page-reporting".into(),
        ]);
        a
    }

    pub fn path(&self, f: &str) -> PathBuf {
        self.dir.join(f)
    }

    pub fn dir_str(&self) -> String {
        self.dir.to_string_lossy().into_owned()
    }

    /// Lend the VM what is assigned to it once crosvm has connected its display (the guest's first frame: a first
    /// boot can take long), while the VM still runs.
    pub fn lend_when_shown(self: &Arc<Self>, vm_pid: u32, limit: Option<Duration>) {
        let me = self.clone();
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            while signal(vm_pid, 0) && limit.is_none_or(|l| t.elapsed() < l) && !me.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if std::fs::read_to_string(me.path("display.log")).is_ok_and(|l| l.contains("crosvm window")) {
                    daemon(Op::HandoverGive { vm: me.dir_str(), items: vec![] });
                    return;
                }
            }
        });
    }

    /// The VM ended (or the launcher was stopped): the profile's cleanup, the hardware back, the helpers stopped.
    pub fn cleanup(&self) {
        if self.cleaned.swap(true, Ordering::SeqCst) {
            return;
        }
        self.stop.store(true, Ordering::SeqCst);
        if let Some(f) = self.before_release.lock().unwrap().take() {
            f(self);
        }
        daemon(Op::HandoverRelease { vm: self.dir_str() }); // only this VM's: other VMs keep theirs
        self.helpers.stop_all();
        let _ = std::fs::remove_file(self.path("run.pid"));
    }
}

/// SIGTERM/SIGINT/SIGHUP (vm.stop, the session ending): clean up, exit 143. Blocked here, before any thread starts,
/// and taken by one thread with sigwait.
fn on_signals(vm: Arc<Vm>) {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        std::thread::spawn(move || {
            let mut sig = 0;
            libc::sigwait(&set, &mut sig);
            println!("signal {sig}: stopping");
            vm.cleanup();
            std::process::exit(143);
        });
    }
}

fn run() -> Res<i32> {
    let dir =std::env::args().nth(1).or_else(|| std::env::var("MSHIP_VM_DIR").ok()).ok_or("usage: mship-launch <vm dir>")?;
    let dir = Path::new(&dir).canonicalize().map_err(|e| format!("{dir}: {e}"))?;
    let m = mship_core::manifest::load(&dir).map_err(err)?;
    let platform = m.platform().0.to_string();
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    // one instance: a second start would remove the running one's sockets and take its hardware back on exit
    let old: Option<i32> = std::fs::read_to_string(dir.join("run.pid")).ok().and_then(|p| p.trim().parse().ok());
    if old.is_some_and(|p| p != std::process::id() as i32 && signal(p as u32, 0)) {
        return Err(format!("{name} is already running"));
    }
    std::fs::write(dir.join("run.pid"), format!("{}\n", std::process::id())).map_err(err)?;
    let vm = Arc::new(Vm {
        dir,
        name,
        conf: Value::Object(m.data.clone()),
        helpers: Helpers::default(),
        stop: Arc::new(AtomicBool::new(false)),
        cleaned: AtomicBool::new(false),
        before_release: Mutex::new(None),
    });
    // vm.json "env" (debugging, e.g. MSHIP_VM_DISPLAY_STATS): for crosvm and the helpers too; set before any thread
    if let Some(env) = vm.conf["env"].as_object() {
        for (k, v) in env {
            unsafe { std::env::set_var(k, v.as_str().unwrap_or_default()) };
        }
    }
    raise_file_limit();
    on_signals(vm.clone());
    let r = match platform.as_str() {
        "cuttlefish-arm64" => android::run(&vm),
        "efi-virt-arm64" => linux::run(&vm),
        p => Err(format!("no launcher for platform {p}")),
    };
    vm.cleanup();
    r
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("mship-launch: {e}");
            ExitCode::FAILURE
        }
    }
}
