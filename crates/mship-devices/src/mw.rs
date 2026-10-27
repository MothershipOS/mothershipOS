//! The mship.wifi 1.x wire protocol, host side. Numbers mirror guest/drivers/mship_wifi/mship_wifi_proto.h
//! (append-only; a test compares both).
//!
//! A message: header (le32 length incl. header, le16 op, le16 flags, le32 id), then attributes as netlink's.

pub use crate::netlink::{Attrs, nla as attr, parse as parse_attrs};

pub const PORT: u32 = 7710;
pub const VERSION: &str = "1.0";
pub const MAX_MSG: usize = 64 * 1024;
pub const HDR: usize = 12;

// ops
pub const HELLO: u16 = 1;
pub const SCAN: u16 = 2;
pub const ABORT_SCAN: u16 = 3;
pub const CONNECT: u16 = 4;
pub const DISCONNECT: u16 = 5;
pub const SET_MAC: u16 = 6;
pub const ADD_KEY: u16 = 7;
pub const DEL_KEY: u16 = 8;
pub const SET_DEFAULT_KEY: u16 = 9;
pub const SET_DEFAULT_MGMT_KEY: u16 = 10;
pub const SET_STATION: u16 = 11;
pub const GET_STATION: u16 = 12;
pub const SET_POWER_SAVE: u16 = 13;
pub const SET_CQM: u16 = 14;
pub const RESULT: u16 = 64;
pub const HELLO_REPLY: u16 = 65;
pub const SCAN_RESULT: u16 = 66;
pub const SCAN_DONE: u16 = 67;
pub const CONNECT_RESULT: u16 = 68;
pub const DISCONNECTED: u16 = 69;
pub const CQM: u16 = 70;
pub const ATTACHED: u16 = 71;
pub const DETACHED: u16 = 72;

// attributes
pub const A_VERSION: u16 = 1;
pub const A_STATUS: u16 = 2;
pub const A_SSID: u16 = 3;
pub const A_BSSID: u16 = 4;
pub const A_BSSID_HINT: u16 = 5;
pub const A_FREQ: u16 = 6;
pub const A_FREQ_HINT: u16 = 7;
pub const A_IES: u16 = 8;
pub const A_AUTH_TYPE: u16 = 9;
pub const A_WPA_VERSIONS: u16 = 10;
pub const A_CIPHERS_PAIRWISE: u16 = 11;
pub const A_CIPHER_GROUP: u16 = 12;
pub const A_AKMS: u16 = 13;
pub const A_CONTROL_PORT_ETHERTYPE: u16 = 14;
pub const A_MFP: u16 = 15;
pub const A_REASON: u16 = 16;
pub const A_MAC: u16 = 17;
pub const A_KEY_IDX: u16 = 18;
pub const A_PAIRWISE: u16 = 19;
pub const A_CIPHER: u16 = 20;
pub const A_KEY: u16 = 21;
pub const A_SEQ: u16 = 22;
pub const A_UNICAST: u16 = 23;
pub const A_MULTICAST: u16 = 24;
pub const A_AUTHORIZED: u16 = 25;
pub const A_ENABLED: u16 = 26;
pub const A_THRESHOLD: u16 = 27;
pub const A_HYSTERESIS: u16 = 28;
pub const A_SIGNAL_MBM: u16 = 29;
pub const A_CAPABILITY: u16 = 30;
pub const A_BEACON_INT: u16 = 31;
pub const A_TSF: u16 = 32;
pub const A_BSS_IES: u16 = 33;
pub const A_REQ_IES: u16 = 34;
pub const A_RESP_IES: u16 = 35;
pub const A_TIMED_OUT: u16 = 36;
pub const A_LOCALLY_GENERATED: u16 = 37;
pub const A_RSSI_EVENT: u16 = 38;
pub const A_CHANNELS: u16 = 39;
pub const A_ATTACHED_CHANNELS: u16 = 40;
pub const A_SSIDS: u16 = 41;
pub const A_FREQS: u16 = 42;
pub const A_ABORTED: u16 = 43;
pub const A_TX_BITRATE: u16 = 44;
pub const A_RX_BITRATE: u16 = 45;
pub const A_TX_PACKETS: u16 = 46;
pub const A_RX_PACKETS: u16 = 47;
pub const A_TX_FAILED: u16 = 48;
pub const A_SIGNAL: u16 = 49;
pub const A_DETACH_REASON: u16 = 50;

pub fn detach_reason(r: &str) -> u32 {
    match r {
        "user" => 1,
        "screen" => 2,
        "stop" => 3,
        "revoked" => 4,
        "gone" => 5,
        _ => 1,
    }
}

pub fn a_u8(t: u16, v: u8) -> Vec<u8> {
    attr(t, &[v])
}

pub fn a_u16(t: u16, v: u16) -> Vec<u8> {
    attr(t, &v.to_le_bytes())
}

pub fn a_u32(t: u16, v: u32) -> Vec<u8> {
    attr(t, &v.to_le_bytes())
}

pub fn a_s32(t: u16, v: i32) -> Vec<u8> {
    attr(t, &v.to_le_bytes())
}

pub fn a_u32s(t: u16, vs: &[u32]) -> Vec<u8> {
    attr(t, &vs.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>())
}

pub fn message(op: u16, id: u32, attrs: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(HDR + attrs.len());
    m.extend_from_slice(&((HDR + attrs.len()) as u32).to_le_bytes());
    m.extend_from_slice(&op.to_le_bytes());
    m.extend_from_slice(&0u16.to_le_bytes());
    m.extend_from_slice(&id.to_le_bytes());
    m.extend_from_slice(attrs);
    m
}

/// (length, op, id) of a header
pub fn header(h: &[u8; HDR]) -> (usize, u16, u32) {
    (
        u32::from_le_bytes(h[0..4].try_into().unwrap()) as usize,
        u16::from_le_bytes([h[4], h[5]]),
        u32::from_le_bytes(h[8..12].try_into().unwrap()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `MW_*`/`MWA_*` number in the guest driver's header equals ours.
    #[test]
    fn matches_guest_header() {
        let h = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../guest/drivers/mship_wifi/mship_wifi_proto.h")).unwrap();
        let ours: std::collections::BTreeMap<&str, u32> = [
            ("MW_PORT", PORT), ("MW_HELLO", HELLO.into()), ("MW_SCAN", SCAN.into()), ("MW_ABORT_SCAN", ABORT_SCAN.into()),
            ("MW_CONNECT", CONNECT.into()), ("MW_DISCONNECT", DISCONNECT.into()), ("MW_SET_MAC", SET_MAC.into()),
            ("MW_ADD_KEY", ADD_KEY.into()), ("MW_DEL_KEY", DEL_KEY.into()), ("MW_SET_DEFAULT_KEY", SET_DEFAULT_KEY.into()),
            ("MW_SET_DEFAULT_MGMT_KEY", SET_DEFAULT_MGMT_KEY.into()), ("MW_SET_STATION", SET_STATION.into()),
            ("MW_GET_STATION", GET_STATION.into()), ("MW_SET_POWER_SAVE", SET_POWER_SAVE.into()), ("MW_SET_CQM", SET_CQM.into()),
            ("MW_RESULT", RESULT.into()), ("MW_HELLO_REPLY", HELLO_REPLY.into()), ("MW_SCAN_RESULT", SCAN_RESULT.into()),
            ("MW_SCAN_DONE", SCAN_DONE.into()), ("MW_CONNECT_RESULT", CONNECT_RESULT.into()), ("MW_DISCONNECTED", DISCONNECTED.into()),
            ("MW_CQM", CQM.into()), ("MW_ATTACHED", ATTACHED.into()), ("MW_DETACHED", DETACHED.into()),
            ("MWA_VERSION", 1), ("MWA_STATUS", 2), ("MWA_SSID", 3), ("MWA_BSSID", 4), ("MWA_BSSID_HINT", 5), ("MWA_FREQ", 6),
            ("MWA_FREQ_HINT", 7), ("MWA_IES", 8), ("MWA_AUTH_TYPE", 9), ("MWA_WPA_VERSIONS", 10), ("MWA_CIPHERS_PAIRWISE", 11),
            ("MWA_CIPHER_GROUP", 12), ("MWA_AKMS", 13), ("MWA_CONTROL_PORT_ETHERTYPE", 14), ("MWA_MFP", 15), ("MWA_REASON", 16),
            ("MWA_MAC", 17), ("MWA_KEY_IDX", 18), ("MWA_PAIRWISE", 19), ("MWA_CIPHER", 20), ("MWA_KEY", 21), ("MWA_SEQ", 22),
            ("MWA_UNICAST", 23), ("MWA_MULTICAST", 24), ("MWA_AUTHORIZED", 25), ("MWA_ENABLED", 26), ("MWA_THRESHOLD", 27),
            ("MWA_HYSTERESIS", 28), ("MWA_SIGNAL_MBM", 29), ("MWA_CAPABILITY", 30), ("MWA_BEACON_INT", 31), ("MWA_TSF", 32),
            ("MWA_BSS_IES", 33), ("MWA_REQ_IES", 34), ("MWA_RESP_IES", 35), ("MWA_TIMED_OUT", 36), ("MWA_LOCALLY_GENERATED", 37),
            ("MWA_RSSI_EVENT", 38), ("MWA_CHANNELS", 39), ("MWA_ATTACHED_CHANNELS", 40), ("MWA_SSIDS", 41), ("MWA_FREQS", 42),
            ("MWA_ABORTED", 43), ("MWA_TX_BITRATE", 44), ("MWA_RX_BITRATE", 45), ("MWA_TX_PACKETS", 46), ("MWA_RX_PACKETS", 47),
            ("MWA_TX_FAILED", 48), ("MWA_SIGNAL", 49), ("MWA_DETACH_REASON", 50),
        ]
        .into_iter()
        .collect();
        // the attribute constants above must equal the A_* ones
        assert_eq!((A_VERSION, A_SSID, A_FREQS, A_DETACH_REASON), (1, 3, 42, 50));
        let mut seen = 0;
        for line in h.lines() {
            let l = line.trim().trim_start_matches("#define").trim();
            let Some((name, rest)) = l.split_once(['=', ' ', '\t']) else { continue };
            let name = name.trim();
            if !(name.starts_with("MW_") || name.starts_with("MWA_")) {
                continue;
            }
            let Ok(v) = rest.trim().trim_start_matches('=').trim().trim_end_matches(',').split(|c: char| !c.is_ascii_digit()).next().unwrap_or("").parse::<u32>() else { continue };
            if let Some(o) = ours.get(name) {
                assert_eq!(*o, v, "{name}");
                seen += 1;
            }
        }
        assert_eq!(seen, ours.len(), "header names not found");
    }
}
