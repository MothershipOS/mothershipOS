//! mothershipOS developer tasks (`cargo xtask <task>`).
//!
//! They run in the build image (xtask/docker/Dockerfile: Ubuntu 26.04 with the phone's arm64 libraries), so the
//! binaries link against exactly what the phone runs.
//!
//! - `check`: clippy (warnings are errors) and the tests
//! - `gpu`: the phone's VMM (host/gpu): crosvm, virglrenderer, minigbm, pinned and patched, for /opt/mship-gpu
//! - `host-kernel`, `initramfs`, `fit`, `abl-dtb`: the phone's boot (motorola_nio; see boot.rs)
//! - `sway`, `vhost-sound`: sway-mship with its wlroots, the VMs' sound backend (host/sway, host/vm-sound)
//! - `dist`: build every program for the phone and pack the release, target/dist/mothershipos-<release>.tar: a root
//!   filesystem overlay (the programs, dist/rootfs and the port's rootfs: systemd units, udev rules, configuration,
//!   the sway session). On the phone, `mship-system install <tar>` installs it.
//!
//! Nothing here builds a VM: each VM is a repository of its own with its own build, and mothershipOS installs it
//! on the phone (`mship-system vm-install`, docs/vm-packages.md).

use anyhow::{Result, bail};

mod boot;
mod gpu;
mod host;
mod util;
use util::*;

pub const TARGET: &str = "aarch64-unknown-linux-gnu";

/// (binary, where it goes on the phone)
const PROGRAMS: &[(&str, &str)] = &[
    ("mothershipd", "usr/local/sbin"),
    ("mship-system", "usr/local/sbin"),
    ("mship-battery", "usr/local/sbin"),
    ("mship-bt-bridge", "usr/local/sbin"),
    ("mship-camera", "usr/local/sbin"),
    ("mship-modem", "usr/local/sbin"),
    ("mship-usb", "usr/local/sbin"),
    ("mship-wifi-bridge", "usr/local/sbin"),
    ("mship", "usr/local/bin"),
    ("mship-shell", "usr/local/bin"),
    ("mship-launch", "usr/local/bin"),
    ("mship-presenter", "usr/local/bin"),
    ("mship-inputd", "usr/local/bin"),
];

/// gfxstream shims (cdylib crate, file name in Cuttlefish's crosvm directory)
const SHIMS: &[(&str, &str)] = &[("mship-egl-shim", "libEGL.so"), ("mship-gles-shim", "libGLESv2.so")];

/// crates that only link for the phone (GTK, GBM): checked there, not tested on the build host
const PHONE_ONLY: &[&str] = &["mship-shell", "mship-presenter"];

fn check() -> Result<()> {
    let mut host = vec!["clippy", "--workspace", "--all-targets"];
    for c in PHONE_ONLY {
        host.extend(["--exclude", c]);
    }
    host.extend(["--", "-D", "warnings"]);
    cargo(&host)?;
    let mut phone = vec!["clippy", "--target", TARGET];
    for c in PHONE_ONLY {
        phone.extend(["-p", c]);
    }
    phone.extend(["--", "-D", "warnings"]);
    cargo(&phone)?;
    let mut test = vec!["test", "--workspace"];
    for c in PHONE_ONLY {
        test.extend(["--exclude", c]);
    }
    cargo(&test)
}

fn dist() -> Result<()> {
    let release = std::fs::read_to_string(root().join("VERSION"))?.trim().to_string();
    let mut args = vec!["build", "--release", "--target", TARGET];
    for (p, _) in PROGRAMS {
        args.extend(["--bin", p]);
    }
    cargo(&args)?;
    let mut args = vec!["build", "--release", "--target", TARGET, "--lib"];
    for (c, _) in SHIMS {
        args.extend(["-p", c]);
    }
    cargo(&args)?;
    let bin = target_dir().join(TARGET).join("release");
    let out = target_dir().join("dist");
    std::fs::create_dir_all(&out)?;
    let path = out.join(format!("mothershipos-{release}.tar"));
    let mut tar = tar::Builder::new(std::fs::File::create(&path)?);
    let rootown = (0, "root");
    for (p, dir) in PROGRAMS {
        add_file(&mut tar, &bin.join(p), &format!("{dir}/{p}"), 0o755, rootown)?;
    }
    for (c, name) in SHIMS {
        add_file(&mut tar, &bin.join(format!("lib{}.so", c.replace('-', "_"))), &format!("opt/crosvm/bin/{name}"), 0o644, rootown)?;
    }
    // dist/rootfs, then the port's rootfs: the files as they go on the phone (under home/mship: the session user's)
    for overlay in [root().join("dist/rootfs"), port().join("rootfs")] {
        let mut files = vec![];
        walk(&overlay, &mut files)?;
        files.sort();
        for f in files {
            let rel = f.strip_prefix(&overlay)?.to_string_lossy().into_owned();
            let owner = if rel.starts_with("home/mship/") { (1000, "mship") } else { rootown };
            add_file(&mut tar, &f, &rel, 0o644, owner)?;
        }
    }
    // the dev link's telnet shell (mship-telnet.service) and mship-init's diagnostics in the root too
    add_file(&mut tar, &boot::busybox_binary()?, "usr/local/sbin/busybox", 0o755, rootown)?;
    let g = gpu::gpu()?;
    add_file(&mut tar, &g.crosvm, &format!("{}/bin/crosvm", gpu::PREFIX), 0o755, rootown)?;
    add_file(&mut tar, &g.virglrenderer, &format!("{}/lib/libvirglrenderer.so.1", gpu::PREFIX), 0o644, rootown)?;
    let sway = host::sway()?;
    add_file(&mut tar, &sway.wlroots, "usr/local/lib/mothership-wlroots/libwlroots-0.19.so", 0o644, rootown)?;
    add_file(&mut tar, &sway.sway, "usr/local/bin/sway-mship", 0o755, rootown)?;
    add_file(&mut tar, &host::vhost_sound()?, "usr/local/bin/vhost-device-sound", 0o755, rootown)?;
    let rel = out.join("release");
    std::fs::write(&rel, format!("{release}\n"))?;
    add_file(&mut tar, &rel, "usr/share/mothership/release", 0o644, rootown)?;
    tar.finish()?;
    println!("{}", path.display());
    Ok(())
}

fn main() -> Result<()> {
    let task = std::env::args().nth(1).unwrap_or_default();
    match task.as_str() {
        "check" => check(),
        "gpu" => gpu::gpu().map(|_| ()),
        "host-kernel" => boot::host_kernel().map(|_| ()),
        "initramfs" => boot::initramfs().map(|_| ()),
        "fit" => boot::fit().map(|_| ()),
        "abl-dtb" => boot::abl_dtb(&std::env::args().skip(2).collect::<Vec<_>>()).map(|_| ()),
        "sway" => host::sway().map(|_| ()),
        "vhost-sound" => host::vhost_sound().map(|_| ()),
        "dist" => dist(),
        "" | "help" => {
            println!(
                "cargo xtask <task>\n  check         clippy (warnings are errors) and the tests\n  gpu           the phone's VMM: crosvm, virglrenderer, minigbm\n  host-kernel   the phone's kernel and modules\n  initramfs     the phone's initramfs (mship-init, busybox, early firmware)\n  fit           the boot image for boot_b (kernel + DTB + initramfs)\n  abl-dtb [--diagnostic] [--dtbo <img>]  the ABL-path board DTB, checked\n  sway          sway-mship and its wlroots\n  vhost-sound   the VMs' sound backend\n  dist          build for the phone and pack the release tarball (target/dist)"
            );
            Ok(())
        }
        other => bail!("unknown task {other:?} (cargo xtask help)"),
    }
}
