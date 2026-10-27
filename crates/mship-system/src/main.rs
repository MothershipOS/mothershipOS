//! mship-system: the mothershipOS system jobs (root), one per systemd unit.
//!
//! - `net`: host networking for the VMs at boot ([`net`])
//! - `rtc restore|save|show`: wall time = PMIC RTC + a saved offset ([`rtc`])
//! - `timesync`: the clock from a public server's HTTP Date header, then the RTC offset saved ([`timesync`])
//! - `pwrkey-rearm [watch]`: keep the power key's interrupt enabled ([`pwrkey`])
//! - `gpu-noevict`: keep GPU buffers out of swap
//! - `install <release tar> [--force] [--no-restart]`: install a release (`cargo xtask dist`) ([`install`])
//! - `setup`: the packages, user, swap file and audio the release needs ([`setup`]; the installer runs it)
//! - `gadget-rebind <udc>`, `typec-host <port-partner>`: the USB-C port, from udev ([`usb`])
//! - `boot-ok`, `boot-image <fit> [--reboot]`: the boot counter, writing a boot image ([`boot`])
//! - `vm-install <repository | package> [--name <id>]`: install or update a VM from its repository (built on the
//!   phone) or a prebuilt package ([`vminstall`])

mod boot;
mod install;
mod net;
mod pwrkey;
mod rtc;
mod setup;
mod timesync;
mod usb;
mod vminstall;

use std::process::ExitCode;

pub type Res<T> = Result<T, String>;

/// Run a system tool; Err with its stderr if it failed.
pub fn run(cmd: &str, args: &[&str]) -> Res<()> {
    let o = std::process::Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if o.status.success() { Ok(()) } else { Err(format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim())) }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn set_clock(t: i64) -> Res<()> {
    let ts = libc::timespec { tv_sec: t, tv_nsec: 0 };
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } != 0 {
        return Err(format!("set clock: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    let r = match a.as_slice() {
        ["net"] => net::up(),
        ["rtc", cmd] => rtc::main(cmd),
        ["timesync"] => timesync::sync(),
        ["pwrkey-rearm"] => pwrkey::check(true).map(|_| ()),
        ["pwrkey-rearm", "watch"] => pwrkey::watch(),
        // with swap present, msm may evict GEM buffers to it; KVM faulting on an evicted buffer a VM has mapped
        // kills the VM ("vcpu hit unknown error: Bad address")
        ["gpu-noevict"] => std::fs::write("/sys/module/msm/parameters/enable_eviction", "N").map_err(|e| format!("msm eviction: {e}")),
        ["setup"] => setup::setup(),
        ["boot-ok"] => boot::boot_ok(),
        ["vm-install", source] => vminstall::install(source, None),
        ["vm-install", source, "--name", id] => vminstall::install(source, Some(id)),
        ["boot-image", fit, flags @ ..] => boot::boot_image(std::path::Path::new(fit), flags.contains(&"--reboot")),
        ["gadget-rebind", udc] => usb::gadget_rebind(udc),
        ["typec-host", partner] => usb::typec_host(partner),
        ["install", tar, flags @ ..] => install::install(std::path::Path::new(tar), flags.contains(&"--force"), !flags.contains(&"--no-restart")),
        _ => Err("usage: mship-system net | rtc restore|save|show | timesync | pwrkey-rearm [watch] | gpu-noevict | setup | vm-install <repository|package> [--name <id>] | boot-ok | boot-image <fit> [--reboot] | gadget-rebind <udc> | typec-host <port-partner> | install <tar> [--force] [--no-restart]".into()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mship-system: {e}");
            ExitCode::FAILURE
        }
    }
}
