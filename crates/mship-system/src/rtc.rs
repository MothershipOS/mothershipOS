//! The PM8150 RTC counter keeps running across reboots and power-off, but Linux cannot set it (TZ / modem own it;
//! stock Android keeps an offset in time_daemon). Same scheme here: wall time = RTC + offset, with the offset
//! (seconds) stored on the rootfs.
//!
//! - `restore` (boot): the system clock from RTC + saved offset
//! - `save` (once the clock is known good: time sync, shutdown): offset = now - RTC
//! - `show`

use crate::{Res, now, set_clock};

const STATE: &str = "/var/lib/mothership/rtc-offset";
/// anything before this is a clock that was never set
const SANE: i64 = 1_700_000_000;

fn rtc() -> Res<i64> {
    let dev = std::env::var("MSHIP_RTC").unwrap_or_else(|_| "/sys/class/rtc/rtc0".into());
    std::fs::read_to_string(format!("{dev}/since_epoch"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| format!("no RTC at {dev}"))
}

fn offset() -> Option<i64> {
    std::fs::read_to_string(STATE).ok()?.trim().parse().ok()
}

pub fn save() -> Res<()> {
    let (raw, now) = (rtc()?, now());
    if now < SANE {
        return Err(format!("system clock not set ({now}), not saving"));
    }
    if let Some(d) = std::path::Path::new(STATE).parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = format!("{STATE}.tmp");
    std::fs::write(&tmp, format!("{}\n", now - raw)).and_then(|_| std::fs::rename(&tmp, STATE)).map_err(|e| format!("{STATE}: {e}"))?;
    if let Ok(f) = std::fs::File::open(STATE) {
        let _ = f.sync_all();
    }
    println!("RTC offset saved: {} s", now - raw);
    Ok(())
}

pub fn main(cmd: &str) -> Res<()> {
    match cmd {
        "restore" => {
            let raw = rtc()?;
            let Some(off) = offset() else {
                println!("no saved RTC offset yet (set after the first time sync)");
                return Ok(());
            };
            set_clock(raw + off)?;
            println!("clock set from RTC + offset: {}", raw + off);
            Ok(())
        }
        "save" => save(),
        "show" => {
            let raw = rtc()?;
            let off = offset();
            println!("rtc={raw} offset={} rtc_time={} system={}", off.map_or("none".into(), |o| o.to_string()),
                     off.map_or("-".into(), |o| (raw + o).to_string()), now());
            Ok(())
        }
        _ => Err("usage: mship-system rtc restore|save|show".into()),
    }
}
