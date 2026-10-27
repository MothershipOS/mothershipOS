//! The device services' control sockets (mship-usb, mship-bt-bridge, mship-wifi-bridge): one datagram request,
//! one reply. The reply goes to the sender's address, so the request is sent from a bound socket: from an unbound
//! one the service cannot answer.

use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const DIR: &str = "/run/mothership";

/// Removes the reply socket when the request is done.
struct Bound(String);

impl Drop for Bound {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Send `msg` to the service at `path` and wait up to `timeout` for its reply.
pub fn request(path: impl AsRef<Path>, msg: &str, timeout: Duration) -> std::io::Result<String> {
    static N: AtomicU64 = AtomicU64::new(0);
    let me = Bound(format!("{DIR}/.ctl-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_file(&me.0);
    let s = UnixDatagram::bind(&me.0)?;
    s.set_read_timeout(Some(timeout))?;
    s.send_to(msg.as_bytes(), path)?;
    let mut buf = vec![0u8; 65536];
    let n = s.recv(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
}
