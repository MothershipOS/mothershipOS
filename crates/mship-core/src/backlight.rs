//! The panel's backlight (sysfs, raw 0..max) and fades between levels. A fade moves evenly in perceived brightness
//! (gamma 2.2: raw = max * L^2.2), not in raw current, so it does not rush through the dark end.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The saved brightness (percent of max), mothershipOS's Display setting
pub const DISPLAY_CONF: &str = "/etc/mothership/display.json";

/// The panel's backlight (mship_core::device: device.json, else the first one)
fn path() -> PathBuf {
    crate::device::backlight().unwrap_or_else(|| "/sys/class/backlight/backlight".into())
}

fn read(name: &str) -> Option<u64> {
    std::fs::read_to_string(path().join(name)).ok()?.trim().parse().ok()
}

/// Set the raw level.
pub fn set(raw: u64) -> std::io::Result<()> {
    std::fs::write(path().join("brightness"), raw.min(max()).to_string())
}

pub fn max() -> u64 {
    read("max_brightness").unwrap_or(4095).max(1)
}

pub fn get() -> u64 {
    read("brightness").unwrap_or(0)
}

/// The saved brightness as a raw level (default 50 %).
pub fn saved() -> u64 {
    let pct = std::fs::read_to_string(DISPLAY_CONF)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["brightness"].as_u64())
        .unwrap_or(50)
        .clamp(1, 100);
    (max() * pct / 100).max(1)
}

/// The raw levels of a fade from `from` to `to` (both raw, of `max`), `steps` of them after `from`.
pub fn ramp(from: u64, to: u64, max: u64, steps: u32) -> Vec<u64> {
    let l = |raw: u64| (raw as f64 / max as f64).powf(1.0 / 2.2);
    let (a, b) = (l(from), l(to));
    (1..=steps.max(1))
        .map(|i| {
            let t = i as f64 / steps.max(1) as f64;
            let t = t * t * (3.0 - 2.0 * t); // ease in and out
            let v = a + (b - a) * t;
            (v.powf(2.2) * max as f64).round() as u64
        })
        .collect()
}

/// Fade the backlight to `to` (raw) over `ms`.
pub fn fade(to: u64, ms: u64) -> std::io::Result<()> {
    let (from, max) = (get(), max());
    let to = to.min(max);
    let steps = (ms / 16).max(1) as u32;
    let start = Instant::now();
    for (i, v) in ramp(from, to, max, steps).into_iter().enumerate() {
        set(v)?;
        let due = Duration::from_millis(ms * (i as u64 + 1) / steps as u64);
        if let Some(d) = due.checked_sub(start.elapsed()) {
            std::thread::sleep(d);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_ends_and_order() {
        let r = ramp(0, 2048, 4095, 30);
        assert_eq!(r.len(), 30);
        assert_eq!(*r.last().unwrap(), 2048);
        assert!(r.windows(2).all(|w| w[0] <= w[1]));
        let down = ramp(2048, 0, 4095, 25);
        assert_eq!(*down.last().unwrap(), 0);
        assert!(down.windows(2).all(|w| w[0] >= w[1]));
    }
}
