//! mship-wifi-bridge: the mothershipOS Wi-Fi bridge (root), the mship.wifi 1.x contract
//! (docs/contracts/mship.wifi-1.md).
//!
//! Each Wi-Fi band handle is a station interface on the phone's wiphy (created here, e.g. mshw2g for 2.4 GHz),
//! restricted to that band. A VM whose wifi slot holds the band talks to us over vsock (port 7710); its requests
//! become nl80211 commands on the band interface and the interface's events go back to it. Frames move in the
//! kernel: tc ingress redirects between the band interface and the VM's Wi-Fi tap, in both directions. The host's
//! IP stack never sees them.
//!
//! Control: datagrams on /run/mothership/wifi-bridge.sock, one reply each: `attach <cid> <band>` (lend a band
//! handle, 2g or 5g, to the VM with that vsock CID), `detach <cid> [user|screen|stop|revoked|gone]`, `status`.
//!
//! Configuration: /etc/mothership/wifi-bridge.json (phy; band interface names). A VM's tap and initial MAC: its
//! `wifi` card (mship_core::vmnet), found by its vsock CID.
//! Bands are lent by mothershipd (hardware assignment); a lend that arrives before the VM's driver connects is kept
//! and applied at its hello. One band per VM at a time (two station interfaces with the same MAC are not allowed
//! by mac80211).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mship_devices::mw::{self, Attrs, a_s32, a_u8, a_u16, a_u32, a_u32s, attr};
use mship_devices::netlink::{self as nl, Genl, get_i32, get_u8, get_u16, get_u32, get_u32s, nla, nla_flag, nla_nest, nla_u8, nla_u16, nla_u32};
use mship_core::handle::{Direction, Handle};
use mship_devices::{for_session, fresh, log, publish_handles};
use serde_json::{Value, json};

const CONF: &str = "/etc/mothership/wifi-bridge.json";
const CTL: &str = "/run/mothership/wifi-bridge.sock";
const VMS: &str = "/var/lib/mothership/vms";

/// <linux/nl80211.h>
mod n {
    pub const CMD_GET_SCAN: u8 = 32;
    pub const CMD_TRIGGER_SCAN: u8 = 33;
    pub const CMD_NEW_SCAN_RESULTS: u8 = 34;
    pub const CMD_SCAN_ABORTED: u8 = 35;
    pub const CMD_ABORT_SCAN: u8 = 114;
    pub const CMD_CONNECT: u8 = 46;
    pub const CMD_DISCONNECT: u8 = 48;
    pub const CMD_NEW_KEY: u8 = 11;
    pub const CMD_DEL_KEY: u8 = 12;
    pub const CMD_SET_KEY: u8 = 10;
    pub const CMD_SET_STATION: u8 = 18;
    pub const CMD_GET_STATION: u8 = 17;
    pub const CMD_SET_POWER_SAVE: u8 = 61;
    pub const CMD_SET_CQM: u8 = 63;
    pub const CMD_NOTIFY_CQM: u8 = 64;
    pub const ATTR_IFINDEX: u16 = 3;
    pub const ATTR_WIPHY_FREQ: u16 = 38;
    pub const ATTR_MAC: u16 = 6;
    pub const ATTR_SSID: u16 = 52;
    pub const ATTR_IE: u16 = 42;
    pub const ATTR_AUTH_TYPE: u16 = 53;
    pub const ATTR_WPA_VERSIONS: u16 = 75;
    pub const ATTR_CIPHER_SUITES_PAIRWISE: u16 = 73;
    pub const ATTR_CIPHER_SUITE_GROUP: u16 = 74;
    pub const ATTR_AKM_SUITES: u16 = 76;
    pub const ATTR_CONTROL_PORT: u16 = 68;
    pub const ATTR_CONTROL_PORT_ETHERTYPE: u16 = 102;
    pub const ATTR_USE_MFP: u16 = 66;
    pub const ATTR_PRIVACY: u16 = 70;
    pub const ATTR_REASON_CODE: u16 = 54;
    pub const ATTR_STATUS_CODE: u16 = 72;
    pub const ATTR_REQ_IE: u16 = 77;
    pub const ATTR_RESP_IE: u16 = 78;
    pub const ATTR_TIMED_OUT: u16 = 65;
    pub const ATTR_DISCONNECTED_BY_AP: u16 = 71;
    pub const ATTR_KEY_IDX: u16 = 8;
    pub const ATTR_KEY_DATA: u16 = 7;
    pub const ATTR_KEY_CIPHER: u16 = 9;
    pub const ATTR_KEY_SEQ: u16 = 10;
    pub const ATTR_KEY_TYPE: u16 = 55;
    pub const ATTR_KEY_DEFAULT: u16 = 11;
    pub const ATTR_KEY_DEFAULT_MGMT: u16 = 40;
    pub const ATTR_KEY_DEFAULT_TYPES: u16 = 110;
    pub const ATTR_STA_FLAGS2: u16 = 67;
    pub const ATTR_STA_INFO: u16 = 21;
    pub const ATTR_PS_STATE: u16 = 93;
    pub const ATTR_CQM: u16 = 94;
    pub const ATTR_SCAN_SSIDS: u16 = 45;
    pub const ATTR_SCAN_FREQUENCIES: u16 = 44;
    pub const ATTR_BSS: u16 = 47;
    pub const KEY_DEFAULT_TYPE_UNICAST: u16 = 1;
    pub const KEY_DEFAULT_TYPE_MULTICAST: u16 = 2;
    pub const KEYTYPE_GROUP: u32 = 0;
    pub const KEYTYPE_PAIRWISE: u32 = 1;
    pub const BSS_BSSID: u16 = 1;
    pub const BSS_FREQUENCY: u16 = 2;
    pub const BSS_TSF: u16 = 3;
    pub const BSS_BEACON_INTERVAL: u16 = 4;
    pub const BSS_CAPABILITY: u16 = 5;
    pub const BSS_INFORMATION_ELEMENTS: u16 = 6;
    pub const BSS_SIGNAL_MBM: u16 = 7;
    pub const BSS_SEEN_MS_AGO: u16 = 10;
    pub const STA_INFO_SIGNAL: u16 = 7;
    pub const STA_INFO_TX_BITRATE: u16 = 8;
    pub const STA_INFO_RX_BITRATE: u16 = 14;
    pub const STA_INFO_TX_PACKETS: u16 = 10;
    pub const STA_INFO_RX_PACKETS: u16 = 9;
    pub const STA_INFO_TX_FAILED: u16 = 12;
    pub const RATE_INFO_BITRATE32: u16 = 5;
    pub const RATE_INFO_BITRATE: u16 = 1;
    pub const ATTR_CQM_RSSI_THOLD: u16 = 1;
    pub const ATTR_CQM_RSSI_HYST: u16 = 2;
    pub const ATTR_CQM_RSSI_THRESHOLD_EVENT: u16 = 3;
    pub const ATTR_CQM_RSSI_LEVEL: u16 = 9;
    pub const STA_FLAG_AUTHORIZED: u32 = 1;
    pub const PS_ENABLED: u32 = 1;
    pub const PS_DISABLED: u32 = 0;
}

type Res<T> = Result<T, (i32, String)>;

fn io(e: std::io::Error) -> (i32, String) {
    (e.raw_os_error().unwrap_or(libc::EIO), e.to_string())
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let o = Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if o.status.success() { Ok(()) } else { Err(format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim())) }
}

fn mac_bytes(s: &str) -> Vec<u8> {
    s.split(':').filter_map(|x| u8::from_str_radix(x, 16).ok()).collect()
}

fn mac_str(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":")
}

/// {"2g": [MHz...], "5g": [...]} of the wiphy's enabled channels (`iw phy <phy> channels`)
fn parse_channels(text: &str) -> BTreeMap<String, Vec<u32>> {
    let mut out = BTreeMap::from([("2g".to_string(), vec![]), ("5g".to_string(), vec![])]);
    for l in text.lines().map(str::trim) {
        if !(l.starts_with("* ") && l.contains("MHz") && !l.contains("disabled")) {
            continue;
        }
        let Some(f) = l.split_whitespace().nth(1).and_then(|f| f.parse::<f64>().ok()).map(|f| f as u32) else { continue };
        match f {
            0..3000 => out.get_mut("2g").unwrap().push(f),
            3000..5950 => out.get_mut("5g").unwrap().push(f),
            _ => {}
        }
    }
    out
}

fn conf_bands_ifaces(c: &Conf) -> Vec<String> {
    c.bands.values().filter(|i| ifindex(i) != 0).cloned().collect()
}

fn ifindex(name: &str) -> u32 {
    std::ffi::CString::new(name).map(|c| unsafe { libc::if_nametoindex(c.as_ptr()) }).unwrap_or(0)
}

fn band_iface_prepare(phy: &str, name: &str, mac: &str) -> Res<u32> {
    let e = |s: String| (libc::EIO, s);
    if ifindex(name) == 0 {
        run("iw", &["phy", phy, "interface", "add", name, "type", "managed"]).map_err(e)?;
    }
    let _ = run("ip", &["link", "set", "dev", name, "down"]);
    run("ip", &["link", "set", "dev", name, "address", mac]).map_err(e)?;
    // the host never talks here
    let _ = std::fs::write(format!("/proc/sys/net/ipv6/conf/{name}/disable_ipv6"), "1");
    run("ip", &["link", "set", "dev", name, "up"]).map_err(e)?;
    Ok(ifindex(name))
}

/// kernel-only data path: everything arriving on a goes out of b (the reverse is set separately)
fn redirect(a: &str, b: &str, on: bool) -> Result<(), String> {
    let _ = run("tc", &["qdisc", "del", "dev", a, "ingress"]);
    if on {
        run("tc", &["qdisc", "add", "dev", a, "handle", "ffff:", "ingress"])?;
        run("tc", &["filter", "add", "dev", a, "parent", "ffff:", "protocol", "all", "basic", "action", "mirred", "egress", "redirect", "dev", b])?;
    }
    Ok(())
}

fn ssid_of(ies: &[u8]) -> Vec<u8> {
    let mut off = 0;
    while off + 2 <= ies.len() {
        let (t, n) = (ies[off], ies[off + 1] as usize);
        if t == 0 {
            return ies[off + 2..(off + 2 + n).min(ies.len())].to_vec();
        }
        off += 2 + n;
    }
    vec![]
}

#[derive(Clone)]
struct VmConf {
    name: String,
    tap: String,
    mac: String,
}

struct Conf {
    phy: String,
    /// the host's own station interface (NetworkManager's)
    host_iface: String,
    /// band -> its interface
    bands: BTreeMap<String, String>,
}

/// The VM with this vsock CID: its native Wi-Fi card (mship_core::vmnet) and the MAC its wlan0 starts with.
fn vm_conf(cid: u32) -> Option<VmConf> {
    let net = mship_core::vmnet::by_cid(cid)?;
    let n = net.card("wifi")?;
    Some(VmConf { name: net.vm.clone(), tap: n.tap.clone(), mac: n.mac.clone() })
}

fn conf() -> Conf {
    let mut c = json!({
        "bands": {"2g": {"iface": "mshw2g"}, "5g": {"iface": "mshw5g"}},
    });
    if let Some(Value::Object(o)) = std::fs::read_to_string(CONF).ok().and_then(|s| serde_json::from_str(&s).ok()) {
        for (k, v) in o {
            c[k] = v;
        }
    }
    // the host's station and its radio: found (mship_core::device), unless wifi-bridge.json names them
    let host_iface = c["host_iface"].as_str().map(String::from).or_else(mship_core::device::wifi_iface).unwrap_or_else(|| "wlan0".into());
    let phy = c["phy"].as_str().map(String::from)
        .or_else(|| std::fs::read_to_string(format!("/sys/class/net/{host_iface}/phy80211/name")).ok().map(|n| n.trim().to_string()))
        .unwrap_or_else(|| "phy0".into());
    Conf {
        phy,
        host_iface,
        bands: c["bands"].as_object().into_iter().flatten().filter_map(|(b, v)| Some((b.clone(), v["iface"].as_str()?.to_string()))).collect(),
    }
}

/// One VM's driver connection.
struct Session {
    w: vsock::VsockStream,
    vm: VmConf,
    mac: String,
    /// the bands lent to this VM, all on one interface (`iface`: the first band's); the guest sees their channels
    bands: BTreeSet<String>,
    iface: Option<String>,
    ifidx: u32,
    /// the frequency of the connection being made or held (0: none)
    freq: u32,
    /// the BSS of the connection being made
    target: Option<String>,
    /// the connection being made, for a retry on another BSS: SSID and the CONNECT attributes without BSSID/frequency
    pending: Option<(Vec<u8>, Vec<u8>)>,
    /// BSSes whose connect timed out (bssid -> when): out of reach of the phone's radio, whatever the scan says
    unreachable: BTreeMap<String, Instant>,
    scan_id: u32,
    scan_started: Instant,
    connected: bool,
    connecting: bool,
    /// bssid -> (freq, ssid, signal) from the last scan
    cache: BTreeMap<String, (u32, Vec<u8>, i32)>,
    /// the connection this session belongs to (a reconnect replaces the session)
    serial: u64,
}

impl Session {
    fn send(&mut self, op: u16, id: u32, attrs: &[u8]) {
        let _ = self.w.write_all(&mw::message(op, id, attrs));
    }

    fn result(&mut self, id: u32, status: i32, extra: &[u8]) {
        self.send(mw::RESULT, id, &[a_s32(mw::A_STATUS, status), extra.to_vec()].concat());
    }
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<u32, Session>,
    /// cid -> band mothershipd lent (applied whenever the VM's driver is connected)
    desired: BTreeMap<u32, BTreeSet<String>>,
    serial: u64,
    /// the phone is suspending (`sleep` .. `resume`): no band is attached, lends are only recorded
    asleep: bool,
}

/// The host's Wi-Fi held off while VMs use Wi-Fi. The QCA6390 firmware crashes when it adds a second station's peer:
/// on the AP a VM is on (seen whenever the host joined, rejoined or roamed onto it) and on another AP of the network
/// (the host locked to another AP crashed it the same way, 2026-11-23). So the host's station stays disconnected
/// (`nmcli device disconnect`: also no autoconnect) from a VM's first connect until no VM has been connected or
/// connecting for HOST_RELEASE; freeing it at each disconnect or timeout let it rejoin while the VM was retrying.
#[derive(Default)]
struct HostPin {
    held: bool,
    idle_since: Option<Instant>,
}

const HOST_RELEASE: Duration = Duration::from_secs(60);

struct Bridge {
    conf: Conf,
    genl: Genl,
    channels: BTreeMap<String, Vec<u32>>,
    mfp_optional: bool,
    st: Mutex<State>,
    /// the host's NetworkManager connection kept off the access points VMs use
    host_pin: Mutex<HostPin>,
}

impl Bridge {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap()
    }

    fn band_channels(&self, band: Option<&str>) -> Vec<u32> {
        band.and_then(|b| self.channels.get(b)).cloned().unwrap_or_default()
    }

    /// The channels of every band the session holds.
    fn session_channels(&self, s: &Session) -> Vec<u32> {
        let mut ch: Vec<u32> = s.bands.iter().flat_map(|b| self.band_channels(Some(b))).collect();
        ch.sort();
        ch.dedup();
        ch
    }

    fn nl(&self, cmd: u8, attrs: &[u8]) -> Res<Vec<Attrs>> {
        self.genl.call(cmd, attrs, false).map_err(io)
    }

    fn dump(&self, cmd: u8, attrs: &[u8]) -> Res<Vec<Attrs>> {
        self.genl.call(cmd, attrs, true).map_err(io)
    }

    // ------------------------------------------------ attach / release
    fn attach(&self, st: &mut State, cid: u32, band: &str) {
        if st.sessions.get(&cid).is_none_or(|s| s.bands.contains(band)) {
            return;
        }
        let Some(band_iface) = self.conf.bands.get(band).cloned() else { return };
        let owner = st.sessions.iter().find(|(c, s)| **c != cid && s.bands.contains(band)).map(|(c, _)| *c);
        if let Some(o) = owner {
            self.detach_band(st, o, band, "revoked");
        }
        let s = st.sessions.get_mut(&cid).unwrap();
        if s.iface.is_none() {
            // the VM's first band: its interface, joined to the VM's card
            let prepared = band_iface_prepare(&self.conf.phy, &band_iface, &s.mac)
                .map_err(|e| e.1)
                .and_then(|idx| redirect(&band_iface, &s.vm.tap, true).and_then(|_| redirect(&s.vm.tap, &band_iface, true)).map(|_| idx))
                // the VM's card has carrier while a band is attached (it is blank otherwise: mship_core::vmnet)
                .and_then(|idx| run("ip", &["link", "set", &s.vm.tap, "up"]).map(|_| idx));
            match prepared {
                Ok(idx) => {
                    s.ifidx = idx;
                    s.iface = Some(band_iface.clone());
                }
                Err(e) => return log(format!("cid {cid}: attach {band}: {e}")),
            }
        }
        // a further band rides on the same interface (the firmware allows two stations: the host's and one more)
        s.bands.insert(band.into());
        let ch = self.session_channels(s);
        let s = st.sessions.get_mut(&cid).unwrap();
        log(format!("cid {cid}: attached {band} ({} <-> {}, {}; bands {:?})", s.iface.as_deref().unwrap_or("?"), s.vm.tap, s.mac, s.bands));
        s.send(mw::ATTACHED, 0, &a_u32s(mw::A_ATTACHED_CHANNELS, &ch));
    }

    /// One band taken from a VM: the others stay (fewer channels); the last one takes the interface with it.
    fn detach_band(&self, st: &mut State, cid: u32, band: &str, reason: &str) {
        let Some(s) = st.sessions.get_mut(&cid) else { return };
        if !s.bands.contains(band) {
            return;
        }
        if s.bands.len() == 1 {
            return self.detach(st, cid, reason);
        }
        s.bands.remove(band);
        let ch = self.session_channels(s);
        let s = st.sessions.get_mut(&cid).unwrap();
        // a connection on the band taken ends
        let on_band = self.band_channels(Some(band)).contains(&s.freq);
        if (s.connected || s.connecting) && on_band {
            let _ = self.nl(n::CMD_DISCONNECT, &[nla_u32(n::ATTR_IFINDEX, s.ifidx), nla_u16(n::ATTR_REASON_CODE, 3)].concat());
            s.send(mw::DISCONNECTED, 0, &[a_u16(mw::A_REASON, 3), a_u8(mw::A_LOCALLY_GENERATED, 1)].concat());
            s.connected = false;
            s.connecting = false;
        }
        s.cache.retain(|_, (f, _, _)| ch.contains(f));
        s.send(mw::ATTACHED, 0, &a_u32s(mw::A_ATTACHED_CHANNELS, &ch));
        log(format!("cid {cid}: released {band} ({reason}); bands {:?}", s.bands));
    }

    fn detach(&self, st: &mut State, cid: u32, reason: &str) {
        let Some(s) = st.sessions.get_mut(&cid) else { return };
        let Some(iface) = s.iface.clone() else { return };
        let bands = s.bands.clone();
        // deauth + keys are dropped by the disconnect; scan results go stale with the band
        let _ = self.nl(n::CMD_DISCONNECT, &[nla_u32(n::ATTR_IFINDEX, s.ifidx), nla_u16(n::ATTR_REASON_CODE, 3)].concat());
        let _ = redirect(&iface, &s.vm.tap, false);
        let _ = redirect(&s.vm.tap, &iface, false);
        let _ = run("ip", &["link", "set", &s.vm.tap, "down"]);
        let _ = run("ip", &["link", "set", "dev", &iface, "down"]);
        if s.connected || s.connecting {
            s.send(mw::DISCONNECTED, 0, &[a_u16(mw::A_REASON, 3), a_u8(mw::A_LOCALLY_GENERATED, 1)].concat());
        }
        s.connected = false;
        s.connecting = false;
        if s.scan_id != 0 {
            s.send(mw::SCAN_DONE, 0, &a_u8(mw::A_ABORTED, 1));
            s.scan_id = 0;
        }
        s.send(mw::DETACHED, 0, &a_u32(mw::A_DETACH_REASON, mw::detach_reason(reason)));
        log(format!("cid {cid}: released {bands:?} ({reason})"));
        s.bands.clear();
        s.iface = None;
        s.ifidx = 0;
        s.cache.clear();
    }

    // ------------------------------------------------ requests
    fn handle(&self, cid: u32, op: u16, id: u32, a: &Attrs) {
        let mut st = self.lock();
        if !st.sessions.contains_key(&cid) {
            return;
        }
        let r = match op {
            mw::HELLO => {
                self.r_hello(&mut st, cid, id, a);
                return;
            }
            mw::SCAN => self.r_scan(&mut st, cid, a).map(|_| vec![]),
            mw::ABORT_SCAN => {
                let idx = st.sessions[&cid].ifidx;
                if idx != 0 {
                    let _ = self.nl(n::CMD_ABORT_SCAN, &nla_u32(n::ATTR_IFINDEX, idx));
                }
                Ok(vec![])
            }
            mw::CONNECT => self.r_connect(&mut st, cid, a).map(|_| vec![]),
            mw::DISCONNECT => {
                let s = st.sessions.get_mut(&cid).unwrap();
                s.connecting = false;
                s.connected = false;
                if s.ifidx != 0 {
                    let reason = get_u16(a, mw::A_REASON).unwrap_or(3);
                    let _ = self.nl(n::CMD_DISCONNECT, &[nla_u32(n::ATTR_IFINDEX, s.ifidx), nla_u16(n::ATTR_REASON_CODE, reason)].concat());
                }
                Ok(vec![])
            }
            mw::SET_MAC => self.r_set_mac(&mut st, cid, a).map(|_| vec![]),
            mw::ADD_KEY => self.r_add_key(&st.sessions[&cid], a).map(|_| vec![]),
            mw::DEL_KEY => {
                let mut at = [nla_u32(n::ATTR_IFINDEX, st.sessions[&cid].ifidx), nla_u8(n::ATTR_KEY_IDX, get_u8(a, mw::A_KEY_IDX).unwrap_or(0))].concat();
                if let Some(m) = a.get(&mw::A_MAC) {
                    at.extend(nla(n::ATTR_MAC, m));
                }
                let _ = self.nl(n::CMD_DEL_KEY, &at); // deleting a key that is already gone is fine
                Ok(vec![])
            }
            mw::SET_DEFAULT_KEY => self.need_band(&st.sessions[&cid]).and_then(|idx| {
                let mut types = vec![];
                if get_u8(a, mw::A_UNICAST).unwrap_or(0) != 0 {
                    types.push(nla_flag(n::KEY_DEFAULT_TYPE_UNICAST));
                }
                if get_u8(a, mw::A_MULTICAST).unwrap_or(0) != 0 {
                    types.push(nla_flag(n::KEY_DEFAULT_TYPE_MULTICAST));
                }
                self.nl(n::CMD_SET_KEY, &[nla_u32(n::ATTR_IFINDEX, idx), nla_u8(n::ATTR_KEY_IDX, get_u8(a, mw::A_KEY_IDX).unwrap_or(0)),
                                          nla_flag(n::ATTR_KEY_DEFAULT), nla_nest(n::ATTR_KEY_DEFAULT_TYPES, types)].concat())
                    .map(|_| vec![])
            }),
            mw::SET_DEFAULT_MGMT_KEY => self.need_band(&st.sessions[&cid]).and_then(|idx| {
                self.nl(n::CMD_SET_KEY, &[nla_u32(n::ATTR_IFINDEX, idx), nla_u8(n::ATTR_KEY_IDX, get_u8(a, mw::A_KEY_IDX).unwrap_or(0)),
                                          nla_flag(n::ATTR_KEY_DEFAULT_MGMT)].concat())
                    .map(|_| vec![])
            }),
            mw::SET_STATION => self.need_band(&st.sessions[&cid]).and_then(|idx| {
                let bit = 1u32 << n::STA_FLAG_AUTHORIZED;
                let set = if get_u8(a, mw::A_AUTHORIZED).unwrap_or(0) != 0 { bit } else { 0 };
                let upd = [bit.to_le_bytes(), set.to_le_bytes()].concat();
                self.nl(n::CMD_SET_STATION, &[nla_u32(n::ATTR_IFINDEX, idx), nla(n::ATTR_MAC, a.get(&mw::A_MAC).map_or(&[][..], |v| v)),
                                              nla(n::ATTR_STA_FLAGS2, &upd)].concat())
                    .map(|_| vec![])
            }),
            mw::GET_STATION => self.r_get_station(&st.sessions[&cid], a),
            mw::SET_POWER_SAVE => {
                let idx = st.sessions[&cid].ifidx;
                if idx == 0 {
                    Ok(vec![])
                } else {
                    let ps = if get_u8(a, mw::A_ENABLED).unwrap_or(0) != 0 { n::PS_ENABLED } else { n::PS_DISABLED };
                    self.nl(n::CMD_SET_POWER_SAVE, &[nla_u32(n::ATTR_IFINDEX, idx), nla_u32(n::ATTR_PS_STATE, ps)].concat()).map(|_| vec![])
                }
            }
            mw::SET_CQM => self.need_band(&st.sessions[&cid]).and_then(|idx| {
                let cqm = [nla(n::ATTR_CQM_RSSI_THOLD, &get_i32(a, mw::A_THRESHOLD).unwrap_or(0).to_le_bytes()),
                           nla_u32(n::ATTR_CQM_RSSI_HYST, get_u32(a, mw::A_HYSTERESIS).unwrap_or(0))];
                self.nl(n::CMD_SET_CQM, &[nla_u32(n::ATTR_IFINDEX, idx), nla_nest(n::ATTR_CQM, cqm)].concat()).map(|_| vec![])
            }),
            _ => Err((libc::EOPNOTSUPP, "op from a newer minor".into())),
        };
        let s = st.sessions.get_mut(&cid).unwrap();
        match r {
            Ok(extra) => s.result(id, 0, &extra),
            Err((e, msg)) => {
                log(format!("cid {cid}: op {op} failed: {msg}"));
                s.result(id, -e, &[]);
            }
        }
    }

    fn need_band(&self, s: &Session) -> Res<u32> {
        if s.iface.is_none() {
            return Err((libc::ENETDOWN, "band not attached".into()));
        }
        Ok(s.ifidx)
    }

    fn r_hello(&self, st: &mut State, cid: u32, id: u32, a: &Attrs) {
        let ver = String::from_utf8_lossy(a.get(&mw::A_VERSION).map_or(&[][..], |v| v)).into_owned();
        let s = st.sessions.get_mut(&cid).unwrap();
        if !ver.starts_with("1.") {
            s.send(mw::HELLO_REPLY, id, &[]);
            return;
        }
        if let Some(m) = a.get(&mw::A_MAC).filter(|m| m.len() == 6) {
            s.mac = mac_str(m);
        }
        let mut all: Vec<u32> = self.channels.values().flatten().copied().collect();
        all.sort();
        all.dedup();
        let attached = self.session_channels(s);
        s.send(mw::HELLO_REPLY, id, &[attr(mw::A_VERSION, mw::VERSION.as_bytes()), a_u32s(mw::A_CHANNELS, &all), a_u32s(mw::A_ATTACHED_CHANNELS, &attached)].concat());
        log(format!("cid {cid}: hello {ver}, mac {}", s.mac));
        // lent before the VM's driver connected (applied at resume while the phone suspends)
        if !st.asleep {
            for band in st.desired.get(&cid).cloned().unwrap_or_default() {
                self.attach(st, cid, &band);
            }
        }
    }

    fn r_scan(&self, st: &mut State, cid: u32, a: &Attrs) -> Res<()> {
        let s = st.sessions.get_mut(&cid).unwrap();
        let idx = self.need_band(s)?;
        let allowed = self.session_channels(s);
        let mut freqs: Vec<u32> = get_u32s(a, mw::A_FREQS).into_iter().filter(|f| allowed.contains(f)).collect();
        if freqs.is_empty() {
            freqs = allowed;
        }
        let raw = a.get(&mw::A_SSIDS).cloned().unwrap_or_default();
        let mut ssids = vec![];
        let mut off = 0;
        while off < raw.len() {
            let l = raw[off] as usize;
            ssids.push(raw[off + 1..(off + 1 + l).min(raw.len())].to_vec());
            off += 1 + l;
        }
        if ssids.is_empty() {
            ssids.push(vec![]);
        }
        let mut at = nla_u32(n::ATTR_IFINDEX, idx);
        at.extend(nla_nest(n::ATTR_SCAN_FREQUENCIES, freqs.iter().enumerate().map(|(i, f)| nla_u32(i as u16, *f))));
        at.extend(nla_nest(n::ATTR_SCAN_SSIDS, ssids.iter().enumerate().map(|(i, s)| nla(i as u16, s))));
        if let Some(ies) = a.get(&mw::A_IES).filter(|i| !i.is_empty()) {
            at.extend(nla(n::ATTR_IE, ies));
        }
        s.scan_id = u32::MAX; // set below to the request id; nonzero now so the event path knows a scan runs
        s.scan_started = Instant::now();
        match self.nl(n::CMD_TRIGGER_SCAN, &at) {
            Ok(_) => Ok(()),
            Err(e) => {
                st.sessions.get_mut(&cid).unwrap().scan_id = 0;
                Err(e)
            }
        }
    }

    /// The host's access point (BSSID) now, if it is associated.
    fn host_bssid(&self) -> Option<String> {
        let out = Command::new("iw").args(["dev", &self.conf.host_iface, "link"]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.strip_prefix("Connected to ")).and_then(|r| r.split_whitespace().next()).map(str::to_lowercase)
    }

    /// A VM joins an AP (or uses Wi-Fi while the host came back): the host's station disconnects, and stays so.
    fn host_leave(&self, _bssid: &[u8], _freq: u32) -> Res<()> {
        let mut pin = self.host_pin.lock().unwrap();
        pin.idle_since = None;
        if !pin.held {
            log("a VM uses Wi-Fi: the host's station is held disconnected");
            pin.held = true;
        }
        // also while it is only activating (no BSSID yet): the device-level disconnect stops that and autoconnect
        let _ = Command::new("nmcli").args(["device", "disconnect", &self.conf.host_iface]).output();
        if let Some(b) = self.host_bssid() {
            return Err((libc::EBUSY, format!("the host is still on {b}")));
        }
        Ok(())
    }

    /// Every 2 s: the host disconnected again if it got associated anyway while VMs use Wi-Fi, and released once no
    /// VM has used Wi-Fi for HOST_RELEASE.
    fn host_guard(&self) {
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let in_use = {
                let st = self.lock();
                st.sessions.values().any(|s| s.connected || s.connecting)
            };
            if !in_use {
                let mut pin = self.host_pin.lock().unwrap();
                if !pin.held || pin.idle_since.get_or_insert_with(Instant::now).elapsed() < HOST_RELEASE {
                    continue;
                }
                *pin = HostPin::default();
                drop(pin);
                log("no VM on Wi-Fi: the host's station is free again");
                let _ = Command::new("timeout").args(["20", "nmcli", "device", "connect", &self.conf.host_iface]).output();
                continue;
            }
            let held = self.host_pin.lock().unwrap().held;
            if !held || self.host_bssid().is_some() {
                if let Some(h) = self.host_bssid() {
                    log(format!("the host got onto {h} while a VM uses Wi-Fi"));
                }
                let _ = self.host_leave(&[], 0);
            }
        }
    }

    fn r_connect(&self, st: &mut State, cid: u32, a: &Attrs) -> Res<()> {
        let s = st.sessions.get_mut(&cid).unwrap();
        let idx = self.need_band(s)?;
        let allowed = self.session_channels(s);
        let ssid = a.get(&mw::A_SSID).cloned().unwrap_or_default();
        let mut bssid = a.get(&mw::A_BSSID).or(a.get(&mw::A_BSSID_HINT)).cloned();
        let mut freq = get_u32(a, mw::A_FREQ).filter(|f| *f != 0).or(get_u32(a, mw::A_FREQ_HINT)).unwrap_or(0);
        if let Some(b) = &bssid
            && freq == 0
            && let Some(c) = s.cache.get(&mac_str(b))
        {
            freq = c.0;
        }
        if bssid.is_none() || (freq != 0 && !allowed.contains(&freq)) {
            // pick the best BSS of this SSID in our band
            if let Some((_, b, f)) = s.cache.iter().filter(|(_, (f, s, _))| *s == ssid && allowed.contains(f)).map(|(b, (f, _, sig))| (*sig, b.clone(), *f)).max() {
                bssid = Some(mac_bytes(&b));
                freq = f;
            }
        }
        // a BSS that timed out recently: the strongest other one of this SSID instead. The guest would report the
        // timeout as an association rejection, which Android counts against the whole network and soon disables it.
        s.unreachable.retain(|_, t| t.elapsed() < Duration::from_secs(300));
        if let Some(b) = bssid.as_deref().map(mac_str)
            && s.unreachable.contains_key(&b)
            && let Some((_, nb, nf)) = s.cache.iter()
                .filter(|(c, (f, cs, _))| *cs == ssid && allowed.contains(f) && !s.unreachable.contains_key(*c))
                .map(|(c, (f, _, sig))| (*sig, c.clone(), *f))
                .max()
        {
            log(format!("cid {cid}: {b} timed out recently, connecting to {nb} ({nf} MHz) instead"));
            bssid = Some(mac_bytes(&nb));
            freq = nf;
        }
        if freq != 0 && !allowed.contains(&freq) {
            return Err((libc::EINVAL, format!("{freq} MHz is not in the attached band")));
        }
        let mut at = [nla_u32(n::ATTR_IFINDEX, idx), nla(n::ATTR_SSID, &ssid)].concat();
        if let Some(ies) = a.get(&mw::A_IES).filter(|i| !i.is_empty()) {
            at.extend(nla(n::ATTR_IE, ies));
        }
        let auth = get_u32(a, mw::A_AUTH_TYPE).unwrap_or(0);
        at.extend(nla_u32(n::ATTR_AUTH_TYPE, auth));
        let wpa = get_u32(a, mw::A_WPA_VERSIONS).unwrap_or(0);
        let pw = a.get(&mw::A_CIPHERS_PAIRWISE).cloned().unwrap_or_default();
        if wpa != 0 {
            at.extend(nla_u32(n::ATTR_WPA_VERSIONS, wpa));
        }
        if !pw.is_empty() {
            at.extend(nla(n::ATTR_CIPHER_SUITES_PAIRWISE, &pw));
        }
        if let Some(g) = get_u32(a, mw::A_CIPHER_GROUP).filter(|g| *g != 0) {
            at.extend(nla_u32(n::ATTR_CIPHER_SUITE_GROUP, g));
        }
        if let Some(akms) = a.get(&mw::A_AKMS).filter(|v| !v.is_empty()) {
            at.extend(nla(n::ATTR_AKM_SUITES, akms));
        }
        if wpa != 0 || !pw.is_empty() {
            at.extend(nla_flag(n::ATTR_PRIVACY));
        }
        if let Some(et) = get_u16(a, mw::A_CONTROL_PORT_ETHERTYPE).filter(|e| *e != 0) {
            at.extend(nla_flag(n::ATTR_CONTROL_PORT));
            at.extend(nla_u16(n::ATTR_CONTROL_PORT_ETHERTYPE, et)); // host order; nl80211 converts
        }
        let mut mfp = get_u32(a, mw::A_MFP).unwrap_or(0);
        if mfp == 2 && !self.mfp_optional {
            // NL80211_MFP_OPTIONAL unsupported by the host wiphy: connect without PMF (fine for WPA2-PSK)
            mfp = 0;
        }
        if mfp != 0 {
            at.extend(nla_u32(n::ATTR_USE_MFP, mfp));
        }
        let base = at.clone();
        if let Some(b) = &bssid {
            at.extend(nla(n::ATTR_MAC, b));
        }
        if freq != 0 {
            at.extend(nla_u32(n::ATTR_WIPHY_FREQ, freq));
        }
        if let Some(b) = &bssid {
            self.host_leave(b, freq)?;
        }
        let s = st.sessions.get_mut(&cid).unwrap();
        s.connecting = true;
        s.connected = false;
        s.freq = freq;
        s.target = bssid.as_deref().map(mac_str);
        s.pending = Some((ssid.clone(), base));
        self.nl(n::CMD_CONNECT, &at)?;
        log(format!("cid {cid}: connect {:?} {} {} MHz auth {auth} wpa {wpa} mfp {mfp}", String::from_utf8_lossy(&ssid),
                    bssid.as_deref().map_or("?".into(), mac_str), if freq != 0 { freq.to_string() } else { "?".into() }));
        Ok(())
    }

    fn r_set_mac(&self, st: &mut State, cid: u32, a: &Attrs) -> Res<()> {
        let s = st.sessions.get_mut(&cid).unwrap();
        let m = a.get(&mw::A_MAC).cloned().unwrap_or_default();
        if m.len() != 6 || m[0] & 2 == 0 || m[0] & 1 != 0 {
            return Err((libc::EINVAL, "MAC must be locally administered unicast".into()));
        }
        if s.connected || s.connecting {
            return Err((libc::EBUSY, "connected".into()));
        }
        s.mac = mac_str(&m);
        if let Some(iface) = s.iface.clone() {
            // re-prepare the interface with the new address
            s.ifidx = band_iface_prepare(&self.conf.phy, &iface, &s.mac)?;
        }
        Ok(())
    }

    fn r_add_key(&self, s: &Session, a: &Attrs) -> Res<()> {
        let idx = self.need_band(s)?;
        let mut key = [nla(n::ATTR_KEY_DATA, a.get(&mw::A_KEY).map_or(&[][..], |v| v)), nla_u8(n::ATTR_KEY_IDX, get_u8(a, mw::A_KEY_IDX).unwrap_or(0)),
                       nla_u32(n::ATTR_KEY_CIPHER, get_u32(a, mw::A_CIPHER).unwrap_or(0))].concat();
        if let Some(seq) = a.get(&mw::A_SEQ).filter(|v| !v.is_empty()) {
            key.extend(nla(n::ATTR_KEY_SEQ, seq));
        }
        let pairwise = get_u8(a, mw::A_PAIRWISE).unwrap_or(0) != 0;
        key.extend(nla_u32(n::ATTR_KEY_TYPE, if pairwise { n::KEYTYPE_PAIRWISE } else { n::KEYTYPE_GROUP }));
        if let Some(m) = a.get(&mw::A_MAC).filter(|v| !v.is_empty()) {
            key.extend(nla(n::ATTR_MAC, m));
        }
        self.nl(n::CMD_NEW_KEY, &[nla_u32(n::ATTR_IFINDEX, idx), key].concat()).map(|_| ())
    }

    fn r_get_station(&self, s: &Session, a: &Attrs) -> Res<Vec<u8>> {
        let idx = self.need_band(s)?;
        let st = self.nl(n::CMD_GET_STATION, &[nla_u32(n::ATTR_IFINDEX, idx), nla(n::ATTR_MAC, a.get(&mw::A_MAC).map_or(&[][..], |v| v))].concat())?;
        let info = st.first().and_then(|x| x.get(&n::ATTR_STA_INFO)).map(|v| nl::parse(v)).unwrap_or_default();
        let rate = |k: u16| {
            let r = info.get(&k).map(|v| nl::parse(v)).unwrap_or_default();
            get_u32(&r, n::RATE_INFO_BITRATE32).or(get_u16(&r, n::RATE_INFO_BITRATE).map(u32::from)).unwrap_or(10)
        };
        let mut extra = vec![];
        if let Some(sig) = info.get(&n::STA_INFO_SIGNAL).and_then(|v| v.first()) {
            extra.extend(a_s32(mw::A_SIGNAL, *sig as i8 as i32));
        }
        extra.extend(a_u32(mw::A_TX_BITRATE, rate(n::STA_INFO_TX_BITRATE)));
        extra.extend(a_u32(mw::A_RX_BITRATE, rate(n::STA_INFO_RX_BITRATE)));
        for (k, t) in [(n::STA_INFO_TX_PACKETS, mw::A_TX_PACKETS), (n::STA_INFO_RX_PACKETS, mw::A_RX_PACKETS), (n::STA_INFO_TX_FAILED, mw::A_TX_FAILED)] {
            if let Some(v) = get_u32(&info, k) {
                extra.extend(a_u32(t, v));
            }
        }
        Ok(extra)
    }

    // ------------------------------------------------ nl80211 events for a session's interface
    fn on_scan_done(&self, st: &mut State, cid: u32, aborted: bool) {
        let s = st.sessions.get_mut(&cid).unwrap();
        if s.scan_id == 0 {
            return;
        }
        let allowed = self.session_channels(s);
        let mut count = 0;
        if !aborted {
            s.cache.clear();
            let window = s.scan_started.elapsed().as_millis() as u32 + 1000;
            for bss in self.dump(n::CMD_GET_SCAN, &nla_u32(n::ATTR_IFINDEX, s.ifidx)).unwrap_or_default() {
                let b = bss.get(&n::ATTR_BSS).map(|v| nl::parse(v)).unwrap_or_default();
                let (Some(bssid), Some(freq)) = (b.get(&n::BSS_BSSID).cloned(), get_u32(&b, n::BSS_FREQUENCY)) else { continue };
                if !allowed.contains(&freq) || get_u32(&b, n::BSS_SEEN_MS_AGO).unwrap_or(0) > window {
                    continue; // other band, or older than this scan
                }
                let ies = b.get(&n::BSS_INFORMATION_ELEMENTS).cloned().unwrap_or_default();
                let sig = get_i32(&b, n::BSS_SIGNAL_MBM).unwrap_or(-8000);
                s.cache.insert(mac_str(&bssid), (freq, ssid_of(&ies), sig));
                let mut m = [attr(mw::A_BSSID, &bssid), a_u32(mw::A_FREQ, freq), a_s32(mw::A_SIGNAL_MBM, sig),
                             a_u16(mw::A_CAPABILITY, get_u16(&b, n::BSS_CAPABILITY).unwrap_or(1)),
                             a_u16(mw::A_BEACON_INT, get_u16(&b, n::BSS_BEACON_INTERVAL).unwrap_or(100))].concat();
                if let Some(tsf) = b.get(&n::BSS_TSF) {
                    m.extend(attr(mw::A_TSF, tsf));
                }
                m.extend(attr(mw::A_IES, &ies));
                s.send(mw::SCAN_RESULT, 0, &m);
                count += 1;
            }
        }
        s.send(mw::SCAN_DONE, 0, &a_u8(mw::A_ABORTED, aborted as u8));
        log(format!("cid {cid}: scan {}, {count} BSS in {}", if aborted { "aborted" } else { "done" }, s.bands.iter().cloned().collect::<Vec<_>>().join("+")));
        s.scan_id = 0;
    }

    /// BSS fields of the connected AP so the guest's cfg80211 knows it
    fn bss_info(&self, ifidx: u32, bssid: &[u8]) -> Vec<u8> {
        for bss in self.dump(n::CMD_GET_SCAN, &nla_u32(n::ATTR_IFINDEX, ifidx)).unwrap_or_default() {
            let b = bss.get(&n::ATTR_BSS).map(|v| nl::parse(v)).unwrap_or_default();
            if b.get(&n::BSS_BSSID).map(Vec::as_slice) != Some(bssid) {
                continue;
            }
            let mut out = a_u32(mw::A_FREQ, get_u32(&b, n::BSS_FREQUENCY).unwrap_or(0));
            if let Some(s) = get_i32(&b, n::BSS_SIGNAL_MBM) {
                out.extend(a_s32(mw::A_SIGNAL_MBM, s));
            }
            if let Some(c) = get_u16(&b, n::BSS_CAPABILITY) {
                out.extend(a_u16(mw::A_CAPABILITY, c));
            }
            if let Some(bi) = get_u16(&b, n::BSS_BEACON_INTERVAL) {
                out.extend(a_u16(mw::A_BEACON_INT, bi));
            }
            if let Some(tsf) = b.get(&n::BSS_TSF) {
                out.extend(attr(mw::A_TSF, tsf));
            }
            out.extend(attr(mw::A_BSS_IES, b.get(&n::BSS_INFORMATION_ELEMENTS).map_or(&[][..], |v| v)));
            return out;
        }
        vec![]
    }

    /// The connect timed out (the BSS does not answer): the same connection to the strongest other BSS of the SSID
    /// in the VM's bands that has not timed out. The guest sees only the final result: it would report a timeout as
    /// an association rejection, which Android counts against the whole network and then disables it.
    fn retry_elsewhere(&self, st: &mut State, cid: u32) -> bool {
        let s = st.sessions.get_mut(&cid).unwrap();
        if let Some(b) = s.target.clone() {
            s.unreachable.insert(b, Instant::now());
        }
        let Some((ssid, base)) = s.pending.clone() else { return false };
        let allowed = self.session_channels(s);
        let s = st.sessions.get_mut(&cid).unwrap();
        let Some((_, nb, nf)) = s.cache.iter()
            .filter(|(c, (f, cs, _))| *cs == ssid && allowed.contains(f) && !s.unreachable.contains_key(*c))
            .map(|(c, (f, _, sig))| (*sig, c.clone(), *f))
            .max()
        else {
            return false;
        };
        log(format!("cid {cid}: {} does not answer, trying {nb} ({nf} MHz)", s.target.as_deref().unwrap_or("?")));
        let b = mac_bytes(&nb);
        if self.host_leave(&b, nf).is_err() {
            return false;
        }
        let s = st.sessions.get_mut(&cid).unwrap();
        s.target = Some(nb);
        s.freq = nf;
        let at = [base, nla(n::ATTR_MAC, &b), nla_u32(n::ATTR_WIPHY_FREQ, nf)].concat();
        self.nl(n::CMD_CONNECT, &at).is_ok()
    }

    fn on_connect(&self, st: &mut State, cid: u32, a: &Attrs) {
        let status = get_u16(a, n::ATTR_STATUS_CODE).unwrap_or(1);
        let timed_out = a.contains_key(&n::ATTR_TIMED_OUT);
        if timed_out && st.sessions[&cid].connecting && self.retry_elsewhere(st, cid) {
            log(format!("cid {cid}: connect result {status} (timeout), retrying"));
            return;
        }
        let ifidx = st.sessions[&cid].ifidx;
        let mut extra = a_s32(mw::A_STATUS, status as i32);
        if let Some(b) = a.get(&n::ATTR_MAC) {
            extra.extend(attr(mw::A_BSSID, b));
            extra.extend(self.bss_info(ifidx, b));
        }
        for (k, t) in [(n::ATTR_REQ_IE, mw::A_REQ_IES), (n::ATTR_RESP_IE, mw::A_RESP_IES)] {
            if let Some(v) = a.get(&k) {
                extra.extend(attr(t, v));
            }
        }
        if timed_out {
            extra.extend(a_u8(mw::A_TIMED_OUT, 1));
        }
        let s = st.sessions.get_mut(&cid).unwrap();
        s.connecting = false;
        s.connected = status == 0 && !timed_out;
        s.pending = None;
        s.send(mw::CONNECT_RESULT, 0, &extra);
        log(format!("cid {cid}: connect result {status}{}", if timed_out { " (timeout)" } else { "" }));
    }

    fn on_disconnect(&self, st: &mut State, cid: u32, a: &Attrs) {
        let s = st.sessions.get_mut(&cid).unwrap();
        if !(s.connected || s.connecting) {
            return;
        }
        let reason = get_u16(a, n::ATTR_REASON_CODE).unwrap_or(1);
        s.connected = false;
        s.connecting = false;
        s.send(mw::DISCONNECTED, 0, &[a_u16(mw::A_REASON, reason), a_u8(mw::A_LOCALLY_GENERATED, !a.contains_key(&n::ATTR_DISCONNECTED_BY_AP) as u8)].concat());
        log(format!("cid {cid}: disconnected ({reason})"));
    }

    fn on_cqm(&self, st: &mut State, cid: u32, a: &Attrs) {
        let c = a.get(&n::ATTR_CQM).map(|v| nl::parse(v)).unwrap_or_default();
        if let Some(ev) = get_u32(&c, n::ATTR_CQM_RSSI_THRESHOLD_EVENT) {
            let mut m = a_u32(mw::A_RSSI_EVENT, ev);
            if let Some(l) = get_i32(&c, n::ATTR_CQM_RSSI_LEVEL) {
                m.extend(a_s32(mw::A_SIGNAL, l));
            }
            st.sessions.get_mut(&cid).unwrap().send(mw::CQM, 0, &m);
        }
    }

    fn event_loop(self: Arc<Self>) {
        let fd = match self.genl.events(&["scan", "mlme"]) {
            Ok(f) => f,
            Err(e) => {
                log(format!("nl80211 events: {e}"));
                std::process::exit(1);
            }
        };
        loop {
            let Ok(evs) = nl::read_events(fd.as_raw_fd(), self.genl.family) else { continue };
            for (cmd, a) in evs {
                let idx = get_u32(&a, n::ATTR_IFINDEX).unwrap_or(0);
                let mut st = self.lock();
                let Some(cid) = st.sessions.iter().find(|(_, s)| idx != 0 && s.ifidx == idx).map(|(c, _)| *c) else { continue };
                match cmd {
                    n::CMD_NEW_SCAN_RESULTS => self.on_scan_done(&mut st, cid, false),
                    n::CMD_SCAN_ABORTED => self.on_scan_done(&mut st, cid, true),
                    n::CMD_CONNECT => self.on_connect(&mut st, cid, &a),
                    n::CMD_DISCONNECT => self.on_disconnect(&mut st, cid, &a),
                    n::CMD_NOTIFY_CQM => self.on_cqm(&mut st, cid, &a),
                    _ => {}
                }
            }
        }
    }

    fn control(&self, msg: &str) -> String {
        let w: Vec<&str> = msg.split_whitespace().collect();
        let mut st = self.lock();
        // a VM by its vsock CID or its directory name (the broker's generic lend: attach <vm> <band>, detach <vm> <reason> <band>)
        let as_cid = |v: &str| v.parse::<u32>().ok().or_else(|| {
            let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(Path::new(VMS).join(v).join("vm.json")).ok()?).ok()?;
            m["cid"].as_u64().map(|c| c as u32)
        });
        let w: Vec<String> = w.iter().enumerate().map(|(i, x)| if i == 1 { as_cid(x).map(|c| c.to_string()).unwrap_or_else(|| x.to_string()) } else { x.to_string() }).collect();
        let w: Vec<&str> = w.iter().map(String::as_str).collect();
        match w.as_slice() {
            ["attach", cid, band, ..] if self.conf.bands.contains_key(*band) && cid.parse::<u32>().is_ok() => {
                let cid: u32 = cid.parse().unwrap();
                let others: Vec<u32> = st.desired.iter().filter(|(c, b)| b.contains(*band) && **c != cid).map(|(c, _)| *c).collect();
                for o in others {
                    // one owner per band
                    if let Some(b) = st.desired.get_mut(&o) {
                        b.remove(*band);
                    }
                    self.detach_band(&mut st, o, band, "revoked");
                }
                st.desired.entry(cid).or_default().insert(band.to_string());
                if !st.asleep {
                    self.attach(&mut st, cid, band);
                }
                "ok".into()
            }
            ["detach", cid, rest @ ..] if cid.parse::<u32>().is_ok() => {
                let cid: u32 = cid.parse().unwrap();
                let reason = rest.first().copied().unwrap_or("user");
                // `detach <cid> <reason> <band>`: that band; without one, all of them
                match rest.get(1).filter(|b| self.conf.bands.contains_key(**b)) {
                    Some(band) => {
                        if let Some(b) = st.desired.get_mut(&cid) {
                            b.remove(*band);
                        }
                        self.detach_band(&mut st, cid, band, reason);
                    }
                    None => {
                        st.desired.remove(&cid);
                        self.detach(&mut st, cid, reason);
                    }
                }
                "ok".into()
            }
            // the phone suspends (systemd sleep hook): no traffic may be queued on the wiphy (ath11k cannot flush a
            // band interface's frames and its suspend fails, then its resume times out); the VMs see their band
            // detached as for a blanked screen, and get it back on resume (their supplicant reconnects)
            ["sleep", ..] => {
                st.asleep = true;
                let cids: Vec<u32> = st.sessions.iter().filter(|(_, s)| s.iface.is_some()).map(|(c, _)| *c).collect();
                for cid in &cids {
                    self.detach(&mut st, *cid, "screen");
                }
                log(format!("sleep: released the bands of {cids:?}"));
                "ok".into()
            }
            ["resume", ..] => {
                st.asleep = false;
                // NetworkManager wakes too: the held host must not associate while the VMs reconnect
                if self.host_pin.lock().unwrap().held {
                    let _ = Command::new("nmcli").args(["device", "disconnect", &self.conf.host_iface]).output();
                }
                let lent: Vec<(u32, BTreeSet<String>)> = st.desired.iter().map(|(c, b)| (*c, b.clone())).collect();
                for (cid, bands) in lent {
                    for band in bands {
                        self.attach(&mut st, cid, &band);
                    }
                }
                log("resume: bands attached again");
                "ok".into()
            }
            ["status", ..] => json!({
                "sessions": st.sessions.iter().map(|(c, s)| (c.to_string(), json!({"bands": s.bands, "iface": s.iface, "mac": s.mac, "connected": s.connected}))).collect::<serde_json::Map<_, _>>(),
                "lent": st.desired.iter().map(|(c, b)| (c.to_string(), json!(b))).collect::<serde_json::Map<_, _>>(),
                "asleep": st.asleep,
            })
            .to_string(),
            _ => "error: attach <vm|cid> <band> | detach <vm|cid> [reason] | sleep | resume | status".into(),
        }
    }

    fn control_loop(self: Arc<Self>) {
        fresh(Path::new(CTL));
        let s = match UnixDatagram::bind(CTL) {
            Ok(s) => s,
            Err(e) => {
                log(format!("{CTL}: {e}"));
                std::process::exit(1);
            }
        };
        for_session(Path::new(CTL), 0o660);
        let mut m = [0u8; 512];
        loop {
            let Ok((len, peer)) = s.recv_from(&mut m) else { continue };
            let reply = self.control(&String::from_utf8_lossy(&m[..len]));
            if let Some(p) = peer.as_pathname() {
                let _ = s.send_to(reply.as_bytes(), p);
            }
        }
    }

    /// One driver connection: requests until it closes.
    fn session(self: Arc<Self>, mut r: vsock::VsockStream, cid: u32, serial: u64) {
        let res = (|| -> std::io::Result<()> {
            loop {
                let mut h = [0u8; mw::HDR];
                r.read_exact(&mut h)?;
                let (len, op, id) = mw::header(&h);
                if !(mw::HDR..=mw::MAX_MSG).contains(&len) {
                    return Ok(());
                }
                let mut body = vec![0u8; len - mw::HDR];
                r.read_exact(&mut body)?;
                let a = mw::parse_attrs(&body);
                if op == mw::SCAN {
                    // the guest's scan id: SCAN_DONE belongs to it
                    self.handle(cid, op, id, &a);
                    if let Some(s) = self.lock().sessions.get_mut(&cid).filter(|s| s.scan_id == u32::MAX) {
                        s.scan_id = id.max(1);
                    }
                } else {
                    self.handle(cid, op, id, &a);
                }
            }
        })();
        let _ = res;
        let mut st = self.lock();
        if st.sessions.get(&cid).is_some_and(|s| s.serial == serial) {
            self.detach(&mut st, cid, "stop");
            st.sessions.remove(&cid);
            log(format!("cid {cid}: gone"));
        }
    }
}

fn serve() -> std::io::Result<()> {
    let conf = conf();
    let genl = Genl::open("nl80211")?;
    // the wiphy registers when the Wi-Fi driver's firmware is up, which can be after this service started: a VM
    // given a band without channels can't scan (seen 2026-11-23); wait for them (up to 60 s)
    let read_channels = || -> std::io::Result<BTreeMap<String, Vec<u32>>> {
        Ok(parse_channels(&String::from_utf8_lossy(&Command::new("iw").args(["phy", &conf.phy, "channels"]).output()?.stdout)))
    };
    let mut channels = read_channels()?;
    for _ in 0..60 {
        if channels.values().any(|c| !c.is_empty()) {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
        channels = read_channels()?;
    }
    let info = Command::new("iw").args(["phy", &conf.phy, "info"]).output()?;
    let mfp_optional = String::from_utf8_lossy(&info.stdout).contains("MFP_OPTIONAL");
    log(format!("nl80211 family {}; channels 2g {}, 5g {}", genl.family, channels["2g"].len(), channels["5g"].len()));
    // no band interface left up by an earlier run: a VM's bands ride on one interface, and an interface still up with
    // the VM's address keeps the next from coming up ("Name not unique on network")
    for iface in conf_bands_ifaces(&conf) {
        let _ = run("ip", &["link", "set", "dev", &iface, "down"]);
    }
    let b = Arc::new(Bridge { conf, genl, channels, mfp_optional, st: Mutex::new(State::default()), host_pin: Mutex::new(HostPin::default()) });
    let e = b.clone();
    std::thread::spawn(move || e.event_loop());
    let c = b.clone();
    std::thread::spawn(move || c.control_loop());
    let g = b.clone();
    std::thread::spawn(move || g.host_guard());
    let bands: Vec<Handle> = b.conf.bands.keys().map(|band| {
        let label = match band.as_str() { "2g" => "Wi-Fi 2.4 GHz".to_string(), "5g" => "Wi-Fi 5 GHz".to_string(), b => format!("Wi-Fi {b}") };
        Handle::new(&format!("wifi:{band}"), "mship.wifi", &label, "network-wireless-symbolic", Direction::Both, &format!("wifi-{band}"))
            .caps(json!({"bands": [band]}))
            .control(CTL, Some(band))
    }).collect();
    publish_handles("wifi-bridge", &bands);
    let l = vsock::VsockListener::bind_with_cid_port(libc::VMADDR_CID_ANY, mw::PORT)?;
    log(format!("listening on vsock port {}", mw::PORT));
    for conn in l.incoming() {
        let Ok(conn) = conn else { continue };
        let cid = conn.peer_addr().map(|a| a.cid()).unwrap_or(0);
        let Some(vm) = vm_conf(cid) else {
            log(format!("cid {cid}: no Wi-Fi slot configured, closing"));
            continue;
        };
        let Ok(w) = conn.try_clone() else { continue };
        let _ = w.set_write_timeout(Some(Duration::from_secs(5)));
        let serial = {
            let mut st = b.lock();
            if st.sessions.contains_key(&cid) {
                b.detach(&mut st, cid, "stop");
            }
            st.serial += 1;
            let serial = st.serial;
            let mac = vm.mac.clone();
            st.sessions.insert(cid, Session { w, vm: vm.clone(), mac, bands: BTreeSet::new(), iface: None, ifidx: 0, freq: 0, target: None, pending: None, unreachable: BTreeMap::new(), scan_id: 0, scan_started: Instant::now(),
                                              connected: false, connecting: false, cache: BTreeMap::new(), serial });
            serial
        };
        log(format!("cid {cid}: connected ({})", vm.name));
        let s = b.clone();
        std::thread::spawn(move || s.session(conn, cid, serial));
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: mship-wifi-bridge (control: {CTL})");
        return std::process::ExitCode::from(2);
    }
    if let Err(e) = serve() {
        eprintln!("mship-wifi-bridge: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_and_ies() {
        let iw = "Band 1:\n\t* 2412 MHz [1]\n\t  Maximum TX power: 20.0 dBm\n\t* 2467 MHz [12] (disabled)\nBand 2:\n\t* 5180 MHz [36]\n\t* 5955 MHz [1]\n";
        let c = parse_channels(iw);
        assert_eq!((c["2g"].clone(), c["5g"].clone()), (vec![2412], vec![5180])); // 6 GHz not lent
        assert_eq!(ssid_of(&[1, 1, 0x82, 0, 3, b'n', b'e', b't']), b"net");
        assert_eq!(mac_str(&mac_bytes("02:6d:73:00:00:03")), "02:6d:73:00:00:03");
    }
}
