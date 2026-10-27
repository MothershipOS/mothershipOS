//! The PM8150 PON KPDPWR (power key) interrupt enable (PON 0x815 bit 0) gets cleared at some point after boot
//! (cause not identified yet: not suspend/resume, not screen off/on; resin, bit 1, stays enabled), so the power key
//! stops interrupting. Rebinding pm8941-pwrkey re-requests the IRQ, which sets the bit again.

use std::time::Duration;

use crate::Res;

const REGS: &str = "/sys/kernel/debug/regmap/0-00/registers";
const DEV: &str = "c440000.spmi:pmic@0:pon@800:pwrkey";
const DRV: &str = "/sys/bus/platform/drivers/pm8941-pwrkey";

fn int_en() -> Option<u32> {
    std::fs::read_to_string(REGS).ok()?.lines().find_map(|l| l.strip_prefix("0815: ").and_then(|v| u32::from_str_radix(v.trim(), 16).ok()))
}

/// Check once, re-arm if needed; true if it had to.
pub fn check(verbose: bool) -> Res<bool> {
    let v = int_en().ok_or("PON regs not readable")?;
    if v & 1 == 0 {
        std::fs::write(format!("{DRV}/unbind"), DEV).and_then(|_| std::fs::write(format!("{DRV}/bind"), DEV)).map_err(|e| format!("rebind: {e}"))?;
        println!("power key IRQ was disabled, re-armed (PON INT_EN {v:#04x} -> {:#04x})", int_en().unwrap_or(0));
        return Ok(true);
    }
    if verbose {
        println!("power key IRQ enabled ({v:#04x})");
    }
    Ok(false)
}

/// Keep checking every 2 s (the service).
pub fn watch() -> Res<()> {
    if let Err(e) = check(true) {
        println!("{e}");
        return Ok(()); // nothing to watch on this kernel
    }
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if let Err(e) = check(false) {
            println!("{e}");
        }
    }
}
