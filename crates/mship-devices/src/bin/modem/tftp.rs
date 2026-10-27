//! TFTP over QRTR: the files a flashless modem keeps on the host.
//!
//! The modem's remote file system: it reads its configuration from `/readonly/firmware/image/` (MCFG MBNs, the
//! firmware directory) and `/readonly/vendor/fsg/` (the carrier configurations of the phone's fsg partition, the
//! `fsg` directory beside them), and reads and writes its own files under `/readwrite/` (dHOB, reports, backups of
//! its RFS). The service is QRTR
//! service 4096, published with the raw instance 3 that the SDX55 looks up (libqrtr's `instance << 8 | version` with
//! version 3). Each transfer gets its own socket, as in tqftpserv; the options are the Qualcomm dialect: `blksize`,
//! `timeoutms`, `tsize`, `wsize` (blocks per acknowledgement), `rsize` (bytes to read), `seek` (start offset) and
//! `append` (writes add to the file). An unanswered or failed write makes the modem give up and reset itself.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::RawFd;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use mship_devices::log;
use mship_devices::qrtr::{PORT_CTRL, Qrtr, ctrl, ctrl_packet};

pub const SERVICE: u32 = 4096;
pub const INSTANCE: u32 = 3;
const READONLY: &str = "/readonly/firmware/image/";
const FSG: &str = "/readonly/vendor/fsg/";
const READWRITE: &str = "/readwrite/";
const RRQ: u16 = 1;
const WRQ: u16 = 2;
const DATA: u16 = 3;
const ACK: u16 = 4;
const ERROR: u16 = 5;
const OACK: u16 = 6;
const ERR_NOT_FOUND: u16 = 1;
const ERR_ACCESS: u16 = 2;
const ERR_ILLEGAL: u16 = 4;
/// a transfer that hears nothing for this long is dropped
const IDLE: Duration = Duration::from_secs(30);

struct Transfer {
    sock: Qrtr,
    peer: (u32, u32),
    name: String,
    file: File,
    write: bool,
    blksize: usize,
    wsize: u32,
    /// read: the first byte and the number of bytes to send
    seek: u64,
    limit: u64,
    /// read: the last block sent; write: the last block written and acknowledged
    block: u32,
    /// write: blocks of the current window
    wbuf: Vec<u8>,
    seen: Instant,
}

impl Transfer {
    fn send(&self, data: &[u8]) {
        if let Err(e) = self.sock.send_to(self.peer.0, self.peer.1, data) {
            log(format!("tftp {}: {e}", self.name));
        }
    }

    /// Read: block `n` (1-based); true when it was the last one.
    fn send_block(&mut self, n: u32) -> bool {
        let start = (n as u64 - 1) * self.blksize as u64;
        let len = self.limit.saturating_sub(start).min(self.blksize as u64) as usize;
        let mut b = vec![0u8; 4 + len];
        b[..2].copy_from_slice(&DATA.to_be_bytes());
        b[2..4].copy_from_slice(&(n as u16).to_be_bytes());
        let ok = self.file.seek(SeekFrom::Start(self.seek + start)).is_ok() && self.file.read_exact(&mut b[4..]).is_ok();
        if !ok {
            b.truncate(4); // the file shrank: end the transfer
        }
        self.send(&b);
        self.block = n;
        b.len() - 4 < self.blksize
    }

    /// Read: the window after block `acked`; true when the transfer is complete.
    fn send_window(&mut self, acked: u32) -> bool {
        if acked > 0 && acked == self.block && self.last_sent_short() {
            return true; // the final block arrived
        }
        for n in acked + 1..=acked + self.wsize {
            if self.send_block(n) {
                break;
            }
        }
        false
    }

    fn last_sent_short(&self) -> bool {
        self.block > 0 && (self.block as u64) * self.blksize as u64 > self.limit
    }
}

fn packet(op: u16, rest: &[u8]) -> Vec<u8> {
    [&op.to_be_bytes()[..], rest].concat()
}

fn error(code: u16, msg: &str) -> Vec<u8> {
    packet(ERROR, &[&code.to_be_bytes()[..], msg.as_bytes(), &[0]].concat())
}

fn ack(block: u32) -> Vec<u8> {
    packet(ACK, &(block as u16).to_be_bytes())
}

/// NUL-separated strings: filename, mode, then option name/value pairs.
fn strings(b: &[u8]) -> Vec<String> {
    let mut v: Vec<String> = b.split(|c| *c == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    if b.last() == Some(&0) {
        v.pop();
    }
    v
}

/// A relative path with no way out of its directory.
fn safe(rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    (!rel.is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)))).then(|| p.to_path_buf())
}

pub struct Tftp {
    sock: Qrtr,
    readonly: PathBuf,
    readwrite: PathBuf,
    transfers: Vec<Transfer>,
}

impl Tftp {
    pub fn new(readonly: &Path, readwrite: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(readwrite)?;
        let sock = Qrtr::open()?;
        sock.publish(SERVICE, INSTANCE)?;
        log(format!("tftp: serving {} (readonly) and {} (readwrite) as QRTR service {SERVICE}:{INSTANCE}",
                    readonly.display(), readwrite.display()));
        Ok(Self { sock, readonly: readonly.into(), readwrite: readwrite.into(), transfers: vec![] })
    }

    pub fn fds(&self) -> Vec<RawFd> {
        std::iter::once(self.sock.fd()).chain(self.transfers.iter().map(|t| t.sock.fd())).collect()
    }

    fn path(&self, name: &str) -> Option<(PathBuf, bool)> {
        if let Some(rel) = name.strip_prefix(READONLY) {
            return safe(rel).map(|r| (self.readonly.join(r), false));
        }
        if let Some(rel) = name.strip_prefix(FSG) {
            return safe(rel).map(|r| (self.readonly.join("fsg").join(r), false));
        }
        name.strip_prefix(READWRITE).and_then(safe).map(|r| (self.readwrite.join(r), true))
    }

    /// Everything readable, on any of our sockets.
    pub fn service(&mut self, ready: &[RawFd]) {
        if ready.contains(&self.sock.fd()) {
            while let Ok(Some((node, port, data))) = self.sock.recv() {
                if port == PORT_CTRL {
                    // the client went away: its transfers end
                    if let Some((cmd, a, b, _, _)) = ctrl_packet(&data) {
                        match cmd {
                            ctrl::BYE => self.transfers.retain(|t| t.peer.0 != a),
                            ctrl::DEL_CLIENT => self.transfers.retain(|t| t.peer != (a, b)),
                            _ => {}
                        }
                    }
                    continue;
                }
                self.request((node, port), &data);
            }
        }
        let mut done = vec![];
        for (i, t) in self.transfers.iter_mut().enumerate() {
            if !ready.contains(&t.sock.fd()) {
                if t.seen.elapsed() > IDLE {
                    log(format!("tftp {}: no answer, dropped", t.name));
                    done.push(i);
                }
                continue;
            }
            while let Ok(Some((node, port, data))) = t.sock.recv() {
                if (node, port) != t.peer || data.len() < 4 {
                    continue;
                }
                t.seen = Instant::now();
                let op = u16::from_be_bytes([data[0], data[1]]);
                let n = u16::from_be_bytes([data[2], data[3]]) as u32;
                let finished = match (op, t.write) {
                    (ERROR, _) => true, // "End of Transfer" (9) is how the modem ends a stat-like read
                    (ACK, false) => {
                        // block numbers are 16 bits on the wire
                        let acked = (t.block & !0xffff) | n;
                        t.send_window(if acked > t.block { acked - 0x10000 } else { acked })
                    }
                    (DATA, true) if n == ((t.block + 1) & 0xffff) => {
                        t.wbuf.extend_from_slice(&data[4..]);
                        t.block += 1;
                        let last = data.len() - 4 < t.blksize;
                        if last || t.block % t.wsize == 0 {
                            if let Err(e) = t.file.write_all(&t.wbuf) {
                                log(format!("tftp {}: {e}", t.name));
                                t.send(&error(ERR_ACCESS, "write failed"));
                                done.push(i);
                                break;
                            }
                            t.wbuf.clear();
                            t.send(&ack(t.block));
                        }
                        last
                    }
                    (DATA, true) => {
                        // out of sequence: the window restarts after the last block written
                        t.block -= t.block % t.wsize;
                        t.wbuf.clear();
                        t.send(&ack(t.block));
                        false
                    }
                    _ => {
                        t.send(&error(ERR_ILLEGAL, "unexpected packet"));
                        true
                    }
                };
                if finished {
                    let _ = t.file.sync_data();
                    done.push(i);
                    break;
                }
            }
        }
        done.dedup();
        for i in done.into_iter().rev() {
            self.transfers.remove(i);
        }
    }

    fn request(&mut self, peer: (u32, u32), data: &[u8]) {
        if data.len() < 4 {
            return;
        }
        let op = u16::from_be_bytes([data[0], data[1]]);
        if op != RRQ && op != WRQ {
            return;
        }
        let s = strings(&data[2..]);
        let (name, mode) = (s.first().cloned().unwrap_or_default(), s.get(1).cloned().unwrap_or_default());
        let opts: BTreeMap<String, String> =
            s[2.min(s.len())..].chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].to_ascii_lowercase(), c[1].clone())).collect();
        let Ok(sock) = Qrtr::open() else { return };
        let reply = |b: &[u8]| {
            let _ = sock.send_to(peer.0, peer.1, b);
        };
        if !mode.eq_ignore_ascii_case("octet") {
            return reply(&error(ERR_ILLEGAL, "octet only"));
        }
        let write = op == WRQ;
        let Some((path, writable)) = self.path(&name) else {
            log(format!("tftp: {} {name}: not a served path", if write { "write" } else { "read" }));
            return reply(&error(ERR_NOT_FOUND, "file not found"));
        };
        if write && !writable {
            return reply(&error(ERR_ACCESS, "read-only"));
        }
        let num = |k: &str, d: u64| opts.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(d);
        let append = num("append", 0) != 0;
        let file = if write {
            if let Some(d) = path.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true);
            if append { o.append(true) } else { o.truncate(true) };
            o.open(&path)
        } else {
            File::open(&path)
        };
        let file = match file {
            Ok(f) => f,
            Err(e) => {
                if write || e.kind() != std::io::ErrorKind::NotFound {
                    log(format!("tftp: {name}: {e}"));
                }
                return reply(&error(ERR_NOT_FOUND, "file not found"));
            }
        };
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let blksize = num("blksize", 512).clamp(8, 65464) as usize;
        let wsize = num("wsize", 1).max(1) as u32;
        let seek = num("seek", 0);
        let rsize = num("rsize", 0);
        let limit = if rsize > 0 { rsize.min(size.saturating_sub(seek)) } else { size.saturating_sub(seek) };
        let mut t = Transfer {
            sock, peer, name: name.clone(), file, write, blksize, wsize, seek, limit, block: 0, wbuf: vec![], seen: Instant::now(),
        };
        if opts.is_empty() {
            if write {
                t.send(&ack(0)); // RFC 1350: a write starts with ACK 0
            } else if t.send_window(0) {
                return;
            }
        } else {
            // the options answered as tqftpserv does: blksize, timeoutms and wsize always, the others when asked
            let mut o = vec![("blksize".to_string(), blksize.to_string()), ("timeoutms".into(), num("timeoutms", 1000).to_string())];
            if opts.contains_key("tsize") {
                o.push(("tsize".into(), if write { num("tsize", 0) } else { size }.to_string()));
            }
            o.push(("wsize".into(), wsize.to_string()));
            if rsize > 0 {
                o.push(("rsize".into(), rsize.to_string()));
            }
            if seek > 0 {
                o.push(("seek".into(), seek.to_string()));
            }
            let body: Vec<u8> = o.iter().flat_map(|(k, v)| [k.as_bytes(), &[0], v.as_bytes(), &[0]].concat()).collect();
            t.send(&packet(OACK, &body));
        }
        self.transfers.push(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_strings() {
        let s = strings(b"/readwrite/dhob.bin\0octet\0blksize\0" as &[u8]);
        assert_eq!(s, ["/readwrite/dhob.bin", "octet", "blksize"]);
        assert!(safe("datablock/id_00").is_some());
        assert!(safe("../etc/passwd").is_none());
        assert!(safe("a/../../b").is_none());
        assert!(safe("").is_none());
    }

    #[test]
    fn error_packet() {
        assert_eq!(error(1, "x"), [0, 5, 0, 1, b'x', 0]);
        assert_eq!(ack(0x1_0002), [0, 4, 0, 2]);
    }
}
