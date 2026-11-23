//! The phone's boot: the host kernel, its initramfs and the boot image (the port's: `MSHIP_PORT`, default
//! motorola_nio; the names come from its port.json, docs/ports.md). Paths below are the port's.
//!
//! - `host-kernel`: the pinned LTS source (kernel/sources.lock.json) with kernel/patches, the
//!   arm64 defconfig trimmed to Qualcomm plus the port's fragment (port.json kernel.fragment, every request checked),
//!   built in the toolchain of the running kernel (xtask/docker/host-kernel: Ubuntu 24.04, gcc 13.3, so existing
//!   modules stay valid): target/host-kernel/build/arch/arm64/boot/Image and the modules (target/host-kernel/modroot,
//!   zstd-compressed, with the EXTERNAL_MODULES in extra/)
//! - `initramfs`: /init = mship-init (static), busybox for the RAM host's shells, early firmware
//!   (boot/initramfs.json) and the kernel's modules: target/boot/initramfs.cpio.gz. The modules travel
//!   with the kernel in the FIT; mship-init mounts them on the root's /usr/lib/modules/<release> (nothing on the
//!   root disk to go stale)
//! - `boot-image`: the port's boot image (port.json boot.format):
//!   - `fit` (the nio: u-boot in hyp boots it): boot.its with boot.dts and the initramfs, e.g. target/boot/nio-el2.itb;
//!     written to the phone with `mship-system boot-image <itb>` (`fit` is the old name of this task)
//!   - `android` (the phone's own bootloader, e.g. `fastboot boot`): an Android boot image (header v0) with the gzip
//!     kernel, the upstream board DTB appended (boot.dtb), the initramfs and boot.cmdline
//! - `abl-dtb [--diagnostic] [--dtbo <stock dtbo image>]`: the nio's ABL-path board DTB (kernel/dts) compiled and
//!   checked against the captured board facts (port.json abl_dtb)

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use sha2::Digest;

use crate::TARGET;
use crate::util::*;

const KBUILD_IMAGE: &str = "mothership-host-kbuild:24.04";

fn host_source() -> Result<serde_json::Value> {
    let lock: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(port().join("kernel/sources.lock.json"))?)?;
    Ok(lock["linux"].clone())
}

fn sha256_hex(data: &[u8]) -> String {
    sha2::Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------- kernel configuration

/// `.config` lines -> one setting per symbol: Some(value) or None ("is not set")
fn config_entries(text: &str) -> Vec<(String, Option<String>)> {
    text.lines()
        .filter_map(|l| {
            if let Some(rest) = l.strip_prefix("# ").and_then(|r| r.strip_suffix(" is not set")) {
                rest.starts_with("CONFIG_").then(|| (rest.to_string(), None))
            } else if l.starts_with("CONFIG_") {
                l.split_once('=').map(|(k, v)| (k.to_string(), Some(v.to_string())))
            } else {
                None
            }
        })
        .collect()
}

/// Set (or unset) symbols in a .config, as scripts/config and merge_config.sh do: the line is replaced, or added.
fn config_set(text: &str, changes: &[(String, Option<String>)]) -> String {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    for (sym, val) in changes {
        let line = match val {
            Some(v) => format!("{sym}={v}"),
            None => format!("# {sym} is not set"),
        };
        let pos = lines.iter().position(|l| l.starts_with(&format!("{sym}=")) || *l == format!("# {sym} is not set"));
        match pos {
            Some(i) => lines[i] = line,
            None => lines.push(line),
        }
    }
    lines.join("\n") + "\n"
}

/// Every request of the fragment holds in the .config ("is not set" holds when the symbol has no value).
fn check_fragment(config: &str, fragment: &str) -> Result<()> {
    let actual: BTreeSet<&str> = config.lines().collect();
    let missing: Vec<String> = config_entries(fragment)
        .into_iter()
        .filter(|(sym, val)| match val {
            Some(v) => !actual.contains(format!("{sym}={v}").as_str()),
            None => actual.iter().any(|l| l.starts_with(&format!("{sym}="))),
        })
        .map(|(sym, val)| val.map_or(format!("# {sym} is not set"), |v| format!("{sym}={v}")))
        .collect();
    ensure!(missing.is_empty(), "unresolved configuration requests:\n{}", missing.join("\n"));
    Ok(())
}

pub struct Kernel {
    pub image: PathBuf,
    pub modroot: PathBuf,
    /// the in-tree board DTBs (arch/arm64/boot/dts: qcom/<board>.dtb)
    pub dtbs: PathBuf,
}

/// The external (not in-tree) modules built with the kernel, by name: each `<name>.c` (headers `<name>*.h`) in
/// motorola_nio/kernel/drivers. Only these: a file there is not built just because it exists (work in progress
/// lives in kernel/wip). Our own drivers belong in the kernel as patches (kernel/patches, Kconfig, config fragment).
const EXTERNAL_MODULES: &[&str] = &[];

/// The boot logo (patch 0036, CONFIG_LOGO_MOTHERSHIP_CLUT224): the port's boot/splash.png as the plain PPM the
/// kernel's pnmtologo takes (at most 224 colors; the background black, as the panel around it). Written only when it
/// changed (no rebuild otherwise).
fn boot_logo(png_path: &Path, ppm: &Path) -> Result<()> {
    let mut dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(png_path).with_context(|| png_path.display().to_string())?));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = dec.read_info()?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf)?;
    let ch = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        t => bail!("{}: color type {t:?} (RGB or indexed expected)", png_path.display()),
    };
    let (w, h) = (info.width as usize, info.height as usize);
    let mut colors = BTreeSet::new();
    let mut out = format!("P3\n# mothershipOS boot logo, from boot/splash.png\n{w} {h}\n255\n");
    for y in 0..h {
        for x in 0..w {
            let p = &buf[(y * w + x) * ch..][..3];
            colors.insert((p[0], p[1], p[2]));
            out.push_str(&format!("{} {} {}\n", p[0], p[1], p[2]));
        }
    }
    ensure!(colors.len() <= 224, "{}: {} colors (the kernel logo takes 224)", png_path.display(), colors.len());
    if std::fs::read_to_string(ppm).ok().as_deref() != Some(out.as_str()) {
        std::fs::write(ppm, out)?;
    }
    Ok(())
}

pub fn host_kernel() -> Result<Kernel> {
    let work = port_target().join("host-kernel");
    std::fs::create_dir_all(&work)?;
    let build_host = format!("KBUILD_BUILD_HOST={}", port_str("kernel/build_host").unwrap_or_else(|_| "mothership-build".into()));
    let tree = kernel_tree(&work, &host_source()?, &port().join("kernel/patches"))?;
    boot_logo(&port().join("boot/splash.png"), &tree.join("drivers/video/logo/logo_mothership_clut224.ppm"))?;
    docker(&["build", "-q", "-t", KBUILD_IMAGE, &root().join("xtask/docker/host-kernel").to_string_lossy()])?;
    let user = format!("{}:{}", unsafe { libc::getuid() }, unsafe { libc::getgid() });
    let vol = format!("{}:/k", host_path(&work)?);
    let tree_name = tree.file_name().unwrap().to_string_lossy().into_owned();
    let kmake = |args: &[&str]| -> Result<()> {
        let mut a: Vec<String> = ["run", "--rm", "-u", &user, "-v", &vol, "-w", "/k"].iter().map(|s| s.to_string()).collect();
        for e in ["KBUILD_BUILD_USER=mothership", build_host.as_str(), "KBUILD_BUILD_TIMESTAMP=2026-09-27 00:00:00 UTC", "KBUILD_BUILD_VERSION=1"] {
            a.extend(["-e".into(), e.into()]);
        }
        a.extend([KBUILD_IMAGE.into(), "make".into(), "-s".into(), "-C".into(), format!("/k/{tree_name}"), "O=/k/build".into(),
                  "ARCH=arm64".into(), "CROSS_COMPILE=aarch64-linux-gnu-".into(), "HOSTCC=gcc".into()]);
        a.extend(args.iter().map(|s| s.to_string()));
        docker(&a.iter().map(String::as_str).collect::<Vec<_>>())
    };
    let out = work.join("build");
    std::fs::create_dir_all(&out)?;
    // configure: arm64 defconfig, other SoC families off (generic arm64 kept), no debug info, the port's fragment
    kmake(&["defconfig"])?;
    let platforms = std::fs::read_to_string(tree.join("arch/arm64/Kconfig.platforms"))?;
    let mut changes: Vec<(String, Option<String>)> = platforms
        .lines()
        .filter_map(|l| l.strip_prefix("config ARCH_"))
        .filter(|s| *s != "QCOM" && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .map(|s| (format!("CONFIG_ARCH_{s}"), None))
        .collect();
    changes.extend([("CONFIG_DEBUG_INFO".into(), None), ("CONFIG_DEBUG_INFO_BTF".into(), None), ("CONFIG_DEBUG_INFO_NONE".into(), Some("y".into()))]);
    let fragment_path = port().join(port_str("kernel/fragment")?);
    let fragment = std::fs::read_to_string(&fragment_path).with_context(|| fragment_path.display().to_string())?;
    changes.extend(config_entries(&fragment));
    let config = out.join(".config");
    std::fs::write(&config, config_set(&std::fs::read_to_string(&config)?, &changes))?;
    kmake(&["olddefconfig"])?;
    check_fragment(&std::fs::read_to_string(&config)?, &fragment)?;
    kmake(&["savedefconfig"])?;
    let committed = port().join(port_str("kernel/defconfig")?);
    if std::fs::read(out.join("defconfig")).ok() != std::fs::read(&committed).ok() {
        println!("note: the resolved defconfig ({}) differs from {}", out.join("defconfig").display(), committed.display());
    }
    // build
    kmake(&["-j", &jobs(), "Image", "modules", "dtbs"])?;
    let modroot = work.join("modroot");
    let _ = std::fs::remove_dir_all(&modroot);
    kmake(&["INSTALL_MOD_PATH=/k/modroot", "INSTALL_MOD_STRIP=1", "modules_install"])?;
    // the external modules (EXTERNAL_MODULES, from motorola_nio/kernel/drivers), one module each, into extra/
    let ext = work.join("ext");
    let _ = std::fs::remove_dir_all(&ext);
    if !EXTERNAL_MODULES.is_empty() {
        std::fs::create_dir_all(&ext)?;
        let mut kbuild = String::new();
        let drivers = port().join("kernel/drivers");
        for m in EXTERNAL_MODULES {
            for e in std::fs::read_dir(&drivers)?.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(m) && (name.ends_with(".c") || name.ends_with(".h")) {
                    std::fs::copy(e.path(), ext.join(&name))?;
                }
            }
            ensure!(ext.join(format!("{m}.c")).exists(), "{m}.c: not in {}", drivers.display());
            kbuild += &format!("obj-m += {m}.o\n");
    }
    std::fs::write(ext.join("Kbuild"), kbuild)?;
    kmake(&["M=/k/ext", "-j", &jobs(), "modules"])?;
    kmake(&["M=/k/ext", "INSTALL_MOD_PATH=/k/modroot", "INSTALL_MOD_DIR=extra", "INSTALL_MOD_STRIP=1", "modules_install"])?;
    }
    let image = out.join("arch/arm64/boot/Image");
    let manifest = serde_json::json!({
        "linux": host_source()?,
        "patches": patches(&port().join("kernel/patches"))?.iter()
            .map(|p| Ok(serde_json::json!({"path": p.strip_prefix(root().parent().unwrap())?.to_string_lossy(), "sha256": sha256(p)?})))
            .collect::<Result<Vec<_>>>()?,
        "image_sha256": sha256(&image)?,
        "config_sha256": sha256(&config)?,
        "fragment_sha256": sha256_hex(fragment.as_bytes()),
    });
    std::fs::write(out.join("build-manifest.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    println!("{}\n{}", image.display(), modroot.display());
    Ok(Kernel { image, modroot, dtbs: out.join("arch/arm64/boot/dts") })
}

// ---------------------------------------------------------------- initramfs

/// A newc cpio archive.
struct Cpio {
    out: Vec<u8>,
    ino: u32,
}

impl Cpio {
    fn new() -> Self {
        Cpio { out: vec![], ino: 1 }
    }

    fn pad(&mut self) {
        while !self.out.len().is_multiple_of(4) {
            self.out.push(0);
        }
    }

    fn entry(&mut self, name: &str, mode: u32, rdev: (u32, u32), data: &[u8]) {
        let fields = [self.ino, mode, 0, 0, 1, 0, data.len() as u32, 0, 0, rdev.0, rdev.1, name.len() as u32 + 1, 0];
        self.ino += 1;
        self.out.extend(b"070701");
        for f in fields {
            self.out.extend(format!("{f:08x}").as_bytes());
        }
        self.out.extend(name.as_bytes());
        self.out.push(0);
        self.pad();
        self.out.extend(data);
        self.pad();
    }

    fn dir(&mut self, name: &str) {
        self.entry(name, 0o040755, (0, 0), &[]);
    }

    fn file(&mut self, name: &str, mode: u32, data: &[u8]) {
        self.entry(name, 0o100000 | mode, (0, 0), data);
    }

    fn finish(mut self) -> Vec<u8> {
        self.entry("TRAILER!!!", 0, (0, 0), &[]);
        self.out
    }
}

/// mship-init, linked static (it is /init: no libc in the initramfs)
fn mship_init() -> Result<PathBuf> {
    let dir = target_dir().join("init");
    let st = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(root())
        .args(["build", "--release", "--target", TARGET, "-p", "mship-init"])
        .env("CARGO_TARGET_DIR", &dir)
        .env("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS", "-C target-feature=+crt-static")
        .status()?;
    ensure!(st.success(), "mship-init: cargo build failed");
    Ok(dir.join(TARGET).join("release/mship-init"))
}

/// The pinned busybox (Ubuntu's busybox-static)
fn busybox(manifest: &serde_json::Value, work: &Path) -> Result<Vec<u8>> {
    let b = &manifest["busybox"];
    let deb = fetch(b["url"].as_str().context("busybox url")?, b["sha256"].as_str().context("busybox sha256")?, &work.join("busybox-static.deb"))?;
    deb_files(&deb)?.into_iter().find(|(p, _, _)| p == "bin/busybox" || p == "usr/bin/busybox").map(|(_, d, _)| d).context("busybox-static: no busybox")
}


const BD_PLACEHOLDER: &str = "local-bd-address = [00 00 00 00 00 00];";

/// The DT with the phone's own Bluetooth address (the port's boot/bd-address, "00:11:22:33:44:55", kept out of git:
/// it identifies the phone) in place of the placeholder; without the file Bluetooth comes up without an address.
fn with_bd_address(dts: &str) -> Result<String> {
    ensure!(dts.contains(BD_PLACEHOLDER), "the board DTS: no `{BD_PLACEHOLDER}`");
    let file = port().join("boot/bd-address");
    let Ok(addr) = std::fs::read_to_string(&file) else {
        println!("note: no {} (the phone's Bluetooth address): Bluetooth gets none", file.display());
        return Ok(dts.to_string());
    };
    let bytes: Vec<&str> = addr.trim().split(':').collect();
    ensure!(bytes.len() == 6 && bytes.iter().all(|b| b.len() == 2 && u8::from_str_radix(b, 16).is_ok()),
            "{}: not an address like 00:11:22:33:44:55", file.display());
    // the DT property is LSB first
    let lsb: Vec<String> = bytes.iter().rev().map(|b| b.to_lowercase()).collect();
    Ok(dts.replace(BD_PLACEHOLDER, &format!("local-bd-address = [{}];", lsb.join(" "))))
}

/// The pinned busybox binary, as a file (the release installs it for mship-telnet.service).
pub fn busybox_binary() -> Result<PathBuf> {
    let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(port().join("boot/initramfs.json"))?)?;
    let work = port_target().join("boot");
    std::fs::create_dir_all(&work)?;
    let out = work.join("busybox");
    std::fs::write(&out, busybox(&manifest, &work)?)?;
    Ok(out)
}

/// The initramfs: mship-init, busybox, early firmware and the kernel's modules (`modroot`, from host_kernel: the
/// same build as the kernel in the FIT, so the two always match; mship-init mounts them on the root's
/// /lib/modules/<release>).
pub fn initramfs(modroot: &Path) -> Result<PathBuf> {
    let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(port().join("boot/initramfs.json"))?)?;
    let work = port_target().join("boot");
    std::fs::create_dir_all(&work)?;
    let mut c = Cpio::new();
    for d in ["bin", "dev", "proc", "sys", "tmp", "usr", "usr/bin"] {
        c.dir(d);
    }
    // the console from the first instruction of /init (devtmpfs comes later)
    c.entry("dev/console", 0o020600, (5, 1), &[]);
    c.file("init", 0o755, &std::fs::read(mship_init()?)?);
    c.file("bin/busybox", 0o755, &busybox(&manifest, &work)?);
    c.entry("bin/sh", 0o120777, (0, 0), b"busybox");
    let mut dirs = BTreeSet::new();
    for f in manifest["firmware"].as_array().context("firmware")? {
        let path = f["path"].as_str().context("firmware path")?;
        let sum = f["sha256"].as_str().context("firmware sha256")?;
        let data = match (f["url"].as_str(), f["local"].as_str()) {
            (Some(url), _) => std::fs::read(fetch(url, sum, &work.join("firmware").join(path))?)?,
            (None, Some(local)) => {
                // initramfs.json `local` paths are repo-prefixed (e.g. motorola_nio/firmware/…),
                // relative to the directory that holds mothershipOS and the port side by side
                let p = root().parent().unwrap().join(local);
                let d = std::fs::read(&p).with_context(|| format!("{}: the phone's firmware (the port's firmware/, not in git)", p.display()))?;
                ensure!(sha256_hex(&d) == sum, "{}: SHA-256 differs from initramfs.json", p.display());
                d
            }
            _ => bail!("{path}: neither url nor local"),
        };
        let mut parent = Path::new(path).parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
            dirs.insert(p.to_path_buf());
            parent = p.parent();
        }
        for d in std::mem::take(&mut dirs) {
            c.dir(&d.to_string_lossy());
        }
        c.file(path, 0o644, &data);
    }
    modules(&mut c, modroot)?;
    let out = work.join("initramfs.cpio.gz");
    let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(&out)?, flate2::Compression::best());
    gz.write_all(&c.finish())?;
    gz.finish()?;
    println!("{}", out.display());
    Ok(out)
}

/// lib/modules/<release> from a modules_install tree (files and directories; the `build` link is not shipped).
fn modules(c: &mut Cpio, modroot: &Path) -> Result<()> {
    let base = modroot.join("lib/modules");
    let mut releases = std::fs::read_dir(&base)?.flatten().map(|e| e.path()).collect::<Vec<_>>();
    ensure!(releases.len() == 1, "{}: one kernel release expected, found {}", base.display(), releases.len());
    let release = releases.pop().unwrap();
    fn walk(c: &mut Cpio, root: &Path, dir: &Path) -> Result<()> {
        let mut entries = std::fs::read_dir(dir)?.flatten().collect::<Vec<_>>();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let name = p.strip_prefix(root)?.to_string_lossy().into_owned();
            let t = e.file_type()?;
            if t.is_dir() {
                c.dir(&name);
                walk(c, root, &p)?;
            } else if t.is_file() {
                c.file(&name, 0o644, &std::fs::read(&p)?);
            }
        }
        Ok(())
    }
    for d in ["lib", "lib/modules"] {
        c.dir(d);
    }
    c.dir(&release.strip_prefix(modroot)?.to_string_lossy());
    walk(c, modroot, &release)
}

// ---------------------------------------------------------------- FIT

/// The port's boot image (port.json boot.format).
pub fn boot_image() -> Result<PathBuf> {
    match port_str("boot/format")?.as_str() {
        "fit" => fit(),
        "android" => android_boot_image(),
        f => bail!("port.json boot.format {f:?}: fit or android"),
    }
}

fn check_size(out: &Path) -> Result<()> {
    let size = std::fs::metadata(out)?.len();
    let max = port_u64("boot/max_size")?;
    ensure!(size <= max, "{}: {size} B exceeds the boot partition ({max} B, port.json boot.max_size)", out.display());
    println!("{} {size} B sha256={}", out.display(), &sha256(out)?[..16]);
    Ok(())
}

/// A FIT for u-boot (the nio's: boot_b, booted by u-boot in hyp_b).
pub fn fit() -> Result<PathBuf> {
    ensure!(port_str("boot/format")? == "fit", "{}: its boot image is not a FIT (cargo xtask boot-image)", port_name());
    let kernel = host_kernel()?;
    println!("modules for this kernel: {}", kernel.modroot.display());
    let initramfs = initramfs(&kernel.modroot)?;
    let work = port_target().join("boot");
    let stage = work.join("fit");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    // the names the .its includes: Image, initramfs.cpio.gz and boot.dtb's name
    std::fs::copy(&kernel.image, stage.join("Image"))?;
    std::fs::copy(&initramfs, stage.join("initramfs.cpio.gz"))?;
    // the board DT (port.json boot.dts; the nio's: the display, audio, USB-C and video additions on top of
    // nio-el2.dts), with this phone's Bluetooth address in it (its /include/s still from the port's boot/)
    let dts_rel = port_str("boot/dts")?;
    let dts_name = Path::new(&dts_rel).file_name().context("boot.dts")?.to_string_lossy().into_owned();
    let dts = with_bd_address(&std::fs::read_to_string(port().join(&dts_rel))?)?;
    std::fs::write(stage.join(&dts_name), dts)?;
    run_in(&stage, &["dtc", "-q", "-i", &port().join("boot").to_string_lossy(), "-I", "dts", "-O", "dtb", "-o", &port_str("boot/dtb")?, &dts_name])?;
    let out = work.join(port_str("boot/image")?);
    // dtc resolves /incbin/ next to the .its, so it goes beside the files
    let its_rel = port_str("boot/its")?;
    let its_name = Path::new(&its_rel).file_name().context("boot.its")?.to_string_lossy().into_owned();
    std::fs::copy(port().join(&its_rel), stage.join(&its_name))?;
    run_in(&stage, &["mkimage", "-q", "-f", &its_name, &out.to_string_lossy()])?;
    check_size(&out)?;
    Ok(out)
}

/// An Android boot image, header version 0 (what mkbootimg writes): the bootloader (ABL) loads the kernel, the
/// ramdisk and the tags (the DTB, here appended to the gzip kernel as ABL takes it on the phones of that time) at
/// base + offset and boots the kernel at EL1 with the image's command line plus its own androidboot.* settings.
pub fn android_boot_image() -> Result<PathBuf> {
    ensure!(port_u64("boot/header_version")? == 0, "port.json boot.header_version: only 0 is built");
    let kernel = host_kernel()?;
    let initramfs = std::fs::read(initramfs(&kernel.modroot)?)?;
    let dtb_rel = port_str("boot/dtb")?;
    let dtb = std::fs::read(kernel.dtbs.join(&dtb_rel)).with_context(|| format!("{dtb_rel}: not built by the kernel (port.json boot.dtb)"))?;
    // Image.gz-dtb
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&std::fs::read(&kernel.image)?)?;
    let mut kimg = gz.finish()?;
    kimg.extend(&dtb);
    let cmdline = port_str("boot/cmdline")?;
    let img = bootimg_v0(&kimg, &initramfs, &BootImgLayout {
        base: port_u64("boot/base")? as u32,
        kernel_offset: port_u64("boot/kernel_offset")? as u32,
        ramdisk_offset: port_u64("boot/ramdisk_offset")? as u32,
        tags_offset: port_u64("boot/tags_offset")? as u32,
        page_size: port_u64("boot/page_size")? as u32,
        cmdline: &cmdline,
    })?;
    let out = port_target().join("boot").join(port_str("boot/image")?);
    std::fs::write(&out, img)?;
    check_size(&out)?;
    Ok(out)
}

pub struct BootImgLayout<'a> {
    pub base: u32,
    pub kernel_offset: u32,
    pub ramdisk_offset: u32,
    pub tags_offset: u32,
    pub page_size: u32,
    pub cmdline: &'a str,
}

/// boot_img_hdr_v0 (AOSP system/tools/mkbootimg) + kernel + ramdisk, each padded to the page size; no second stage.
pub fn bootimg_v0(kernel: &[u8], ramdisk: &[u8], l: &BootImgLayout) -> Result<Vec<u8>> {
    use sha1::Digest as _;
    ensure!(l.cmdline.len() < 512 + 1024, "boot command line longer than 1535 bytes");
    let mut h = Vec::with_capacity(l.page_size as usize);
    let u32le = |h: &mut Vec<u8>, v: u32| h.extend(v.to_le_bytes());
    h.extend(b"ANDROID!");
    for v in [kernel.len() as u32, l.base + l.kernel_offset, ramdisk.len() as u32, l.base + l.ramdisk_offset, 0, 0, l.base + l.tags_offset,
              l.page_size, 0, 0] {
        u32le(&mut h, v);
    }
    h.extend([0u8; 16]); // name
    let cmd = l.cmdline.as_bytes();
    let (main, extra) = cmd.split_at(cmd.len().min(511));
    let mut field = [0u8; 512];
    field[..main.len()].copy_from_slice(main);
    h.extend(field);
    // id: SHA-1 over each part and its size (mkbootimg); the bootloader may compare it
    let mut sha = sha1::Sha1::new();
    for part in [kernel, ramdisk, &[]] {
        sha.update(part);
        sha.update((part.len() as u32).to_le_bytes());
    }
    let mut id = [0u8; 32];
    id[..20].copy_from_slice(&sha.finalize());
    h.extend(id);
    let mut field = [0u8; 1024];
    field[..extra.len()].copy_from_slice(extra);
    h.extend(field);
    let page = l.page_size as usize;
    let pad = |v: &mut Vec<u8>| v.resize(v.len().div_ceil(page) * page, 0);
    pad(&mut h);
    let mut img = h;
    img.extend(kernel);
    pad(&mut img);
    img.extend(ramdisk);
    pad(&mut img);
    Ok(img)
}

// ---------------------------------------------------------------- the port's root firmware

/// The firmware the port pins for the root filesystem (port.json `firmware`: a manifest of {path, url, sha256}),
/// fetched once and checked: (path in the root, local file). Ports without one: none (the nio's firmware is placed
/// on the phone by hand, motorola_nio/firmware).
pub fn port_firmware() -> Result<Vec<(String, PathBuf)>> {
    let Ok(rel) = port_str("firmware") else { return Ok(vec![]) };
    let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(port().join(&rel))?)?;
    let dir = port_target().join("firmware");
    let mut out = vec![];
    for f in manifest["files"].as_array().context("firmware manifest: files")? {
        let path = f["path"].as_str().context("firmware path")?;
        ensure!(path.starts_with("lib/firmware/") && !path.contains(".."), "{path}: not under lib/firmware/");
        let local = fetch(f["url"].as_str().context("firmware url")?, f["sha256"].as_str().context("firmware sha256")?, &dir.join(path))?;
        out.push((path.to_string(), local));
    }
    Ok(out)
}

// ---------------------------------------------------------------- the ABL-path board DTB

fn dtb_reg(node: &fdt::node::FdtNode) -> Option<Vec<u32>> {
    let v = node.property("reg")?.value;
    Some(v.chunks(4).map(|c| u32::from_be_bytes(c.try_into().unwrap_or([0; 4]))).collect())
}

fn cells(fdt: &fdt::Fdt, path: &str, prop: &str) -> Result<Vec<u32>> {
    let n = fdt.find_node(path).with_context(|| format!("{path}: no node"))?;
    let v = n.property(prop).with_context(|| format!("{path}: no {prop}"))?.value;
    Ok(v.chunks(4).map(|c| u32::from_be_bytes(c.try_into().unwrap_or([0; 4]))).collect())
}

fn string(fdt: &fdt::Fdt, path: &str, prop: &str) -> Result<String> {
    let n = fdt.find_node(path).with_context(|| format!("{path}: no node"))?;
    n.property(prop).and_then(|p| p.as_str()).map(String::from).with_context(|| format!("{path}: no {prop}"))
}

/// The DTS of a DTB (dtc), without the subtrees and root properties an overlay may change.
fn real_nodes(dtb: &Path) -> Result<Vec<String>> {
    let o = Command::new("dtc").args(["-q", "-I", "dtb", "-O", "dts"]).arg(dtb).output()?;
    ensure!(o.status.success(), "dtc {}", dtb.display());
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let mut out = vec![];
    let mut skip = false;
    for l in text.lines() {
        if l == "\tabl-overlay-sink {" || l == "\t__symbols__ {" {
            skip = true;
            continue;
        }
        if skip {
            skip = l != "\t};";
            continue;
        }
        let root_prop = l.starts_with('\t') && !l.starts_with("\t\t") && l.ends_with(';') && !l.ends_with("};") && !l.contains('{');
        if !root_prop {
            out.push(l.to_string());
        }
    }
    Ok(out)
}

pub fn abl_dtb(args: &[String]) -> Result<PathBuf> {
    let diagnostic = args.iter().any(|a| a == "--diagnostic");
    let dtbo = args.iter().position(|a| a == "--dtbo").and_then(|i| args.get(i + 1)).map(PathBuf::from);
    let work = port_target().join("host-kernel");
    let tree = kernel_tree(&work, &host_source()?, &port().join("kernel/patches"))?;
    let base = port_str("abl_dtb/variant").context("this port has no ABL-path board DTB")?;
    let variant = if diagnostic { format!("{base}-diagnostic") } else { base.clone() };
    let out = port_target().join(if diagnostic { "abl-dtb-diagnostic" } else { "abl-dtb" });
    std::fs::create_dir_all(&out)?;
    let board = port().join("kernel/dts").join(format!("{variant}.dts"));
    let pp = out.join(format!("{variant}.dts.pp"));
    let dtb = out.join(format!("{variant}.dtb"));
    let o = Command::new("gcc")
        .args(["-E", "-nostdinc", "-undef", "-D__DTS__", "-x", "assembler-with-cpp", "-I"])
        .arg(tree.join("scripts/dtc/include-prefixes"))
        .arg("-I")
        .arg(tree.join("arch/arm64/boot/dts/qcom"))
        .arg(&board)
        .output()?;
    ensure!(o.status.success(), "preprocessing {}: {}", board.display(), String::from_utf8_lossy(&o.stderr));
    std::fs::write(&pp, &o.stdout)?;
    let o = Command::new("dtc").args(["-I", "dts", "-O", "dtb", "-o"]).arg(&dtb).arg(&pp).output()?;
    std::fs::write(out.join("dtc.log"), &o.stderr)?;
    ensure!(o.status.success(), "dtc {}", board.display());

    let bytes = std::fs::read(&dtb)?;
    let fdt = fdt::Fdt::new(&bytes).map_err(|e| anyhow::anyhow!("{}: {e:?}", dtb.display()))?;
    let evidence: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(port().join(port_str("abl_dtb/evidence")?))?)?;
    // fixed reservations: exactly the captured ones, no-map, not overlapping
    let rm = fdt.find_node("/reserved-memory").context("no /reserved-memory")?;
    let mut actual = vec![];
    for n in rm.children() {
        let Some(reg) = dtb_reg(&n) else { continue };
        ensure!(reg.len() == 4, "{}: expected one 64-bit address/size pair", n.name);
        ensure!(n.property("no-map").is_some(), "fixed reservation without no-map: {}", n.name);
        actual.push((reg, n.name.to_string()));
    }
    let expected: BTreeSet<Vec<u32>> = evidence["fixed_reserved_memory"]
        .as_array()
        .context("fixed_reserved_memory")?
        .iter()
        .map(|r| r["cells"].as_array().unwrap_or(&vec![]).iter().filter_map(|c| c.as_u64().map(|c| c as u32)).collect())
        .collect();
    let have: BTreeSet<Vec<u32>> = actual.iter().map(|(r, _)| r.clone()).collect();
    ensure!(have == expected && actual.len() == expected.len(), "fixed memory reservations differ from the captured board facts");
    let mut intervals: Vec<(u64, u64, &str)> =
        actual.iter().map(|(r, n)| (((r[0] as u64) << 32) | r[1] as u64, ((r[2] as u64) << 32) | r[3] as u64, n.as_str())).collect();
    intervals.sort();
    for (i, (start, size, node)) in intervals.iter().enumerate() {
        ensure!(*size != 0 && (i == 0 || intervals[i - 1].0 + intervals[i - 1].1 <= *start), "empty or overlapping fixed reservation: {node}");
    }
    for (prop, key) in [("qcom,msm-id", "msm_id"), ("qcom,board-id", "abl_base_board_id")] {
        let want: Vec<u32> = evidence[key].as_array().context(key)?.iter().filter_map(|v| v.as_u64().map(|v| v as u32)).collect();
        ensure!(cells(&fdt, "/", prop)? == want, "board selection IDs changed: {prop}");
    }
    // the bootloader's own demands, from the board facts (the nio's ABL: the model must name Qualcomm, bootargs present)
    if let Some(m) = evidence["abl_model_must_contain"].as_str() {
        ensure!(string(&fdt, "/", "model")?.contains(m), "the bootloader rejects a base DTB whose model lacks {m:?}");
    }
    if evidence["abl_requires_bootargs"].as_bool() == Some(true) {
        ensure!(!string(&fdt, "/chosen", "bootargs")?.is_empty(), "the bootloader asserts when /chosen/bootargs is missing");
    }
    let uart = string(&fdt, "/aliases", "serial0")?;
    let console = evidence["enabled_console_address"].as_u64().context("enabled_console_address")? as u32;
    ensure!(cells(&fdt, &uart, "reg")?.get(1) == Some(&console) && string(&fdt, &uart, "status")? == "okay", "console does not match the enabled downstream UART");
    if diagnostic {
        let path = "/reserved-memory/memory@af000000";
        ensure!(string(&fdt, path, "compatible")? == "ramoops", "diagnostic ramoops missing");
        ensure!(string(&fdt, "/chosen", "bootargs")?.contains("rdinit=/init"), "diagnostic bootargs missing rdinit=/init");
        for (prop, v) in [("console-size", 0x40000), ("pmsg-size", 0x40000), ("record-size", 0x3f800)] {
            ensure!(cells(&fdt, path, prop)? == [v], "ramoops layout changed: {prop}");
        }
        // the stock dtbo, applied as ABL does, may only change the sink subtree, root properties and symbols
        if let Some(dtbo) = &dtbo {
            let data = std::fs::read(dtbo)?;
            let be = |o: usize| u32::from_be_bytes(data[o..o + 4].try_into().unwrap()) as usize;
            ensure!(be(0) == 0xd7b7ab1e, "{}: not an Android dtbo image", dtbo.display());
            let (entry_size, count, entries) = (be(12), be(16), be(20));
            let before = real_nodes(&dtb)?;
            for i in 0..count {
                let (size, offset) = (be(entries + i * entry_size), be(entries + i * entry_size + 4));
                let overlay = out.join(format!("stock-dtbo-{i}.dtbo"));
                let merged = out.join(format!("stock-dtbo-{i}-applied.dtb"));
                std::fs::write(&overlay, &data[offset..offset + size])?;
                let st = Command::new("fdtoverlay").arg("-i").arg(&dtb).arg("-o").arg(&merged).arg(&overlay).status()?;
                ensure!(st.success(), "fdtoverlay entry {i}");
                ensure!(real_nodes(&merged)? == before, "stock dtbo entry {i} modifies real nodes");
            }
        }
    }
    let manifest = serde_json::json!({
        "artifact": dtb.to_string_lossy(), "linux": host_source()?["version"],
        "sha256": sha256(&dtb)?, "dts_sha256": sha256(&board)?,
        "fixed_reservations_checked": actual.len(), "stock_dtbo_overlay_checked": diagnostic && dtbo.is_some(),
    });
    std::fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(dtb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bd_address_goes_in_lsb_first() {
        let addr = "00:11:22:33:44:AB";
        let lsb: Vec<String> = addr.split(':').rev().map(|b| b.to_lowercase()).collect();
        assert_eq!(format!("local-bd-address = [{}];", lsb.join(" ")), "local-bd-address = [ab 44 33 22 11 00];");
    }

    #[test]
    fn config_merge_and_check() {
        let cfg = "CONFIG_A=y\n# CONFIG_B is not set\nCONFIG_C=m\n";
        let frag = "CONFIG_B=y\n# CONFIG_C is not set\nCONFIG_D=\"x\"\n";
        let merged = config_set(cfg, &config_entries(frag));
        assert_eq!(merged, "CONFIG_A=y\nCONFIG_B=y\n# CONFIG_C is not set\nCONFIG_D=\"x\"\n");
        check_fragment(&merged, frag).unwrap();
        assert!(check_fragment(cfg, frag).is_err());
    }

    #[test]
    fn bootimg_header_v0() {
        let l = BootImgLayout { base: 0, kernel_offset: 0x8000, ramdisk_offset: 0x100_0000, tags_offset: 0x100, page_size: 4096, cmdline: "console=tty0" };
        let img = bootimg_v0(&[1; 5000], &[2; 10], &l).unwrap();
        assert_eq!(&img[..8], b"ANDROID!");
        let u = |o: usize| u32::from_le_bytes(img[o..o + 4].try_into().unwrap());
        assert_eq!((u(8), u(12), u(16), u(20), u(32), u(36), u(40)), (5000, 0x8000, 10, 0x100_0000, 0x100, 4096, 0));
        assert_eq!(&img[64..76], b"console=tty0");
        // header page, two kernel pages, one ramdisk page
        assert_eq!(img.len(), 4 * 4096);
        assert_eq!(img[4096], 1);
        assert_eq!(img[3 * 4096], 2);
    }

    #[test]
    fn cpio_newc() {
        let mut c = Cpio::new();
        c.file("init", 0o755, b"abc");
        let out = c.finish();
        assert!(out.starts_with(b"070701"));
        assert_eq!(out.len() % 4, 0);
        assert!(out.windows(10).any(|w| w == b"TRAILER!!!"));
    }
}
