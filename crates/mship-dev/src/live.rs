//! The UART monitor's shared log (motorola_nio/tools/uart/live.log, or $NSH_LIVE_LOG): remote commands and their
//! output are mirrored there, so the monitor shows them next to the serial console.

use std::io::Write;

pub fn path() -> std::path::PathBuf {
    std::env::var_os("NSH_LIVE_LOG").map(Into::into).unwrap_or_else(|| crate::repo().join("motorola_nio/tools/uart/live.log"))
}

/// HH:MM:SS.mmm (local time)
pub fn stamp() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let t = now.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}.{:03}", tm.tm_hour, tm.tm_min, tm.tm_sec, now.subsec_millis())
}

pub fn line(source: &str, text: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path()) {
        let _ = writeln!(f, "{} | [{source}] {text}", stamp());
    }
}
