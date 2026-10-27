//! QRTR (AF_QIPCRTR): the IPC router to a Qualcomm modem's services, and QMI messages over it.
//!
//! A QRTR address is (node, port). The name service on the local node publishes services and answers lookups with
//! control packets from port `PORT_CTRL`. A QMI message over QRTR has no QMUX header: type, transaction, message id,
//! length, then TLVs, all little endian. A QMI client is a socket: the service forgets the client (and whatever it
//! started, e.g. a data call) when the socket closes.

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

pub const AF_QIPCRTR: i32 = 42;
pub const PORT_CTRL: u32 = 0xffff_fffe;

/// <linux/qrtr.h> control packet types
pub mod ctrl {
    pub const BYE: u32 = 3;
    pub const NEW_SERVER: u32 = 4;
    pub const DEL_SERVER: u32 = 5;
    pub const DEL_CLIENT: u32 = 6;
    pub const NEW_LOOKUP: u32 = 10;
}

/// QMI message types
pub const QMI_REQUEST: u8 = 0;
pub const QMI_RESPONSE: u8 = 2;
pub const QMI_INDICATION: u8 = 4;

#[repr(C)]
#[derive(Default)]
struct SockaddrQrtr {
    family: u16,
    node: u32,
    port: u32,
}

/// One QRTR datagram socket.
pub struct Qrtr {
    fd: OwnedFd,
    pub node: u32,
    pub port: u32,
}

impl Qrtr {
    pub fn open() -> std::io::Result<Self> {
        let fd = unsafe { libc::socket(AF_QIPCRTR, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut sa = SockaddrQrtr::default();
        let mut len = std::mem::size_of::<SockaddrQrtr>() as libc::socklen_t;
        if unsafe { libc::getsockname(fd.as_raw_fd(), (&mut sa as *mut SockaddrQrtr).cast(), &mut len) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { fd, node: sa.node, port: sa.port })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn send_to(&self, node: u32, port: u32, data: &[u8]) -> std::io::Result<()> {
        let sa = SockaddrQrtr { family: AF_QIPCRTR as u16, node, port };
        let n = unsafe {
            libc::sendto(self.fd(), data.as_ptr().cast(), data.len(), 0, (&sa as *const SockaddrQrtr).cast(),
                         std::mem::size_of::<SockaddrQrtr>() as libc::socklen_t)
        };
        if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }

    /// The next datagram, if one is waiting: (node, port, data).
    pub fn recv(&self) -> std::io::Result<Option<(u32, u32, Vec<u8>)>> {
        let mut b = vec![0u8; 65536];
        let mut sa = SockaddrQrtr::default();
        let mut len = std::mem::size_of::<SockaddrQrtr>() as libc::socklen_t;
        let n = unsafe {
            libc::recvfrom(self.fd(), b.as_mut_ptr().cast(), b.len(), 0, (&mut sa as *mut SockaddrQrtr).cast(), &mut len)
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::WouldBlock { Ok(None) } else { Err(e) };
        }
        b.truncate(n as usize);
        Ok(Some((sa.node, sa.port, b)))
    }

    /// A control packet to the local name service.
    fn ns(&self, cmd: u32, service: u32, instance: u32) -> std::io::Result<()> {
        let mut p = Vec::with_capacity(20);
        for v in [cmd, service, instance, 0, 0] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        self.send_to(self.node, PORT_CTRL, &p)
    }

    /// Offer a service on this socket. `instance` is the raw field the peers look up (libqrtr's `instance << 8 |
    /// version`).
    pub fn publish(&self, service: u32, instance: u32) -> std::io::Result<()> {
        self.ns(ctrl::NEW_SERVER, service, instance)
    }

    /// Every server of `service` (0: all services), now and as they come and go: NEW_SERVER / DEL_SERVER control
    /// packets on this socket; an all-zero NEW_SERVER ends the initial list.
    pub fn lookup(&self, service: u32) -> std::io::Result<()> {
        self.ns(ctrl::NEW_LOOKUP, service, 0)
    }
}

/// A control packet: (cmd, service, instance, node, port); for BYE / DEL_CLIENT service and instance hold the
/// node and port instead (the kernel's union).
pub fn ctrl_packet(data: &[u8]) -> Option<(u32, u32, u32, u32, u32)> {
    let w = |i: usize| data.get(i * 4..i * 4 + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    Some((w(0)?, w(1).unwrap_or(0), w(2).unwrap_or(0), w(3).unwrap_or(0), w(4).unwrap_or(0)))
}

// ---------------------------------------------------------------- QMI

pub type Tlvs = BTreeMap<u8, Vec<u8>>;

pub fn tlv(t: u8, v: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(3 + v.len());
    b.push(t);
    b.extend_from_slice(&(v.len() as u16).to_le_bytes());
    b.extend_from_slice(v);
    b
}

pub fn tlv_u8(t: u8, v: u8) -> Vec<u8> {
    tlv(t, &[v])
}

pub fn tlv_u32(t: u8, v: u32) -> Vec<u8> {
    tlv(t, &v.to_le_bytes())
}

pub fn qmi_encode(kind: u8, txn: u16, msg: u16, tlvs: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = tlvs.concat();
    let mut b = Vec::with_capacity(7 + body.len());
    b.push(kind);
    b.extend_from_slice(&txn.to_le_bytes());
    b.extend_from_slice(&msg.to_le_bytes());
    b.extend_from_slice(&(body.len() as u16).to_le_bytes());
    b.extend_from_slice(&body);
    b
}

#[derive(Debug, Clone)]
pub struct QmiMsg {
    pub kind: u8,
    pub txn: u16,
    pub msg: u16,
    pub tlvs: Tlvs,
}

impl QmiMsg {
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < 7 {
            return None;
        }
        let len = u16::from_le_bytes([b[5], b[6]]) as usize;
        let body = b.get(7..7 + len)?;
        let mut tlvs = Tlvs::new();
        let mut off = 0;
        while off + 3 <= body.len() {
            let n = u16::from_le_bytes([body[off + 1], body[off + 2]]) as usize;
            let v = body.get(off + 3..off + 3 + n)?;
            tlvs.insert(body[off], v.to_vec());
            off += 3 + n;
        }
        Some(Self { kind: b[0], txn: u16::from_le_bytes([b[1], b[2]]), msg: u16::from_le_bytes([b[3], b[4]]), tlvs })
    }

    /// The result TLV (0x02): Err(QMI error code) on failure.
    pub fn result(&self) -> Result<(), u16> {
        match self.tlvs.get(&2) {
            Some(v) if v.len() >= 4 && u16::from_le_bytes([v[0], v[1]]) == 0 => Ok(()),
            Some(v) if v.len() >= 4 => Err(u16::from_le_bytes([v[2], v[3]])),
            _ => Err(0xffff),
        }
    }

    pub fn u32(&self, t: u8) -> Option<u32> {
        self.tlvs.get(&t).filter(|v| v.len() >= 4).map(|v| u32::from_le_bytes(v[..4].try_into().unwrap()))
    }
}

/// A client of one QMI service (its own socket).
pub struct QmiClient {
    sock: Qrtr,
    pub service: u32,
    node: u32,
    port: u32,
    txn: u16,
    /// indications that arrived while waiting for a response
    pending: Vec<QmiMsg>,
}

impl QmiClient {
    pub fn new(service: u32, node: u32, port: u32) -> std::io::Result<Self> {
        Ok(Self { sock: Qrtr::open()?, service, node, port, txn: 0, pending: vec![] })
    }

    pub fn fd(&self) -> RawFd {
        self.sock.fd()
    }

    /// A request and its response. Err: the transport failed, no answer in `timeout`, or the QMI error.
    pub fn request(&mut self, msg: u16, tlvs: &[Vec<u8>], timeout: Duration) -> Result<QmiMsg, String> {
        self.txn = self.txn.wrapping_add(1).max(1);
        let txn = self.txn;
        self.sock.send_to(self.node, self.port, &qmi_encode(QMI_REQUEST, txn, msg, tlvs))
            .map_err(|e| format!("qmi {:#x}/{msg:#06x}: {e}", self.service))?;
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("qmi {:#x}/{msg:#06x}: no response", self.service));
            }
            if crate::poll(&[self.fd()], Some(left)).is_empty() {
                continue;
            }
            while let Some((node, port, data)) = self.sock.recv().map_err(|e| format!("qmi {:#x}: {e}", self.service))? {
                if (node, port) != (self.node, self.port) {
                    continue;
                }
                let Some(m) = QmiMsg::decode(&data) else { continue };
                if m.kind == QMI_RESPONSE && m.txn == txn && m.msg == msg {
                    return m.result().map(|_| m.clone()).map_err(|e| format!("qmi {:#x}/{msg:#06x}: error {e:#x}", self.service));
                }
                if m.kind == QMI_INDICATION {
                    self.pending.push(m);
                }
            }
        }
    }

    /// Indications received so far (without blocking). Err: the service is gone.
    pub fn indications(&mut self) -> std::io::Result<Vec<QmiMsg>> {
        let mut out = std::mem::take(&mut self.pending);
        while let Some((node, port, data)) = self.sock.recv()? {
            if (node, port) == (self.node, self.port)
                && let Some(m) = QmiMsg::decode(&data).filter(|m| m.kind == QMI_INDICATION)
            {
                out.push(m);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qmi_round_trip() {
        let b = qmi_encode(QMI_RESPONSE, 7, 0x2d, &[tlv(2, &[0, 0, 0, 0]), tlv_u32(0x1e, 0x0a211b4d), tlv_u8(0x11, 1)]);
        let m = QmiMsg::decode(&b).unwrap();
        assert_eq!((m.kind, m.txn, m.msg), (QMI_RESPONSE, 7, 0x2d));
        assert!(m.result().is_ok());
        assert_eq!(m.u32(0x1e), Some(0x0a211b4d));
        assert_eq!(m.tlvs[&0x11], [1]);
        let err = QmiMsg::decode(&qmi_encode(QMI_RESPONSE, 1, 0x20, &[tlv(2, &[1, 0, 0x1a, 0])])).unwrap();
        assert_eq!(err.result(), Err(0x1a));
        assert!(QmiMsg::decode(&b[..b.len() - 1]).is_none()); // truncated
    }

    #[test]
    fn control_packets() {
        let p: Vec<u8> = [ctrl::NEW_SERVER, 4096, 3, 1, 16384].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(ctrl_packet(&p), Some((ctrl::NEW_SERVER, 4096, 3, 1, 16384)));
    }
}
