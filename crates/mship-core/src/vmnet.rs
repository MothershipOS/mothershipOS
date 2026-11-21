//! A VM's network cards: the same for every VM of a platform, blank (no address, no carrier) until a handle is
//! attached to the VM. Each card belongs to one kind of handle, whose service does all the work while attached:
//! - `internet`: the shared-internet handle (mship-net: the VM's address, DHCP, NAT out of the phone's uplink)
//! - `cellular`: mobile data (mship-modem: the data call's address routed to the card)
//! - `wifi`: a native Wi-Fi band (mship-wifi-bridge: the band interface redirected to the card)
//! - `ethernet`: Cuttlefish's eth1 (no handle: always empty)
//!
//! Cards per platform: cuttlefish-arm64 `cellular`, `ethernet`, `internet`, `wifi` (the guest's eth0..eth3); any
//! other platform one `internet` card. Names follow from the VM's vsock CID (vm.json `cid`) and the card's index:
//! tap `vm<cid>n<i>`, MAC `02:6d:76:<cid>:00:<i>` (the `wifi` card `02:6d:73:...`: the mship.wifi contract's mark). mothershipd creates the cards when the VM starts and removes them
//! when it stops, and records them in `/run/mothership/vmnet/<vm>.json` for the launcher and the services.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const STATE_DIR: &str = "/run/mothership/vmnet";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nic {
    pub kind: String,
    pub tap: String,
    pub mac: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VmNet {
    /// the VM's directory name
    pub vm: String,
    pub cid: u32,
    pub nics: Vec<Nic>,
}

impl VmNet {
    pub fn card(&self, kind: &str) -> Option<&Nic> {
        self.nics.iter().find(|n| n.kind == kind)
    }
}

/// The cards of a platform, in the order the guest sees them.
pub fn kinds(platform: &str) -> &'static [&'static str] {
    match platform {
        // Cuttlefish's guest: eth0 = mobile data (buried), eth1 = ethernet, eth2 = Wi-Fi (virt_wifi), eth3 = mship_wifi
        "cuttlefish-arm64" => &["cellular", "ethernet", "internet", "wifi"],
        _ => &["internet"],
    }
}

/// A VM's cards (vm.json "cid": 3 if missing, as the launchers default).
pub fn plan_for(vm: &str, conf: &Value) -> VmNet {
    let cid = conf["cid"].as_u64().unwrap_or(3) as u32;
    let nics = kinds(conf["platform"]["id"].as_str().unwrap_or(""))
        .iter()
        .enumerate()
        .map(|(i, k)| {
            // 02:6d:73 marks the native Wi-Fi card (the mship.wifi contract: the guest's driver takes it); the others
            // must not carry it
            let oui = if *k == "wifi" { "02:6d:73" } else { "02:6d:76" };
            Nic { kind: (*k).into(), tap: format!("vm{cid}n{i}"), mac: format!("{oui}:{:02x}:00:{:02x}", cid & 0xff, i) }
        })
        .collect();
    VmNet { vm: vm.into(), cid, nics }
}

/// The addresses the shared-internet handle gives a VM on its `internet` card: (host, guest, prefix).
pub fn internet_addrs(cid: u32) -> (String, String, u8) {
    (format!("10.200.{cid}.1"), format!("10.200.{cid}.2"), 24)
}

pub fn state_path(vm: &str) -> PathBuf {
    Path::new(STATE_DIR).join(format!("{vm}.json"))
}

/// A running VM's cards.
pub fn load(vm: &str) -> Option<VmNet> {
    serde_json::from_str(&std::fs::read_to_string(state_path(vm)).ok()?).ok()
}

/// Every running VM's cards.
pub fn all() -> Vec<VmNet> {
    let mut v: Vec<VmNet> = std::fs::read_dir(STATE_DIR)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
        .collect();
    v.sort_by(|a, b| a.vm.cmp(&b.vm));
    v
}

/// The running VM with this CID.
pub fn by_cid(cid: u32) -> Option<VmNet> {
    all().into_iter().find(|n| n.cid == cid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards() {
        let p = plan_for("pixelos", &serde_json::json!({"cid": 3, "platform": {"id": "cuttlefish-arm64"}}));
        assert_eq!(p.nics.iter().map(|n| n.kind.as_str()).collect::<Vec<_>>(), ["cellular", "ethernet", "internet", "wifi"]);
        assert_eq!(p.card("wifi").unwrap().tap, "vm3n3");
        assert_eq!(p.card("wifi").unwrap().mac, "02:6d:73:03:00:03");
        let p = plan_for("sailfish", &serde_json::json!({"cid": 5, "platform": {"id": "efi-virt-arm64"}}));
        assert_eq!(p.nics.len(), 1);
        assert_eq!(p.card("internet").unwrap().tap, "vm5n0");
        assert_eq!(p.card("internet").unwrap().mac, "02:6d:76:05:00:00");
        assert_eq!(internet_addrs(5).1, "10.200.5.2");
    }
}
