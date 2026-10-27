//! The phone's boot: the host kernel, its initramfs and the FIT u-boot boots (motorola_nio).
//!
//! - `host-kernel`: the pinned LTS source (motorola_nio/kernel/sources.lock.json) with motorola_nio/kernel/patches, the
//!   arm64 defconfig trimmed to Qualcomm plus the nio fragment (configs/nio-host.config, every request checked),
//!   built in the toolchain of the running kernel (xtask/docker/host-kernel: Ubuntu 24.04, gcc 13.3, so existing
//!   modules stay valid): target/host-kernel/build/arch/arm64/boot/Image and the modules (target/host-kernel/modroot,
//!   with motorola_nio/kernel/drivers in extra/)
//! - `initramfs`: /init = mship-init (static), busybox for the RAM host's shells, early firmware
//!   (motorola_nio/boot/initramfs.json): target/boot/initramfs.cpio.gz
//! - `fit`: the FIT for boot_b (motorola_nio/boot/nio-el2.its: host kernel + nio-el2-display.dts + initramfs),
//!   target/boot/nio-el2.itb; written to the phone with `mship-system boot-image <itb>`
//! - `abl-dtb [--diagnostic] [--dtbo <stock dtbo image>]`: the ABL-path board DTB (motorola_nio/kernel/dts) compiled and
//!   checked against the captured board facts (motorola_nio/kernel/evidence/nio-board.json)

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use sha2::Digest;

use crate::TARGET;
use crate::util::*;

const KBUILD_IMAGE: &str = "mothership-host-kbuild:24.04";
/// boot_b: 24576 blocks of 4 KiB
const BOOT_B_SIZE: u64 = 24576 * 4096;

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
}

pub fn host_kernel() -> Result<Kernel> {
    let work = target_dir().join("host-kernel");
    std::fs::create_dir_all(&work)?;
    let tree = kernel_tree(&work, &host_source()?, &port().join("kernel/patches"))?;
    docker(&["build", "-q", "-t", KBUILD_IMAGE, &root().join("xtask/docker/host-kernel").to_string_lossy()])?;
    let user = format!("{}:{}", unsafe { libc::getuid() }, unsafe { libc::getgid() });
    let vol = format!("{}:/k", host_path(&work)?);
    let tree_name = tree.file_name().unwrap().to_string_lossy().into_owned();
    let kmake = |args: &[&str]| -> Result<()> {
        let mut a: Vec<String> = ["run", "--rm", "-u", &user, "-v", &vol, "-w", "/k"].iter().map(|s| s.to_string()).collect();
        for e in ["KBUILD_BUILD_USER=mothership", "KBUILD_BUILD_HOST=nio-build", "KBUILD_BUILD_TIMESTAMP=2026-09-27 00:00:00 UTC", "KBUILD_BUILD_VERSION=1"] {
            a.extend(["-e".into(), e.into()]);
        }
        a.extend([KBUILD_IMAGE.into(), "make".into(), "-s".into(), "-C".into(), format!("/k/{tree_name}"), "O=/k/build".into(),
                  "ARCH=arm64".into(), "CROSS_COMPILE=aarch64-linux-gnu-".into(), "HOSTCC=gcc".into()]);
        a.extend(args.iter().map(|s| s.to_string()));
        docker(&a.iter().map(String::as_str).collect::<Vec<_>>())
    };
    let out = work.join("build");
    std::fs::create_dir_all(&out)?;
    // configure: arm64 defconfig, other SoC families off (generic arm64 kept), no debug info, the nio fragment
    kmake(&["defconfig"])?;
    let platforms = std::fs::read_to_string(tree.join("arch/arm64/Kconfig.platforms"))?;
    let mut changes: Vec<(String, Option<String>)> = platforms
        .lines()
        .filter_map(|l| l.strip_prefix("config ARCH_"))
        .filter(|s| *s != "QCOM" && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .map(|s| (format!("CONFIG_ARCH_{s}"), None))
        .collect();
    changes.extend([("CONFIG_DEBUG_INFO".into(), None), ("CONFIG_DEBUG_INFO_BTF".into(), None), ("CONFIG_DEBUG_INFO_NONE".into(), Some("y".into()))]);
    let fragment = std::fs::read_to_string(port().join("kernel/configs/nio-host.config"))?;
    changes.extend(config_entries(&fragment));
    let config = out.join(".config");
    std::fs::write(&config, config_set(&std::fs::read_to_string(&config)?, &changes))?;
    kmake(&["olddefconfig"])?;
    check_fragment(&std::fs::read_to_string(&config)?, &fragment)?;
    kmake(&["savedefconfig"])?;
    let committed = port().join("kernel/configs/nio-host.defconfig");
    if std::fs::read(out.join("defconfig")).ok() != std::fs::read(&committed).ok() {
        println!("note: the resolved defconfig ({}) differs from {}", out.join("defconfig").display(), committed.display());
    }
    // build
    kmake(&["-j", &jobs(), "Image", "modules"])?;
    let modroot = work.join("modroot");
    let _ = std::fs::remove_dir_all(&modroot);
    kmake(&["INSTALL_MOD_PATH=/k/modroot", "INSTALL_MOD_STRIP=1", "modules_install"])?;
    // our out-of-tree drivers (motorola_nio/kernel/drivers/*.c, headers beside them), one module each, into the
    // release's extra/
    let ext = work.join("ext");
    let _ = std::fs::remove_dir_all(&ext);
    std::fs::create_dir_all(&ext)?;
    let mut kbuild = String::new();
    for e in std::fs::read_dir(port().join("kernel/drivers"))?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "c" || x == "h") {
            std::fs::copy(&p, ext.join(p.file_name().unwrap()))?;
        }
        if p.extension().is_some_and(|x| x == "c") {
            kbuild += &format!("obj-m += {}.o\n", p.file_stem().unwrap().to_string_lossy());
        }
    }
    std::fs::write(ext.join("Kbuild"), kbuild)?;
    kmake(&["M=/k/ext", "-j", &jobs(), "modules"])?;
    kmake(&["M=/k/ext", "INSTALL_MOD_PATH=/k/modroot", "INSTALL_MOD_DIR=extra", "INSTALL_MOD_STRIP=1", "modules_install"])?;
    let image = out.join("arch/arm64/boot/Image");
    let manifest = serde_json::json!({
        "linux": host_source()?,
        "patches": patches(&port().join("kernel/patches"))?.iter()
            .map(|p| Ok(serde_json::json!({"path": p.strip_prefix(root())?.to_string_lossy(), "sha256": sha256(p)?})))
            .collect::<Result<Vec<_>>>()?,
        "image_sha256": sha256(&image)?,
        "config_sha256": sha256(&config)?,
        "fragment_sha256": sha256_hex(fragment.as_bytes()),
    });
    std::fs::write(out.join("build-manifest.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    println!("{}\n{}", image.display(), modroot.display());
    Ok(Kernel { image, modroot })
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
    ensure!(dts.contains(BD_PLACEHOLDER), "nio-el2-display.dts: no `{BD_PLACEHOLDER}`");
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
    let work = target_dir().join("boot");
    std::fs::create_dir_all(&work)?;
    let out = work.join("busybox");
    std::fs::write(&out, busybox(&manifest, &work)?)?;
    Ok(out)
}

pub fn initramfs() -> Result<PathBuf> {
    let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(port().join("boot/initramfs.json"))?)?;
    let work = target_dir().join("boot");
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
                let p = root().join(local);
                let d = std::fs::read(&p).with_context(|| format!("{}: the phone's firmware (motorola_nio/firmware, not in git)", p.display()))?;
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
    let out = work.join("initramfs.cpio.gz");
    let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(&out)?, flate2::Compression::best());
    gz.write_all(&c.finish())?;
    gz.finish()?;
    println!("{}", out.display());
    Ok(out)
}

// ---------------------------------------------------------------- FIT

pub fn fit() -> Result<PathBuf> {
    let kernel = host_kernel()?;
    println!("modules for this kernel: {}", kernel.modroot.display());
    let initramfs = initramfs()?;
    let work = target_dir().join("boot");
    let stage = work.join("fit");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    // the names nio-el2.its includes
    std::fs::copy(&kernel.image, stage.join("Image"))?;
    std::fs::copy(&initramfs, stage.join("initramfs.cpio.gz"))?;
    // the board DT: nio-el2-display.dts (the display, audio, USB-C and video additions on top of nio-el2.dts), with
    // this phone's Bluetooth address in it (its /include/s still from the port's boot/)
    let dts = with_bd_address(&std::fs::read_to_string(port().join("boot/nio-el2-display.dts"))?)?;
    std::fs::write(stage.join("nio-el2-display.dts"), dts)?;
    run_in(&stage, &["dtc", "-q", "-i", &port().join("boot").to_string_lossy(), "-I", "dts", "-O", "dtb", "-o", "nio.dtb", "nio-el2-display.dts"])?;
    let out = work.join("nio-el2.itb");
    // dtc resolves /incbin/ next to the .its, so it goes beside the files
    std::fs::copy(port().join("boot/nio-el2.its"), stage.join("nio-el2.its"))?;
    run_in(&stage, &["mkimage", "-q", "-f", "nio-el2.its", &out.to_string_lossy()])?;
    let size = std::fs::metadata(&out)?.len();
    ensure!(size <= BOOT_B_SIZE, "FIT {size} B exceeds boot_b ({BOOT_B_SIZE} B)");
    println!("{} {size} B sha256={}", out.display(), &sha256(&out)?[..16]);
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
    let work = target_dir().join("host-kernel");
    let tree = kernel_tree(&work, &host_source()?, &port().join("kernel/patches"))?;
    let variant = if diagnostic { "sm8250-motorola-nio-diagnostic" } else { "sm8250-motorola-nio" };
    let out = target_dir().join(if diagnostic { "nio-dtb-diagnostic" } else { "nio-dtb" });
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
    let evidence: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(port().join("kernel/evidence/nio-board.json"))?)?;
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
    ensure!(string(&fdt, "/", "model")?.contains("Qualcomm Technologies, Inc."), "Motorola ABL rejects a base DTB whose model lacks \"Qualcomm Technologies, Inc.\"");
    ensure!(!string(&fdt, "/chosen", "bootargs")?.is_empty(), "Motorola ABL asserts when /chosen/bootargs is missing");
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
    fn cpio_newc() {
        let mut c = Cpio::new();
        c.file("init", 0o755, b"abc");
        let out = c.finish();
        assert!(out.starts_with(b"070701"));
        assert_eq!(out.len() % 4, 0);
        assert!(out.windows(10).any(|w| w == b"TRAILER!!!"));
    }
}
