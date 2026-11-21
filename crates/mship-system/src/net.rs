//! Host networking for the VMs, at boot (mship-vm-net.service): forwarding and the shared internet's chain. A VM's
//! network cards are blank taps that come and go with it (mship_core::vmnet); the handles' services (mship-net,
//! mship-modem, mship-wifi-bridge) give them their network while attached.

use crate::{Res, run};

/// legacy iptables (the kernel has no nf_tables)
const IPT: &str = "iptables-legacy";
const OLD_DNSMASQ_PID: &str = "/run/mship-vm-dnsmasq.pid";

fn exists(dev: &str) -> bool {
    std::path::Path::new("/sys/class/net").join(dev).exists()
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
    for m in ["tun", "nf_conntrack", "iptable_filter", "iptable_nat", "xt_MASQUERADE", "xt_conntrack"] {
        run("modprobe", &[m])?;
    }
    sysctl("net.ipv4.ip_forward", "1")?;
    // the shared internet's chain: its rules are mship-net's while it is attached to a VM
    chain("filter", "MSHIP_VM", "FORWARD", true)?;
    chain("nat", "MSHIP_VM", "POSTROUTING", true)?;
    // what older releases set up for fixed VMs: their taps and DHCP
    if let Some(pid) = std::fs::read_to_string(OLD_DNSMASQ_PID).ok().and_then(|p| p.trim().parse::<i32>().ok()) {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    for t in ["pxmtap0", "pxetap0", "pxwtap0", "pxwltap0", "ubtap0"] {
        if exists(t) {
            let _ = run("ip", &["link", "delete", t]);
        }
    }
    println!("VM networking: forwarding and chains ready (each VM's interfaces come with it)");
    Ok(())
}
