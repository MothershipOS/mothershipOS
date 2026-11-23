//! `deploy [image] [--no-reboot]`: write the port's boot image (`cargo xtask boot-image`) to the running phone's boot
//! partition (the port's device.json boot.partition, e.g. the nio's boot_b) over the dev link (served over HTTP from
//! 10.42.0.1, fetched and written by the phone's shell, read back and compared), then reboot. How to recover from an
//! image that doesn't boot is in the port's boot notes.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::Digest;

use crate::shell;

/// Serve one file at /new.itb until dropped.
fn serve(data: Vec<u8>) -> Result<u16> {
    let l = TcpListener::bind(("10.42.0.1", 0)).context("the dev link (10.42.0.1) is not up")?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let mut req = [0u8; 4096];
            let _ = c.read(&mut req);
            let _ = c.write_all(format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", data.len()).as_bytes());
            let _ = c.write_all(&data);
        }
    });
    Ok(port)
}

/// The port's boot image as `cargo xtask boot-image` writes it (target/boot/ for the default port, target/<port>/boot/
/// for the others).
pub fn default_image() -> std::path::PathBuf {
    let name = crate::port_json("port.json")["boot"]["image"].as_str().unwrap_or("boot.img").to_string();
    let target = crate::repo().join("mothershipOS/target");
    let dir = if crate::port_name() == "motorola_nio" { target } else { target.join(crate::port_name()) };
    dir.join("boot").join(name)
}

pub fn main(fit: &Path, reboot: bool) -> Result<i32> {
    let dev_conf = crate::port_json("rootfs/etc/mothership/device.json");
    let Some(part) = dev_conf["boot"]["partition"].as_str() else {
        bail!("{}: device.json names no boot.partition (this phone boots its image another way: the port's boot notes)", crate::port_name());
    };
    let data = std::fs::read(fit).with_context(|| fit.display().to_string())?;
    let sum: String = sha2::Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    let size = data.len();
    let port = serve(data)?;
    // runs in busybox on the phone (the RAM host has no mship-system)
    let flash = format!(
        "D=$(for d in /sys/class/block/*; do grep -q '^PARTNAME={part}$' $d/uevent && basename $d; done); \
         [ -n \"$D\" ] || {{ echo NO_BOOT_PARTITION; exit 1; }}; cd /tmp; rm -f new.itb; \
         WG=wget; command -v wget >/dev/null || WG='busybox wget'; $WG -q -O new.itb http://10.42.0.1:{port}/new.itb; \
         [ \"$(sha256sum new.itb | cut -d' ' -f1)\" = {sum} ] || {{ echo DL_BAD; exit 1; }}; echo DL_OK target=/dev/$D; \
         dd if=new.itb of=/dev/$D bs=4096 conv=fsync 2>/dev/null; sync; echo 3 > /proc/sys/vm/drop_caches; \
         [ \"$(head -c {size} /dev/$D | sha256sum | cut -d' ' -f1)\" = {sum} ] && echo FLASH_VERIFIED || echo FLASH_MISMATCH; rm -f new.itb"
    );
    let (out, _) = shell::run(&flash, Duration::from_secs(180))?;
    for l in out.lines().filter(|l| l.contains("DL_") || l.contains("FLASH_") || l.contains("NO_BOOT")) {
        println!("{l}");
    }
    if !out.contains("FLASH_VERIFIED") {
        bail!("deploy FAILED:\n{}", out.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
    }
    if reboot {
        let _ = shell::run("sync; (sleep 1; reboot -f) >/dev/null 2>&1 &", Duration::from_secs(5));
        println!("deployed {} sha256={}; rebooting", fit.display(), &sum[..16]);
    }
    Ok(0)
}
