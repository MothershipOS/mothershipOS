//! A desktop Linux (Ubuntu 26.04) under crosvm, on the external display.
//!
//! - GPU: virtio-gpu DRM native context (the guest's Mesa freedreno drives the host Adreno through our
//!   virglrenderer's msm renderer, as PixelOS), cross-domain for buffer allocation; virgl too, for the 2D buffers of
//!   the boot splash and console
//! - boot: the VM's own kernel (vm.json boot.kernel, from its package; the Ubuntu VM's has every driver built in,
//!   mothershipOS's guest drivers too; no modules, no initrd), straight onto the disk's first partition
//! - display: the external display (USB-C DP alt mode, hardware item display-ext), hot-plugged like on a real
//!   machine: virtio-gpu starts without a display; when a monitor is connected the guest gets one at the monitor's
//!   preferred mode (`crosvm gpu add-displays`, the virtio-gpu display event), and loses it when it is unplugged.
//!   crosvm draws into the VM's presenter (MSHIP_VM_OUTPUT=DP-1), which shows it on DP-1 through a DRM lease while
//!   the VM holds display-ext
//! - keyboard and mouse: the monitor's / a dock's USB devices (hardware item usb, crosvm xHCI via crosvm.sock)
//! - sound: virtio-snd from vhost-device-sound (ALSA), played on the external display's audio (DisplayPort over
//!   USB-C: the card's MultiMedia2 routed to DISPLAY_PORT_RX)
//! - battery: the phone's battery and charging state (mship.battery, read only): crosvm's goldfish battery, fed by
//!   mship-battery; the guest kernel's goldfish_battery driver makes it the power_supply devices `battery` and `ac`
//!   that UPower and GNOME read, as on hardware
//! - network: tap ubtap0 (mship-vm-net: always NATed, the guest is 192.168.98.2); console: serial.log (hvc0)

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mship_core::api::Op;
use mship_core::uevent::{poll, read_uevent, uevents};

use crate::{CROSVM, GPU_LIBS, PRESENTER, Res, Vm, camera_nodes, daemon, env_or, err, log_file, signal, sway, venus_decoder, wait_for};

const VHOST_SOUND: &str = "/usr/local/bin/vhost-device-sound";
/// where releases before 2026.1's VM packages put the guests' kernel (VMs installed then name none)
const OLD_GUEST_KERNEL: &str = "/usr/share/mothership/guest/Image";
const CARD: &str = "mothershipniosp";
const ASOUNDRC: &str = r#"pcm.mship_dp_mix { type dmix; ipc_key 5678293; ipc_perm 0660; slave { pcm "hw:CARD=mothershipniosp,DEV=1"; rate 48000; channels 2; format S16_LE; period_size 1024; buffer_size 4096 } }
pcm.!default { type asym; playback.pcm { type plug; slave.pcm "mship_dp_mix" } capture.pcm "null" }
ctl.!default { type hw; card mothershipniosp }
"#;
const EXT: &str = "DP-1";

fn drm_dp(file: &str) -> Option<String> {
    std::fs::read_dir("/sys/class/drm").ok()?.flatten().find_map(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with("card") && n.ends_with("-DP-1") { std::fs::read_to_string(e.path().join(file)).ok() } else { None }
    })
}

/// Keep the DP device open (silence into the dmix) while the VM runs: the sound backend opens and closes its streams
/// in quick succession, and a DisplayPort PCM reopened within ~0.5 s of its close fails (the DSP's AFE port start
/// times out); the clients then only come and go on the dmix. Reopened when it ends (a DP modeset stops it).
fn hold_dp(vm: &Arc<Vm>, audio: &Path) -> Arc<Mutex<Option<u32>>> {
    let cur = Arc::new(Mutex::new(None));
    let (me, cur2, home) = (vm.clone(), cur.clone(), audio.to_path_buf());
    std::thread::spawn(move || {
        while !me.stop.load(Ordering::Relaxed) {
            let log = std::fs::OpenOptions::new().append(true).create(true).open(me.path("sound.log"));
            let mut c = Command::new("aplay");
            c.args(["-q", "-D", "mship_dp_mix", "-t", "raw", "-f", "S16_LE", "-c", "2", "-r", "48000", "/dev/zero"])
                .env("HOME", &home)
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            if let Ok(l) = log {
                c.stderr(l);
            }
            if let Ok(mut child) = c.spawn() {
                *cur2.lock().unwrap() = Some(child.id());
                let _ = child.wait();
                *cur2.lock().unwrap() = None;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    cur
}

/// The monitor's preferred mode ("WxH", the first of the connector's modes), when one is connected.
fn monitor() -> Option<String> {
    if !drm_dp("status").is_some_and(|s| s.starts_with("connected")) {
        return None;
    }
    drm_dp("modes").and_then(|m| m.lines().next().map(String::from)).filter(|m| m.contains('x'))
}

fn gpu(vm: &Vm, args: &[String]) -> bool {
    let sock = vm.dir.join("crosvm.sock");
    Command::new(CROSVM).arg("gpu").args(args).arg(&sock).env("LD_LIBRARY_PATH", GPU_LIBS)
        .stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// The guest's display follows the monitor: connected -> a display at its preferred mode, unplugged -> none, another
/// monitor -> the old display out, one at the new mode in (virtio-gpu tells the guest, which reconfigures as on a
/// real hotplug). Kernel uevents wake it (the connector's hotplug), a slow tick catches anything missed. While this VM
/// holds display-ext, DP-1 is offered to its presenter again whenever the monitor is connected and not leased.
fn follow_monitor(vm: &Arc<Vm>, vm_pid: u32) {
    let me = vm.clone();
    std::thread::spawn(move || {
        let ue = uevents().ok();
        let mut shown: Option<String> = None;
        while signal(vm_pid, 0) && !me.stop.load(Ordering::Relaxed) {
            let now = monitor();
            if now != shown && me.dir.join("crosvm.sock").exists() {
                if shown.is_some() && gpu(&me, &["remove-displays".into(), "--display-id=0".into()]) {
                    println!("{}: display removed ({})", me.name, shown.as_deref().unwrap_or(""));
                    shown = None;
                }
                if let Some(m) = &now
                    && shown.is_none()
                    && gpu(&me, &["add-displays".into(), format!("--gpu-display=mode=windowed[{}],dpi=[160,160],refresh-rate=60", m.replace('x', ","))])
                {
                    println!("{}: display added ({m})", me.name);
                    shown = now.clone();
                }
            }
            if shown.is_some() && daemon(Op::HandoverStatus).is_some_and(|h| h["display-ext"] == me.dir_str().as_str()) {
                let log = std::fs::read_to_string(me.path("display.log")).unwrap_or_default();
                let last = log.lines().rev().find(|l| l.contains("lease granted") || l.contains("lease finished"));
                if !last.is_some_and(|l| l.contains("lease granted")) {
                    sway(&format!("lease_output {EXT} off"));
                    sway(&format!("lease_output {EXT} on"));
                }
            }
            match &ue {
                Some(fd) => {
                    use std::os::fd::AsRawFd;
                    poll(&[fd.as_raw_fd()], Some(Duration::from_secs(5)));
                    while read_uevent(fd.as_raw_fd()).is_some() {}
                    std::thread::sleep(Duration::from_millis(300)); // the connector's modes settle after its uevent
                }
                None => std::thread::sleep(Duration::from_secs(2)),
            }
        }
    });
}

pub fn run(vm: &Arc<Vm>) -> Res<i32> {
    let d = vm.dir.clone();
    let ds = vm.dir_str();
    // first start of a new VM: cloud-init's NoCloud seed over HTTP on the VM's own tap
    let mut ds_param = String::new();
    if d.join("seed").is_dir() && !d.join("seed/used").exists() {
        crate::seed::serve(d.join("seed"), "192.168.98.1:8123", vm.stop.clone())?;
        ds_param = " ds=nocloud;s=http://192.168.98.1:8123/".into();
    }
    // vm.json "boot": {"params": "..."}: more kernel parameters (debugging)
    if let Some(p) = vm.conf["boot"]["params"].as_str() {
        ds_param = format!("{ds_param} {p}");
    }
    for f in ["display.sock", "crosvm.sock", "sound.sock"] {
        let _ = std::fs::remove_file(d.join(f));
    }
    // copy: the presenter scans out its own GPU copy of each frame (the guest's flips complete at once and it
    // would draw into the buffer still on screen: tearing)
    let log = log_file(&d, "display")?;
    vm.helpers.spawn("presenter", Command::new(PRESENTER).arg(d.join("display.sock"))
        .env("MSHIP_VM_OUTPUT", env_or("MSHIP_VM_OUTPUT", EXT))
        .env("MSHIP_VM_PRESENT", env_or("MSHIP_VM_PRESENT", "copy"))
        .stdout(log.try_clone().map_err(err)?).stderr(log))?;
    wait_for(&d.join("display.sock"), 30);

    // sound: the backend opens ALSA "default", which its own config makes the DisplayPort device. dmix: the backend
    // keeps a placeholder open per stream while it opens the real one; plug: the guest's rate, channels and format;
    // capture: none (DP has no input)
    let sound = env_or("MSHIP_VM_SOUND", "1") != "0";
    let audio = d.join("audio");
    let mut snd = vec![];
    let holder = if sound {
        std::fs::create_dir_all(&audio).map_err(err)?;
        std::fs::write(audio.join(".asoundrc"), ASOUNDRC).map_err(err)?;
        let log = log_file(&d, "sound")?;
        let _ = Command::new("amixer").args(["-q", "-c", CARD, "cset", "name=DISPLAY_PORT_RX Audio Mixer MultiMedia2", "1"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status();
        let holder = hold_dp(vm, &audio);
        std::thread::sleep(Duration::from_secs(1));
        vm.helpers.spawn("sound", Command::new(VHOST_SOUND).args(["--socket", &format!("{ds}/sound.sock"), "--backend", "alsa"])
            .env("HOME", &audio).env("RUST_LOG", env_or("MSHIP_VM_SOUND_LOG", "warn"))
            .stdout(log.try_clone().map_err(err)?).stderr(log))?;
        // max-queue-size: the backend takes at most 64 entries a queue (crosvm offers 32768 by default)
        if wait_for(&d.join("sound.sock"), 30) {
            snd = vec!["--vhost-user".to_string(), format!("type=sound,socket={ds}/sound.sock,max-queue-size=64")];
        }
        Some(holder)
    } else {
        None
    };
    // on the way out: the sound stream first (DP audio left playing delays the DP pipe's shutdown), then the
    // presenter while it still holds its lease (it switches its pipe off on the way out, which it can't once the
    // lease is revoked); then the hardware goes back
    *vm.before_release.lock().unwrap() = Some(Box::new(move |vm: &Vm| {
        if let Some(pid) = holder.and_then(|h| *h.lock().unwrap()) {
            signal(pid, libc::SIGTERM);
        }
        vm.helpers.stop("sound", Duration::from_secs(1));
        vm.helpers.stop("presenter", Duration::from_secs(3));
    }));

    let mut args: Vec<String> = vec![
        "--log-level=info".into(), "run".into(), "--name".into(), vm.name.clone(), "--disable-sandbox".into(),
        "--params".into(), format!("root=/dev/vda1 rootfstype=ext4 rootwait ro console=hvc0 quiet splash{ds_param}"),
        "--block".into(), format!("path={ds}/{}", vm.conf["boot"]["disk"].as_str().unwrap_or("hdd.raw")),
        format!("--serial=type=file,path={ds}/serial.log,hardware=virtio-console,num=1,console=true"),
        "--net".into(), "tap-name=ubtap0,mac=52:54:00:12:34:56".into(),
        "--vsock".into(), format!("cid={}", vm.conf["cid"].as_u64().unwrap_or(4)),
        "--wayland-sock".into(), format!("{ds}/display.sock"),
        // virgl2: rutabaga starts virglrenderer's GL renderer only with it (the 2D buffers of the boot splash and the
        // console); the guest's Mesa still takes the DRM native context
        // no display until a monitor is connected (follow_monitor)
        "--gpu".into(), "backend=virglrenderer,context-types=virgl:virgl2:drm:cross-domain,egl=true,gles=true,no-display=true".into(),
        "--socket".into(), format!("{ds}/crosvm.sock"),
    ];
    args.extend(vm.pool_args());
    args.extend(snd);
    // battery slot (mship.battery): goldfish battery our crosvm feeds from mship-battery
    if vm.conf["slots"]["battery"].is_object() {
        args.extend(["--battery".into(), "type=goldfish".into()]);
    }
    // HW video decode (mship.video): the host Venus decoder via virtio-media (built into the guest kernel)
    if env_or("MSHIP_VM_VIDEO", "1") != "0"
        && let Some(v) = venus_decoder()
    {
        args.push(format!("--v4l2-proxy={v}"));
    }
    // cameras (mship.camera): one V4L2 capture device each, also via virtio-media
    if vm.conf["slots"]["camera"].is_object() {
        args.extend(camera_nodes(&vm.name).into_iter().map(|n| format!("--v4l2-proxy={n}")));
    }
    // the VM's own kernel (vm.json boot.kernel: its package ships it)
    let kernel = vm.conf["boot"]["kernel"].as_str().map(|k| vm.path(k)).unwrap_or_else(|| OLD_GUEST_KERNEL.into());
    if !kernel.exists() {
        return Err(format!("{}: no kernel (vm.json boot.kernel)", kernel.display()));
    }
    args.push(kernel.to_string_lossy().into());
    let mut child = Command::new(CROSVM).args(&args).env("LD_LIBRARY_PATH", GPU_LIBS).spawn().map_err(|e| format!("crosvm: {e}"))?;
    println!("{}: crosvm {}", vm.name, child.id());
    // what is assigned to the VM: at its first frame on a monitor, or once crosvm is up when there is none
    if monitor().is_some() {
        vm.lend_when_shown(child.id(), None);
    } else {
        let (me, pid) = (vm.clone(), child.id());
        std::thread::spawn(move || {
            if wait_for(&me.dir.join("crosvm.sock"), 300) && signal(pid, 0) {
                daemon(Op::HandoverGive { vm: me.dir_str(), items: vec![] });
            }
        });
    }
    follow_monitor(vm, child.id());
    let status = child.wait().map_err(err)?;
    Ok(status.code().unwrap_or(1))
}
