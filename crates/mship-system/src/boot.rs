//! The phone's boot:
//!
//! - `boot-ok`: this boot came up (mship-boot-ok.service, after multi-user): the initramfs's boot counter
//!   (/var/lib/mship-boot/attempts; after 3 unfinished boots mship-init stays in the RAM host) starts again
//! - `boot-image <image> [--reboot]`: write a boot image (`cargo xtask boot-image`) to the port's boot partition
//!   (device.json boot.partition, e.g. the nio's boot_b), read it back and compare. If it then fails to boot, the
//!   port's boot notes say how to recover.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::Digest;

use crate::{Res, run};

const ATTEMPTS: &str = "/var/lib/mship-boot/attempts";

pub fn boot_ok() -> Res<()> {
    std::fs::create_dir_all(Path::new(ATTEMPTS).parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(ATTEMPTS, "0\n").map_err(|e| format!("{ATTEMPTS}: {e}"))?;
    if let Ok(mut k) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = writeln!(k, "mothership: BOOT_OK");
    }
    Ok(())
}

/// The block device of a partition by its GPT name.
fn partition(name: &str) -> Option<PathBuf> {
    std::fs::read_dir("/sys/class/block").ok()?.flatten().find_map(|e| {
        let u = std::fs::read_to_string(e.path().join("uevent")).ok()?;
        u.lines().any(|l| l == format!("PARTNAME={name}")).then(|| Path::new("/dev").join(e.file_name()))
    })
}

pub fn boot_image(fit: &Path, reboot: bool) -> Res<()> {
    let data = std::fs::read(fit).map_err(|e| format!("{}: {e}", fit.display()))?;
    let name = mship_core::device::conf()["boot"]["partition"].as_str().ok_or("device.json names no boot.partition: this port isn't installed by writing a partition")?;
    let dev = partition(name).ok_or(format!("no {name} partition"))?;
    let size: u64 = std::fs::read_to_string(Path::new("/sys/class/block").join(dev.file_name().unwrap()).join("size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|sectors| sectors * 512)
        .ok_or(format!("{name}: no size"))?;
    if data.len() as u64 > size {
        return Err(format!("{} is {} B, {name} holds {size} B", fit.display(), data.len()));
    }
    let want: Vec<u8> = sha2::Sha256::digest(&data).to_vec();
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&dev).map_err(|e| format!("{}: {e}", dev.display()))?;
    f.write_all(&data).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", dev.display()))?;
    // read back from the device, not the page cache
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
    let mut back = vec![0u8; data.len()];
    f.seek(SeekFrom::Start(0)).and_then(|_| f.read_exact(&mut back)).map_err(|e| format!("{}: {e}", dev.display()))?;
    if sha2::Sha256::digest(&back).to_vec() != want {
        return Err(format!("{}: the written image reads back differently: do not reboot", dev.display()));
    }
    println!("{} written to {} and verified", fit.display(), dev.display());
    if reboot {
        run("systemctl", &["reboot"])?;
    }
    Ok(())
}
