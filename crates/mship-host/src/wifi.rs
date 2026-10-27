//! The phone's Wi-Fi (NetworkManager), and lending its uplink to a VM as shared internet.

use std::collections::BTreeMap;
use std::path::Path;

use mship_core::status::{WifiNetwork, WifiStatus};

use crate::util::{Result, fail, load_value, run, save_json};

pub const IFACE: &str = "wlp1s0";
const OWNERS: &str = "/run/mothership/owners.json";
/// the guest's eth2 -> virt_wifi wlan0 (mship-vm-net)
const VM_WIFI_TAP: &str = "pxwtap0";
const VM_WIFI_NET: &str = "192.168.96.0/24";
const VM_TAPS: &[&str] = &["pxwtap0", "pxetap0", "pxmtap0"];

/// Split one `nmcli -t` line (':' separated, '\:' and '\\' escaped).
pub fn nm_split(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    fields.last_mut().unwrap().push(n);
                }
            }
            ':' => fields.push(String::new()),
            c => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

// ---------------------------------------------------------------- hardware owners
/// {device: "host" | "vm:<vm dir>"}
pub fn owners() -> BTreeMap<String, String> {
    let mut o: BTreeMap<String, String> = load_value(OWNERS)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    o.entry("wifi".into()).or_insert_with(|| "host".into());
    o
}

/// "host" | the VM's name from its vm.json
pub fn owner_name(owner: &str) -> String {
    match owner.strip_prefix("vm:") {
        None => owner.to_string(),
        Some(d) => load_value(Path::new(d).join("vm.json"))
            .and_then(|v| v["name"].as_str().map(String::from))
            .unwrap_or_else(|| Path::new(d).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
    }
}

fn ipt(args: &[&str]) -> bool {
    run("iptables-legacy", args).ok()
}

/// VM internet through the phone's Wi-Fi. The host keeps the radio and its connection (it is the uplink); the VM's
/// wifi tap is NATed out of the Wi-Fi interface while lent. Chains MSHIP_VM (filter FORWARD, nat POSTROUTING) come
/// from mship-vm-net at boot; whatever is not lent is dropped.
fn vm_net(lent: bool) -> Result<()> {
    if !ipt(&["-n", "-L", "MSHIP_VM"]) {
        return fail("VM networking is not set up (mship-vm-net.service)");
    }
    ipt(&["-F", "MSHIP_VM"]);
    ipt(&["-t", "nat", "-F", "MSHIP_VM"]);
    if lent {
        ipt(&["-A", "MSHIP_VM", "-i", VM_WIFI_TAP, "-o", IFACE, "-j", "ACCEPT"]);
        ipt(&["-A", "MSHIP_VM", "-i", IFACE, "-o", VM_WIFI_TAP, "-m", "conntrack", "--ctstate",
              "ESTABLISHED,RELATED", "-j", "ACCEPT"]);
        ipt(&["-t", "nat", "-A", "MSHIP_VM", "-s", VM_WIFI_NET, "-o", IFACE, "-j", "MASQUERADE"]);
    }
    for tap in VM_TAPS {
        ipt(&["-A", "MSHIP_VM", "-i", tap, "-j", "DROP"]);
    }
    // an unlent internet slot has no carrier: the network disappears in the guest (Android would otherwise keep
    // preferring a validated-looking Ethernet over native Wi-Fi), and existing flows fail at once
    run("ip", &["link", "set", "dev", VM_WIFI_TAP, if lent { "up" } else { "down" }]);
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OwnerChange {
    pub device: String,
    pub owner: String,
    pub owner_name: String,
    pub previous: String,
}

/// Who uses a device; today only "wifi" (the uplink as a VM's shared internet).
pub fn owner_set(dev: &str, owner: &str) -> Result<OwnerChange> {
    if dev != "wifi" {
        return fail(format!("unknown device {dev}"));
    }
    let vm_ok = owner.strip_prefix("vm:").is_some_and(|d| Path::new(d).join("vm.json").is_file());
    if owner != "host" && !vm_ok {
        return fail("owner must be host or vm:<vm dir>");
    }
    let mut o = owners();
    let previous = o.insert(dev.into(), owner.into()).unwrap_or_else(|| "host".into());
    vm_net(owner != "host")?;
    // older builds released the interface for a VM: make sure mothershipOS manages it
    run("nmcli", &["device", "set", IFACE, "managed", "yes"]);
    save_json(OWNERS, &o)?;
    Ok(OwnerChange { device: dev.into(), owner: owner.into(), owner_name: owner_name(owner), previous })
}

fn require_host() -> Result<()> {
    let o = owners();
    if o["wifi"] != "host" {
        return fail(format!("Wi-Fi is lent to {}; take it back first", owner_name(&o["wifi"])));
    }
    Ok(())
}

// ---------------------------------------------------------------- status and control
fn after<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let i = text.find(key)? + key.len();
    text[i..].split_whitespace().next()
}

pub fn status() -> WifiStatus {
    let o = owners();
    let mut st = WifiStatus {
        radio: run("nmcli", &["-t", "radio", "wifi"]).stdout.trim() == "enabled",
        owner: o["wifi"].clone(),
        owner_name: owner_name(&o["wifi"]),
        present: Path::new("/sys/class/net").join(IFACE).exists(),
        state: "unavailable".into(),
        ..Default::default()
    };
    for line in run("nmcli", &["-t", "-f", "DEVICE,STATE,CONNECTION", "device"]).stdout.lines() {
        let f = nm_split(line);
        if f.first().map(String::as_str) == Some(IFACE) && f.len() >= 3 {
            st.state = f[1].clone();
            st.ssid = (!f[2].is_empty()).then(|| f[2].clone());
        }
    }
    if st.state == "connected" {
        st.ip = run("nmcli", &["-t", "-f", "IP4.ADDRESS", "device", "show", IFACE])
            .stdout
            .lines()
            .next()
            .and_then(|l| nm_split(l).get(1).map(|a| a.split('/').next().unwrap_or_default().to_string()));
        let link = run("iw", &["dev", IFACE, "link"]).stdout;
        st.signal = after(&link, "signal: ").and_then(|s| s.parse().ok());
        st.freq = after(&link, "freq: ").and_then(|s| s.parse().ok());
    }
    for line in run("nmcli", &["-t", "-f", "NAME,TYPE", "connection", "show"]).stdout.lines() {
        let f = nm_split(line);
        if f.get(1).map(String::as_str) == Some("802-11-wireless") {
            st.saved.push(f[0].clone());
        }
    }
    st
}

fn band(freq: &str) -> &'static str {
    match freq.chars().next() {
        Some('5') => "5 GHz",
        Some('6') => "6 GHz",
        _ => "2.4 GHz",
    }
}

/// Visible networks, one per SSID (the connected one, else the strongest), connected first, then by signal.
pub fn scan() -> Result<Vec<WifiNetwork>> {
    require_host()?;
    let out = run("nmcli", &["-t", "-f", "IN-USE,SSID,SIGNAL,SECURITY,FREQ", "device", "wifi", "list", "ifname", IFACE,
                              "--rescan", "yes"]).stdout;
    Ok(best_networks(&out, &status().saved))
}

/// `nmcli -t -f IN-USE,SSID,SIGNAL,SECURITY,FREQ device wifi list` -> the network list. Pure.
pub fn best_networks(nmcli: &str, saved: &[String]) -> Vec<WifiNetwork> {
    let mut best: BTreeMap<String, WifiNetwork> = BTreeMap::new();
    for line in nmcli.lines() {
        let f = nm_split(line);
        if f.len() < 5 || f[1].is_empty() {
            continue;
        }
        let n = WifiNetwork {
            ssid: f[1].clone(),
            signal: f[2].parse().unwrap_or(0),
            security: if f[3] == "--" { String::new() } else { f[3].clone() },
            band: band(&f[4]).into(),
            active: f[0] == "*",
            saved: saved.contains(&f[1]),
        };
        let keep = best.get(&n.ssid).is_none_or(|b| n.active || (n.signal > b.signal && !b.active));
        if keep {
            best.insert(n.ssid.clone(), n);
        }
    }
    let mut nets: Vec<WifiNetwork> = best.into_values().collect();
    nets.sort_by_key(|n| (!n.active, -n.signal));
    nets
}

/// Connect to a network; `password` empty = a saved or open one.
pub fn connect(ssid: &str, password: &str) -> Result<WifiStatus> {
    require_host()?;
    let saved = status().saved.iter().any(|s| s == ssid);
    let o = if saved && password.is_empty() {
        run("nmcli", &["-w", "30", "connection", "up", "id", ssid])
    } else {
        if saved {
            run("nmcli", &["connection", "delete", "id", ssid]);
        }
        let mut args = vec!["-w", "30", "device", "wifi", "connect", ssid, "ifname", IFACE];
        if !password.is_empty() {
            args.extend(["password", password]);
        }
        run("nmcli", &args)
    };
    if !o.ok() {
        let msg = o.message();
        let last = msg.lines().last().unwrap_or("connection failed");
        return fail(last.strip_prefix("Error: ").unwrap_or(last).to_string());
    }
    Ok(status())
}

pub fn disconnect() -> Result<WifiStatus> {
    require_host()?;
    run("nmcli", &["device", "disconnect", IFACE]);
    Ok(status())
}

pub fn forget(ssid: &str) -> WifiStatus {
    run("nmcli", &["connection", "delete", "id", ssid]);
    status()
}

pub fn radio(on: bool) -> Result<WifiStatus> {
    require_host()?;
    run("nmcli", &["radio", "wifi", if on { "on" } else { "off" }]);
    Ok(status())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_escapes() {
        assert_eq!(nm_split(r"wlp1s0:connected:My\:Net"), ["wlp1s0", "connected", "My:Net"]);
        assert_eq!(nm_split(r"a\\:b"), [r"a\", "b"]);
        assert_eq!(nm_split(":x:"), ["", "x", ""]);
    }

    #[test]
    fn networks_best_per_ssid() {
        let out = " :Home:60:WPA2:2412 MHz\n*:Home:40:WPA2:5180 MHz\n :Cafe:80:--:2437 MHz\n :Home:90:WPA2:5200 MHz\n :::\n";
        let nets = best_networks(out, &["Cafe".into()]);
        assert_eq!(nets.len(), 2);
        assert_eq!((nets[0].ssid.as_str(), nets[0].signal, nets[0].active), ("Home", 40, true)); // connected wins
        assert_eq!(nets[0].band, "5 GHz");
        assert_eq!((nets[1].ssid.as_str(), nets[1].security.as_str(), nets[1].saved), ("Cafe", "", true));
    }
}
