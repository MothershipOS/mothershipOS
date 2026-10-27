//! The modem slot (mship.modem 1.x): AT commands between a VM's radio stack and the phone's modem.
//!
//! The guest's RIL connects to the host over vsock (cuttlefish-arm64: the modem simulator port, 9600). While the VM
//! holds the cellular handle, its commands go to the modem's WWAN AT port unchanged and the modem's responses and
//! unsolicited results come back, with three exceptions: the packet-data commands. A data call is the host's to
//! make, because its packets must reach the VM's data NIC: `ATD*99***<cid>#` and `AT+CGACT=1,<cid>` start a QMI
//! session on the context (answered `OK`, as Cuttlefish's modem simulator does, never `CONNECT`), `AT+CGACT=0,...`
//! ends the VM's sessions only (never the modem's default bearer), `AT+CGDATA` is refused. In the answers to
//! `AT+CGDCONT?` and `AT+CGCONTRDP` an active context shows its address and gateway.
//!
//! Empty slot: the guest's bytes are dropped and nothing answers.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

use mship_devices::log;

pub const PORT: u32 = 9600;
/// bytes queued towards a guest that doesn't read; beyond that output is dropped
const OUT_MAX: usize = 1 << 20;

/// What a guest command asks for.
#[derive(Debug, PartialEq)]
pub enum Cmd {
    /// to the modem, as is
    Forward(Vec<u8>),
    /// start data on these contexts
    Activate(Vec<u8>),
    /// end the VM's data on these contexts (none: all of them)
    Deactivate(Vec<u8>),
    /// answered by the host: OK (true) or ERROR
    Answer(bool),
}

/// An active context of the holder: shown in the context answers.
pub struct Context {
    /// "a.b.c.d/len"
    pub address: String,
    pub gateway: String,
}

/// Whole command units from the guest's stream: a command line up to CR, or an SMS PDU up to Ctrl-Z / ESC (after
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

fn ints(s: &str) -> Vec<u8> {
    s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
}

/// What one unit asks for; `defined` = the context the guest defined last (`AT+CGDCONT=<cid>,...`).
pub fn classify(unit: &[u8], defined: &mut Option<u8>) -> Cmd {
    let t = String::from_utf8_lossy(unit).trim().to_ascii_uppercase();
    if let Some(rest) = t.strip_prefix("AT+CGDCONT=") {
        if let Some(cid) = ints(rest).first().filter(|_| rest.contains(',')) {
            *defined = Some(*cid);
        }
        return Cmd::Forward(unit.to_vec());
    }
    if let Some(rest) = t.strip_prefix("ATD*99") {
        // ATD*99***<cid># (the simulator's RIL dials 1 for whichever context it just defined)
        let dialed: Option<u8> = rest.trim_end_matches('#').rsplit('*').next().and_then(|c| c.parse().ok());
        return Cmd::Activate(vec![defined.or(dialed).unwrap_or(1)]);
    }
    if let Some(rest) = t.strip_prefix("AT+CGACT=") {
        let mut v = ints(rest);
        let state = if v.is_empty() { 0 } else { v.remove(0) };
        v.retain(|c| *c != 0);
        return match (state, v.is_empty()) {
            (1, true) => Cmd::Answer(true), // "AT+CGACT=1,0": the simulator's RIL resetting, nothing to do
            (1, false) => Cmd::Activate(v),
            _ => Cmd::Deactivate(v),
        };
    }
    if t.starts_with("AT+CGDATA") {
        return Cmd::Answer(false);
    }
    Cmd::Forward(unit.to_vec())
}

/// A modem line as the holder sees it: active contexts show their address and gateway.
pub fn rewrite(line: &str, ctx: &BTreeMap<u8, Context>) -> String {
    let body = line.trim_end_matches(['\r', '\n']);
    let ending = &line[body.len()..];
    let (prefix, at) = if let Some(r) = body.strip_prefix("+CGDCONT: ") {
        ("+CGDCONT: ", r)
    } else if let Some(r) = body.strip_prefix("+CGCONTRDP: ") {
        ("+CGCONTRDP: ", r)
    } else {
        return line.to_string();
    };
    let mut f: Vec<String> = at.split(',').map(String::from).collect();
    let Some(c) = f.first().and_then(|c| c.trim().parse::<u8>().ok()).and_then(|cid| ctx.get(&cid)) else {
        return line.to_string();
    };
    if prefix == "+CGDCONT: " && f.len() > 3 {
        // cid,"type","apn","address",...
        f[3] = format!("\"{}\"", c.address);
    } else if prefix == "+CGCONTRDP: " && f.len() > 4 && f[4].trim_matches('"').is_empty() {
        // cid,bearer,apn,local address,gateway,dns...
        f[4] = c.gateway.clone();
    }
    format!("{prefix}{}{ending}", f.join(","))
}

pub struct Guest {
    stream: vsock::VsockStream,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    pdu: bool,
    defined: Option<u8>,
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
}

impl At {
    pub fn new(path: &str) -> std::io::Result<Self> {
        let listener = vsock::VsockListener::bind_with_cid_port(libc::VMADDR_CID_ANY, PORT)?;
        listener.set_nonblocking(true)?;
        log(format!("modem slot: vsock port {PORT}, AT port {path}"));
        Ok(Self { listener, guests: BTreeMap::new(), path: path.into(), port: None, pbuf: vec![], owner: None })
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

    pub fn accept(&mut self) {
        while let Ok((stream, addr)) = self.listener.accept() {
            let cid = addr.cid();
            let _ = stream.set_nonblocking(true);
            if self.guests.insert(cid, Guest { stream, inbuf: vec![], out: vec![], pdu: false, defined: None }).is_some() {
                log(format!("cid {cid}: radio reconnected"));
            } else {
                log(format!("cid {cid}: radio connected{}", if self.owner == Some(cid) { "" } else { " (slot empty)" }));
            }
        }
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

    pub fn answer(&mut self, cid: u32, ok: bool) {
        if let Some(g) = self.guests.get_mut(&cid) {
            g.queue(if ok { b"\r\nOK\r\n" } else { b"\r\nERROR\r\n" });
        }
    }

    /// A guest's readable stream: its commands (only the holder's count; an empty slot drops them).
    pub fn read_guest(&mut self, cid: u32) -> Vec<Cmd> {
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
            log(format!("cid {cid}: radio disconnected"));
            self.guests.remove(&cid);
            return vec![];
        }
        if self.owner != Some(cid) {
            return vec![];
        }
        g.inbuf.extend_from_slice(&b[..n]);
        let units = split(&mut g.inbuf, &mut g.pdu);
        units.iter().map(|u| classify(u, &mut g.defined)).collect()
    }

    /// The modem's readable port: to the holder, contexts rewritten.
    pub fn read_modem(&mut self, ctx: &BTreeMap<u8, Context>) {
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
            out.extend_from_slice(rewrite(&String::from_utf8_lossy(&line), ctx).as_bytes());
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
        let mut pdu = false;
        let mut b = b"ATE0\rAT+CMGS=23\r0011000B91\x1aAT+CSQ\rAT+C".to_vec();
        let u = split(&mut b, &mut pdu);
        assert_eq!(u, [b"ATE0\r".to_vec(), b"AT+CMGS=23\r".to_vec(), b"0011000B91\x1a".to_vec(), b"AT+CSQ\r".to_vec()]);
        assert_eq!(b, b"AT+C");
        assert!(!pdu);
    }

    #[test]
    fn data_commands() {
        let mut d = None;
        assert!(matches!(classify(b"AT+CGDCONT=3,\"IP\",\"o2internet\",,0,0\r", &mut d), Cmd::Forward(_)));
        assert_eq!(d, Some(3));
        assert_eq!(classify(b"ATD*99***1#\r", &mut d), Cmd::Activate(vec![3])); // the context just defined
        assert_eq!(classify(b"AT+CGACT=1,0\r", &mut d), Cmd::Answer(true));
        assert_eq!(classify(b"AT+CGACT=1,2\r", &mut d), Cmd::Activate(vec![2]));
        assert_eq!(classify(b"AT+CGACT=0,2,3\r", &mut d), Cmd::Deactivate(vec![2, 3]));
        assert_eq!(classify(b"AT+CGACT=0\r", &mut d), Cmd::Deactivate(vec![]));
        assert_eq!(classify(b"AT+CGDATA=\"PPP\",1\r", &mut d), Cmd::Answer(false));
        let mut none = None;
        assert_eq!(classify(b"ATD*99***2#\r", &mut none), Cmd::Activate(vec![2]));
        assert!(matches!(classify(b"AT+CGACT?\r", &mut d), Cmd::Forward(_)));
    }

    #[test]
    fn context_answers() {
        let ctx = BTreeMap::from([(1u8, Context { address: "10.33.27.77/30".into(), gateway: "10.33.27.78".into() })]);
        assert_eq!(rewrite("+CGDCONT: 1,\"IPV4V6\",\"\",\"0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0\",0,0,0,0\r\n", &ctx),
                   "+CGDCONT: 1,\"IPV4V6\",\"\",\"10.33.27.77/30\",0,0,0,0\r\n");
        assert_eq!(rewrite("+CGCONTRDP: 1,6,o2internet,10.33.27.77,,31.3.32.48,\r\n", &ctx),
                   "+CGCONTRDP: 1,6,o2internet,10.33.27.77,10.33.27.78,31.3.32.48,\r\n");
        let other = "+CGDCONT: 2,\"IPV4V6\",\"ims\",\"0.0.0.0\",0,0\r\n";
        assert_eq!(rewrite(other, &ctx), other);
        assert_eq!(rewrite("+CSQ: 21,99\r\n", &ctx), "+CSQ: 21,99\r\n");
    }
}
