//! PixelOS (Cuttlefish arm64, custom_nio_vm) under crosvm, on the panel.
//!
//! - GPU: native = virtio-gpu DRM native context (the guest's Mesa freedreno/turnip drives the host Adreno through
//!   virglrenderer's msm renderer), cross-domain for gralloc; gfxstream (host GLES translation) is the fallback when
//!   the package has no native initrd
//! - display: crosvm draws into the VM's presenter, which shows it on the panel through a DRM lease (vm.json
//!   "display_mode": "lease"); Android's display power switches the leased CRTC (the guest kernel log's
//!   VIRTUAL_DEVICE_DISPLAY_POWER_MODE_CHANGED: SIGUSR1 off, SIGUSR2 on)
//! - input: touch, volume keys and power key through mship-inputd, only while lent (handover -> input.ctl)
//! - battery: goldfish battery fed from mship-battery; usb: crosvm xHCI via crosvm.sock (mship-usb)
//! - bluetooth: hvc5 -> mship-bt-bridge; network: taps pxmtap0/pxetap0/pxwtap0/pxwltap0 (mship-vm-net)
//! - adb: `adb connect vsock:3:5555`; logs: kernel.log (hvc0), logcat.log (hvc2), serial.log (UART)

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use crate::{CROSVM, GPU_LIBS, INPUTD, PRESENTER, Res, Vm, camera_nodes, env_or, err, evdev, follow, log_file, signal, venus_decoder, wait_for};

/// Cuttlefish's own crosvm and gfxstream (the fallback)
const CF_CROSVM: &str = "/opt/crosvm/bin/crosvm";
const CF_LIBS: &str = "/opt/crosvm/bin";
/// Cuttlefish services the guest connects to on the host: accepted and drained (6900 = lights)
const STUB_PORTS: &[u32] = &[6900];
const PANEL: (u32, u32) = (1080, 2520);

fn vsock_stub(port: u32) {
    std::thread::spawn(move || {
        let Ok(l) = vsock::VsockListener::bind_with_cid_port(libc::VMADDR_CID_ANY, port) else {
            println!("vsock {port}: cannot listen");
            return;
        };
        for c in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut c = c;
                let _ = std::io::copy(&mut c, &mut std::io::sink());
            });
        }
    });
}

/// hvc<N> = virtio-console num N+1 (Cuttlefish layout: 0 kernel, 1 shell, 2 logcat, 3-17 host channels)
fn hvc(vm: &Vm, n: u32, bt: &str) -> String {
    let d = vm.dir_str();
    match n {
        0 => format!("--serial=type=file,path={d}/kernel.log,hardware=virtio-console,num=1"),
        1 => format!("--serial=type=file,path={d}/console.log,hardware=virtio-console,num=2"),
        2 => format!("--serial=type=file,path={d}/logcat.log,hardware=virtio-console,num=3"),
        // bt slot (mship.bt): the Bluetooth HAL's H4 stream to mship-bt-bridge
        5 if std::path::Path::new(bt).exists() => {
            format!("--serial=type=unix-stream,path={bt},input-unix-stream,hardware=virtio-console,num=6")
        }
        n => format!("--serial=type=sink,hardware=virtio-console,num={}", n + 1),
    }
}

pub fn run(vm: &Arc<Vm>) -> Res<i32> {
    let d = &vm.dir;
    let p = d.join("pkg");
    for port in STUB_PORTS {
        vsock_stub(*port);
    }
    let native = env_or("MSHIP_VM_GPU", "native") == "native"
        && p.join("initrd-crosvm-native.img").exists()
        && std::path::Path::new(CROSVM).exists();
    let host_vk = env_or("MSHIP_VM_HOST_VULKAN", "false") == "true";
    let (crosvm, libs, initrd, gpu) = if native {
        // drm = the msm native context (Mesa); cross-domain = gralloc: the guest's minigbm asks the host to
        // allocate, our crosvm allocates with minigbm's msm backend and returns the exact layout + modifier
        (CROSVM, format!("{GPU_LIBS}:{CF_LIBS}"), p.join("initrd-crosvm-native.img"),
         "backend=virglrenderer,context-types=drm:cross-domain,egl=true,gles=true".to_string())
    } else {
        let ctx = if host_vk { "gfxstream-gles:gfxstream-vulkan:gfxstream-composer" } else { "gfxstream-gles:gfxstream-composer" };
        (CF_CROSVM, CF_LIBS.to_string(), p.join("initrd-crosvm.img"),
         format!("backend=gfxstream,egl=true,surfaceless=true,vulkan={host_vk},context-types={ctx}"))
    };
    // guest frame rate = the panel's only mode (90 Hz); 60 on a 90 Hz panel shows frames 2-1-2-1 vblanks (judder)
    let hz = env_or("MSHIP_VM_REFRESH", "90");

    // input: touch + volume keys + power key, reach the VM only while lent
    for f in ["touch.sock", "keys.sock", "input.ctl"] {
        let _ = std::fs::remove_file(d.join(f));
    }
    let touch = std::env::var("MSHIP_VM_TOUCH").ok().or_else(|| evdev("NT36672C")).unwrap_or_default();
    let keys = std::env::var("MSHIP_VM_KEYS").ok().unwrap_or_else(|| {
        // vol up + assist, vol down
        format!("{},{}", evdev("gpio-keys").unwrap_or_default(), evdev("pm8941_resin").unwrap_or_default())
    });
    let pwr = std::env::var("MSHIP_VM_POWER").ok().or_else(|| evdev("pm8941_pwrkey")).unwrap_or_default();
    let log = log_file(d, "inputd")?;
    vm.helpers.spawn("inputd", Command::new(INPUTD).args([&vm.dir_str(), &touch, &keys, &pwr])
        .stdout(log.try_clone().map_err(err)?).stderr(log))?;
    wait_for(&d.join("keys.sock"), 30);

    // the display goes to the panel through a DRM lease, never as a sway window
    let _ = std::fs::remove_file(d.join("display.sock"));
    let log = log_file(d, "display")?;
    let presenter = vm.helpers.spawn("presenter", Command::new(PRESENTER).arg(d.join("display.sock"))
        .stdout(log.try_clone().map_err(err)?).stderr(log))?;
    wait_for(&d.join("display.sock"), 30);
    // Android's display power -> the panel (ranchu logs it to the guest kernel log, console=hvc0, loglevel 5)
    std::fs::write(d.join("kernel.log"), b"").map_err(err)?;
    follow(d.join("kernel.log"), vm.stop.clone(), move |l| {
        if l.contains("VIRTUAL_DEVICE_DISPLAY_POWER_MODE_CHANGED") {
            if l.contains("mode=OFF") {
                signal(presenter, libc::SIGUSR1);
            } else if l.contains("mode=ON") {
                signal(presenter, libc::SIGUSR2);
            }
        }
    });

    for f in ["console.sock", "crosvm.sock"] {
        let _ = std::fs::remove_file(d.join(f));
    }
    let bt = format!("/run/mothership/bt/{}.sock", vm.name); // served by mship-bt-bridge while it runs
    let ds = vm.dir_str();
    let mut args: Vec<String> = vec![
        "--log-level=info".into(), "run".into(), "--name".into(), "pixelos".into(), "--disable-sandbox".into(),
        "--initrd".into(), initrd.to_string_lossy().into(),
        "--params".into(), std::fs::read_to_string(p.join("cmdline.crosvm")).map_err(|e| format!("cmdline.crosvm: {e}"))?.trim().into(),
        "--block".into(), format!("path={ds}/disk.img"),
        format!("--serial=type=file,path={ds}/serial.log,hardware=serial,num=1,console=true,earlycon=true"),
    ];
    args.extend((0..18).map(|n| hvc(vm, n, &bt)));
    args.extend(["--vsock".into(), format!("cid={}", vm.conf["cid"].as_u64().unwrap_or(3))]);
    if crosvm == CROSVM {
        // battery slot (mship.battery): goldfish battery our crosvm feeds from mship-battery
        args.extend(["--battery".into(), "type=goldfish".into()]);
        // HW video decode (mship.video): the host Venus decoder via virtio-media
        if env_or("MSHIP_VM_VIDEO", "true") == "true"
            && let Some(v) = venus_decoder()
        {
            args.push(format!("--v4l2-proxy={v}"));
        }
        // cameras (mship.camera): one V4L2 capture device each, also via virtio-media
        if vm.conf["slots"]["camera"].is_object() {
            args.extend(camera_nodes(&vm.name).into_iter().map(|n| format!("--v4l2-proxy={n}")));
        }
    }
    args.extend(["--socket".into(), format!("{ds}/crosvm.sock")]);
    args.extend(vm.pool_args());
    for tap in ["tap-name=pxmtap0", "tap-name=pxetap0", "tap-name=pxwtap0", "tap-name=pxwltap0,mac=02:6d:73:00:00:03"] {
        args.extend(["--net".into(), tap.into()]);
    }
    args.extend([
        "--wayland-sock".into(), format!("{ds}/display.sock"),
        "--gpu".into(), format!("{gpu}{},displays=[[mode=windowed[{},{}],dpi=[420,420],refresh-rate={hz}]]",
                               env_or("MSHIP_VM_GPU_EXTRA", ""), PANEL.0, PANEL.1),
        format!("--input=multi-touch[path={ds}/touch.sock,width={},height={},name=NT36672C]", PANEL.0, PANEL.1),
        format!("--input=keyboard[path={ds}/keys.sock]"),
        p.join("Image").to_string_lossy().into(),
    ]);
    let mut cmd = Command::new(crosvm);
    cmd.args(&args).env("LD_LIBRARY_PATH", libs).env("EGL_PLATFORM", "surfaceless")
        // gfxstream's EGL blob cache lookup spins in Mesa's compiles; Mesa's own shader cache is not needed here
        .env("MESA_SHADER_CACHE_DISABLE", "true");
    // zero copy (gfxstream): the GL shim hands the guest's scanout buffers to the presenter as dmabufs
    if !native && env_or("MSHIP_VM_ZERO_COPY", "true") == "true" && std::path::Path::new(CF_LIBS).join("libGLESv2.so").exists() {
        cmd.env("LD_PRELOAD", format!("{CF_LIBS}/libGLESv2.so")).env("MSHIP_ZC_SOCK", format!("{ds}/display.sock-zc"));
    }
    let mut child = cmd.spawn().map_err(|e| format!("crosvm: {e}"))?;
    println!("pixelos: crosvm {} ({})", child.id(), if native { "native context" } else { "gfxstream" });
    vm.lend_when_shown(child.id(), Some(Duration::from_secs(60)));
    let status = child.wait().map_err(err)?;
    Ok(status.code().unwrap_or(1))
}
