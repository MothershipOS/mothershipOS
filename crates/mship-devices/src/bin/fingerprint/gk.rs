//! The host's Gatekeeper identity for enrolment authorization, and the Keymaster app's files.
//!
//! The FPC app commits a template only after an enrolment authorized by a HardwareAuthToken with its own challenge,
//! signed with the auth-token key it shares with the phone's Keymaster app. A guest's Gatekeeper is a different,
//! software one: its token cannot be checked here and the FPC app would refuse it. So each VM has its own credential
//! in the phone's real Gatekeeper (a random secret only this service knows, never the user's screen lock): when the
//! VM holding the handle asks to enrol with its own (guest-verified) token for our challenge, this service verifies
//! the VM's credential against the same challenge and hands the genuine token to the FPC app. Nothing is forged; the
//! trust decision is that the VM holding the handle vouches for its user, as with every exclusive handle.
//!
//! Gatekeeper messages are the stock HAL's: `u32 command` + a CBOR map (enroll 0x21001 {3 uid, 8 password},
//! verify 0x21002 {3 uid, 4 challenge, 6 handle, 9 password}); the response is `u32 status, u32 len, CBOR map`.

use std::path::{Path, PathBuf};

use super::tee::{Res, Tee};

const GK_ENROLL: u32 = 0x21001;
const GK_VERIFY: u32 = 0x21002;
const KM_GET_AUTH_TOKEN_KEY: u32 = 0x205;
/// the Keymaster app's files over reboots (the driver holds them while the system runs)
const STORE: &str = "tz-store.bin";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Write `data` to `path` (mode 0600) through a temporary file and rename.
pub fn write_private(path: &Path, data: &[u8]) -> Res<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let tmp = path.with_extension("new");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(data).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

// ------------------------------------------------------------------ CBOR (the subset Gatekeeper uses)

fn cbor_head(out: &mut Vec<u8>, major: u8, v: u64) {
    let m = major << 5;
    match v {
        0..=23 => out.push(m | v as u8),
        24..=0xff => out.extend([m | 24, v as u8]),
        0x100..=0xffff => {
            out.push(m | 25);
            out.extend((v as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(m | 26);
            out.extend((v as u32).to_be_bytes());
        }
        _ => {
            out.push(m | 27);
            out.extend(v.to_be_bytes());
        }
    }
}

enum Val {
    U(u64),
    B(Vec<u8>),
}

fn request(cmd: u32, fields: &[(u64, Val)]) -> Vec<u8> {
    let mut out = cmd.to_le_bytes().to_vec();
    cbor_head(&mut out, 5, fields.len() as u64);
    for (k, v) in fields {
        cbor_head(&mut out, 0, *k);
        match v {
            Val::U(n) => cbor_head(&mut out, 0, *n),
            Val::B(b) => {
                cbor_head(&mut out, 2, b.len() as u64);
                out.extend_from_slice(b);
            }
        }
    }
    out
}

fn read_head(p: &mut &[u8]) -> Option<(u8, u64)> {
    let (&t, rest) = p.split_first()?;
    *p = rest;
    let n = match t & 31 {
        v @ 0..=23 => return Some((t >> 5, v as u64)),
        v @ 24..=27 => 1usize << (v - 24),
        _ => return None,
    };
    if p.len() < n {
        return None;
    }
    let v = p[..n].iter().fold(0u64, |a, b| a << 8 | *b as u64);
    *p = &p[n..];
    Some((t >> 5, v))
}

/// The response's map (unsigned and byte string values) by key.
fn response(rsp: &[u8]) -> Option<std::collections::BTreeMap<u64, Val>> {
    let len = u32::from_le_bytes(rsp.get(4..8)?.try_into().ok()?) as usize;
    let mut p = rsp.get(8..8 + len)?;
    let (major, n) = read_head(&mut p)?;
    if major != 5 || n > 32 {
        return None;
    }
    let mut m = std::collections::BTreeMap::new();
    for _ in 0..n {
        let (0, key) = read_head(&mut p)? else { return None };
        match read_head(&mut p)? {
            (0, v) => {
                m.insert(key, Val::U(v));
            }
            (2, l) if (l as usize) <= p.len() => {
                m.insert(key, Val::B(p[..l as usize].to_vec()));
                p = &p[l as usize..];
            }
            _ => return None,
        }
    }
    Some(m)
}

// ------------------------------------------------------------------ the VM's credential

/// One VM's Gatekeeper credential (`<state>/<vm>/gatekeeper`, root only): uid, secret, password handle.
pub struct Credential {
    path: PathBuf,
    uid: u32,
    secret: Vec<u8>,
    handle: Vec<u8>,
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.secret.fill(0);
    }
}

impl Credential {
    /// The VM's credential, enrolled in Gatekeeper the first time. `uid`: used when it is new.
    pub fn load(tee: &Tee, dir: &Path, uid: u32) -> Res<Self> {
        let path = dir.join("gatekeeper");
        if let Ok(s) = std::fs::read_to_string(&path) {
            let v: serde_json::Value = serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?;
            let get = |k: &str| v[k].as_str().and_then(unhex);
            if let (Some(uid), Some(secret), Some(handle)) = (v["uid"].as_u64(), get("secret"), get("handle")) {
                return Ok(Self { path, uid: uid as u32, secret, handle });
            }
        }
        let mut c = Self { path, uid, secret: random(32)?, handle: vec![] };
        c.enroll(tee)?;
        Ok(c)
    }

    fn enroll(&mut self, tee: &Tee) -> Res<()> {
        let req = request(GK_ENROLL, &[(3, Val::U(self.uid as u64)), (8, Val::B(self.secret.clone()))]);
        let (s, rsp) = tee.km(&req, 4096)?;
        let handle = match response(&rsp).and_then(|mut m| m.remove(&6)) {
            Some(Val::B(h)) if s == 0 && !h.is_empty() && h.len() <= 64 => h,
            _ => return Err(format!("gatekeeper enroll: status {s}")),
        };
        self.handle = handle;
        let v = serde_json::json!({"uid": self.uid, "secret": hex(&self.secret), "handle": hex(&self.handle)});
        write_private(&self.path, v.to_string().as_bytes())
    }

    /// A genuine HardwareAuthToken for `challenge`, serialized as the FPC app takes it (69 bytes: version,
    /// challenge, user id, authenticator id, authenticator type, timestamp, MAC).
    pub fn token(&mut self, tee: &Tee, challenge: u64) -> Res<Vec<u8>> {
        match self.verify(tee, challenge) {
            Ok(t) => Ok(t),
            Err(e) => {
                // a handle the Gatekeeper no longer accepts (its files lost): a fresh credential
                mship_devices::log(format!("gatekeeper: {e}; enrolling the VM's credential again"));
                self.secret = random(32)?;
                self.enroll(tee)?;
                self.verify(tee, challenge)
            }
        }
    }

    fn verify(&self, tee: &Tee, challenge: u64) -> Res<Vec<u8>> {
        let req = request(GK_VERIFY, &[(3, Val::U(self.uid as u64)), (4, Val::U(challenge)), (6, Val::B(self.handle.clone())),
                                       (9, Val::B(self.secret.clone()))]);
        let (s, rsp) = tee.km(&req, 4096)?;
        let m = response(&rsp).filter(|_| s == 0).ok_or(format!("gatekeeper verify: status {s}"))?;
        let u = |k| match m.get(&k) {
            Some(Val::U(v)) => Some(*v),
            _ => None,
        };
        let (Some(0), Some(ch), Some(uid), Some(auth_id), Some(auth_type), Some(ts), Some(Val::B(mac))) =
            (u(11), u(4), u(12), u(13), u(14), u(15), m.get(&16))
        else {
            return Err("gatekeeper verify: incomplete token".into());
        };
        if ch != challenge || mac.len() != 32 || auth_type > u32::MAX as u64 {
            return Err("gatekeeper verify: token for another challenge".into());
        }
        let mut t = vec![0u8];
        t.extend(ch.to_le_bytes());
        t.extend(uid.to_le_bytes());
        t.extend(auth_id.to_le_bytes());
        t.extend((auth_type as u32).to_le_bytes());
        t.extend(ts.to_le_bytes());
        t.extend_from_slice(mac);
        Ok(t)
    }
}

/// Keymaster's auth-token key, relayed to the FPC app as the stock HAL does (once per boot of the apps).
pub fn share_token_key(tee: &Tee) -> Res<()> {
    let mut req = vec![0u8; 64];
    req[..4].copy_from_slice(&KM_GET_AUTH_TOKEN_KEY.to_le_bytes());
    req[4..8].copy_from_slice(&2u32.to_le_bytes());
    let (s, mut rsp) = tee.km(&req, 960)?;
    let off = u32::from_le_bytes(rsp[4..8].try_into().unwrap()) as usize;
    let len = u32::from_le_bytes(rsp[8..12].try_into().unwrap()) as usize;
    if s != 0 || !(12..=960).contains(&off) || len == 0 || len > 960 - off {
        rsp.fill(0);
        return Err(format!("keymaster token key: status {s}"));
    }
    let r = tee.auth_blob(5, &rsp[off..off + len]);
    rsp.fill(0);
    match r? {
        0 => Ok(()),
        s => Err(format!("FPC token key: {s}")),
    }
}

fn random(n: usize) -> Res<Vec<u8>> {
    let mut b = vec![0u8; n];
    if unsafe { libc::getrandom(b.as_mut_ptr().cast(), n, 0) } != n as isize {
        return Err("getrandom failed".into());
    }
    Ok(b)
}

// ------------------------------------------------------------------ the Keymaster app's files

/// Restore the saved files into the driver before the Keymaster app runs this boot. Ok(false): it already ran (the
/// driver still holds the current files).
pub fn store_restore(tee: &Tee, state: &Path) -> Res<bool> {
    let Ok(b) = std::fs::read(state.join(STORE)) else {
        return Ok(true); // nothing saved yet: the app starts with no files
    };
    let mut p = &b[..];
    let mut take = |n: usize| -> Res<&[u8]> {
        if p.len() < n {
            return Err(format!("{STORE}: truncated"));
        }
        let (a, r) = p.split_at(n);
        p = r;
        Ok(a)
    };
    let mut fresh = true;
    let count = u32::from_le_bytes(take(4)?.try_into().unwrap());
    for _ in 0..count {
        let plen = u16::from_le_bytes(take(2)?.try_into().unwrap()) as usize;
        let path = String::from_utf8_lossy(take(plen)?).into_owned();
        let dlen = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        let data = take(dlen)?.to_vec();
        if !tee.store_put(&path, &data)? {
            fresh = false;
            break;
        }
    }
    Ok(fresh)
}

/// Save the files when the Keymaster app changed them since `seen` (its generation); returns the new generation.
pub fn store_save(tee: &Tee, state: &Path, seen: u32) -> Res<u32> {
    let (generation, files) = tee.store_get()?;
    if generation == seen {
        return Ok(generation);
    }
    let mut b = (files.len() as u32).to_le_bytes().to_vec();
    for (path, data) in &files {
        b.extend((path.len() as u16).to_le_bytes());
        b.extend_from_slice(path.as_bytes());
        b.extend((data.len() as u32).to_le_bytes());
        b.extend_from_slice(data);
    }
    write_private(&state.join(STORE), &b)?;
    Ok(generation)
}
