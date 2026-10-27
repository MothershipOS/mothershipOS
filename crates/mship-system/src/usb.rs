//! The phone's USB-C port, from udev:
//!
//! - `gadget-rebind <udc>`: the dwc3 drops its UDC while the port is a host (dock, display) and registers it again
//!   as a device: bind the dev link gadget (the host initramfs's "m": NCM 10.42.0.2 + ACM) to it again, so the PC
//!   link comes back by itself
//! - `typec-host <port>-partner`: a PD partner that left us as the USB device (monitors and docks don't always ask
//!   for the data role swap, and as the device the phone never enters their alt modes, e.g. DisplayPort): ask for
//!   the host role. Non-PD links (the dev link to a PC over a USB-A cable) are left alone.

use std::path::Path;
use std::time::Duration;

use crate::Res;

const GADGET: &str = "/sys/kernel/config/usb_gadget/m";

fn log(msg: &str) {
    // to the journal, as the old logger -t lines
    let _ = std::process::Command::new("logger").args(["-t", "mship-usb-c", msg]).status();
}

pub fn gadget_rebind(udc: &str) -> Res<()> {
    let bound = std::fs::read_to_string(Path::new(GADGET).join("UDC")).map(|u| !u.trim().is_empty());
    // no gadget, already bound, or tools' virtual UDC (dummy_hcd)
    if bound.ok() != Some(false) || udc.starts_with("dummy_udc") {
        return Ok(());
    }
    std::fs::write(Path::new(GADGET).join("UDC"), udc).map_err(|e| format!("bind the gadget to {udc}: {e}"))?;
    log(&format!("dev link gadget bound to {udc}"));
    Ok(())
}

pub fn typec_host(partner: &str) -> Res<()> {
    let port = Path::new("/sys/class/typec").join(partner.strip_suffix("-partner").unwrap_or(partner));
    let pd = || {
        std::fs::read_to_string(format!("{}-partner/supports_usb_power_delivery", port.display())).is_ok_and(|s| s.trim() == "yes")
    };
    // the PD contract first
    for _ in 0..10 {
        if pd() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if !pd() || !std::fs::read_to_string(port.join("data_role")).is_ok_and(|r| r.contains("[device]")) {
        return Ok(());
    }
    std::fs::write(port.join("data_role"), "host").map_err(|e| format!("{}: host data role: {e}", port.display()))?;
    log(&format!("requested the host data role from the PD partner on {}", port.file_name().unwrap_or_default().to_string_lossy()));
    Ok(())
}
