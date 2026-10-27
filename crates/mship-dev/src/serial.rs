//! A serial port, raw, with a read timeout (termios).

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

use anyhow::{Context, Result};

pub struct Serial(pub File);

impl Serial {
    pub fn open(path: &str, baud: libc::speed_t, timeout: Duration) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let f = std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open(path).with_context(|| path.to_string())?;
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        unsafe {
            libc::tcgetattr(f.as_raw_fd(), &mut t);
            libc::cfmakeraw(&mut t);
            libc::cfsetspeed(&mut t, baud);
            t.c_cflag |= libc::CLOCAL | libc::CREAD;
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = (timeout.as_millis() / 100).clamp(1, 255) as u8;
            if libc::tcsetattr(f.as_raw_fd(), libc::TCSANOW, &t) != 0 {
                anyhow::bail!("{path}: {}", std::io::Error::last_os_error());
            }
        }
        Ok(Serial(f))
    }

    /// Whatever arrives within the timeout (empty if nothing).
    pub fn read_some(&mut self) -> Result<Vec<u8>> {
        let mut b = vec![0u8; 65536];
        let n = self.0.read(&mut b)?;
        b.truncate(n);
        Ok(b)
    }

    pub fn write_all(&mut self, d: &[u8]) -> Result<()> {
        self.0.write_all(d)?;
        Ok(())
    }

    pub fn flush_input(&mut self) {
        unsafe { libc::tcflush(self.0.as_raw_fd(), libc::TCIFLUSH) };
    }
}
