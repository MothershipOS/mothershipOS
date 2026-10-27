//! Host programs built from upstream sources with our patches (pinned in each directory's source.json):
//!
//! - `sway` (host/sway): mothershipOS's wlroots (Ubuntu's 0.19.2 with wlroots-patches: a leased CRTC stays with its
//!   lessee, so the panel and the USB-C display can be leased to two VMs at once), in /usr/local/lib/mothership-wlroots,
//!   and sway-mship (sway 1.11 with sway-patches: the lease_output command), built against Ubuntu's wlroots headers
//!   (same features, checked) and run on ours (mship-sway.service.d/lease.conf)
//! - `vhost-sound` (host/vm-sound): vhost-device-sound, the VMs' virtio-snd backend (ALSA), with our patches

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::TARGET;
use crate::util::*;

fn source_json(dir: &str) -> Result<serde_json::Value> {
    Ok(serde_json::from_str(&std::fs::read_to_string(root().join(dir).join("source.json"))?)?)
}

/// A pinned tarball unpacked afresh (its top directory) with `patches` applied.
fn unpacked(s: &serde_json::Value, name: &str, work: &Path, patches_dir: &Path) -> Result<PathBuf> {
    let (version, url, sum) =
        (s["version"].as_str().context("version")?, s["url"].as_str().context("url")?, s["sha256"].as_str().context("sha256")?);
    let archive = fetch(url, sum, &work.join(format!("{name}-{version}.tar.gz")))?;
    let tree = work.join(format!("{name}-{version}"));
    let _ = std::fs::remove_dir_all(&tree);
    tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(&archive)?)).unpack(work)?;
    apply(&tree, &patches(patches_dir)?)?;
    Ok(tree)
}

pub struct Sway {
    pub wlroots: PathBuf,
    pub sway: PathBuf,
}

pub fn sway() -> Result<Sway> {
    let s = source_json("host/sway")?;
    let work = target_dir().join("sway");
    let cross = meson_cross(&work)?;
    // Xwayland is a feature of Ubuntu's wlroots; meson only needs its pkg-config file
    let pc = work.join("pkgconfig");
    std::fs::create_dir_all(&pc)?;
    std::fs::write(pc.join("xwayland.pc"), "prefix=/usr\nxwayland=/usr/bin/Xwayland\nhave_listenfd=true\nhave_terminate_delay=true\n\nName: Xwayland\nDescription: X server for Wayland\nVersion: 24.1.0\n")?;
    let env_pc = format!("{}:/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig", pc.display());
    let meson = |tree: &Path, opts: &[&str]| -> Result<()> {
        let st = Command::new("meson")
            .current_dir(tree)
            .args(["setup", "build", "--cross-file", &cross.to_string_lossy(), "--buildtype=release"])
            .args(opts)
            .env("PKG_CONFIG_PATH", &env_pc)
            .status()?;
        ensure!(st.success(), "{}: meson setup failed", tree.display());
        run_in(tree, &["ninja", "-C", "build"])
    };
    let wlr = unpacked(&s["wlroots"], "wlroots", &work, &root().join("host/sway/wlroots-patches"))?;
    meson(&wlr, &["-Dwerror=false", "-Dexamples=false", "-Dxwayland=enabled", "-Dbackends=drm,libinput,x11", "-Drenderers=gles2,vulkan",
                  "-Dallocators=gbm,udmabuf", "-Dsession=enabled", "-Dcolor-management=enabled"])?;
    // the ABI-relevant features must be Ubuntu's: sway-mship is built against Ubuntu's headers
    let ours = std::fs::read_to_string(wlr.join("build/include/wlr/config.h"))?;
    let ubuntu = std::fs::read_to_string("/usr/include/wlroots-0.19/wlr/config.h").context("Ubuntu's wlroots headers (libwlroots-0.19-dev:arm64)")?;
    ensure!(ours == ubuntu, "wlroots: our wlr/config.h differs from Ubuntu's: keep the features the same");
    let sway = unpacked(&s["sway"], "sway", &work, &root().join("host/sway/sway-patches"))?;
    meson(&sway, &["-Dman-pages=disabled", "-Dwerror=false"])?;
    Ok(Sway { wlroots: wlr.join("build/libwlroots-0.19.so"), sway: sway.join("build/sway/sway") })
}

pub fn vhost_sound() -> Result<PathBuf> {
    let s = source_json("host/vm-sound")?;
    let (name, version, sum) = (s["crate"].as_str().context("crate")?, s["version"].as_str().context("version")?, s["sha256"].as_str().context("sha256")?);
    let work = target_dir().join("vhost-sound");
    let archive = fetch(&format!("https://crates.io/api/v1/crates/{name}/{version}/download"), sum, &work.join(format!("{name}-{version}.crate")))?;
    let tree = work.join(format!("{name}-{version}"));
    let _ = std::fs::remove_dir_all(&tree);
    tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(&archive)?)).unpack(&work)?;
    apply(&tree, &patches(&root().join("host/vm-sound/patches"))?)?;
    let st = Command::new("cargo")
        .current_dir(&tree)
        .args(["build", "--release", "--locked", "--target", TARGET, "--no-default-features", "--features", "alsa-backend"])
        .env("CARGO_TARGET_DIR", work.join("target"))
        .status()?;
    ensure!(st.success(), "{name}: cargo build failed");
    Ok(work.join("target").join(TARGET).join("release").join(name))
}
