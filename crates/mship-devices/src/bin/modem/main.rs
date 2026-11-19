//! mship-modem: the phone's modem (root), the mship.modem 1.x contract (docs/contracts/mship.modem-1.md).
//!
//! The modem itself (an SDX55 on PCIe/MHI, booted by the kernel's Sahara server) needs a host to live:
//! - **files**: its remote file system over QRTR TFTP (`tftp`): configuration from the firmware directory, its own
//!   files in /var/lib/mothership/modem/readwrite. Without answers it resets itself.
//! - **NV**: what it syncs back goes to its NV partitions (`efs`).
//! - **SIM**: a card's USIM application initializes only once a provisioning session is bound to it: done whenever
//!   a card shows up (QMI UIM).
//! - **data**: the host's own mobile data while no VM holds the modem: a QMI WDS session on QMAP mux 1, the rmnet
//!   link `cell0`, default route with metric 700 (Wi-Fi stays preferred).
//!
//! The handle `cellular` (group `cellular`, exclusive) lends the modem to a VM: its stock radio stack talks to the
//! modem slot (`at`) in the Cuttlefish modem simulator's dialect (`radio`), answered from the modem's state over QMI
//! (`nas`: network and signal; WMS: incoming SMS; VOICE: call events) or passed to the modem's AT port where it is
//! standard 27.007. Its data calls are sessions on their own mux (`cellvm<cid>`) routed to the VM's data NIC
//! (cuttlefish-arm64: the first NIC, tap pxmtap0), in the kernel. While lent, the host has no mobile data.
//!
//! Control: datagrams on /run/mothership/modem.sock, one reply each: `attach <vm>` (by its directory name),
//! `detach <vm> [user|screen|stop|revoked|gone]`, `status`. Configuration (optional): /etc/mothership/modem.json
//! `{"host_data": true, "profile": 1}`.

mod at;
mod efs;
mod nas;
mod radio;
mod tftp;

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use mship_devices::qrtr::{PORT_CTRL, QmiClient, QmiMsg, Qrtr, ctrl, ctrl_packet, tlv, tlv_u8, tlv_u32};
use mship_devices::{for_session, fresh, log, poll, read_uevent, uevents};
use serde_json::{Value, json};

const CONF: &str = "/etc/mothership/modem.json";
const CTL: &str = "/run/mothership/modem.sock";
const VMS: &str = "/var/lib/mothership/vms";
const STATE: &str = "/var/lib/mothership/modem";
const FIRMWARE: &str = "/lib/firmware/qcom/sdx55m";
const REASONS: &[&str] = &["user", "screen", "stop", "revoked", "gone"];

/// QMI services
const WDS: u32 = 0x01;
const DMS: u32 = 0x02;
const NAS: u32 = 0x03;
const WMS: u32 = 0x05;
const VOICE: u32 = 0x09;
const UIM: u32 = 0x0b;
const WDA: u32 = 0x1a;
const DPM: u32 = 0x2f;
const TIMEOUT: Duration = Duration::from_secs(10);

/// The data path: the modem's hardware data channel (MHI IP_HW0 = PCIe endpoint, interface 4, channels 100/101) as
/// mhi_net's interface, demultiplexed by rmnet (QMAP)
const DATA_DEV: &str = "mhi_hwip0";
const EP_PCIE: u32 = 3;
const EP_IFACE: u32 = 4;
const EP_RX: u32 = 101;
const EP_TX: u32 = 100;
const HOST_IF: &str = "cell0";
const HOST_MUX: u8 = 1;
const HOST_METRIC: &str = "700";
/// how long the network takes to release the host's session (a VM's session on the same APN is refused meanwhile)
const HOST_RELEASE: Duration = Duration::from_secs(5);
/// a VM's context <cid>: mux VM_MUX + cid, link cellvm<cid>, routing table VM_TABLE + cid
const VM_MUX: u8 = 10;
const VM_TABLE: u32 = 1700;
/// cuttlefish-arm64's mobile data NIC (the guest's buried_eth0)
const VM_DATA_TAP: &str = "pxmtap0";
const CHAIN: &str = "MSHIP_CELL";
/// legacy iptables (the kernel has no nf_tables)
const IPT: &str = "iptables-legacy";

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let o = Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if o.status.success() { Ok(()) } else { Err(format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim())) }
}

fn sysctl(key: &str, v: &str) {
    let _ = std::fs::write(format!("/proc/sys/{}", key.replace('.', "/")), v);
}

fn exists(dev: &str) -> bool {
    Path::new("/sys/class/net").join(dev).exists()
}

/// The VM directory name with this vsock CID / the CID of a VM directory.
fn vm_cid(name: &str) -> Option<u32> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(Path::new(VMS).join(name).join("vm.json")).ok()?).ok()?;
    v["cid"].as_u64().map(|c| c as u32)
}

/// The modem's AT port: the WWAN subsystem names it wwan<N>at<M>.
fn at_port() -> String {
    std::fs::read_dir("/dev").into_iter().flatten().flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("wwan") && n.contains("at"))
        .min()
        .map(|n| format!("/dev/{n}"))
        .unwrap_or_else(|| "/dev/wwan0at0".into())
}

// ---------------------------------------------------------------- SIM

struct App {
    kind: u8,
    aid: Vec<u8>,
}

struct Card {
    present: bool,
    apps: Vec<App>,
}

/// UIM card status (TLV 0x10): the primary GW session's index (0xffff: none) and the cards by slot.
fn card_status(v: &[u8]) -> Option<(u16, Vec<Card>)> {
    let mut i = 0;
    let mut take = |n: usize| {
        let s = v.get(i..i + n);
        i += n;
        s
    };
    let gw = u16::from_le_bytes(take(2)?.try_into().ok()?);
    take(6)?;
    let mut cards = vec![];
    for _ in 0..take(1)?[0] {
        let c = take(6)?; // state, UPIN state/retries, UPUK retries, error, number of applications
        let mut apps = vec![];
        for _ in 0..c[5] {
            let a = take(7)?; // type, state, perso state/feature/retries/unblock retries, AID length
            let aid = take(a[6] as usize)?.to_vec();
            take(7)?; // UPIN replaces PIN1, PIN1/PUK1/PIN2/PUK2 states and retries
            apps.push(App { kind: a[0], aid });
        }
        cards.push(Card { present: c[0] == 1, apps });
    }
    Some((gw, cards))
}

// ---------------------------------------------------------------- data sessions

struct Session {
    wds: QmiClient,
    handle: u32,
    iface: String,
    addr: Ipv4Addr,
    prefix: u32,
    gw: Ipv4Addr,
    dns: Vec<Ipv4Addr>,
    mtu: u32,
}

impl Session {
    fn active(&self) -> radio::Active {
        radio::Active {
            address: format!("{}/{}", self.addr, self.prefix),
            gateway: self.gw.to_string(),
            dns: self.dns.iter().map(|d| d.to_string()).collect(),
        }
    }

    /// End it at the modem (closing the client would too).
    fn stop(&mut self) {
        let _ = self.wds.request(0x21, &[tlv_u32(0x01, self.handle)], Duration::from_secs(5));
    }
}

/// What a session connects to.
enum Target<'a> {
    /// a 3GPP profile of the modem's
    Profile(u8),
    /// an APN (the VM's contexts: the modem's profiles stay as they are)
    Apn(&'a str),
}

/// A session, bound to QMAP mux `mux` (its rmnet link `iface`, up).
fn start_session(wds_at: (u32, u32), target: Target, mux: u8, iface: &str) -> Result<Session, String> {
    if !exists(iface) {
        run("ip", &["link", "add", iface, "link", DATA_DEV, "type", "rmnet", "mux_id", &mux.to_string()])?;
    }
    let mut wds = QmiClient::new(WDS, wds_at.0, wds_at.1).map_err(|e| e.to_string())?;
    let ep = [EP_PCIE.to_le_bytes(), EP_IFACE.to_le_bytes()].concat();
    wds.request(0xa2, &[tlv(0x10, &ep), tlv_u8(0x11, mux)], TIMEOUT)?; // bind mux data port
    // start network: profile or APN, IPv4
    let to = match target {
        Target::Profile(p) => tlv_u8(0x31, p),
        Target::Apn(a) => tlv(0x14, a.as_bytes()),
    };
    let r = wds.request(0x20, &[to, tlv_u8(0x19, 4)], Duration::from_secs(60))?;
    let handle = r.u32(0x01).ok_or("start network: no packet data handle")?;
    // current settings: DNS, IP address, gateway, MTU
    let s = wds.request(0x2d, &[tlv_u32(0x10, 0x10 | 0x100 | 0x200 | 0x2000)], TIMEOUT)?;
    let ip = |t| s.u32(t).map(Ipv4Addr::from);
    let addr = ip(0x1e).ok_or("no IPv4 address")?;
    let mask = s.u32(0x21).unwrap_or(u32::MAX);
    let mut ses = Session {
        wds, handle, iface: iface.into(), addr, prefix: mask.count_ones(), gw: ip(0x20).unwrap_or(Ipv4Addr::UNSPECIFIED),
        dns: [ip(0x15), ip(0x16)].into_iter().flatten().collect(), mtu: s.u32(0x29).filter(|m| *m >= 576).unwrap_or(1500),
    };
    if let Err(e) = run("ip", &["link", "set", iface, "mtu", &ses.mtu.to_string(), "up"]) {
        ses.stop();
        return Err(e);
    }
    Ok(ses)
}

fn host_up(s: &Session) -> Result<(), String> {
    run("ip", &["addr", "replace", &format!("{}/{}", s.addr, s.prefix), "dev", &s.iface])?;
    run("ip", &["route", "replace", "default", "dev", &s.iface, "metric", HOST_METRIC])?;
    if Path::new("/run/systemd/resolve").exists() && !s.dns.is_empty() {
        let mut a = vec!["dns".to_string(), s.iface.clone()];
        a.extend(s.dns.iter().map(|d| d.to_string()));
        let _ = run("resolvectl", &a.iter().map(String::as_str).collect::<Vec<_>>());
    }
    Ok(())
}

fn host_down(iface: &str) {
    let _ = run("ip", &["route", "del", "default", "dev", iface]);
    let _ = run("ip", &["addr", "flush", "dev", iface]);
    if Path::new("/run/systemd/resolve").exists() {
        let _ = run("resolvectl", &["revert", iface]);
    }
}

/// A VM context's packets between its data NIC and its rmnet link, in the kernel: the guest's address routed to the
/// tap, its traffic out of the link (its own routing table), forwarding allowed for exactly that address; the host
/// answers the guest's ARP for anything (proxy ARP), so the guest needs no real gateway.
fn vm_route(cid: u8, s: &Session, up: bool) {
    let a = s.addr.to_string();
    let table = (VM_TABLE + cid as u32).to_string();
    let (rt, ipt) = if up { ("replace", "-A") } else { ("del", "-D") };
    if up {
        forward_first();
        sysctl(&format!("net.ipv4.conf.{VM_DATA_TAP}.proxy_arp"), "1");
        sysctl(&format!("net.ipv4.conf.{}.rp_filter", s.iface), "0");
        let _ = run("ip", &["rule", "del", "from", &a, "iif", VM_DATA_TAP, "table", &table]);
    }
    let rule = if up { "add" } else { "del" };
    let tbl = format!("{a}/32");
    let steps: [(&str, Vec<&str>); 5] = [
        ("ip", vec!["rule", rule, "from", &a, "iif", VM_DATA_TAP, "table", &table, "priority", &table]),
        ("ip", vec!["route", rt, &tbl, "dev", VM_DATA_TAP]),
        ("ip", vec!["route", rt, "default", "dev", &s.iface, "table", &table]),
        (IPT, vec![ipt, CHAIN, "-i", VM_DATA_TAP, "-o", &s.iface, "-s", &a, "-j", "ACCEPT"]),
        (IPT, vec![ipt, CHAIN, "-i", &s.iface, "-o", VM_DATA_TAP, "-d", &a, "-j", "ACCEPT"]),
    ];
    for (cmd, args) in steps {
        // taking a context down: whatever is already gone is fine; bringing it up: every step is needed
        if let Err(e) = run(cmd, &args)
            && up
        {
            log(format!("data: context {cid} route: {e}"));
        }
    }
}

/// Our jump at the head of FORWARD: mship-vm-net inserts its own chain at the head too (it drops whatever the VM taps
/// forward), and it may do so after us; called again whenever a VM context comes up.
fn forward_first() {
    while run(IPT, &["-D", "FORWARD", "-j", CHAIN]).is_ok() {}
    if let Err(e) = run(IPT, &["-I", "FORWARD", "1", "-j", CHAIN]) {
        log(format!("firewall: {e}"));
    }
}

/// What a previous run left of the VMs' data paths (sessions end with the process, their routes don't): our
/// policy rules (priorities VM_TABLE..), their tables, the per-address routes to the data tap.
fn stale_routes() {
    let rules = Command::new("ip").args(["rule", "show"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    for pref in rules.lines().filter_map(|l| l.split(':').next()?.trim().parse::<u32>().ok()) {
        if (VM_TABLE..VM_TABLE + 256).contains(&pref) {
            let _ = run("ip", &["rule", "del", "pref", &pref.to_string()]);
            let _ = run("ip", &["route", "flush", "table", &pref.to_string()]);
        }
    }
    if exists(VM_DATA_TAP) {
        let _ = run("ip", &["route", "flush", "dev", VM_DATA_TAP, "scope", "link"]);
    }
}

/// Our forwarding chain, before mship-vm-net's (which drops whatever the VM taps forward), and MSS clamping on the
/// mobile links (their MTU is the network's, usually below the guest's 1500).
fn firewall() {
    if run(IPT, &["-N", CHAIN]).is_err() {
        let _ = run(IPT, &["-F", CHAIN]);
    }
    forward_first();
    let clamp = ["FORWARD", "-o", "cell+", "-p", "tcp", "--tcp-flags", "SYN,RST", "SYN", "-j", "TCPMSS", "--clamp-mss-to-pmtu"];
    if run(IPT, &[&["-t", "mangle", "-C"][..], &clamp].concat()).is_err() {
        let _ = run(IPT, &[&["-t", "mangle", "-A"][..], &clamp].concat());
    }
}

// ---------------------------------------------------------------- the service

struct Modem {
    ns: Qrtr,
    /// (service, node) -> port, from the name service
    servers: BTreeMap<(u32, u32), u32>,
    /// the modem's QRTR node (where UIM is)
    node: Option<u32>,
    uim: Option<QmiClient>,
    /// WMS: incoming SMS (routed to us, handed to the holder)
    wms: Option<QmiClient>,
    /// VOICE: call events (an incoming call reaches the AT port only as this)
    voice: Option<QmiClient>,
    /// NAS: the network (registration, technology, signal)
    nas: Option<QmiClient>,
    net: nas::Net,
    ids: radio::Ids,
    /// the slot's dialect state (the holder's registration modes, contexts, what it was told)
    radio: radio::Radio,
    /// when the host's own session last ended: the network takes a moment to release it, and refuses a second
    /// session on the same APN meanwhile
    host_ended: Option<Instant>,
    /// incoming SMS handed to the holder, waiting for its AT+CNMA (WMS transaction ids, oldest first)
    sms_acks: std::collections::VecDeque<u32>,
    /// DPM: the hardware data port opened (kept open with its client)
    dpm: Option<QmiClient>,
    host_data: bool,
    profile: u8,
    host: Option<Session>,
    host_retry: Instant,
    host_err: String,
    /// the holder's contexts
    vm: BTreeMap<u8, Session>,
    at: at::At,
    /// the VM holding the handle (directory name)
    lent: Option<String>,
}

impl Modem {
    fn server(&self, service: u32) -> Option<(u32, u32)> {
        let node = self.node?;
        self.servers.get(&(service, node)).map(|p| (node, *p))
    }

    fn on_ns(&mut self) {
        while let Ok(Some((_, port, data))) = self.ns.recv() {
            if port != PORT_CTRL {
                continue;
            }
            let Some((cmd, service, _, node, port)) = ctrl_packet(&data) else { continue };
            match cmd {
                ctrl::NEW_SERVER if service != 0 => {
                    self.servers.insert((service, node), port);
                    if service == UIM && self.node.is_none() {
                        self.node = Some(node);
                        self.modem_up();
                    }
                    // WMS and NAS may come after UIM
                    if service == WMS && self.node == Some(node) && self.wms.is_none() {
                        self.wms_up();
                    }
                    if service == NAS && self.node == Some(node) && self.nas.is_none() {
                        self.nas_up();
                    }
                    if service == VOICE && self.node == Some(node) && self.voice.is_none() {
                        self.voice_up();
                    }
                }
                ctrl::DEL_SERVER => {
                    self.servers.remove(&(service, node));
                    if service == UIM && self.node == Some(node) {
                        self.modem_down();
                    }
                }
                ctrl::BYE => {
                    // a node left: service holds the node
                    self.servers.retain(|(_, n), _| *n != service);
                    if self.node == Some(service) {
                        self.modem_down();
                    }
                }
                _ => {}
            }
        }
    }

    fn modem_up(&mut self) {
        log(format!("modem up (QRTR node {})", self.node.unwrap_or(0)));
        let Some((n, p)) = self.server(UIM) else { return };
        match QmiClient::new(UIM, n, p) {
            Ok(mut u) => {
                // card status indications
                if let Err(e) = u.request(0x2e, &[tlv_u32(0x01, 1)], TIMEOUT) {
                    log(format!("sim: event registration: {e}"));
                }
                self.uim = Some(u);
                self.provision();
            }
            Err(e) => log(format!("uim: {e}")),
        }
        self.wms_up();
        self.nas_up();
        self.voice_up();
        self.identity();
        self.host_retry = Instant::now() + Duration::from_secs(5);
        if self.lent.is_some() {
            self.at.open_port();
        }
    }

    fn modem_down(&mut self) {
        log("modem down");
        self.node = None;
        self.uim = None;
        self.wms = None;
        self.voice = None;
        self.nas = None;
        self.net = Default::default();
        self.ids = Default::default();
        self.sms_acks.clear();
        self.dpm = None;
        if let Some(h) = self.host.take() {
            host_down(&h.iface);
        }
        for (cid, s) in std::mem::take(&mut self.vm) {
            vm_route(cid, &s, false);
        }
        self.at.close_port();
    }

    /// A present card's USIM gets the primary provisioning session if nothing has one. No slot power cycle after
    /// it: that aborts the card's initialization.
    fn provision(&mut self) {
        let Some(u) = self.uim.as_mut() else { return };
        let st = match u.request(0x2f, &[], TIMEOUT) {
            Ok(r) => r,
            Err(e) => return log(format!("sim: card status: {e}")),
        };
        let Some((gw, cards)) = st.tlvs.get(&0x10).and_then(|v| card_status(v)) else { return };
        if gw != 0xffff {
            return; // provisioned
        }
        let Some((slot, app)) = cards.iter().enumerate().filter(|(_, c)| c.present)
            .find_map(|(i, c)| c.apps.iter().find(|a| a.kind == 2).map(|a| (i as u8 + 1, a)))
        else {
            return; // no card, or no USIM
        };
        let mut info = vec![slot, app.aid.len() as u8];
        info.extend_from_slice(&app.aid);
        match u.request(0x38, &[tlv(0x01, &[0, 1]), tlv(0x10, &info)], TIMEOUT) {
            Ok(_) => log(format!("sim: slot {slot}: USIM provisioned (primary)")),
            Err(e) => log(format!("sim: slot {slot}: provisioning: {e}")),
        }
    }

    /// Incoming SMS to us: every point-to-point class transferred (not stored), acknowledged by us, and the
    /// new-message indications on. The routes hold while this client lives (the modem reverts them when it goes).
    fn wms_up(&mut self) {
        let Some((n, p)) = self.server(WMS) else { return }; // not yet: on_ns brings it up when it appears
        let mut w = match QmiClient::new(WMS, n, p) {
            Ok(w) => w,
            Err(e) => return log(format!("sms: {e}")),
        };
        // route: message type (0 point-to-point), class (0-3, 4 none), storage (0xff none), action (2 transfer only)
        let mut routes = 5u16.to_le_bytes().to_vec();
        for class in 0..5u8 {
            routes.extend_from_slice(&[0, class, 0xff, 2]);
        }
        let r = w.request(0x0032, &[tlv(0x01, &routes)], TIMEOUT).and_then(|_| w.request(0x0001, &[tlv_u8(0x10, 1)], TIMEOUT));
        match r {
            Ok(_) => {
                log("sms: incoming SMS over WMS");
                self.wms = Some(w);
            }
            Err(e) => log(format!("sms: {e}")),
        }
    }

    /// The network: voice over circuit switching preferred (the modem's default "PS preferred" makes the network
    /// route incoming calls to IMS, where nothing of ours is registered: they fail with SIP 480 instead of falling
    /// back to CS; until the modem's next power cycle, so never in its NV), indications for registration and signal
    /// (thresholds around the levels Android draws), and the current state.
    fn nas_up(&mut self) {
        let Some((n, p)) = self.server(NAS) else { return };
        let mut c = match QmiClient::new(NAS, n, p) {
            Ok(c) => c,
            Err(e) => return log(format!("nas: {e}")),
        };
        // Set System Selection Preference: 0x23 voice domain (2 CS preferred), 0x17 change duration (0 power cycle)
        if let Err(e) = c.request(0x0033, &[tlv_u32(0x23, 2), tlv_u8(0x17, 0)], TIMEOUT) {
            log(format!("voice: domain preference: {e}"));
        }
        let i16s = |v: &[i16]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let with_count = |n: usize, b: Vec<u8>| [vec![n as u8], b].concat();
        let rssi: Vec<u8> = [-105i8, -95, -85, -75, -65].iter().map(|x| *x as u8).collect();
        let rsrp = [-125i16, -120, -115, -110, -105, -100, -95, -90, -85, -80];
        let snr = [0i16, 50, 100, 150, 200]; // 0.1 dB
        // Config Signal Info: 0x10 RSSI, 0x13 LTE SNR, 0x16 RSRP thresholds
        let cfg = c.request(0x0050, &[tlv(0x10, &with_count(rssi.len(), rssi.clone())), tlv(0x13, &with_count(snr.len(), i16s(&snr))),
                                      tlv(0x16, &with_count(rsrp.len(), i16s(&rsrp)))], TIMEOUT);
        // Register Indications: 0x13 serving system, 0x19 signal info
        let reg = c.request(0x0003, &[tlv_u8(0x13, 1), tlv_u8(0x19, 1)], TIMEOUT);
        if let Err(e) = cfg.and(reg) {
            log(format!("nas: indications: {e}"));
        }
        if let Ok(m) = c.request(0x0024, &[], TIMEOUT) {
            self.net.serving_system(&m, false);
        }
        if let Ok(m) = c.request(0x004f, &[], TIMEOUT) {
            self.net.signal_info(&m);
        }
        self.nas = Some(c);
        self.plmn_name();
        log(format!("nas: {}", self.net_summary()));
    }

    /// The operator's names for the current PLMN.
    fn plmn_name(&mut self) {
        let (Some(c), Some(p)) = (self.nas.as_mut(), self.net.plmn.as_ref()) else { return };
        let plmn = [p.mcc.to_le_bytes(), p.mnc.to_le_bytes()].concat();
        // Get PLMN Name: 0x01 PLMN, 0x11 MNC includes PCS digit
        if let Ok(m) = c.request(0x0044, &[tlv(0x01, &plmn), tlv_u8(0x11, p.mnc3 as u8)], TIMEOUT) {
            self.net.plmn_name(&m);
        }
    }

    fn net_summary(&self) -> String {
        let n = &self.net;
        let sig = match (n.signal.lte_rsrp, n.signal.nr_rsrp) {
            (Some(l), Some(r)) => format!("LTE RSRP {l} dBm, NR RSRP {r} dBm"),
            (Some(l), None) => format!("LTE RSRP {l} dBm"),
            (None, Some(r)) => format!("NR RSRP {r} dBm"),
            _ => "no signal values".into(),
        };
        let rat = match (n.rat, n.endc()) {
            (Some(_), true) => "LTE + NR (EN-DC)".to_string(),
            (Some(r), false) => format!("{r:?}"),
            (None, _) => "no service".into(),
        };
        let op = n.plmn.as_ref().map(|p| format!("{} {}", p.numeric(), p.long)).unwrap_or_default();
        format!("{:?}/{:?}, {rat}, {op}, {sig}", n.cs, n.ps)
    }

    /// The modem's IMEI (DMS) and the card's ICCID (EF ICCID over UIM).
    fn identity(&mut self) {
        if self.ids.imei.is_none()
            && let Some((n, p)) = self.server(DMS)
            && let Ok(m) = QmiClient::new(DMS, n, p).map_err(|e| e.to_string()).and_then(|mut c| c.request(0x0025, &[], TIMEOUT))
        {
            self.ids.imei = m.tlvs.get(&0x11).map(|v| String::from_utf8_lossy(v).trim().to_string()).filter(|i| !i.is_empty());
        }
        self.ids.iccid = self.uim.as_mut().and_then(read_iccid);
    }

    /// Call notification events: the modem's AT port reports no RING for an incoming call (it alerts the network
    /// and waits), so the holder gets RING / NO CARRIER from these and finds the call with AT+CLCC, answers with ATA.
    fn voice_up(&mut self) {
        let Some((n, p)) = self.server(VOICE) else { return };
        let r = QmiClient::new(VOICE, n, p).map_err(|e| e.to_string())
            .and_then(|mut c| c.request(0x0003, &[tlv_u8(0x13, 1)], TIMEOUT).map(|_| c));
        match r {
            Ok(c) => {
                log("voice: call events");
                self.voice = Some(c);
            }
            Err(e) => log(format!("voice: {e}")),
        }
    }

    /// WMS Send Ack for an incoming SMS; refused = memory capacity exceeded (the network retries later).
    fn sms_ack(&mut self, txn: u32, ok: bool) {
        let Some(w) = self.wms.as_mut() else { return };
        let mut info = txn.to_le_bytes().to_vec();
        info.extend_from_slice(&[1, ok as u8]); // protocol 1: 3GPP
        let mut tlvs = vec![tlv(0x01, &info)];
        if !ok {
            tlvs.push(tlv(0x11, &[22, 0xd3])); // RP cause 22, TP cause 0xd3: memory capacity exceeded
        }
        if let Err(e) = w.request(0x0037, &tlvs, TIMEOUT) {
            log(format!("sms: ack: {e}"));
        }
    }

    /// An incoming SMS (WMS event report): to the holder as +CMT, or refused while nobody holds the modem.
    fn sms_in(&mut self, m: &QmiMsg) {
        // transfer route MT message: ack indicator u8 (0: we must ack), transaction id u32, format u8, len u16, TPDU
        let Some(v) = m.tlvs.get(&0x11) else { return };
        let Some(sms) = transfer_route_sms(v) else { return log("sms: malformed indication") };
        let delivered = match radio::cmt(&sms.raw) {
            Some(line) if self.lent.is_some() => self.at.tell_holder(&[line]),
            _ => false,
        };
        log(format!("sms: incoming ({} bytes){}", sms.raw.len(), if delivered { "" } else { ", nobody to take it: refused" }));
        if sms.must_ack {
            if delivered {
                self.sms_acks.push_back(sms.txn);
            } else {
                self.sms_ack(sms.txn, false);
            }
        }
    }

    /// The hardware data port and the QMAP format, once per modem boot.
    fn data_path(&mut self) -> Result<(u32, u32), String> {
        let wds = self.server(WDS).ok_or("no WDS service")?;
        if self.dpm.is_none() {
            let (dn, dp) = self.server(DPM).ok_or("no DPM service")?;
            let (an, ap) = self.server(WDA).ok_or("no WDA service")?;
            let _ = run("modprobe", &["rmnet"]);
            let mut dpm = QmiClient::new(DPM, dn, dp).map_err(|e| e.to_string())?;
            let port: Vec<u8> = [1u8].into_iter().chain([EP_PCIE, EP_IFACE, EP_RX, EP_TX].iter().flat_map(|v| v.to_le_bytes())).collect();
            if let Err(e) = dpm.request(0x20, &[tlv(0x11, &port)], TIMEOUT) {
                log(format!("data: open port: {e} (already open?)"));
            }
            let mut wda = QmiClient::new(WDA, an, ap).map_err(|e| e.to_string())?;
            let ep = [EP_PCIE.to_le_bytes(), EP_IFACE.to_le_bytes()].concat();
            // raw IP in QMAP both ways, downlink aggregation 32 datagrams / 31 KiB
            wda.request(0x20, &[tlv_u8(0x10, 0), tlv_u32(0x11, 2), tlv_u32(0x12, 5), tlv_u32(0x13, 5), tlv_u32(0x15, 32),
                                tlv_u32(0x16, 31744), tlv(0x17, &ep)], TIMEOUT)?;
            run("ip", &["link", "set", DATA_DEV, "up"])?;
            self.dpm = Some(dpm);
        }
        Ok(wds)
    }

    /// The host's mobile data, while it holds the modem.
    fn host_data(&mut self) {
        if !self.host_data || self.lent.is_some() || self.host.is_some() || self.node.is_none() || Instant::now() < self.host_retry {
            return;
        }
        let r = self.data_path().and_then(|wds| start_session(wds, Target::Profile(self.profile), HOST_MUX, HOST_IF))
            .and_then(|s| host_up(&s).map(|_| s));
        match r {
            Ok(s) => {
                log(format!("data: {} {}/{} via {}, DNS {:?}, MTU {}", s.iface, s.addr, s.prefix, s.gw, s.dns, s.mtu));
                self.host = Some(s);
                self.host_err.clear();
            }
            Err(e) => {
                if e != self.host_err {
                    log(format!("data: {e} (retrying)"));
                    self.host_err = e;
                }
                self.host_retry = Instant::now() + Duration::from_secs(20);
            }
        }
    }

    fn host_stop(&mut self) {
        if let Some(mut h) = self.host.take() {
            h.stop();
            host_down(&h.iface);
            self.host_ended = Some(Instant::now());
            log("data: host session ended");
        }
    }

    /// Sessions that ended at the modem (network detach, no coverage); card changes.
    fn indications(&mut self, ready: &[RawFd]) {
        if let Some(u) = self.uim.as_mut().filter(|u| ready.contains(&u.fd())) {
            match u.indications() {
                Ok(i) if i.iter().any(|m| m.msg == 0x32) => {
                    self.provision();
                    self.identity();
                }
                Ok(_) => {}
                Err(_) => self.uim = None,
            }
        }
        if let Some(c) = self.nas.as_mut().filter(|c| ready.contains(&c.fd())) {
            match c.indications() {
                Ok(i) => {
                    let plmn = self.net.plmn.as_ref().map(|p| (p.mcc, p.mnc));
                    let state = (self.net.cs, self.net.ps, self.net.rat, self.net.endc());
                    for m in &i {
                        match m.msg {
                            0x0024 => self.net.serving_system(m, true),
                            0x0051 => self.net.signal_info(m),
                            _ => {}
                        }
                    }
                    let plmn_changed = self.net.plmn.as_ref().map(|p| (p.mcc, p.mnc)) != plmn;
                    if plmn_changed {
                        self.plmn_name();
                    }
                    if plmn_changed || state != (self.net.cs, self.net.ps, self.net.rat, self.net.endc()) {
                        log(format!("nas: {}", self.net_summary()));
                    }
                    self.tell_changes();
                }
                Err(_) => self.nas = None,
            }
        }
        if let Some(v) = self.voice.as_mut().filter(|v| ready.contains(&v.fd())) {
            match v.indications() {
                Ok(i) => {
                    for m in i.iter().filter(|m| m.msg == 0x002e) {
                        if let Some(line) = m.tlvs.get(&0x01).and_then(|c| radio::call_event(c))
                            && self.lent.is_some()
                        {
                            self.at.tell_holder(&[line.to_string()]);
                        }
                    }
                }
                Err(_) => self.voice = None,
            }
        }
        if let Some(w) = self.wms.as_mut().filter(|w| ready.contains(&w.fd())) {
            match w.indications() {
                Ok(i) => {
                    for m in i.iter().filter(|m| m.msg == 0x0001) {
                        self.sms_in(m);
                    }
                }
                Err(_) => self.wms = None,
            }
        }
        // packet service status: connection status 1 = disconnected
        let ended = |s: &mut Session| match s.wds.indications() {
            Ok(i) => i.iter().any(|m| m.msg == 0x22 && m.tlvs.get(&0x01).is_some_and(|v| v.first() == Some(&1))),
            Err(_) => true,
        };
        if let Some(h) = self.host.as_mut().filter(|h| ready.contains(&h.wds.fd()))
            && ended(h)
        {
            let h = self.host.take().unwrap();
            host_down(&h.iface);
            log("data: host session ended by the network");
            self.host_retry = Instant::now() + Duration::from_secs(5);
        }
        let gone: Vec<u8> = self.vm.iter_mut().filter(|(_, s)| ready.contains(&s.wds.fd())).filter_map(|(c, s)| ended(s).then_some(*c)).collect();
        for cid in gone {
            let s = self.vm.remove(&cid).unwrap();
            vm_route(cid, &s, false);
            log(format!("data: context {cid} of {} ended by the network", self.lent.as_deref().unwrap_or("?")));
        }
    }

    // ------------------------------------------------ the handle
    fn guest(&mut self, cid: u32) {
        if self.at.owner != Some(cid) || self.lent.is_none() {
            for unit in self.at.read_guest(cid) {
                self.at.tell(cid, &radio::empty_slot(&unit));
            }
            return;
        }
        for unit in self.at.read_guest(cid) {
            let active: BTreeMap<u8, radio::Active> = self.vm.iter().map(|(c, s)| (*c, s.active())).collect();
            let reply: Vec<String> = match self.radio.command(&unit, &self.net, &self.ids, &active) {
                radio::Route::Reply(lines) => lines,
                radio::Route::Modem(b) => {
                    if self.at.send_modem(&b) { vec![] } else { vec!["ERROR".into()] }
                }
                radio::Route::SmsAck(ok) => {
                    if let Some(txn) = self.sms_acks.pop_front() {
                        self.sms_ack(txn, ok);
                    }
                    vec!["OK".into()]
                }
                radio::Route::Activate(ctxs) => {
                    let ok = ctxs.iter().all(|c| self.activate(*c));
                    vec![if ok { "OK" } else { "ERROR" }.into()]
                }
                radio::Route::Deactivate(ctxs) => {
                    let all: Vec<u8> = if ctxs.is_empty() { self.vm.keys().copied().collect() } else { ctxs };
                    for c in all {
                        if let Some(mut s) = self.vm.remove(&c) {
                            s.stop();
                            vm_route(c, &s, false);
                            log(format!("data: context {c} ended by the VM"));
                        }
                    }
                    vec!["OK".into()]
                }
                radio::Route::Mode(m) => {
                    // Set System Selection Preference: 0x11 mode preference, 0x17 change duration (0 power cycle)
                    let r = self.nas.as_mut().ok_or("no NAS".to_string())
                        .and_then(|c| c.request(0x0033, &[tlv(0x11, &m.to_le_bytes()), tlv_u8(0x17, 0)], TIMEOUT));
                    match r {
                        Ok(_) => vec!["+CTEC: DONE".into(), "OK".into()],
                        Err(e) => {
                            log(format!("radio: mode preference {m:#x}: {e}"));
                            vec!["ERROR".into()]
                        }
                    }
                }
            };
            if !reply.is_empty() {
                self.at.tell_holder(&reply);
            }
        }
        self.tell_changes();
    }

    /// The holder's unsolicited results for what changed (network, signal, physical channels).
    fn tell_changes(&mut self) {
        if self.lent.is_none() {
            return;
        }
        let active: BTreeMap<u8, radio::Active> = self.vm.iter().map(|(c, s)| (*c, s.active())).collect();
        let lines = self.radio.changes(&self.net, &active);
        if !lines.is_empty() {
            self.at.tell_holder(&lines);
        }
    }

    /// The holder's data call on context `cid` (an existing one is reused: the guest's RIL never ends calls).
    fn activate(&mut self, cid: u8) -> bool {
        if self.vm.contains_key(&cid) {
            return true;
        }
        let iface = format!("cellvm{cid}");
        let Some(apn) = self.radio.contexts.get(&cid).map(|c| c.apn.clone()) else {
            log(format!("data: context {cid}: not defined"));
            return false;
        };
        // a new call on an APN replaces the VM's earlier one there: its RIL never ends calls, so that one was
        // abandoned (and the network may refuse a second session on the same APN)
        let stale: Vec<u8> = self.vm.keys().copied()
            .filter(|c| self.radio.contexts.get(c).is_some_and(|x| x.apn.eq_ignore_ascii_case(&apn))).collect();
        for c in stale {
            if let Some(mut s) = self.vm.remove(&c) {
                s.stop();
                vm_route(c, &s, false);
                log(format!("data: context {c} replaced by context {cid} ({apn})"));
            }
        }
        // the host's session on the same APN must be gone at the network first
        if let Some(wait) = self.host_ended.map(|t| (t + HOST_RELEASE).saturating_duration_since(Instant::now())) {
            std::thread::sleep(wait);
        }
        let mut r = self.data_path().and_then(|wds| start_session(wds, Target::Apn(&apn), VM_MUX + cid, &iface));
        if r.is_err() {
            std::thread::sleep(Duration::from_secs(2)); // once more: a session released just before can linger
            r = self.data_path().and_then(|wds| start_session(wds, Target::Apn(&apn), VM_MUX + cid, &iface));
        }
        match r {
            Ok(s) => {
                vm_route(cid, &s, true);
                log(format!("data: context {cid} for {}: {}/{} on {iface}", self.lent.as_deref().unwrap_or("?"), s.addr, s.prefix));
                self.vm.insert(cid, s);
                true
            }
            Err(e) => {
                log(format!("data: context {cid}: {e}"));
                false
            }
        }
    }

    fn attach(&mut self, vm: &str) -> Result<(), String> {
        let cid = vm_cid(vm).ok_or_else(|| format!("{vm}: no vsock CID in its vm.json"))?;
        if self.lent.as_deref() != Some(vm) {
            if let Some(old) = self.lent.clone() {
                self.release(&old, "revoked");
            }
            self.host_stop(); // exclusive: the host has no mobile data while a VM holds the modem
            self.lent = Some(vm.into());
            self.at.owner = Some(cid);
            if self.node.is_some() {
                self.at.open_port();
            }
            // fresh on attach: the guest's RIL reconnects and initializes the modem itself
            self.radio.reset();
            self.at.kick(cid);
            log(format!("{vm}: attached (cid {cid})"));
        }
        Ok(())
    }

    /// Abort, never complete: calls hung up, the VM's data calls ended, its radio stack cut off.
    fn release(&mut self, vm: &str, reason: &str) {
        if self.lent.as_deref() != Some(vm) {
            return;
        }
        self.at.send_modem(b"AT+CHUP\r");
        // the guest's radio stack may have switched the radio off (Android's RIL starts with AT+CFUN=0): the host
        // gets the modem back with its radio on
        self.at.send_modem(b"AT+CFUN=1\r");
        self.at.close_port();
        for (c, mut s) in std::mem::take(&mut self.vm) {
            s.stop();
            vm_route(c, &s, false);
        }
        if let Some(cid) = self.at.owner.take() {
            self.at.kick(cid);
        }
        self.lent = None;
        self.host_retry = Instant::now();
        log(format!("{vm}: released ({reason}); the modem is the host's again"));
    }

    fn control(&mut self, msg: &str) -> String {
        let w: Vec<&str> = msg.split_whitespace().collect();
        match w.as_slice() {
            ["attach", vm] => match self.attach(vm) {
                Ok(()) => "ok".into(),
                Err(e) => format!("error: {e}"),
            },
            ["detach", vm, rest @ ..] => {
                let reason = rest.first().filter(|r| REASONS.contains(r)).copied().unwrap_or("user");
                self.release(vm, reason);
                "ok".into()
            }
            ["status", ..] => json!({
                "modem": self.node.map(|n| format!("qrtr node {n}")),
                "lent": self.lent,
                "host_data": self.host.as_ref().map(|s| json!({"iface": s.iface, "address": format!("{}/{}", s.addr, s.prefix), "dns": s.dns, "mtu": s.mtu}))
                    .unwrap_or_else(|| if self.host_err.is_empty() { Value::Null } else { json!({"error": self.host_err}) }),
                "vm_data": self.vm.iter().map(|(c, s)| (c.to_string(), json!({"iface": s.iface, "address": format!("{}/{}", s.addr, s.prefix)})))
                    .collect::<serde_json::Map<_, _>>(),
                "radios": self.at.guests.keys().collect::<Vec<_>>(),
                "network": self.node.map(|_| self.net_summary()),
                "at_port": self.at.port_fd().is_some(),
            })
            .to_string(),
            _ => "error: attach <vm> | detach <vm> [reason] | status".into(),
        }
    }
}

fn serve() -> std::io::Result<()> {
    let conf: Value = std::fs::read_to_string(CONF).ok().and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
    let state = Path::new(STATE);
    let mut tftp = tftp::Tftp::new(Path::new(FIRMWARE), &state.join("readwrite"))?;
    let uev = uevents()?;
    efs::sync(&state.join("efs-backup")); // dumps from before we started
    firewall();
    stale_routes();
    let ns = Qrtr::open()?;
    ns.lookup(0)?;
    let mut m = Modem {
        ns, servers: BTreeMap::new(), node: None, uim: None, wms: None, voice: None, nas: None, net: Default::default(), ids: Default::default(),
        radio: Default::default(), host_ended: None, sms_acks: Default::default(), dpm: None,
        host_data: conf["host_data"].as_bool().unwrap_or(true), profile: conf["profile"].as_u64().unwrap_or(1) as u8,
        host: None, host_retry: Instant::now(), host_err: String::new(), vm: BTreeMap::new(),
        at: at::At::new(&at_port())?, lent: None,
    };
    fresh(Path::new(CTL));
    let ctl = UnixDatagram::bind(CTL)?;
    for_session(Path::new(CTL), 0o660);
    loop {
        let mut fds = vec![ctl.as_raw_fd(), m.ns.fd(), uev.as_raw_fd(), m.at.listener_fd()];
        fds.extend(tftp.fds());
        fds.extend(m.uim.as_ref().map(|u| u.fd()));
        fds.extend(m.wms.as_ref().map(|w| w.fd()));
        fds.extend(m.voice.as_ref().map(|v| v.fd()));
        fds.extend(m.nas.as_ref().map(|c| c.fd()));
        fds.extend(m.host.as_ref().map(|h| h.wds.fd()));
        fds.extend(m.vm.values().map(|s| s.wds.fd()));
        fds.extend(m.at.port_fd());
        let guests = m.at.guest_fds();
        fds.extend(guests.iter().map(|g| g.0));
        let wait = if m.at.backlog() { Duration::from_millis(20) } else { Duration::from_secs(5) };
        let ready = poll(&fds, Some(wait));
        if ready.contains(&ctl.as_raw_fd()) {
            let mut b = [0u8; 512];
            if let Ok((n, peer)) = ctl.recv_from(&mut b) {
                let reply = m.control(&String::from_utf8_lossy(&b[..n]));
                if let Some(p) = peer.as_pathname() {
                    let _ = ctl.send_to(reply.as_bytes(), p);
                }
            }
        }
        if ready.contains(&m.ns.fd()) {
            m.on_ns();
        }
        if ready.contains(&uev.as_raw_fd()) {
            let mut dump = false;
            while let Some(e) = read_uevent(uev.as_raw_fd()) {
                dump |= e.split(|c| *c == 0).any(|f| f == b"SUBSYSTEM=devcoredump");
            }
            if dump {
                efs::sync(&state.join("efs-backup"));
            }
        }
        tftp.service(&ready);
        m.indications(&ready);
        if ready.contains(&m.at.listener_fd()) && m.at.accept() {
            m.radio.reset(); // the holder's RIL starts over
        }
        if m.at.port_fd().is_some_and(|p| ready.contains(&p)) {
            m.at.read_modem(radio::modem_line);
        }
        for (fd, cid) in guests {
            if ready.contains(&fd) {
                m.guest(cid);
            }
        }
        m.at.flush();
        if m.lent.is_some() && m.node.is_some() && m.at.port_fd().is_none() {
            m.at.open_port(); // the modem came back, or its port did
        }
        m.host_data();
    }
}

/// The card's ICCID: EF ICCID (2FE2 under the MF) read over UIM, on the card's slot-1 session.
fn read_iccid(u: &mut QmiClient) -> Option<String> {
    // Read Transparent: 0x01 session (6 = card on slot 1, no AID), 0x02 file (id, path 3F00), 0x03 offset/length
    let r = u.request(0x0020, &[tlv(0x01, &[6, 0]), tlv(0x02, &[0xe2, 0x2f, 2, 0x00, 0x3f]), tlv(0x03, &[0, 0, 0, 0])], TIMEOUT).ok()?;
    let v = r.tlvs.get(&0x11)?; // read result: length u16, content
    let n = u16::from_le_bytes([*v.first()?, *v.get(1)?]) as usize;
    Some(iccid(v.get(2..2 + n)?)).filter(|i| !i.is_empty())
}

/// ICCID digits from EF ICCID (swapped BCD, low nibble first, F = filler).
fn iccid(b: &[u8]) -> String {
    b.iter().flat_map(|x| [x & 0xf, x >> 4]).filter(|d| *d < 10).map(|d| char::from(b'0' + d)).collect()
}

struct MtSms {
    must_ack: bool,
    txn: u32,
    /// the TPDU (no SMSC address)
    raw: Vec<u8>,
}

/// WMS event report TLV 0x11 (transfer route MT message).
fn transfer_route_sms(v: &[u8]) -> Option<MtSms> {
    let txn = u32::from_le_bytes(v.get(1..5)?.try_into().ok()?);
    let len = u16::from_le_bytes(v.get(6..8)?.try_into().ok()?) as usize;
    Some(MtSms { must_ack: *v.first()? == 0, txn, raw: v.get(8..8 + len)?.to_vec() })
}

fn main() -> std::process::ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: mship-modem (control: {CTL})");
        return std::process::ExitCode::from(2);
    }
    if let Err(e) = serve() {
        eprintln!("mship-modem: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_status_parse() {
        // primary GW none; one present card with a USIM (AID 4 bytes)
        let mut v = vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1];
        v.extend([1, 0, 0, 0, 0, 1]);
        v.extend([2, 1, 0, 0, 0, 0, 4, 0xa0, 0, 0, 0x87]);
        v.extend([0, 1, 3, 10, 1, 3, 10]);
        let (gw, cards) = card_status(&v).unwrap();
        assert_eq!(gw, 0xffff);
        assert!(cards[0].present);
        assert_eq!((cards[0].apps[0].kind, cards[0].apps[0].aid.clone()), (2, vec![0xa0, 0, 0, 0x87]));
        assert!(card_status(&v[..v.len() - 1]).is_none());
    }

    #[test]
    fn incoming_sms() {
        // ack indicator 0, txn 0x2a, format 6, a 4-byte TPDU
        let v = [0, 0x2a, 0, 0, 0, 6, 4, 0, 0x04, 0x0c, 0x91, 0xaa];
        let s = transfer_route_sms(&v).unwrap();
        assert!(s.must_ack);
        assert_eq!(s.txn, 0x2a);
        assert_eq!(s.raw, [0x04, 0x0c, 0x91, 0xaa]);
        assert!(transfer_route_sms(&v[..10]).is_none());
    }

    #[test]
    fn iccid_digits() {
        // EF ICCID: swapped BCD, F = filler
        assert_eq!(iccid(&[0x98, 0x12, 0x60, 0x10, 0x32, 0x54, 0x76, 0x98, 0x10, 0xf2]), "8921060123456789012");
    }
}
