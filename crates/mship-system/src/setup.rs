//! `setup`: what the release needs on the phone beyond its files (the installer runs it; every step is idempotent):
//! the packages it uses, the session user, the swap file, and the session's audio (PipeWire as the session user).

use std::path::Path;

use crate::{Res, run};

/// Ubuntu packages mothershipOS uses on the phone
const PACKAGES: &[&str] = &[
    // the session
    "sway", "swaybg", "swayidle", "seatd", "foot", "fonts-dejavu-core", "xkb-data", "grim",
    "libgl1-mesa-dri", "libegl1", "libgles2", "libgbm1", "libgtk-4-1", "libadwaita-1-0",
    // network
    "network-manager", "wpasupplicant", "iw", "wireless-regdb", "rfkill", "dnsmasq-base", "iptables",
    // audio
    "alsa-utils", "alsa-ucm-conf", "pipewire", "pipewire-pulse", "wireplumber", "libspa-0.2-bluetooth",
];
const USER: &str = "mship";
const GROUPS: &str = "video,render,input,audio";
const SWAP: &str = "/var/lib/mothership/swapfile";
/// the swap file: room for two VMs side by side on the 7.5 GB phone (RAM stays first: swappiness 10)
const SWAP_SIZE: i64 = 4 << 30;

fn installed(pkg: &str) -> bool {
    std::process::Command::new("dpkg-query")
        .args(["-W", "-f", "${Status}", pkg])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("ok installed"))
}

fn packages() -> Res<()> {
    let missing: Vec<&str> = PACKAGES.iter().copied().filter(|p| !installed(p)).collect();
    if missing.is_empty() {
        return Ok(());
    }
    println!("installing {}", missing.join(" "));
    let mut args = vec!["install", "-y", "-qq", "--no-install-recommends"];
    args.extend(&missing);
    let st = std::process::Command::new("apt-get").args(&args).env("DEBIAN_FRONTEND", "noninteractive").status().map_err(|e| e.to_string())?;
    if !st.success() {
        return Err(format!("apt-get install {}: failed", missing.join(" ")));
    }
    Ok(())
}

fn user() -> Res<u32> {
    if run("id", &["-u", USER]).is_err() {
        run("useradd", &["-m", "-s", "/bin/bash", "-G", GROUPS, USER])?;
    } else {
        run("usermod", &["-aG", GROUPS, USER])?;
    }
    let o = std::process::Command::new("id").args(["-u", USER]).output().map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&o.stdout).trim().parse().map_err(|e| format!("id -u {USER}: {e}"))
}

fn swap() -> Res<()> {
    if Path::new(SWAP).exists() {
        return Ok(());
    }
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(SWAP).map_err(|e| format!("{SWAP}: {e}"))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, SWAP_SIZE) } != 0 {
        let e = std::io::Error::last_os_error();
        let _ = std::fs::remove_file(SWAP);
        return Err(format!("{SWAP}: {e}"));
    }
    drop(f);
    run("mkswap", &[SWAP])?;
    run("systemctl", &["start", "var-lib-mothership-swapfile.swap"])
}

/// PipeWire for the session user (the VMs' and the shell's sound), reachable from the sway session.
fn audio(uid: u32) -> Res<()> {
    run("loginctl", &["enable-linger", USER])?;
    let dropin = "/etc/systemd/system/mship-sway.service.d/audio.conf";
    let want = format!("[Service]\nEnvironment=PIPEWIRE_RUNTIME_DIR=/run/user/{uid}\nEnvironment=PULSE_SERVER=unix:/run/user/{uid}/pulse/native\n");
    if std::fs::read_to_string(dropin).ok().as_deref() != Some(want.as_str()) {
        std::fs::write(dropin, want).map_err(|e| format!("{dropin}: {e}"))?;
        run("systemctl", &["daemon-reload"])?;
    }
    run("systemctl", &["start", &format!("user@{uid}.service")])?;
    let st = std::process::Command::new("runuser")
        .args(["-u", USER, "--", "systemctl", "--user", "enable", "--now", "pipewire.socket", "pipewire.service",
               "pipewire-pulse.socket", "pipewire-pulse.service", "wireplumber.service"])
        .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))
        .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path=/run/user/{uid}/bus"))
        .status()
        .map_err(|e| e.to_string())?;
    if !st.success() {
        return Err("enable PipeWire for the session user: failed".into());
    }
    Ok(())
}

pub fn setup() -> Res<()> {
    packages()?;
    let uid = user()?;
    swap()?;
    audio(uid)?;
    run("systemctl", &["enable", "--now", "seatd.service"])?;
    println!("set up");
    Ok(())
}
