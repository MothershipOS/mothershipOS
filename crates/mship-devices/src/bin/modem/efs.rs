//! The modem's NV write-back: what a flashless SDX55 syncs back over its EFS channel goes to its NV partitions.
//!
//! In mission mode the modem hands its NV (calibration, IMEI, network settings) back with Sahara memory debug on the
//! EFS channel; the kernel's Sahara server (mhi_sahara) reads the region and offers it as a device coredump
//! (`/sys/class/devcoredump/devcdN`). The region is an image of the partition it came from (`m9kefs1` ->
//! `mdm1m9kefs1`), which boots the modem next time (Sahara image 16). Written only when the dump is the modem's,
//! the region names a writable NV copy, its image id matches the partition's and it fits; the partition's contents
//! before the first write are kept once.

use std::io::Write;
use std::path::{Path, PathBuf};

use mship_devices::log;

const DUMPS: &str = "/sys/class/devcoredump";
/// the driver of the device a modem dump comes from
const DRIVER: &str = "mhi-pci-generic";
const PARTITIONS: &str = "/dev/disk/by-partlabel";
/// the modem's NV copies it writes back (m9kefs3 is the factory copy: read only)
const WRITABLE: &[&str] = &["m9kefs1", "m9kefs2"];
/// mhi_sahara's dump format (qaic's): header, table of 64-byte entries, the regions back to back
const MAGIC: u64 = 0x1234567890abcdef;
const HEADER: usize = 32;
const ENTRY: usize = 64;

#[derive(Debug, PartialEq)]
pub struct Region {
    pub filename: String,
    pub data: Vec<u8>,
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8).map(|v| u64::from_le_bytes(v.try_into().unwrap()))
}

fn cstr(b: &[u8]) -> String {
    String::from_utf8_lossy(b.split(|c| *c == 0).next().unwrap_or_default()).into_owned()
}

/// The regions of a dump; None if it is not one.
pub fn parse(d: &[u8]) -> Option<Vec<Region>> {
    if u64_at(d, 0)? != MAGIC || u64_at(d, 8)? != 1 {
        return None;
    }
    let table = u64_at(d, 24)? as usize;
    if !table.is_multiple_of(ENTRY) {
        return None;
    }
    let mut off = HEADER.checked_add(table)?;
    let mut out = vec![];
    for i in 0..table / ENTRY {
        let e = d.get(HEADER + i * ENTRY..HEADER + (i + 1) * ENTRY)?;
        let len = u64_at(e, 16)? as usize;
        out.push(Region { filename: cstr(&e[44..64]), data: d.get(off..off.checked_add(len)?)?.to_vec() });
        off += len;
    }
    Some(out)
}

/// Is this dump the modem's?
fn from_modem(dump: &Path) -> bool {
    std::fs::read_link(dump.join("failing_device/driver"))
        .is_ok_and(|d| d.file_name().is_some_and(|n| n == DRIVER))
}

fn write_region(r: &Region, backups: &Path) -> Result<String, String> {
    if !WRITABLE.contains(&r.filename.as_str()) {
        return Err(format!("{}: not a writable NV copy, left alone", r.filename));
    }
    let dev = Path::new(PARTITIONS).join(format!("mdm1{}", r.filename));
    let current = std::fs::read(&dev).map_err(|e| format!("{}: {e}", dev.display()))?;
    if r.data.len() > current.len() || r.data.len() < 4 {
        return Err(format!("{}: {} bytes for a {} byte partition", r.filename, r.data.len(), current.len()));
    }
    // the image id the modem asks Sahara for (16, 17)
    if r.data[..4] != current[..4] {
        return Err(format!("{}: image id {:02x?} is not the partition's {:02x?}", r.filename, &r.data[..4], &current[..4]));
    }
    if current[..r.data.len()] == r.data[..] {
        return Ok(format!("{}: unchanged", dev.display()));
    }
    std::fs::create_dir_all(backups).map_err(|e| e.to_string())?;
    let backup = backups.join(format!("mdm1{}.orig", r.filename));
    if !backup.exists() {
        std::fs::write(&backup, &current).map_err(|e| format!("{}: {e}", backup.display()))?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).open(&dev).map_err(|e| format!("{}: {e}", dev.display()))?;
    f.write_all(&r.data).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", dev.display()))?;
    Ok(format!("{}: {} bytes written", dev.display(), r.data.len()))
}

/// Every modem dump waiting: written back, then released.
pub fn sync(backups: &Path) {
    let dumps: Vec<PathBuf> = std::fs::read_dir(DUMPS).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.join("data").exists() && from_modem(p)).collect();
    for dump in dumps {
        match std::fs::read(dump.join("data")).ok().as_deref().and_then(parse) {
            Some(regions) => {
                for r in &regions {
                    match write_region(r, backups) {
                        Ok(m) => log(format!("nv sync: {m}")),
                        Err(e) => log(format!("nv sync: {e}")),
                    }
                }
            }
            None => log(format!("nv sync: {}: not a Sahara dump", dump.display())),
        }
        // writing anything releases the dump
        let _ = std::fs::write(dump.join("data"), b"1");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump(regions: &[(&str, &[u8])]) -> Vec<u8> {
        let mut d = vec![];
        let total: usize = regions.iter().map(|r| r.1.len()).sum();
        for v in [MAGIC, 1, (HEADER + regions.len() * ENTRY + total) as u64, (regions.len() * ENTRY) as u64] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        for (name, data) in regions {
            let mut e = vec![0u8; ENTRY];
            e[16..24].copy_from_slice(&(data.len() as u64).to_le_bytes());
            e[44..44 + name.len()].copy_from_slice(name.as_bytes());
            d.extend_from_slice(&e);
        }
        for (_, data) in regions {
            d.extend_from_slice(data);
        }
        d
    }

    #[test]
    fn regions() {
        let d = dump(&[("m9kefs1", &[16, 0, 0, 0, 9]), ("x", &[1, 2])]);
        let r = parse(&d).unwrap();
        assert_eq!(r, [Region { filename: "m9kefs1".into(), data: vec![16, 0, 0, 0, 9] }, Region { filename: "x".into(), data: vec![1, 2] }]);
        assert!(parse(&d[..d.len() - 1]).is_none()); // truncated
        let mut bad = d.clone();
        bad[0] ^= 1;
        assert!(parse(&bad).is_none());
    }

    #[test]
    fn only_nv_copies() {
        let r = Region { filename: "m9kefs3".into(), data: vec![20, 0, 0, 0] };
        assert!(write_region(&r, Path::new("/nonexistent")).unwrap_err().contains("not a writable"));
    }
}
