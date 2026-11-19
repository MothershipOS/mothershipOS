//! The modem slot's transport: the VMs' radio stacks over vsock (cuttlefish-arm64: the modem simulator port, 9600)
//! and the modem's WWAN AT port. What a command means is `radio`'s business (for the VM holding the handle, and for
//! the others, which see an empty slot): this moves whole command units in and whole lines out.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

use mship_devices::log;

pub const PORT: u32 = 9600;
/// bytes queued towards a guest that doesn't read; beyond that output is dropped
const OUT_MAX: usize = 1 << 20;

/// Whole command units from a guest's stream: a command line up to CR, or an SMS PDU up to Ctrl-Z / ESC (after
/// `+CMGS=` / `+CMGW=`). The rest stays in `buf`.
pub fn split(buf: &mut Vec<u8>, pdu: &mut bool) -> Vec<Vec<u8>> {
    let mut out = vec![];
    loop {
        let end = if *pdu { buf.iter().position(|c| *c == 0x1a || *c == 0x1b) } else { buf.iter().position(|c| *c == b'\r') };
        let Some(end) = end else { break };
        let unit: Vec<u8> = buf.drain(..=end).collect();
        if *pdu {
            *pdu = false;
        } else {
            let t = String::from_utf8_lossy(&unit).trim().to_ascii_uppercase();
            *pdu = t.starts_with("AT+CMGS=") || t.starts_with("AT+CMGW=");
        }
        out.push(unit);
    }
    out
}

pub struct Guest {
    stream: vsock::VsockStream,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    pdu: bool,
}

impl Guest {
    fn queue(&mut self, b: &[u8]) {
        if self.out.len() + b.len() <= OUT_MAX {
            self.out.extend_from_slice(b);
        }
        self.flush();
    }

    /// false: the guest is gone
    fn flush(&mut self) -> bool {
        while !self.out.is_empty() {
            match self.stream.write(&self.out) {
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }
        true
    }
}

pub struct At {
    listener: vsock::VsockListener,
    pub guests: BTreeMap<u32, Guest>,
    path: String,
    port: Option<std::fs::File>,
    pbuf: Vec<u8>,
    /// the vsock CID of the VM holding the handle
    pub owner: Option<u32>,
    /// guests already logged as connected to the empty slot (logged once)
    idle: std::collections::BTreeSet<u32>,
}

impl At {
    pub fn new(path: &str) -> std::io::Result<Self> {
        let listener = vsock::VsockListener::bind_with_cid_port(libc::VMADDR_CID_ANY, PORT)?;
        listener.set_nonblocking(true)?;
        log(format!("modem slot: vsock port {PORT}, AT port {path}"));
        Ok(Self { listener, guests: BTreeMap::new(), path: path.into(), port: None, pbuf: vec![], owner: None, idle: Default::default() })
    }

    pub fn listener_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    pub fn port_fd(&self) -> Option<RawFd> {
        self.port.as_ref().map(|p| p.as_raw_fd())
    }

    pub fn backlog(&self) -> bool {
        self.guests.values().any(|g| !g.out.is_empty())
    }

    pub fn guest_fds(&self) -> Vec<(RawFd, u32)> {
        self.guests.iter().map(|(c, g)| (g.stream.as_raw_fd(), *c)).collect()
    }

    /// New connections; true if the holder (re)connected (its RIL starts over).
    pub fn accept(&mut self) -> bool {
        let mut holder = false;
        while let Ok((stream, addr)) = self.listener.accept() {
            let cid = addr.cid();
            let _ = stream.set_nonblocking(true);
            let again = self.guests.insert(cid, Guest { stream, inbuf: vec![], out: vec![], pdu: false }).is_some();
            if self.owner == Some(cid) {
                self.idle.remove(&cid);
                holder = true;
                log(format!("cid {cid}: radio {}", if again { "reconnected" } else { "connected" }));
            } else if self.idle.insert(cid) {
                log(format!("cid {cid}: radio connected (slot empty)"));
            }
        }
        holder
    }

    /// Close a guest's connection: its RIL reconnects and starts over (fresh on attach, after release).
    pub fn kick(&mut self, cid: u32) {
        if let Some(g) = self.guests.remove(&cid) {
            let _ = g.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    pub fn open_port(&mut self) -> bool {
        if self.port.is_some() {
            return true;
        }
        let f = std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(&self.path);
        match f {
            Ok(f) => {
                self.port = Some(f);
                self.pbuf.clear();
                true
            }
            Err(e) => {
                log(format!("{}: {e}", self.path));
                false
            }
        }
    }

    pub fn close_port(&mut self) {
        self.port = None;
        self.pbuf.clear();
    }

    /// To the modem (raw), if the port is open.
    pub fn send_modem(&mut self, b: &[u8]) -> bool {
        match self.port.as_mut().map(|p| p.write_all(b)) {
            Some(Ok(())) => true,
            Some(Err(e)) => {
                log(format!("{}: {e}", self.path));
                self.close_port();
                false
            }
            None => false,
        }
    }

    /// Lines to the holder (each framed CR LF, as a modem sends them); false: no holder connected.
    pub fn tell_holder(&mut self, lines: &[String]) -> bool {
        match self.owner {
            Some(c) => self.tell(c, lines),
            None => false,
        }
    }

    /// Lines to a guest; false: not connected.
    pub fn tell(&mut self, cid: u32, lines: &[String]) -> bool {
        match self.guests.get_mut(&cid) {
            Some(g) => {
                for l in lines {
                    g.queue(format!("\r\n{l}\r\n").as_bytes());
                }
                true
            }
            None => false,
        }
    }

    /// A guest's readable stream: its command units.
    pub fn read_guest(&mut self, cid: u32) -> Vec<Vec<u8>> {
        let Some(g) = self.guests.get_mut(&cid) else { return vec![] };
        if !g.flush() {
            self.guests.remove(&cid);
            return vec![];
        }
        let mut b = vec![0u8; 16384];
        let n = match g.stream.read(&mut b) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return vec![],
            Err(_) => 0,
        };
        if n == 0 {
            if self.owner == Some(cid) {
                log(format!("cid {cid}: radio disconnected"));
            }
            self.guests.remove(&cid);
            return vec![];
        }
        g.inbuf.extend_from_slice(&b[..n]);
        split(&mut g.inbuf, &mut g.pdu)
    }

    /// The modem's readable port: its lines to the holder, those `keep` passes.
    pub fn read_modem(&mut self, keep: impl Fn(&str) -> bool) {
        let Some(p) = self.port.as_mut() else { return };
        let mut b = vec![0u8; 16384];
        match p.read(&mut b) {
            Ok(0) => return self.close_port(),
            Ok(n) => self.pbuf.extend_from_slice(&b[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(e) => {
                log(format!("{}: {e}", self.path));
                return self.close_port();
            }
        }
        let mut out = vec![];
        while let Some(i) = self.pbuf.iter().position(|c| *c == b'\n') {
            let line: Vec<u8> = self.pbuf.drain(..=i).collect();
            if keep(&String::from_utf8_lossy(&line)) {
                out.extend_from_slice(&line);
            }
        }
        // the SMS prompt ("> ") has no line end
        if self.pbuf.trim_ascii_start().starts_with(b">") {
            out.append(&mut self.pbuf);
        }
        if let Some(g) = self.owner.and_then(|c| self.guests.get_mut(&c)) {
            g.queue(&out);
        }
    }

    pub fn flush(&mut self) {
        let gone: Vec<u32> = self.guests.iter_mut().filter_map(|(c, g)| (!g.flush()).then_some(*c)).collect();
        for c in gone {
            self.guests.remove(&c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        let mut b = b"AT+CSQ\rAT+CMGS=23\r0011000B91\x1aAT+CL".to_vec();
        let mut pdu = false;
        let u = split(&mut b, &mut pdu);
        assert_eq!(u, vec![b"AT+CSQ\r".to_vec(), b"AT+CMGS=23\r".to_vec(), b"0011000B91\x1a".to_vec()]);
        assert_eq!(b, b"AT+CL");
        assert!(!pdu);
    }
}
