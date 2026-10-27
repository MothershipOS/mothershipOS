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
//! The handle `cellular` (group `cellular`, exclusive) lends the modem to a VM: its radio stack talks AT through the
//! modem slot (`at`), and its data calls are sessions on their own mux (`cellvm<cid>`) routed to the VM's data NIC
//! (cuttlefish-arm64: the first NIC, tap pxmtap0), in the kernel. While lent, the host has no mobile data.
//!
//! Control: datagrams on /run/mothership/modem.sock, one reply each: `attach <vm>` (by its directory name),
//! `detach <vm> [user|screen|stop|revoked|gone]`, `status`. Configuration (optional): /etc/mothership/modem.json
//! `{"host_data": true, "profile": 1}`.

mod at;
mod efs;
mod tftp;

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use mship_devices::qrtr::{PORT_CTRL, QmiClient, Qrtr, ctrl, ctrl_packet, tlv, tlv_u8, tlv_u32};
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
    fn context(&self) -> at::Context {
        at::Context { address: format!("{}/{}", self.addr, self.prefix), gateway: self.gw.to_string() }
    }

    /// End it at the modem (closing the client would too).
    fn stop(&mut self) {
        let _ = self.wds.request(0x21, &[tlv_u32(0x01, self.handle)], Duration::from_secs(5));
    }
}

/// A session on 3GPP profile `profile`, bound to QMAP mux `mux` (its rmnet link `iface`, up).
fn start_session(wds_at: (u32, u32), profile: u8, mux: u8, iface: &str) -> Result<Session, String> {
    if !exists(iface) {
        run("ip", &["link", "add", iface, "link", DATA_DEV, "type", "rmnet", "mux_id", &mux.to_string()])?;
    }
    let mut wds = QmiClient::new(WDS, wds_at.0, wds_at.1).map_err(|e| e.to_string())?;
    let ep = [EP_PCIE.to_le_bytes(), EP_IFACE.to_le_bytes()].concat();
    wds.request(0xa2, &[tlv(0x10, &ep), tlv_u8(0x11, mux)], TIMEOUT)?; // bind mux data port
    // start network: profile, IPv4
    let r = wds.request(0x20, &[tlv_u8(0x31, profile), tlv_u8(0x19, 4)], Duration::from_secs(60))?;
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
        sysctl(&format!("net.ipv4.conf.{VM_DATA_TAP}.proxy_arp"), "1");
        sysctl(&format!("net.ipv4.conf.{}.rp_filter", s.iface), "0");
        let _ = run("ip", &["rule", "del", "from", &a, "iif", VM_DATA_TAP, "table", &table]);
        let _ = run("ip", &["rule", "add", "from", &a, "iif", VM_DATA_TAP, "table", &table, "priority", &table]);
    } else {
        let _ = run("ip", &["rule", "del", "from", &a, "iif", VM_DATA_TAP, "table", &table]);
    }
    let _ = run("ip", &["route", rt, &format!("{a}/32"), "dev", VM_DATA_TAP]);
    let _ = run("ip", &["route", rt, "default", "dev", &s.iface, "table", &table]);
    let _ = run(IPT, &[ipt, CHAIN, "-i", VM_DATA_TAP, "-o", &s.iface, "-s", &a, "-j", "ACCEPT"]);
    let _ = run(IPT, &[ipt, CHAIN, "-i", &s.iface, "-o", VM_DATA_TAP, "-d", &a, "-j", "ACCEPT"]);
}

/// Our forwarding chain, before mship-vm-net's (which drops whatever the VM taps forward), and MSS clamping on the
/// mobile links (their MTU is the network's, usually below the guest's 1500).
fn firewall() {
    if run(IPT, &["-N", CHAIN]).is_err() {
        let _ = run(IPT, &["-F", CHAIN]);
    }
    if run(IPT, &["-C", "FORWARD", "-j", CHAIN]).is_err() {
        let _ = run(IPT, &["-I", "FORWARD", "1", "-j", CHAIN]);
    }
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
        self.host_retry = Instant::now() + Duration::from_secs(5);
        if self.lent.is_some() {
            self.at.open_port();
        }
    }

    fn modem_down(&mut self) {
        log("modem down");
        self.node = None;
        self.uim = None;
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
        let r = self.data_path().and_then(|wds| start_session(wds, self.profile, HOST_MUX, HOST_IF))
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
            log("data: host session ended");
        }
    }

    /// Sessions that ended at the modem (network detach, no coverage); card changes.
    fn indications(&mut self, ready: &[RawFd]) {
        if let Some(u) = self.uim.as_mut().filter(|u| ready.contains(&u.fd())) {
            match u.indications() {
                Ok(i) if i.iter().any(|m| m.msg == 0x32) => self.provision(),
                Ok(_) => {}
                Err(_) => self.uim = None,
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
        for c in self.at.read_guest(cid) {
            match c {
                at::Cmd::Forward(b) => {
                    if !self.at.send_modem(&b) {
                        self.at.answer(cid, false);
                    }
                }
                at::Cmd::Answer(ok) => self.at.answer(cid, ok),
                at::Cmd::Activate(ctxs) => {
                    let ok = ctxs.iter().all(|c| self.activate(*c));
                    self.at.answer(cid, ok);
                }
                at::Cmd::Deactivate(ctxs) => {
                    let all: Vec<u8> = if ctxs.is_empty() { self.vm.keys().copied().collect() } else { ctxs };
                    for c in all {
                        if let Some(mut s) = self.vm.remove(&c) {
                            s.stop();
                            vm_route(c, &s, false);
                            log(format!("data: context {c} ended by the VM"));
                        }
                    }
                    self.at.answer(cid, true);
                }
            }
        }
    }

    /// The holder's data call on context `cid` (an existing one is reused: the guest's RIL never ends calls).
    fn activate(&mut self, cid: u8) -> bool {
        if self.vm.contains_key(&cid) {
            return true;
        }
        let iface = format!("cellvm{cid}");
        match self.data_path().and_then(|wds| start_session(wds, cid, VM_MUX + cid, &iface)) {
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
    let ns = Qrtr::open()?;
    ns.lookup(0)?;
    let mut m = Modem {
        ns, servers: BTreeMap::new(), node: None, uim: None, dpm: None,
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
        if ready.contains(&m.at.listener_fd()) {
            m.at.accept();
        }
        if m.at.port_fd().is_some_and(|p| ready.contains(&p)) {
            let ctx: BTreeMap<u8, at::Context> = m.vm.iter().map(|(c, s)| (*c, s.context())).collect();
            m.at.read_modem(&ctx);
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
}
