//! The PM8150 PON KPDPWR (power key) interrupt enable (PON 0x815 bit 0) gets cleared at some point after boot
//! (cause not identified yet: not suspend/resume, not screen off/on; resin, bit 1, stays enabled), so the power key
//! stops interrupting. Rebinding pm8941-pwrkey re-requests the IRQ, which sets the bit again.

use std::os::unix::fs::FileExt;
use std::time::Duration;

use crate::Res;

const REGS: &str = "/sys/kernel/debug/regmap/0-00/registers";
const DEV: &str = "c440000.spmi:pmic@0:pon@800:pwrkey";
const DRV: &str = "/sys/bus/platform/drivers/pm8941-pwrkey";

const LINE: &str = "0815: ";

/// The byte offset of INT_EN's line in the register dump: found once in the whole dump (every PMIC register read
/// over SPMI, ~0.2 s of CPU), after that only that line is read (regmap's debugfs starts at the read offset)
fn int_en_at() -> Option<u64> {
    let all = std::fs::read_to_string(REGS).ok()?;
    let at = all.find(&format!("\n{LINE}"))? + 1;
    Some(at as u64)
}

fn int_en(at: &mut Option<u64>) -> Option<u32> {
    for _ in 0..2 {
        let off = match *at {
            Some(o) => o,
            None => (*at).insert(int_en_at()?).to_owned(),
        };
        let mut b = [0u8; 9];
        let line = std::fs::File::open(REGS).ok()?.read_at(&mut b, off).ok().and_then(|n| std::str::from_utf8(&b[..n]).ok().map(str::to_owned));
        if let Some(v) = line.as_deref().and_then(|l| l.strip_prefix(LINE)).and_then(|v| u32::from_str_radix(v.trim(), 16).ok()) {
            return Some(v);
        }
        *at = None; // the dump's layout changed: find the line again
    }
    None
}

/// Check once, re-arm if needed; true if it had to.
pub fn check(verbose: bool) -> Res<bool> {
    check_at(&mut None, verbose)
}

fn check_at(at: &mut Option<u64>, verbose: bool) -> Res<bool> {
    let v = int_en(at).ok_or("PON regs not readable")?;
    if v & 1 == 0 {
        std::fs::write(format!("{DRV}/unbind"), DEV).and_then(|_| std::fs::write(format!("{DRV}/bind"), DEV)).map_err(|e| format!("rebind: {e}"))?;
        println!("power key IRQ was disabled, re-armed (PON INT_EN {v:#04x} -> {:#04x})", int_en(at).unwrap_or(0));
        return Ok(true);
    }
    if verbose {
        println!("power key IRQ enabled ({v:#04x})");
    }
    Ok(false)
}

/// Keep checking every 2 s (the service).
pub fn watch() -> Res<()> {
    let mut at = None;
    if let Err(e) = check_at(&mut at, true) {
        println!("{e}");
        return Ok(()); // nothing to watch on this kernel
    }
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if let Err(e) = check_at(&mut at, false) {
            println!("{e}");
        }
    }
}
