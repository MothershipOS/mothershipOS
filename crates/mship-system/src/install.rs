//! Install a release (`cargo xtask dist`: a root filesystem overlay). Each file is written beside its target and
//! renamed over it (a running program is replaced, not modified); what the release retires (the script stack before
//! the Rust one) is removed. VMs must be stopped first (their launchers and bridges are replaced): `--force` skips
//! that check.

use std::io::Read;
use std::path::Path;

use crate::{Res, run};

const VMS: &str = "/var/lib/mothership/vms";

/// The script stack the Rust programs replace: removed on install.
const RETIRED: &[&str] = &[
    "/usr/local/lib/mothership/mothershipd",
    "/usr/local/lib/mothership/battery",
    "/usr/local/lib/mothership/bt-bridge",
    "/usr/local/lib/mothership/usb",
    "/usr/local/lib/mothership/wifi-bridge",
    "/usr/local/sbin/mship-admin",
    "/usr/local/sbin/mship-admin.bak",
    "/usr/local/sbin/__pycache__",
    "/usr/local/sbin/mship-vm-net",
    "/usr/local/sbin/mship-rtc",
    "/usr/local/sbin/mship-timesync",
    "/usr/local/sbin/mship-pwrkey-rearm",
    "/usr/local/bin/mothership-shell",
    "/usr/local/bin/mothership-shell.bak",
    "/usr/local/bin/mship-handover",
    "/usr/local/bin/mship-handover.bak-script",
    "/usr/local/bin/mship-home",
    "/usr/local/bin/mship-idle",
    "/usr/local/bin/mship-power",
    "/usr/local/bin/mship-vm-display",
    "/usr/local/bin/mship-vm-display.bak-20261025",
    "/usr/local/bin/mship-vm-display.prev",
    "/usr/local/bin/mship-vm-inputd",
    "/usr/local/bin/mship-vm-kernel",
    "/usr/local/bin/mship-vm-pixelos",
    "/usr/local/bin/mship-vm-pixelos-crosvm",
    "/usr/local/bin/mship-vm-touch-fwd",
    "/usr/local/bin/mship-vm-ubuntu-desktop",
    "/usr/local/bin/mship-vm-ubuntu-touch",
    "/usr/local/bin/mship-vm-vsock-stub",
    // Linux guests run no mothershipOS program any more (their kernel has it all)
    "/usr/share/mothership/guest/mship-agent",
    "/usr/share/mothership/guest/mship-agent.service",
    "/usr/share/mothership/guest/systemd-suspend.service.d",
    // archived prototypes (motorola_nio/archive/prototypes), mship-init (nio-diag), the old power harness, or gone with the
    // Ubuntu Touch VM
    "/usr/local/sbin/mship-dockwatch",
    "/usr/local/sbin/mship-usb-test-device",
    "/usr/local/sbin/mship-power-test",
    "/usr/local/share/mship-power-tests",
    "/usr/local/bin/nio-diag",
    "/usr/local/bin/gcmd",
    "/usr/local/bin/gsh",
    "/usr/local/bin/gssh",
    "/usr/local/bin/vmrestart",
    "/usr/local/bin/sway-mship.prev",
    "/opt/mship-gpu/bin/virgl_test_server",
    "/opt/mship-gpu/bin/crosvm.prev",
    "/opt/mship-gpu/bin/crosvm.bak-20261021",
    "/opt/mship-gpu/bin/crosvm.bak-20261025",
    "/opt/mship-gpu/include",
    "/opt/mship-gpu/lib/pkgconfig",
    "/opt/mship-gpu/lib/libvirglrenderer.so.1.11.0.p",
    "/usr/local/sbin/mship-gadget-rebind",
    "/usr/local/sbin/mship-typec-host",
    // the shell needs no sudo any more: mothershipd does the privileged work
    "/etc/sudoers.d/50-mothership",
    // applied by mothershipd at start
    "/etc/systemd/system/mship-cpu.service",
    "/etc/systemd/system/mship-display.service",
];
const RETIRED_UNITS: &[&str] = &["mship-cpu.service", "mship-display.service"];
/// restarted after an install (the long-running services and the session)
const RESTART: &[&str] = &["mship-vm-net.service", "mship-battery.service", "mship-bt-bridge.service", "mship-camera.service", "mship-modem.service", "mship-usb.service", "mship-wifi-bridge.service",
                           "mship-pwrkey-rearm.service", "mothershipd.service", "mship-sway.service"];

fn remove(p: &Path) -> Res<()> {
    let r = if std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
    r.map_err(|e| format!("{}: {e}", p.display()))?;
    println!("removed {}", p.display());
    Ok(())
}

fn running_vms() -> Vec<String> {
    std::fs::read_dir(VMS)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            std::fs::read_to_string(e.path().join("run.pid")).ok().and_then(|p| p.trim().parse::<i32>().ok()).is_some_and(|p| unsafe { libc::kill(p, 0) } == 0)
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// vm.json "launcher" pointing at a retired script: removed (mothershipd starts mship-launch).
fn fix_manifests() {
    for e in std::fs::read_dir(VMS).into_iter().flatten().flatten() {
        let p = e.path().join("vm.json");
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let retired = v["launcher"].as_str().is_some_and(|l| RETIRED.contains(&l));
        if let (true, Some(o)) = (retired, v.as_object_mut()) {
            o.remove("launcher");
            let tmp = p.with_extension("json.tmp");
            use std::os::unix::fs::MetadataExt;
            let owner = std::fs::metadata(&p).map(|m| (m.uid(), m.gid())).unwrap_or((0, 0));
            if std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default() + "\n")
                .and_then(|_| std::os::unix::fs::chown(&tmp, Some(owner.0), Some(owner.1)))
                .and_then(|_| std::fs::rename(&tmp, &p))
                .is_ok()
            {
                println!("{}: launcher -> mship-launch", p.display());
            }
        }
    }
}

fn systemd(units: &[String]) -> Res<()> {
    run("systemctl", &["daemon-reload"])?;
    // the timer starts the time sync, the swap unit the swap file
    let enable: Vec<&str> = units.iter().map(String::as_str).filter(|u| *u != "mship-timesync.service").collect();
    let mut args = vec!["enable", "--quiet"];
    args.extend(enable);
    run("systemctl", &args)
}

fn restart_services() {
    for u in RESTART {
        // the session last: it restarts the shell
        if let Err(e) = run("systemctl", &["restart", u]) {
            println!("{e}");
        }
    }
}

pub fn install(tar: &Path, force: bool, restart: bool) -> Res<()> {
    let vms = running_vms();
    if !vms.is_empty() && !force {
        return Err(format!("stop the VMs first ({}), or --force", vms.join(", ")));
    }
    for u in RETIRED_UNITS {
        let _ = run("systemctl", &["disable", "--now", "--quiet", u]);
    }
    for p in RETIRED {
        if std::fs::symlink_metadata(p).is_ok() {
            remove(Path::new(p))?;
        }
    }
    let f = std::fs::File::open(tar).map_err(|e| format!("{}: {e}", tar.display()))?;
    let mut a = tar::Archive::new(f);
    for entry in a.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let rel = entry.path().map_err(|e| e.to_string())?.into_owned();
        let dst = Path::new("/").join(&rel);
        let (mode, uid, gid) = {
            let h = entry.header();
            (h.mode().unwrap_or(0o644), h.uid().unwrap_or(0) as u32, h.gid().unwrap_or(0) as u32)
        };
        let mut data = vec![];
        entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
        if std::fs::read(&dst).is_ok_and(|old| old == data) {
            continue; // unchanged
        }
        if let Some(d) = dst.parent()
            && !d.exists()
        {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            if uid != 0 {
                let _ = std::os::unix::fs::chown(d, Some(uid), Some(gid));
            }
        }
        // written beside it, then renamed over: a running program is replaced, not modified
        let tmp = dst.with_extension("mship-new");
        std::fs::write(&tmp, &data).map_err(|e| format!("{}: {e}", tmp.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
        std::os::unix::fs::chown(&tmp, Some(uid), Some(gid)).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        println!("{}", dst.display());
    }
    fix_manifests();
    // every unit of the release, changed or not
    let all: Vec<String> = tar::Archive::new(std::fs::File::open(tar).map_err(|e| e.to_string())?)
        .entries()
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|e| e.path().ok().map(|p| p.to_string_lossy().into_owned()))
        .filter_map(|p| p.strip_prefix("etc/systemd/system/").filter(|u| !u.contains('/')).map(String::from))
        .collect();
    systemd(&all)?;
    // udev rules and sysctl settings of the release
    let _ = run("udevadm", &["control", "--reload"]);
    let _ = run("sysctl", &["-q", "--system"]);
    crate::setup::setup()?;
    if restart {
        restart_services();
    }
    println!("installed");
    Ok(())
}
