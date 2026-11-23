//! The FPC Trusted Application through the port's transport (`/dev/fpc_tee`, motorola_nio kernel driver
//! fpc1541_tee): TAC command buffers `{u32 group, u32 cmd, payload}`, the finger interrupt, the Keymaster app and
//! its file store. Command layouts follow the vendor HAL's `fpc_tee_*` builders (motorola_nio docs/fingerprint.md).

use std::fs::File;
use std::os::fd::AsRawFd;

const LOAD: libc::c_ulong = 0x6603;
const WAIT_IRQ: libc::c_ulong = 0x4004_6606;
const TAC: libc::c_ulong = 0xC010_6607;
const KM: libc::c_ulong = 0xC020_6608;
const STORE_GET: libc::c_ulong = 0xC118_6609;
const STORE_PUT: libc::c_ulong = 0x4118_660A;

/// the main command buffer of the vendor HAL
const MAIN_LEN: usize = 0x1490;
/// db transfer chunk. The TA takes up to 0xfff90 bytes a chunk (the vendor HAL's size), but the kernel's QSEECOM
/// shared-memory pool is 256 KiB: larger buffers fail with ENOMEM. The TA moves exactly the length asked for.
const DB_CHUNK: usize = 0x10000;

#[repr(C)]
struct TacArg {
    buf: u64,
    len: u32,
    status: i32,
}

#[repr(C)]
struct KmArg {
    req: u64,
    req_len: u32,
    rsp_len: u32,
    rsp: u64,
    status: i32,
    reserved: u32,
}

#[repr(C)]
struct FileArg {
    index: u32,
    size: u32,
    buf: u64,
    buf_len: u32,
    generation: u32,
    path: [u8; 256],
}

pub type Res<T> = Result<T, String>;

/// FPC packs errors in bits 20..25 when bits 26..30 are neither 0 nor 0x1f.
pub fn fpc_err(s: i32) -> i32 {
    let u = s as u32;
    let k = (u >> 26) & 0x1f;
    if k != 0 && k != 0x1f && s <= 0 { -(((u >> 20) & 0x3f) as i32) } else { s }
}

pub struct Tee {
    dev: File,
    pub app_id: i32,
}

fn w(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())
}

fn set(b: &mut [u8], i: usize, v: u32) {
    b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
}

impl Tee {
    pub fn open(path: &str) -> Res<Self> {
        let dev = std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
        let app_id = unsafe { libc::ioctl(dev.as_raw_fd(), LOAD) };
        if app_id <= 0 {
            return Err(format!("TA load: {}", std::io::Error::last_os_error()));
        }
        Ok(Self { dev, app_id })
    }

    /// One TAC exchange; the buffer is updated in place. Err: transport failure.
    pub fn tac(&self, buf: &mut [u8]) -> Res<i32> {
        let mut a = TacArg { buf: buf.as_mut_ptr() as u64, len: buf.len() as u32, status: 0 };
        if unsafe { libc::ioctl(self.dev.as_raw_fd(), TAC, &mut a) } < 0 {
            return Err(format!("TAC: {}", std::io::Error::last_os_error()));
        }
        Ok(a.status)
    }

    /// A main-buffer command with payload words from index 2; returns (status, buffer).
    pub fn cmd(&self, group: u32, cmd: u32, words: &[u32]) -> Res<(i32, Vec<u8>)> {
        let mut b = vec![0u8; MAIN_LEN];
        set(&mut b, 0, group);
        set(&mut b, 1, cmd);
        for (i, v) in words.iter().enumerate() {
            set(&mut b, 2 + i, *v);
        }
        let s = self.tac(&mut b)?;
        Ok((s, b))
    }

    /// The finger interrupt line high within `ms` (level based, as the vendor HAL's fpc_irq_wait).
    pub fn wait_irq(&self, ms: i32) -> bool {
        unsafe { libc::ioctl(self.dev.as_raw_fd(), WAIT_IRQ, &ms) == 0 }
    }

    // ------------------------------------------------------------------ sensor (group 0xa)

    pub fn sensor(&self, c: u32) -> Res<i32> {
        Ok(fpc_err(self.cmd(0xa, c, &[])?.0))
    }

    pub fn build_info(&self) -> Res<String> {
        let mut b = vec![0u8; 0x418];
        set(&mut b, 0, 0xc);
        set(&mut b, 1, 1);
        set(&mut b, 5, 0x400);
        let s = self.tac(&mut b)?;
        if s != 0 {
            return Err(format!("build_info: {s}"));
        }
        let t = &b[0x18..];
        Ok(String::from_utf8_lossy(&t[..t.iter().position(|c| *c == 0).unwrap_or(t.len())]).into_owned())
    }

    // ------------------------------------------------------------------ bio (group 0xb)

    pub fn bio(&self, c: u32, words: &[u32]) -> Res<(i32, Vec<u8>)> {
        self.cmd(0xb, c, words)
    }

    pub fn finger_ids(&self, max: u32) -> Res<Vec<u32>> {
        let (s, b) = self.bio(8, &[max])?;
        if s != 0 {
            return Err(format!("finger ids: {s}"));
        }
        Ok((0..w(&b, 2).min(max) as usize).map(|i| w(&b, 3 + i)).collect())
    }

    pub fn db_id(&self) -> Res<u64> {
        let (s, b) = self.bio(0xb, &[])?;
        if s != 0 {
            return Err(format!("db id: {s}"));
        }
        Ok(u64::from_le_bytes(b[0x10..0x18].try_into().unwrap()))
    }

    /// The template DB of the active group out of the TA (fpc_tee_store_template_db).
    pub fn db_read(&self) -> Res<Vec<u8>> {
        let (s, b) = self.cmd(4, 6, &[])?;
        let size = w(&b, 2) as usize;
        if s < 0 || size == 0 {
            return Err(format!("db size: {s} ({size} bytes)"));
        }
        let r = (|| {
            let (s, _) = self.cmd(4, 0xe, &[0, 0, size as u32])?; // {dir 0 = from the TA @0xc, size @0x10}
            if s < 0 {
                return Err(format!("db open: {s}"));
            }
            let chunk = size.min(DB_CHUNK);
            let mut buf = vec![0u8; chunk + 0x70];
            let mut out = Vec::with_capacity(size);
            let mut left = size;
            while left > 0 {
                buf.fill(0);
                set(&mut buf, 0, 4);
                set(&mut buf, 1, 0x10);
                set(&mut buf, 2, chunk as u32);
                let n = left.min(DB_CHUNK);
                let s = self.tac(&mut buf)?;
                if s < 0 || (left - n) as i32 != s {
                    return Err(format!("db read: {s} (expected {} left)", left - n));
                }
                out.extend_from_slice(&buf[0xc..0xc + n]);
                left -= n;
            }
            Ok(out)
        })();
        let _ = self.cmd(4, 0xf, &[]);
        r
    }

    /// A template DB into the TA (fpc_tee_load_template_db). Err(-18): corrupted.
    pub fn db_write(&self, data: &[u8]) -> Res<()> {
        let (s, _) = self.cmd(4, 0xe, &[0, 1, data.len() as u32])?; // dir 1 = to the TA
        let r = (|| {
            if s < 0 {
                return Err(format!("db open: {s}"));
            }
            let chunk = data.len().min(DB_CHUNK);
            let mut buf = vec![0u8; chunk + 0x70];
            let mut off = 0;
            while off < data.len() {
                let n = (data.len() - off).min(DB_CHUNK);
                buf.fill(0);
                set(&mut buf, 0, 4);
                set(&mut buf, 1, 0x11);
                set(&mut buf, 2, chunk as u32);
                buf[0xc..0xc + n].copy_from_slice(&data[off..off + n]);
                let s = self.tac(&mut buf)?;
                if s < 0 {
                    return Err(format!("db write: {s} ({})", fpc_err(s)));
                }
                off += n;
                if (data.len() - off) as i32 != s {
                    return Err(format!("db write: {s} (expected {} left)", data.len() - off));
                }
            }
            Ok(())
        })();
        let _ = self.cmd(4, 0xf, &[]);
        r
    }

    // ------------------------------------------------------------------ hw auth (group 3)

    pub fn enrol_challenge(&self) -> Res<u64> {
        let (s, b) = self.cmd(3, 2, &[])?;
        if s != 0 {
            return Err(format!("enrol challenge: {s}"));
        }
        Ok(u64::from_le_bytes(b[8..16].try_into().unwrap()))
    }

    /// `{3, cmd, u32 len, data}` (authorize enrol: 3, token key: 5)
    pub fn auth_blob(&self, cmd: u32, data: &[u8]) -> Res<i32> {
        let mut b = vec![0u8; 12 + data.len()];
        set(&mut b, 0, 3);
        set(&mut b, 1, cmd);
        set(&mut b, 2, data.len() as u32);
        b[12..].copy_from_slice(data);
        let s = self.tac(&mut b);
        b.fill(0);
        s
    }

    // ------------------------------------------------------------------ Keymaster app

    /// One request to keymaster64: (app status, response bytes).
    pub fn km(&self, req: &[u8], rsp_len: usize) -> Res<(i32, Vec<u8>)> {
        let mut rsp = vec![0u8; rsp_len];
        let mut a = KmArg { req: req.as_ptr() as u64, req_len: req.len() as u32, rsp_len: rsp_len as u32, rsp: rsp.as_mut_ptr() as u64, status: 0, reserved: 0 };
        if unsafe { libc::ioctl(self.dev.as_raw_fd(), KM, &mut a) } < 0 {
            return Err(format!("keymaster: {}", std::io::Error::last_os_error()));
        }
        Ok((a.status, rsp))
    }

    /// The Keymaster app's files: (generation, [(path, data)]).
    pub fn store_get(&self) -> Res<(u32, Vec<(String, Vec<u8>)>)> {
        let mut files = vec![];
        let mut generation = 0;
        for index in 0.. {
            let mut a = FileArg { index, size: 0, buf: 0, buf_len: 0, generation: 0, path: [0; 256] };
            if unsafe { libc::ioctl(self.dev.as_raw_fd(), STORE_GET, &mut a) } < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::ENOENT) {
                    generation = a.generation;
                    break;
                }
                return Err(format!("store: {e}"));
            }
            let mut data = vec![0u8; a.size as usize];
            a.buf = data.as_mut_ptr() as u64;
            a.buf_len = a.size;
            if unsafe { libc::ioctl(self.dev.as_raw_fd(), STORE_GET, &mut a) } < 0 {
                return Err(format!("store: {}", std::io::Error::last_os_error()));
            }
            data.truncate(a.size as usize);
            let p = &a.path[..a.path.iter().position(|c| *c == 0).unwrap_or(256)];
            files.push((String::from_utf8_lossy(p).into_owned(), data));
        }
        Ok((generation, files))
    }

    /// Ok(false): the Keymaster app already ran this boot (the driver keeps its files).
    pub fn store_put(&self, path: &str, data: &[u8]) -> Res<bool> {
        let mut a = FileArg { index: 0, size: data.len() as u32, buf: data.as_ptr() as u64, buf_len: data.len() as u32, generation: 0, path: [0; 256] };
        if path.is_empty() || path.len() > 255 {
            return Err(format!("store: bad path {path:?}"));
        }
        a.path[..path.len()].copy_from_slice(path.as_bytes());
        if unsafe { libc::ioctl(self.dev.as_raw_fd(), STORE_PUT, &a) } < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EBUSY) {
                return Ok(false);
            }
            return Err(format!("store {path}: {e}"));
        }
        Ok(true)
    }
}
