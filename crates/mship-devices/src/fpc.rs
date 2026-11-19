//! Transport for the nio's FPC fingerprint sensor: a thin wrapper over the
//! kernel's `/dev/fpc_tee` (motorola_nio `fpc1541_tee`). It powers and resets
//! the sensor, loads the (vendor-signed) Trusted Application, relays opaque
//! command buffers to it, and waits on the sensor interrupt. It interprets none
//! of the command bytes — the FPC command protocol lives above this, in the
//! fingerprint service (see motorola_nio/docs/fingerprint.md).
//!
//! The ioctl numbers mirror `fpc1541_tee.h` exactly.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

const FPC_DEV: &str = "/dev/fpc_tee";

// _IOC(dir, type, nr, size) as the kernel encodes it on arm64/asm-generic.
const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr as u32) as libc::c_ulong
}
const NONE: u32 = 0;
const WRITE: u32 = 1;
const RW: u32 = 3;
const M: u8 = b'f';

const IOC_POWER: libc::c_ulong = ioc(WRITE, M, 1, 4);
const IOC_RESET: libc::c_ulong = ioc(WRITE, M, 2, 4);
const IOC_LOAD: libc::c_ulong = ioc(NONE, M, 3, 0);
const IOC_UNLOAD: libc::c_ulong = ioc(NONE, M, 4, 0);
const IOC_SEND: libc::c_ulong = ioc(RW, M, 5, std::mem::size_of::<Cmd>());
const IOC_WAIT_IRQ: libc::c_ulong = ioc(WRITE, M, 6, 4);

/// Mirrors `struct fpc_tee_cmd`; `repr(C)` gives the same 32-byte layout.
#[repr(C)]
struct Cmd {
    req: u64,
    req_len: u32,
    rsp: u64,
    rsp_len: u32,
}

pub struct Fpc {
    dev: File,
}

fn errno<T>(ret: libc::c_int, ok: T) -> io::Result<T> {
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ok) }
}

impl Fpc {
    pub fn open() -> io::Result<Self> {
        Ok(Self { dev: OpenOptions::new().read(true).write(true).open(FPC_DEV)? })
    }

    fn ioc_int(&self, req: libc::c_ulong, arg: libc::c_int) -> io::Result<libc::c_int> {
        let r = unsafe { libc::ioctl(self.dev.as_raw_fd(), req, &arg as *const _) };
        errno(r, r)
    }

    /// Power the sensor rail on or off.
    pub fn power(&self, on: bool) -> io::Result<()> {
        self.ioc_int(IOC_POWER, on as _).map(drop)
    }

    /// Assert (in reset) or release the reset line.
    pub fn reset(&self, assert: bool) -> io::Result<()> {
        self.ioc_int(IOC_RESET, assert as _).map(drop)
    }

    /// Load cmnlib + the TA; returns the loaded app id.
    pub fn load(&self) -> io::Result<i32> {
        let r = unsafe { libc::ioctl(self.dev.as_raw_fd(), IOC_LOAD) };
        errno(r, r)
    }

    pub fn unload(&self) -> io::Result<()> {
        let r = unsafe { libc::ioctl(self.dev.as_raw_fd(), IOC_UNLOAD) };
        errno(r, ()).map(drop)
    }

    /// Send one request to the TA and read its response into `rsp` (its length
    /// is the cap the TA may write).
    pub fn send(&self, req: &[u8], rsp: &mut [u8]) -> io::Result<()> {
        let c = Cmd {
            req: req.as_ptr() as u64,
            req_len: req.len() as u32,
            rsp: rsp.as_mut_ptr() as u64,
            rsp_len: rsp.len() as u32,
        };
        let r = unsafe { libc::ioctl(self.dev.as_raw_fd(), IOC_SEND, &c as *const _) };
        errno(r, ()).map(drop)
    }

    /// Wait up to `timeout_ms` for a sensor interrupt. `Ok(false)` on timeout.
    pub fn wait_irq(&self, timeout_ms: i32) -> io::Result<bool> {
        match self.ioc_int(IOC_WAIT_IRQ, timeout_ms) {
            Ok(_) => Ok(true),
            Err(e) if e.raw_os_error() == Some(libc::ETIMEDOUT) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The ioctl encodings must match fpc1541_tee.h, which asm-generic builds the
    // same way: dir<<30 | size<<16 | type<<8 | nr.
    #[test]
    fn ioctl_numbers_match_the_header() {
        assert_eq!(IOC_POWER, (1 << 30) | (4 << 16) | ((b'f' as u64) << 8) | 1);
        assert_eq!(IOC_LOAD, ((b'f' as u64) << 8) | 3);
        assert_eq!(IOC_SEND, (3 << 30) | (32 << 16) | ((b'f' as u64) << 8) | 5);
        assert_eq!(std::mem::size_of::<Cmd>(), 32);
    }
}
