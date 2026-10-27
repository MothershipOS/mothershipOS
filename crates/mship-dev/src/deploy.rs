//! `deploy-fit [fit] [--no-reboot]`: write a FIT (`cargo xtask fit`: target/boot/nio-el2.itb) to the running phone's
//! boot_b over the dev link (served over HTTP from 10.42.0.1, fetched and written by the phone's shell, read back
//! and compared), then reboot. If the new FIT fails to boot the phone drops to EDL: flash a known-good FIT to boot_b
//! (LUN5 LBA137440) with `mship-dev edl` (motorola_nio/docs/boot.md).

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

pub fn main(fit: &Path, reboot: bool) -> Result<i32> {
    let data = std::fs::read(fit).with_context(|| fit.display().to_string())?;
    let sum: String = sha2::Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    let size = data.len();
    let port = serve(data)?;
    // runs in busybox on the phone (the RAM host has no mship-system)
    let flash = format!(
        "D=$(for d in /sys/class/block/*; do grep -q '^PARTNAME=boot_b$' $d/uevent && basename $d; done); \
         [ -n \"$D\" ] || {{ echo NO_BOOT_B; exit 1; }}; cd /tmp; rm -f new.itb; \
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
