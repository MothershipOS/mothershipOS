//! mship-net: the phone's internet for a VM (root), the mship.net-shared 1.x contract. Handle `internet:uplink`
//! (group `internet`), exclusive: one VM at a time.
//!
//! Every VM has a blank `internet` card (mship_core::vmnet: a tap with no address and no carrier). Attached, this
//! service gives the VM its network on that card: the host's address, DHCP (the guest gets its fixed address:
//! `vmnet::internet_addrs`), NAT out of whatever uplink the phone has (Wi-Fi, mobile data, USB), carrier. Detached,
//! all of it goes again and the card is blank. Rules: chain MSHIP_VM (filter FORWARD first, nat POSTROUTING; made
//! empty at boot by mship-vm-net).
//!
//! Control: datagrams on /run/mothership/net.sock, one reply each: `attach <vm>` (by its directory name),
//! `detach <vm> [user|screen|stop|revoked|gone]`, `status`.

use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::process::Command;

use mship_core::handle::{Direction, Handle};
use mship_core::vmnet;
use mship_devices::{for_session, fresh, log, publish_handles};
use serde_json::json;

const CTL: &str = "/run/mothership/net.sock";
/// legacy iptables (the kernel has no nf_tables)
const IPT: &str = "iptables-legacy";
const DNS: &str = "8.8.8.8,1.1.1.1";

fn run(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok_and(|o| o.status.success())
}

fn pidfile(vm: &str) -> String {
    format!("/run/mothership/net-{vm}.dnsmasq.pid")
}

fn stop_dhcp(vm: &str) {
    if let Some(pid) = std::fs::read_to_string(pidfile(vm)).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
        unsafe { libc::kill(pid, libc::SIGTERM) };
        for _ in 0..30 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    let _ = std::fs::remove_file(pidfile(vm));
}

struct Net {
    /// the VM holding the handle, and its card
    holder: Option<(String, String)>,
}

impl Net {
    fn attach(&mut self, vm: &str) -> Result<(), String> {
        if self.holder.as_ref().is_some_and(|(h, _)| h == vm) {
            return Ok(());
        }
        if let Some((h, _)) = self.holder.clone() {
            self.detach(&h, "revoked");
        }
        let net = vmnet::load(vm).ok_or(format!("{vm}: not running (no network cards)"))?;
        let card = net.card("internet").ok_or(format!("{vm}: no internet card"))?.clone();
        let (host, guest, prefix) = vmnet::internet_addrs(net.cid);
        let subnet = format!("{}.0/{prefix}", host.rsplit_once('.').map_or("", |x| x.0));
        let tap = card.tap.as_str();
        run("ip", &["addr", "replace", &format!("{host}/{prefix}"), "dev", tap]);
        // complete checksums towards the guest: with the tap's tx checksum offload, DHCP replies reach it with the
        // checksum left to the receiver, and DHCP clients that read raw frames (connman, busybox) drop them
        run("ethtool", &["-K", tap, "tx", "off"]);
        run("ip", &["link", "set", tap, "up"]);
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        run(IPT, &["-F", "MSHIP_VM"]);
        run(IPT, &["-t", "nat", "-F", "MSHIP_VM"]);
        run(IPT, &["-A", "MSHIP_VM", "-i", tap, "-j", "ACCEPT"]);
        run(IPT, &["-A", "MSHIP_VM", "-o", tap, "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT"]);
        run(IPT, &["-t", "nat", "-A", "MSHIP_VM", "-s", &subnet, "!", "-d", &subnet, "-j", "MASQUERADE"]);
        stop_dhcp(vm);
        let ok = run("dnsmasq", &[
            "--conf-file=/dev/null", &format!("--pid-file={}", pidfile(vm)), "--bind-interfaces", "--except-interface=lo",
            &format!("--interface={tap}"), &format!("--dhcp-range={tap},{guest},{guest},12h"),
            &format!("--dhcp-host={},{guest}", card.mac), &format!("--dhcp-option={tap},6,{DNS}"),
            &format!("--log-facility=/run/mothership/net-{vm}.dnsmasq.log"),
        ]);
        if !ok {
            log(format!("{vm}: dnsmasq did not start"));
        }
        // for the host's status (the Wi-Fi page: "lent to")
        let _ = mship_host::wifi::owner_set("wifi", &format!("vm:{}/{vm}", mship_host::vm::VMS));
        self.holder = Some((vm.into(), card.tap.clone()));
        log(format!("{vm}: attached ({tap}: host {host}, guest {guest})"));
        Ok(())
    }

    fn detach(&mut self, vm: &str, reason: &str) {
        let Some((h, tap)) = self.holder.clone().filter(|(h, _)| h == vm) else { return };
        run(IPT, &["-F", "MSHIP_VM"]);
        run(IPT, &["-t", "nat", "-F", "MSHIP_VM"]);
        stop_dhcp(&h);
        if Path::new("/sys/class/net").join(&tap).exists() {
            run("ip", &["addr", "flush", "dev", &tap]);
            run("ip", &["link", "set", &tap, "down"]);
        }
        let _ = mship_host::wifi::owner_set("wifi", "host");
        self.holder = None;
        log(format!("{vm}: released ({reason})"));
    }

    fn control(&mut self, msg: &str) -> String {
        let w: Vec<&str> = msg.split_whitespace().collect();
        match w.as_slice() {
            ["attach", vm] => match self.attach(vm) {
                Ok(()) => "ok".into(),
                Err(e) => format!("error: {e}"),
            },
            ["detach", vm, rest @ ..] => {
                self.detach(vm, rest.first().copied().unwrap_or("user"));
                "ok".into()
            }
            ["status", ..] => json!({"holder": self.holder.as_ref().map(|h| &h.0), "card": self.holder.as_ref().map(|h| &h.1)}).to_string(),
            _ => "error: attach <vm> | detach <vm> [reason] | status".into(),
        }
    }
}

fn serve() -> std::io::Result<()> {
    // nothing is lent at start (mothershipd lends again what is assigned): no rules from an earlier run
    run(IPT, &["-F", "MSHIP_VM"]);
    run(IPT, &["-t", "nat", "-F", "MSHIP_VM"]);
    fresh(Path::new(CTL));
    let ctl = UnixDatagram::bind(CTL)?;
    for_session(Path::new(CTL), 0o660);
    publish_handles("net", &[Handle::new("internet:uplink", "mship.net-shared", "Shared internet", "network-transmit-receive-symbolic",
                                         Direction::Both, "internet")
        .caps(json!({"ipv4": true}))
        .control(CTL, None)]);
    let mut n = Net { holder: None };
    log(format!("serving {CTL}"));
    let mut m = [0u8; 512];
    loop {
        let Ok((len, peer)) = ctl.recv_from(&mut m) else { continue };
        let reply = n.control(&String::from_utf8_lossy(&m[..len]));
        if let Some(p) = peer.as_pathname() {
            let _ = ctl.send_to(reply.as_bytes(), p);
        }
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: mship-net (control: {CTL})");
        return std::process::ExitCode::from(2);
    }
    match serve() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mship-net: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
