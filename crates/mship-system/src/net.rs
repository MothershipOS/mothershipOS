//! Host networking for the VMs, at boot (mship-vm-net.service).
//!
//! PixelOS (Cuttlefish) expects eth0 (mobile, buried), eth1 (ethernet), eth2 (wrapped into virt_wifi wlan0): three
//! persistent taps owned by the session user; the ethernet and wifi taps get DHCP (dnsmasq) so the guest always
//! sees its networks. Internet is part of the hardware handover, not set up here: lending "internet" to a VM NATs
//! its wifi tap out through the phone's uplink; until then (and after taking it back) the MSHIP_VM chains drop
//! everything the VM tries to forward. pxwltap0 is the native Wi-Fi data path (mship.wifi: mship-wifi-bridge
//! redirects it to a band interface). The Ubuntu desktop VM has its own network, ubtap0, always NATed out of the
//! phone's uplink (fixed lease 192.168.98.2), independent of the lendable internet slot.

use std::os::fd::AsRawFd;

use crate::{Res, run};

const USER: &str = "mship";
/// legacy iptables (the kernel has no nf_tables)
const IPT: &str = "iptables-legacy";
const DNSMASQ_PID: &str = "/run/mship-vm-dnsmasq.pid";

const TUNSETIFF: libc::c_ulong = 0x400454ca;
const TUNSETPERSIST: libc::c_ulong = 0x400454cb;
const TUNSETOWNER: libc::c_ulong = 0x400454cc;
const IFF_TAP: libc::c_short = 0x0002;
const IFF_NO_PI: libc::c_short = 0x1000;
const IFF_VNET_HDR: libc::c_short = 0x4000;

fn uid(name: &str) -> u32 {
    let n = std::ffi::CString::new(name).unwrap();
    let pw = unsafe { libc::getpwnam(n.as_ptr()) };
    if pw.is_null() { 1000 } else { unsafe { (*pw).pw_uid } }
}

fn exists(dev: &str) -> bool {
    std::path::Path::new("/sys/class/net").join(dev).exists()
}

/// A persistent tap owned by the session user (as `ip tuntap add mode tap user mship vnet_hdr`), up.
fn tap(name: &str) -> Res<()> {
    if !exists(name) {
        let f = std::fs::OpenOptions::new().read(true).write(true).open("/dev/net/tun").map_err(|e| format!("/dev/net/tun: {e}"))?;
        let mut ifr = [0u8; 40]; // struct ifreq: name[16], flags
        ifr[..name.len()].copy_from_slice(name.as_bytes());
        ifr[16..18].copy_from_slice(&(IFF_TAP | IFF_NO_PI | IFF_VNET_HDR).to_ne_bytes());
        let fd = f.as_raw_fd();
        unsafe {
            if libc::ioctl(fd, TUNSETIFF, ifr.as_mut_ptr()) != 0
                || libc::ioctl(fd, TUNSETOWNER, uid(USER) as libc::c_ulong) != 0
                || libc::ioctl(fd, TUNSETPERSIST, 1 as libc::c_ulong) != 0
            {
                return Err(format!("tap {name}: {}", std::io::Error::last_os_error()));
            }
        }
    }
    run("ip", &["link", "set", name, "up"])
}

fn sysctl(key: &str, v: &str) -> Res<()> {
    std::fs::write(format!("/proc/sys/{}", key.replace('.', "/")), v).map_err(|e| format!("{key}: {e}"))
}

fn ipt(args: &[&str]) -> Res<()> {
    run(IPT, args)
}

/// A chain of ours, empty, hooked into `hook` (`-I` first, `-A` last).
fn chain(table: &str, chain: &str, hook: &str, first: bool) -> Res<()> {
    if ipt(&["-t", table, "-N", chain]).is_err() {
        ipt(&["-t", table, "-F", chain])?;
    }
    if ipt(&["-t", table, "-C", hook, "-j", chain]).is_err() {
        ipt(&["-t", table, if first { "-I" } else { "-A" }, hook, "-j", chain])?;
    }
    Ok(())
}

pub fn up() -> Res<()> {
    tap("pxmtap0")?;
    tap("pxetap0")?;
    run("ip", &["addr", "replace", "192.168.97.1/24", "dev", "pxetap0"])?;
    tap("pxwtap0")?;
    run("ip", &["addr", "replace", "192.168.96.1/24", "dev", "pxwtap0"])?;
    run("ip", &["link", "set", "pxwtap0", "down"])?; // carrier only while lent
    tap("pxwltap0")?;
    tap("ubtap0")?;
    run("ip", &["addr", "replace", "192.168.98.1/24", "dev", "ubtap0"])?;
    sysctl("net.ipv6.conf.pxwltap0.disable_ipv6", "1")?;
    for m in ["nf_conntrack", "iptable_filter", "iptable_nat", "xt_MASQUERADE", "xt_conntrack"] {
        run("modprobe", &[m])?;
    }
    sysctl("net.ipv4.ip_forward", "1")?;
    // the lendable internet slot: closed from boot (mothershipd's owner change rewrites MSHIP_VM)
    chain("filter", "MSHIP_VM", "FORWARD", true)?;
    chain("nat", "MSHIP_VM", "POSTROUTING", true)?;
    for t in ["pxwtap0", "pxetap0", "pxmtap0"] {
        ipt(&["-A", "MSHIP_VM", "-i", t, "-j", "DROP"])?;
    }
    // always-on VM uplinks: their own chains, untouched by the internet slot's owner changes
    chain("filter", "MSHIP_VMNAT", "FORWARD", false)?;
    chain("nat", "MSHIP_VMNAT", "POSTROUTING", false)?;
    ipt(&["-A", "MSHIP_VMNAT", "-i", "ubtap0", "-j", "ACCEPT"])?;
    ipt(&["-A", "MSHIP_VMNAT", "-o", "ubtap0", "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT"])?;
    ipt(&["-t", "nat", "-A", "MSHIP_VMNAT", "-s", "192.168.98.0/24", "!", "-d", "192.168.98.0/24", "-j", "MASQUERADE"])?;
    // a running instance keeps its addresses until it has exited
    if let Some(pid) = std::fs::read_to_string(DNSMASQ_PID).ok().and_then(|p| p.trim().parse::<i32>().ok()) {
        unsafe { libc::kill(pid, libc::SIGTERM) };
        for _ in 0..30 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    run("dnsmasq", &[
        "--conf-file=/dev/null", &format!("--pid-file={DNSMASQ_PID}"), "--bind-interfaces", "--except-interface=lo",
        "--interface=pxwtap0", "--interface=pxetap0", "--interface=ubtap0",
        "--dhcp-range=pxwtap0,192.168.96.2,192.168.96.20,12h", "--dhcp-range=pxetap0,192.168.97.2,192.168.97.20,12h",
        "--dhcp-range=ubtap0,192.168.98.2,192.168.98.20,12h", "--dhcp-host=52:54:00:12:34:56,192.168.98.2",
        "--dhcp-option=pxwtap0,6,8.8.8.8,1.1.1.1", "--dhcp-option=pxetap0,6,8.8.8.8,1.1.1.1",
        "--dhcp-option=ubtap0,6,8.8.8.8,1.1.1.1", "--log-facility=/run/mship-vm-dnsmasq.log",
    ])?;
    println!("VM networking: taps pxmtap0 pxetap0 pxwtap0 (closed until internet is lent), pxwltap0, ubtap0 (Ubuntu, NATed), DHCP up");
    Ok(())
}
