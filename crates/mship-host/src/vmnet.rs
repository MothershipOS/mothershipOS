//! A VM's network cards on the host (root): blank taps (no address, no carrier), created when the VM starts and
//! removed when it stops (mship_core::vmnet). Everything else is the attached handle's service: mship-net (the
//! shared internet), mship-modem (mobile data), mship-wifi-bridge (a Wi-Fi band).

use std::os::fd::AsRawFd;
use std::path::Path;

use mship_core::vmnet::{self, VmNet};

use crate::util::{Result, fail, run};

const USER: &str = "mship";

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
    Path::new("/sys/class/net").join(dev).exists()
}

/// A persistent tap owned by the session user (as `ip tuntap add mode tap user mship vnet_hdr`).
fn tap(name: &str) -> Result<()> {
    if exists(name) {
        return Ok(());
    }
    let f = std::fs::OpenOptions::new().read(true).write(true).open("/dev/net/tun").map_err(|e| crate::util::Error(format!("/dev/net/tun: {e}")))?;
    let mut ifr = [0u8; 40]; // struct ifreq: name[16], flags
    ifr[..name.len()].copy_from_slice(name.as_bytes());
    ifr[16..18].copy_from_slice(&(IFF_TAP | IFF_NO_PI | IFF_VNET_HDR).to_ne_bytes());
    let fd = f.as_raw_fd();
    unsafe {
        if libc::ioctl(fd, TUNSETIFF, ifr.as_mut_ptr()) != 0
            || libc::ioctl(fd, TUNSETOWNER, uid(USER) as libc::c_ulong) != 0
            || libc::ioctl(fd, TUNSETPERSIST, 1 as libc::c_ulong) != 0
        {
            return fail(format!("tap {name}: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn sysctl(key: &str, v: &str) {
    let _ = std::fs::write(format!("/proc/sys/{}", key.replace('.', "/")), v);
}

/// A VM's cards, blank, before its launcher starts; recorded for the launcher and the services.
pub fn up(vm: &str, conf: &serde_json::Value) -> Result<VmNet> {
    let net = vmnet::plan_for(vm, conf);
    std::fs::create_dir_all(vmnet::STATE_DIR)?;
    // a CID taken by another running VM would give two VMs the same cards
    if let Some(other) = vmnet::all().into_iter().find(|o| o.cid == net.cid && o.vm != vm) {
        return fail(format!("{vm}: vsock CID {} is {}'s (vm.json \"cid\" must differ)", net.cid, other.vm));
    }
    for n in &net.nics {
        tap(&n.tap)?;
        // no IPv6 on the host side: a blank card must not autoconfigure or answer anything
        sysctl(&format!("net.ipv6.conf.{}.disable_ipv6", n.tap), "1");
        let _ = run("ip", &["link", "set", &n.tap, "down"]);
    }
    std::fs::write(vmnet::state_path(vm), serde_json::to_string_pretty(&net).unwrap_or_default())?;
    Ok(net)
}

/// A VM's cards, gone (it has stopped; its handles were taken back first).
pub fn down(vm: &str) {
    let Some(net) = vmnet::load(vm) else { return };
    for n in &net.nics {
        let _ = run("ip", &["link", "delete", &n.tap]);
    }
    let _ = std::fs::remove_file(vmnet::state_path(vm));
}
