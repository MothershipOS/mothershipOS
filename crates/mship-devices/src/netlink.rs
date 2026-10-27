//! Generic netlink (nl80211): attributes, requests, multicast events.

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Mutex;

/// type -> payload (the first of each type)
pub type Attrs = BTreeMap<u16, Vec<u8>>;

const NETLINK_GENERIC: i32 = 16;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLM_F_DUMP: u16 = 0x300;
const NLA_F_NESTED: u16 = 0x8000;
const SOL_NETLINK: i32 = 270;
const NETLINK_ADD_MEMBERSHIP: i32 = 1;
const GENL_ID_CTRL: u16 = 16;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;

/// One attribute: length, type, payload, padded to 4 bytes (little endian, as `mship.wifi` and the phone).
pub fn nla(t: u16, data: &[u8]) -> Vec<u8> {
    let n = 4 + data.len();
    let mut b = Vec::with_capacity(n + 3);
    b.extend_from_slice(&(n as u16).to_le_bytes());
    b.extend_from_slice(&t.to_le_bytes());
    b.extend_from_slice(data);
    b.resize(n.div_ceil(4) * 4, 0);
    b
}

pub fn nla_u8(t: u16, v: u8) -> Vec<u8> {
    nla(t, &[v])
}

pub fn nla_u16(t: u16, v: u16) -> Vec<u8> {
    nla(t, &v.to_le_bytes())
}

pub fn nla_u32(t: u16, v: u32) -> Vec<u8> {
    nla(t, &v.to_le_bytes())
}

pub fn nla_flag(t: u16) -> Vec<u8> {
    nla(t, &[])
}

pub fn nla_nest(t: u16, parts: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    nla(t | NLA_F_NESTED, &parts.into_iter().flatten().collect::<Vec<u8>>())
}

pub fn parse(buf: &[u8]) -> Attrs {
    let mut out = Attrs::new();
    let mut off = 0;
    while off + 4 <= buf.len() {
        let n = u16::from_le_bytes([buf[off], buf[off + 1]]) as usize;
        let t = u16::from_le_bytes([buf[off + 2], buf[off + 3]]) & 0x3fff;
        if n < 4 || off + n > buf.len() {
            break;
        }
        out.entry(t).or_insert_with(|| buf[off + 4..off + n].to_vec());
        off += n.div_ceil(4) * 4;
    }
    out
}

/// a nested array -> its payloads
pub fn list(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut out = vec![];
    let mut off = 0;
    while off + 4 <= buf.len() {
        let n = u16::from_le_bytes([buf[off], buf[off + 1]]) as usize;
        if n < 4 || off + n > buf.len() {
            break;
        }
        out.push(buf[off + 4..off + n].to_vec());
        off += n.div_ceil(4) * 4;
    }
    out
}

pub fn get_u8(a: &Attrs, t: u16) -> Option<u8> {
    a.get(&t).and_then(|v| v.first().copied())
}

pub fn get_u16(a: &Attrs, t: u16) -> Option<u16> {
    a.get(&t).filter(|v| v.len() >= 2).map(|v| u16::from_le_bytes([v[0], v[1]]))
}

pub fn get_u32(a: &Attrs, t: u16) -> Option<u32> {
    a.get(&t).filter(|v| v.len() >= 4).map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
}

pub fn get_i32(a: &Attrs, t: u16) -> Option<i32> {
    get_u32(a, t).map(|v| v as i32)
}

pub fn get_u32s(a: &Attrs, t: u16) -> Vec<u32> {
    a.get(&t).map(|v| v.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()).unwrap_or_default()
}

fn socket() -> std::io::Result<OwnedFd> {
    unsafe {
        let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, NETLINK_GENERIC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut sa: libc::sockaddr_nl = std::mem::zeroed();
        sa.nl_family = libc::AF_NETLINK as u16;
        if libc::bind(fd.as_raw_fd(), (&sa as *const libc::sockaddr_nl).cast(), std::mem::size_of::<libc::sockaddr_nl>() as u32) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn recv(fd: RawFd) -> std::io::Result<Vec<u8>> {
    let mut b = vec![0u8; 1 << 20];
    let n = unsafe { libc::recv(fd, b.as_mut_ptr().cast(), b.len(), 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    b.truncate(n as usize);
    Ok(b)
}

/// netlink messages in a buffer: (type, seq, payload)
fn messages(data: &[u8]) -> Vec<(u16, u32, &[u8])> {
    let mut out = vec![];
    let mut off = 0;
    while off + 16 <= data.len() {
        let ln = u32::from_ne_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        if ln < 16 || off + ln > data.len() {
            break;
        }
        let typ = u16::from_ne_bytes([data[off + 4], data[off + 5]]);
        let seq = u32::from_ne_bytes(data[off + 8..off + 12].try_into().unwrap());
        out.push((typ, seq, &data[off + 16..off + ln]));
        off += ln.div_ceil(4) * 4;
    }
    out
}

/// A generic netlink family (requests are serialized: one socket, one outstanding request).
pub struct Genl {
    fd: Mutex<(OwnedFd, u32)>,
    pub family: u16,
    pub groups: BTreeMap<String, u32>,
}

impl Genl {
    pub fn open(name: &str) -> std::io::Result<Self> {
        let mut g = Self { fd: Mutex::new((socket()?, 0)), family: 0, groups: BTreeMap::new() };
        let mut fname = name.as_bytes().to_vec();
        fname.push(0);
        let fam = g.request(GENL_ID_CTRL, CTRL_CMD_GETFAMILY, &nla(CTRL_ATTR_FAMILY_NAME, &fname), false)?;
        let fam = fam.first().ok_or_else(|| std::io::Error::other(format!("no {name} family")))?;
        g.family = get_u16(fam, CTRL_ATTR_FAMILY_ID).ok_or_else(|| std::io::Error::other("no family id"))?;
        for grp in list(fam.get(&CTRL_ATTR_MCAST_GROUPS).map_or(&[][..], |v| v)) {
            let ga = parse(&grp);
            if let (Some(n), Some(id)) = (ga.get(&CTRL_ATTR_MCAST_GRP_NAME), get_u32(&ga, CTRL_ATTR_MCAST_GRP_ID)) {
                g.groups.insert(String::from_utf8_lossy(n).trim_end_matches('\0').to_string(), id);
            }
        }
        Ok(g)
    }

    pub fn request(&self, family: u16, cmd: u8, attrs: &[u8], dump: bool) -> std::io::Result<Vec<Attrs>> {
        let mut guard = self.fd.lock().unwrap();
        guard.1 += 1;
        let seq = guard.1;
        let flags = NLM_F_REQUEST | NLM_F_ACK | if dump { NLM_F_DUMP } else { 0 };
        let mut m = Vec::with_capacity(20 + attrs.len());
        m.extend_from_slice(&((20 + attrs.len()) as u32).to_ne_bytes());
        m.extend_from_slice(&family.to_ne_bytes());
        m.extend_from_slice(&flags.to_ne_bytes());
        m.extend_from_slice(&seq.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes());
        m.extend_from_slice(&[cmd, 1, 0, 0]);
        m.extend_from_slice(attrs);
        let fd = guard.0.as_raw_fd();
        if unsafe { libc::send(fd, m.as_ptr().cast(), m.len(), 0) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut out = vec![];
        loop {
            let data = recv(fd)?;
            for (typ, s, payload) in messages(&data) {
                if s != seq {
                    continue;
                }
                match typ {
                    NLMSG_ERROR => {
                        let err = i32::from_ne_bytes(payload[..4].try_into().unwrap());
                        if err != 0 {
                            return Err(std::io::Error::from_raw_os_error(-err));
                        }
                        if !dump {
                            return Ok(out);
                        }
                    }
                    NLMSG_DONE => return Ok(out),
                    _ if payload.len() >= 4 => out.push(parse(&payload[4..])),
                    _ => {}
                }
            }
        }
    }

    /// A request to this family.
    pub fn call(&self, cmd: u8, attrs: &[u8], dump: bool) -> std::io::Result<Vec<Attrs>> {
        self.request(self.family, cmd, attrs, dump)
    }

    /// A socket that receives this family's multicast groups.
    pub fn events(&self, groups: &[&str]) -> std::io::Result<OwnedFd> {
        let fd = socket()?;
        for g in groups {
            let id = *self.groups.get(*g).ok_or_else(|| std::io::Error::other(format!("no multicast group {g}")))?;
            let r = unsafe {
                libc::setsockopt(fd.as_raw_fd(), SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, (&id as *const u32).cast(), 4)
            };
            if r != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(fd)
    }
}

/// The next batch of this family's multicast messages: (cmd, attributes). Blocks.
pub fn read_events(fd: RawFd, family: u16) -> std::io::Result<Vec<(u8, Attrs)>> {
    let data = recv(fd)?;
    Ok(messages(&data)
        .into_iter()
        .filter(|(t, _, p)| *t == family && p.len() >= 4)
        .map(|(_, _, p)| (p[0], parse(&p[4..])))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attrs_round_trip() {
        let b = [nla_u32(3, 7), nla(52, b"net"), nla_flag(70), nla_nest(44, [nla_u32(0, 2412), nla_u32(1, 2437)])].concat();
        assert_eq!(b.len() % 4, 0);
        let a = parse(&b);
        assert_eq!(get_u32(&a, 3), Some(7));
        assert_eq!(a[&52], b"net");
        assert_eq!(a[&70], b"");
        let freqs: Vec<u32> = list(&a[&44]).iter().map(|f| u32::from_le_bytes(f[..4].try_into().unwrap())).collect();
        assert_eq!(freqs, [2412, 2437]);
    }
}
