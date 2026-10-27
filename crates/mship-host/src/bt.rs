//! Bluetooth (BlueZ, through bluetoothctl): the controller, paired devices, scanning, pairing.

use mship_core::status::{BtDevice, BtFound, BtStatus};
use regex::Regex;

use crate::util::{Result, fail};

/// bluetoothctl output without its colour codes. `timeout`: how long it stays up (scan, pair, connect).
fn btctl(args: &[&str], timeout: Option<u32>, agent: bool) -> String {
    let mut cmd: Vec<String> = Vec::new();
    if agent {
        cmd.extend(["--agent".into(), "NoInputNoOutput".into()]);
    }
    if let Some(t) = timeout {
        cmd.extend(["--timeout".into(), t.to_string()]);
    }
    cmd.extend(args.iter().map(|a| a.to_string()));
    let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
    // bluetoothctl can hang without a controller: bound it
    let limit = (timeout.unwrap_or(10) + 15).to_string();
    let mut all = vec![limit.as_str(), "bluetoothctl"];
    all.extend(refs);
    let o = crate::util::run("timeout", &all);
    let re = Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(&(o.stdout + &o.stderr), "").into_owned()
}

pub fn valid_mac(mac: &str) -> Result<String> {
    let mac = mac.to_uppercase();
    if Regex::new(r"^([0-9A-F]{2}:){5}[0-9A-F]{2}$").unwrap().is_match(&mac) { Ok(mac) } else { fail("bad Bluetooth address") }
}

/// `bluetoothctl info` -> the device. Pure.
pub fn parse_info(mac: &str, text: &str) -> BtDevice {
    let mut d = BtDevice { mac: mac.into(), ..Default::default() };
    for l in text.lines() {
        let Some((k, v)) = l.trim().split_once(": ") else { continue };
        let v = Some(v.to_string());
        match k {
            "Name" => d.name = v,
            "Alias" => d.alias = v,
            "Icon" => d.icon = v,
            "Paired" => d.paired = v,
            "Bonded" => d.bonded = v,
            "Trusted" => d.trusted = v,
            "Connected" => d.connected = v,
            "RSSI" => d.rssi = v.and_then(|v| v.split_whitespace().next().map(String::from)),
            _ => {}
        }
    }
    d
}

/// `bluetoothctl devices` -> (mac, name). Pure.
pub fn parse_devices(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("Device "))
        .filter_map(|l| l.split_once(' ').map(|(m, n)| (m.to_string(), n.to_string())))
        .collect()
}

fn devices(which: Option<&str>) -> Vec<(String, String)> {
    let mut args = vec!["devices"];
    args.extend(which);
    parse_devices(&btctl(&args, None, false))
}

pub fn status() -> BtStatus {
    let show = btctl(&["show"], None, false);
    let field = |k: &str| {
        show.lines().find_map(|l| l.trim().strip_prefix(&format!("{k}: ")).map(|v| v.trim().to_string()))
    };
    BtStatus {
        present: show.contains("Controller"),
        powered: field("Powered"),
        discoverable: field("Discoverable"),
        pairable: field("Pairable"),
        name: field("Name"),
        alias: field("Alias"),
        address: show.lines().find_map(|l| l.trim().strip_prefix("Controller ").and_then(|r| r.split_whitespace().next()).map(String::from)),
        paired: devices(Some("Paired")).iter().map(|(m, _)| parse_info(m, &btctl(&["info", m], None, false))).collect(),
    }
}

pub fn power(on: bool) -> BtStatus {
    btctl(&["power", if on { "on" } else { "off" }], None, false);
    status()
}

pub fn discoverable(on: bool) -> BtStatus {
    let v = if on { "on" } else { "off" };
    btctl(&["discoverable", v], None, false);
    btctl(&["pairable", v], None, false);
    status()
}

/// Devices found in a 10 s scan (not the paired ones), named ones first, then by signal.
pub fn scan() -> Vec<BtFound> {
    let out = btctl(&["scan", "on"], Some(10), false);
    let re = Regex::new(r"Device (\S+) RSSI: (?:0x[0-9a-f]+ \()?(-?\d+)").unwrap();
    let rssi: std::collections::BTreeMap<String, i32> =
        re.captures_iter(&out).filter_map(|c| Some((c[1].to_string(), c[2].parse().ok()?))).collect();
    let paired: Vec<String> = devices(Some("Paired")).into_iter().map(|(m, _)| m).collect();
    let mut found: Vec<BtFound> = devices(None)
        .into_iter()
        .filter(|(m, _)| !paired.contains(m))
        .map(|(mac, name)| BtFound { named: name.replace('-', ":") != mac, rssi: rssi.get(&mac).copied(), mac, name })
        .collect();
    found.sort_by_key(|d| (!d.named, -d.rssi.unwrap_or(-999)));
    found
}

fn first_match(re: &str, text: &str, default: &str) -> String {
    Regex::new(re).unwrap().captures(text).map(|c| c[1].to_string()).unwrap_or_else(|| default.into())
}

pub fn pair(mac: &str) -> Result<BtStatus> {
    let mac = valid_mac(mac)?;
    let out = btctl(&["pair", &mac], Some(30), true);
    if !out.contains("Pairing successful") && !out.contains("Already") {
        return fail(first_match(r"Failed to pair: (\S+)", &out, "pairing failed"));
    }
    btctl(&["trust", &mac], None, false);
    btctl(&["connect", &mac], Some(15), false);
    Ok(status())
}

pub fn connect(mac: &str, on: bool) -> Result<BtStatus> {
    let mac = valid_mac(mac)?;
    let out = btctl(&[if on { "connect" } else { "disconnect" }, &mac], Some(15), false);
    if on && !out.contains("Connection successful") {
        return fail(first_match(r"Failed to connect: (\S+)", &out, "connect failed"));
    }
    Ok(status())
}

pub fn remove(mac: &str) -> Result<BtStatus> {
    let mac = valid_mac(mac)?;
    btctl(&["remove", &mac], None, false);
    Ok(status())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        assert_eq!(valid_mac("aa:bb:cc:dd:ee:ff").unwrap(), "AA:BB:CC:DD:EE:FF");
        assert!(valid_mac("aa:bb").is_err());
        let devs = parse_devices("Device AA:BB:CC:DD:EE:FF Headphones\nDevice 11:22:33:44:55:66 11-22-33-44-55-66\nnoise\n");
        assert_eq!(devs[0], ("AA:BB:CC:DD:EE:FF".into(), "Headphones".into()));
        let d = parse_info("AA", "Device AA\n\tName: Phones\n\tPaired: yes\n\tRSSI: -60 (0xffc4)\n");
        assert_eq!((d.name.as_deref(), d.paired.as_deref(), d.rssi.as_deref()), (Some("Phones"), Some("yes"), Some("-60")));
    }
}
