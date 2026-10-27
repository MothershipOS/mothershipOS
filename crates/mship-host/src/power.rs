//! Screen & sleep settings, the phone's screen state, battery and charging, display brightness, power off.

use std::collections::BTreeMap;
use std::path::Path;

use mship_core::status::{BatteryConf, BatteryStatus, Charger, PowerConf, PowerStatus};

use crate::util::{Result, fail, load_json, rd, rd_u64, run, save_json, sleep_s, wr};

pub const POWER_CONF: &str = "/etc/mothership/power.json";
/// "on" | "off": written on the power key / idle timeout, read by the sleep policy
pub const SCREEN_STATE: &str = "/run/mothership/screen";
const BATTERY_CONF: &str = "/etc/mothership/battery.json";
const DISPLAY_CONF: &str = "/etc/mothership/display.json";
const PSY: &str = "/sys/class/power_supply";
const BACKLIGHT: &str = "/sys/class/backlight/backlight";

// ---------------------------------------------------------------- screen & sleep
pub fn power_conf() -> PowerConf {
    load_json(POWER_CONF)
}

pub fn screen_on() -> bool {
    rd(SCREEN_STATE, "on") != "off"
}

pub fn set_screen(on: bool) -> Result<()> {
    std::fs::create_dir_all("/run/mothership")?;
    wr(SCREEN_STATE, if on { "on\n" } else { "off\n" })?;
    Ok(())
}

/// `last_decision`: the daemon's latest sleep line (it knows; the settings page shows it).
pub fn power_status(last_decision: &str) -> PowerStatus {
    PowerStatus {
        conf: power_conf(),
        screen: if screen_on() { "on" } else { "off" }.into(),
        sleepd: last_decision.into(),
        suspends: rd_u64("/sys/power/suspend_stats/success"),
    }
}

pub fn set_power(key: &str, value: &str, last_decision: &str) -> Result<PowerStatus> {
    let mut c = power_conf();
    let num = || value.parse::<i64>().map(|v| v.max(0) as u64).map_err(|_| crate::util::Error(format!("{key}: not a number")));
    match key {
        "screen_timeout" => c.screen_timeout = num()?,
        "sleep_delay" => c.sleep_delay = num()?,
        "sleep" => c.sleep = crate::util::truthy(value),
        "stay_awake_usb" => c.stay_awake_usb = crate::util::truthy(value),
        _ => return fail(format!("unknown power setting {key}")),
    }
    save_json(POWER_CONF, &c)?;
    Ok(power_status(last_decision))
}

// ---------------------------------------------------------------- battery & charging
/// A power supply's uevent properties (POWER_SUPPLY_X=v -> x: v).
fn psy(name: &str) -> BTreeMap<String, String> {
    rd(Path::new(PSY).join(name).join("uevent"), "")
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim_start_matches("POWER_SUPPLY_").to_lowercase(), v.to_string()))
        .collect()
}

pub fn battery_conf() -> BatteryConf {
    load_json(BATTERY_CONF)
}

pub fn battery_status() -> BatteryStatus {
    battery_from(&psy("battery"), &psy("pm8150b-charger"), battery_conf())
}

/// uevent properties of the battery and the charger -> the status. Pure.
pub fn battery_from(b: &BTreeMap<String, String>, u: &BTreeMap<String, String>, conf: BatteryConf) -> BatteryStatus {
    if b.is_empty() {
        return BatteryStatus { present: false, ..Default::default() };
    }
    let int = |m: &BTreeMap<String, String>, k: &str| m.get(k).and_then(|v| v.parse::<i64>().ok());
    let num = |m: &BTreeMap<String, String>, k: &str| int(m, k).unwrap_or(0) as f64;
    let charger = (!u.is_empty()).then(|| Charger {
        online: int(u, "online").unwrap_or(0) != 0,
        kind: u.get("usb_type").and_then(|t| Some(t[t.find('[')? + 1..t.find(']')?].to_string())),
        input_limit_ma: int(u, "current_max").unwrap_or(0) / 1000,
        voltage: num(u, "voltage_now") / 1e6,
        charge_current_ma: int(u, "constant_charge_current").unwrap_or(0) / 1000,
        charge_type: u.get("charge_type").cloned(),
    });
    BatteryStatus {
        present: true,
        capacity: int(b, "capacity"),
        status: b.get("status").cloned(),
        health: b.get("health").cloned(),
        voltage: num(b, "voltage_now") / 1e6,
        current: num(b, "current_now") / 1e3,
        power: num(b, "power_now") / 1e6,
        temp: num(b, "temp") / 10.0,
        full_design_mah: int(b, "charge_full_design").unwrap_or(0) / 1000,
        end_threshold: int(b, "charge_control_end_threshold"),
        behaviour: b.get("charge_behaviour").cloned(),
        charger,
        conf,
    }
}

fn battery_apply_conf(c: &BatteryConf) -> Result<()> {
    let b = Path::new(PSY).join("battery");
    for _ in 0..60 {
        if b.join("charge_control_end_threshold").exists() {
            break;
        }
        sleep_s(0.5);
    }
    if !b.join("charge_control_end_threshold").exists() {
        return fail("no battery power supply (qcom_pm8150b_charger not loaded?)");
    }
    if c.end_threshold < 100 {
        wr(b.join("charge_control_end_threshold"), c.end_threshold)?; // also lowers start to end - 5 if needed
        wr(b.join("charge_control_start_threshold"), c.end_threshold.saturating_sub(5))?;
    } else {
        wr(b.join("charge_control_end_threshold"), 100)?; // end first: the driver keeps start <= end - 5
        wr(b.join("charge_control_start_threshold"), 95)?;
    }
    wr(b.join("charge_behaviour"), if c.charging { "auto" } else { "inhibit-charge" })?;
    Ok(())
}

pub fn set_battery(key: &str, value: &str) -> Result<BatteryStatus> {
    let mut c = battery_conf();
    match key {
        "end_threshold" => match value.parse::<u32>() {
            Ok(v) if (50..=100).contains(&v) => c.end_threshold = v,
            _ => return fail("charge limit must be 50-100 %"),
        },
        "charging" => c.charging = crate::util::truthy(value),
        _ => return fail(format!("unknown battery setting {key}")),
    }
    battery_apply_conf(&c)?;
    save_json(BATTERY_CONF, &c)?;
    Ok(battery_status())
}

/// At boot: the battery driver starts at 100 %; apply the saved charge limit.
pub fn battery_apply() -> Result<BatteryStatus> {
    battery_apply_conf(&battery_conf())?;
    Ok(battery_status())
}

// ---------------------------------------------------------------- display
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct DisplayConf {
    brightness: Option<u32>,
}

pub fn brightness() -> u32 {
    let mx = rd_u64(Path::new(BACKLIGHT).join("max_brightness")).max(1);
    ((rd_u64(Path::new(BACKLIGHT).join("brightness")) * 100) as f64 / mx as f64).round() as u32
}

pub fn set_brightness(pct: u32, save: bool) -> Result<u32> {
    let mx = rd(Path::new(BACKLIGHT).join("max_brightness"), "4095").parse::<u64>().unwrap_or(4095);
    let pct = pct.clamp(1, 100);
    wr(Path::new(BACKLIGHT).join("brightness"), (mx * pct as u64 / 100).max(1))?;
    if save {
        let mut c: DisplayConf = load_json(DISPLAY_CONF);
        c.brightness = Some(pct);
        save_json(DISPLAY_CONF, &c)?;
    }
    Ok(pct)
}

/// At boot: the saved brightness (the backlight appears once the panel/WLED drivers probed).
pub fn display_apply() -> Result<u32> {
    for _ in 0..60 {
        if Path::new(BACKLIGHT).join("brightness").exists() {
            break;
        }
        sleep_s(0.5);
    }
    if !Path::new(BACKLIGHT).join("brightness").exists() {
        return fail("no backlight");
    }
    match load_json::<DisplayConf>(DISPLAY_CONF).brightness {
        Some(p) => set_brightness(p, false),
        None => Ok(brightness()),
    }
}

/// Power off or reboot now (the caller stops the VMs first).
pub fn shutdown(reboot: bool) {
    run("sync", &[]);
    let _ = std::process::Command::new("systemctl")
        .args(["--no-block", if reboot { "reboot" } else { "poweroff" }])
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(s: &str) -> BTreeMap<String, String> {
        s.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim_start_matches("POWER_SUPPLY_").to_lowercase(), v.into())).collect()
    }

    #[test]
    fn battery_values() {
        let b = props("POWER_SUPPLY_CAPACITY=96\nPOWER_SUPPLY_STATUS=Not charging\nPOWER_SUPPLY_VOLTAGE_NOW=4294422\n\
                       POWER_SUPPLY_CURRENT_NOW=-698000\nPOWER_SUPPLY_TEMP=401\nPOWER_SUPPLY_CHARGE_FULL_DESIGN=5000000\n\
                       POWER_SUPPLY_CHARGE_CONTROL_END_THRESHOLD=80");
        let u = props("POWER_SUPPLY_ONLINE=1\nPOWER_SUPPLY_USB_TYPE=Unknown [PD] SDP\nPOWER_SUPPLY_CURRENT_MAX=1500000");
        let s = battery_from(&b, &u, BatteryConf { end_threshold: 80, charging: true });
        assert!(s.present);
        assert_eq!((s.capacity, s.end_threshold, s.full_design_mah), (Some(96), Some(80), 5000));
        assert_eq!((s.current, s.temp), (-698.0, 40.1));
        assert!((s.voltage - 4.294422).abs() < 1e-9);
        let c = s.charger.unwrap();
        assert_eq!((c.online, c.kind.as_deref(), c.input_limit_ma), (true, Some("PD"), 1500));
        assert!(!battery_from(&BTreeMap::new(), &u, BatteryConf::default()).present);
    }
}
