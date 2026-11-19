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

pub const VHOST_SOUND: &str = "/usr/local/bin/vhost-device-sound";

/// mship.audio 1.0: the guest's streams, in stream (= guest PCM device) order: direction, the VM's node suffix,
/// description. Outputs play into the VM's sink `mship-vm-<vm>.<suffix>`, inputs record from its source of that name.
pub const AUDIO_STREAMS: &[(&str, &str, &str)] = &[
    ("out", "speaker", "speaker"),
    ("out", "earpiece", "earpiece"),
    ("out", "headset", "headset"),
    ("in", "mic", "microphone"),
    ("in", "backmic", "back microphone"),
    ("in", "headsetmic", "headset microphone"),
];
/// the headset jack: headphone switch on the headset output (stream 2), mic switch on the headset mic (stream 5)
const AUDIO_JACKS: &str = "2:hp,5:mic";

/// The backend's ALSA PCMs (mship_pcm<stream>): each stream into / from its own PipeWire node of the VM.
fn audio_asoundrc(base: &str) -> String {
    AUDIO_STREAMS.iter().enumerate()
        .map(|(i, (_, suffix, _))| format!("pcm.mship_pcm{i} {{ type plug; slave.pcm \"pipewire:NODE={base}.{suffix}\" }}\n"))
        .collect()
}

/// Run pw-loopback with `args` while the VM runs: it ends with PipeWire (a session restart), and the VM's stream then
/// has nothing to play into or record from (it never falls back to another node) until it is back.
fn supervise_loopback(vm: &Vm, args: Vec<String>, runtime: &str, log: &std::fs::File) -> Res<()> {
    let (stop, rt, lb_log) = (vm.stop.clone(), runtime.to_string(), log.try_clone().map_err(err)?);
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let child = Command::new("pw-loopback").args(&args).env("XDG_RUNTIME_DIR", &rt)
                .stdin(Stdio::null()).stdout(Stdio::null())
                .stderr(lb_log.try_clone().map(Stdio::from).unwrap_or(Stdio::null())).spawn();
            let Ok(mut child) = child else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            while !stop.load(Ordering::Relaxed) && child.try_wait().ok().flatten().is_none() {
                std::thread::sleep(Duration::from_millis(500));
            }
            let _ = child.kill();
            let _ = child.wait();
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    Ok(())
}

/// The VM's audio slot (mship.audio 1.x): a virtio-snd device (vhost-device-sound) with one stream per phone audio
/// device (AUDIO_STREAMS) and the headset jack, so the guest routes like on the phone. Each stream plays into / records
/// from its own PipeWire node of the VM, `mship-vm-<vm>.<device>` (pw-loopbacks, helpers of this VM); mship-audio links
/// them to the phone's devices the VM holds (speaker, earpiece, headset, microphone), to silence otherwise, and tells
/// the backend the jack state (`<vm dir>/audio/jacks`). The streams never fall back to other nodes. The crosvm
/// arguments for the device; none without the slot or when its backend does not come up.
pub fn audio_slot(vm: &Vm) -> Res<Vec<String>> {
    let slot = vm.conf["slots"].as_object().is_some_and(|s| s.values().any(|x| x["contract"] == "mship.audio"));
    if !slot || env_or("MSHIP_VM_SOUND", "1") == "0" {
        return Ok(vec![]);
    }
    let base = format!("mship-vm-{}", vm.name);
    let home = vm.dir.join("audio");
    std::fs::create_dir_all(&home).map_err(err)?;
    std::fs::write(home.join(".asoundrc"), audio_asoundrc(&base)).map_err(err)?;
    std::fs::write(home.join("jacks"), "hp=0 mic=0\n").map_err(err)?;
    let sock = vm.dir.join("sound.sock");
    let _ = std::fs::remove_file(&sock);
    let runtime = format!("/run/user/{}", unsafe { libc::getuid() });
    let title = vm.conf["name"].as_str().unwrap_or(&vm.name).replace('"', "");
    let log = log_file(&vm.dir, "sound")?;
    for (dir, suffix, what) in AUDIO_STREAMS {
        let node = format!("{base}.{suffix}");
        let args = if *dir == "out" {
            // a sink; its output `.out` is linked by mship-audio. Passive output: the phone's device only runs while
            // the guest plays into it
            vec![
                "-n".to_string(), node.clone(),
                "--capture-props".into(), format!("{{ media.class=Audio/Sink node.name={node} node.description=\"{title} {what}\" \
                                                  node.virtual=true priority.session=0 priority.driver=0 }}"),
                "--playback-props".into(), format!("{{ node.name={node}.out node.description=\"{title} {what}\" \
                                                   node.autoconnect=false node.dont-reconnect=true node.passive=true }}"),
            ]
        } else {
            // a mono source; its input `-in` is linked by mship-audio. Passive input: the phone's mic only runs while
            // the guest records
            vec![
                "-n".to_string(), node.clone(), "-c".into(), "1".into(), "-m".into(), "[ MONO ]".into(),
                "--capture-props".into(), format!("{{ node.name={node}-in node.description=\"{title} {what}\" \
                                                  node.autoconnect=false node.dont-reconnect=true node.passive=true }}"),
                "--playback-props".into(), format!("{{ media.class=Audio/Source node.name={node} node.description=\"{title} {what}\" \
                                                   node.virtual=true priority.session=0 priority.driver=0 }}"),
            ]
        };
        supervise_loopback(vm, args, &runtime, &log)?;
    }
    let streams: Vec<&str> = AUDIO_STREAMS.iter().map(|(d, _, _)| *d).collect();
    vm.helpers.spawn("sound", Command::new(VHOST_SOUND)
        .args(["--socket", &sock.to_string_lossy(), "--backend", "alsa"])
        .env("HOME", &home).env("XDG_RUNTIME_DIR", &runtime)
        .env("MSHIP_SOUND_STREAMS", streams.join(","))
        .env("MSHIP_SOUND_JACKS", AUDIO_JACKS)
        // the targets are per stream, in the ALSA configuration
        .env("PIPEWIRE_PROPS", format!("{{ node.dont-fallback=true node.dont-reconnect=true application.name=\"{title}\" }}"))
        .env("RUST_LOG", env_or("MSHIP_VM_SOUND_LOG", "warn"))
        .stdout(log.try_clone().map_err(err)?).stderr(log))?;
    if !wait_for(&sock, 30) {
        println!("{}: no sound device (vhost-device-sound did not start)", vm.name);
        return Ok(vec![]);
    }
    let jack = serve_jack_switches(vm, &home.join("jacks"))?;
    // max-queue-size: the backend takes at most 64 entries a queue (crosvm offers 32768 by default)
    Ok(vec![
        "--vhost-user".into(), format!("type=sound,socket={},max-queue-size=64", sock.display()),
        format!("--input=switches[path={}]", jack.display()),
    ])
}

/// The headset jack as input switches (SW_HEADPHONE_INSERT, SW_MICROPHONE_INSERT) of a virtio-input switches device,
/// as on a phone: the guest's virtio-snd jacks are phantom jacks (ALSA controls, no input device), and Android's wired
/// accessory handling (and a Linux desktop's) wants the switches. crosvm connects to `<vm dir>/jack.sock`; the state
/// follows `jacks` (`hp=0|1 mic=0|1`, written by mship-audio).
fn serve_jack_switches(vm: &Vm, jacks: &Path) -> Res<PathBuf> {
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    const EV_SYN: u16 = 0;
    const EV_SW: u16 = 5;
    const SW_HEADPHONE_INSERT: u16 = 2;
    const SW_MICROPHONE_INSERT: u16 = 4;
    let path = vm.dir.join("jack.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(err)?;
    let (stop, jacks) = (vm.stop.clone(), jacks.to_path_buf());
    let read = move || -> (bool, bool) {
        let s = std::fs::read_to_string(&jacks).unwrap_or_default();
        (s.contains("hp=1"), s.contains("mic=1"))
    };
    let event = |t: u16, c: u16, v: u32| -> [u8; 8] {
        let mut e = [0u8; 8];
        e[..2].copy_from_slice(&t.to_le_bytes());
        e[2..4].copy_from_slice(&c.to_le_bytes());
        e[4..].copy_from_slice(&v.to_le_bytes());
        e
    };
    std::thread::spawn(move || {
        // crosvm connects once at start (again after a reconnect); the state is sent on connect and on every change
        for conn in listener.incoming() {
            let Ok(mut c) = conn else { continue };
            let mut sent: Option<(bool, bool)> = None;
            while !stop.load(Ordering::Relaxed) {
                let now = read();
                if sent != Some(now) {
                    let mut buf = Vec::with_capacity(24);
                    buf.extend(event(EV_SW, SW_HEADPHONE_INSERT, now.0 as u32));
                    buf.extend(event(EV_SW, SW_MICROPHONE_INSERT, now.1 as u32));
                    buf.extend(event(EV_SYN, 0, 0));
                    if c.write_all(&buf).is_err() {
                        break;
                    }
                    sent = Some(now);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
    });
    Ok(path)
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
    /// the guest frees (in 2 MB blocks, so the huge pages stay whole).
    ///
    /// Also what makes a VM run near native: guest RAM in huge pages (--hugepages; dist/rootfs tmpfiles allows shmem
    /// THP), so KVM maps it in 2 MB blocks; and a GIC ITS, so virtio-pci devices use MSI-X: edge interrupts injected
    /// in the kernel and spread over the vCPUs, instead of shared level lines on vCPU 0 whose every interrupt also
    /// exits to crosvm for the ISR read (the GPU's fence round trip 245 -> 200 us, disk 4k reads 15k -> 30k IOPS).
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
            "--hugepages".into(),
            "--irqchip".into(), "kernel[allow-vgic-its]".into(),
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
