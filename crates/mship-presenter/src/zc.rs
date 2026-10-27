//! Zero copy with gfxstream: the GL shim in crosvm (host/crosvm-shims/gles_zc.c) exports the guest's scanout
//! ColorBuffers as dmabufs over <socket>-zc (SOCK_SEQPACKET, one message per frame, the fd on a buffer's first
//! frame) and these are scanned out directly; crosvm's own frames then only pace. Also the per-second stats
//! (MSHIP_VM_DISPLAY_STATS=1).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;

use drm::control::framebuffer;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction, RegistrationToken};

use crate::kms::{Kms, Planes};
use crate::{State, log};

const FRAME: u32 = 1;
const FORGET: u32 = 2;
const MAX: usize = 16;
/// struct zc_msg: op, id, width, height, fourcc, stride, offset, pad (u32), modifier (u64)
const MSG: usize = 40;

#[derive(Clone, Copy, Default)]
struct Msg {
    op: u32,
    id: u32,
    width: u32,
    height: u32,
    fourcc: u32,
    stride: u32,
    offset: u32,
    modifier: u64,
}

fn parse(b: &[u8; MSG]) -> Msg {
    let u = |i: usize| u32::from_ne_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
    Msg { op: u(0), id: u(1), width: u(2), height: u(3), fourcc: u(4), stride: u(5), offset: u(6), modifier: u64::from_ne_bytes(b[32..40].try_into().unwrap()) }
}

struct Buf {
    m: Msg,
    fd: OwnedFd,
    fb: Option<framebuffer::Handle>,
    /// forgotten by the shim, removed once off screen
    dead: bool,
}

#[derive(Default)]
pub struct Stats {
    on: bool,
    t0: Option<Instant>,
    pub commits: u32,
    frames: u32,
    flips: u32,
    pub dropped: u32,
    /// copy mode: guest frames copied, their GPU time (total, max), frames that found every copy slot busy
    pub copies: u32,
    pub copy_us: u64,
    pub copy_max_us: u64,
    pub no_slot: u32,
}

impl Stats {
    pub fn flip(&mut self) {
        self.flips += 1;
        self.tick();
    }

    fn tick(&mut self) {
        if !self.on {
            return;
        }
        let t0 = *self.t0.get_or_insert_with(Instant::now);
        if t0.elapsed().as_secs() < 1 {
            return;
        }
        if self.commits + self.frames + self.flips > 0 {
            let copies = if self.copies > 0 {
                format!(", copies {} (avg {:.1} ms, max {:.1} ms), no free slot {}", self.copies, self.copy_us as f64 / self.copies as f64 / 1000.0,
                        self.copy_max_us as f64 / 1000.0, self.no_slot)
            } else {
                String::new()
            };
            log(format!("stats/s: crosvm commits {}, zc frames {}, flips {}, dropped {}{copies}", self.commits, self.frames, self.flips, self.dropped));
        }
        *self = Stats { on: true, t0: Some(Instant::now()), ..Default::default() };
    }
}

pub struct Zc {
    bufs: Vec<Option<Buf>>,
    conn: Option<(RegistrationToken, OwnedFd)>,
    /// the shim delivers frames: crosvm's frames only pace
    pub active: bool,
    /// newest guest frame (survives lease changes)
    pub last: Option<usize>,
    pub stats: Stats,
    path: String,
    frame_kind: String,
    warned: bool,
}

impl Zc {
    pub fn new(stats: bool) -> Self {
        Zc { bufs: (0..MAX).map(|_| None).collect(), conn: None, active: false, last: None, stats: Stats { on: stats, ..Default::default() }, path: String::new(), frame_kind: String::new(), warned: false }
    }

    /// Log which way frames go, when it changes.
    /// Logs a guest frame's kind once, when it changes (how frames arrive is visible in display.log).
    pub fn log_frame_kind(&mut self, kind: String) {
        if self.frame_kind != kind {
            log(format!("guest frames: {kind}"));
            self.frame_kind = kind;
        }
    }

    pub fn log_path(&mut self, p: &str) {
        let kind = p.split_whitespace().nth(1).unwrap_or("").to_string();
        if self.path != kind {
            self.path = kind;
            log(p);
        }
    }

    pub fn warn_once(&mut self, msg: &str) {
        if !self.warned {
            self.warned = true;
            log(msg);
        }
    }

    pub fn dead(&self, i: usize) -> bool {
        self.bufs[i].as_ref().is_none_or(|b| b.dead)
    }

    /// The buffer's framebuffer on this lease (made on first use): (fb, w, h).
    pub fn fb(&mut self, i: usize, k: &Kms) -> Option<(framebuffer::Handle, u32, u32)> {
        let b = self.bufs[i].as_mut()?;
        if b.fb.is_none() {
            let p = Planes { size: (b.m.width, b.m.height), format: b.m.fourcc, modifier: b.m.modifier, fds: vec![(b.fd.try_clone().ok()?, b.m.offset, b.m.stride)] };
            match k.add_fb(&p) {
                Ok(fb) => b.fb = Some(fb),
                Err(e) => {
                    log(format!("zc: buffer {} not scanout-capable: {e}", b.m.id));
                    b.dead = true;
                    return None;
                }
            }
        }
        Some((b.fb?, b.m.width, b.m.height))
    }

    fn free(&mut self, i: usize, k: Option<&Kms>) {
        if self.last == Some(i) {
            self.last = None;
        }
        if let (Some(b), Some(k)) = (self.bufs[i].take(), k)
            && let Some(fb) = b.fb
        {
            k.rm_fb(fb);
        }
    }

    /// The buffer went off screen.
    pub fn off_screen(&mut self, i: usize, k: Option<&Kms>) {
        if self.bufs[i].as_ref().is_some_and(|b| b.dead) {
            self.free(i, k);
        }
    }

    /// A lease ended: its framebuffers are gone with it.
    pub fn forget_fbs(&mut self) {
        for b in self.bufs.iter_mut().flatten() {
            b.fb = None;
        }
    }
}

pub fn listen(st: &mut State, path: &str) {
    let _ = std::fs::remove_file(path);
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
    if fd < 0 {
        return;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    a.sun_family = libc::AF_UNIX as u16;
    for (d, s) in a.sun_path.iter_mut().zip(path.bytes().take(107)) {
        *d = s as libc::c_char;
    }
    let ok = unsafe {
        libc::bind(fd.as_raw_fd(), (&a as *const libc::sockaddr_un).cast(), std::mem::size_of::<libc::sockaddr_un>() as u32) == 0
            && libc::listen(fd.as_raw_fd(), 2) == 0
    };
    if !ok {
        log(format!("zc: cannot listen on {path}: {}", std::io::Error::last_os_error()));
        return;
    }
    let _ = st.lh.insert_source(Generic::new(fd, Interest::READ, Mode::Level), |_, l, st: &mut State| {
        let c = unsafe { libc::accept4(l.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) };
        if c < 0 {
            return Ok(PostAction::Continue);
        }
        let c = unsafe { OwnedFd::from_raw_fd(c) };
        if let Some((t, _)) = st.zc.conn.take() {
            st.lh.remove(t); // one GPU per VM: the newest connection wins
        }
        let src = Generic::new(c.try_clone().expect("dup"), Interest::READ, Mode::Level);
        if let Ok(t) = st.lh.insert_source(src, |_, c, st: &mut State| {
            on_msg(st, c.as_raw_fd());
            Ok(PostAction::Continue)
        }) {
            st.zc.conn = Some((t, c));
            log("zc: gfxstream shim connected");
        }
        Ok(PostAction::Continue)
    });
}

/// One message from the shim (a frame, or a buffer it forgot); its end: back to crosvm's own frames.
fn on_msg(st: &mut State, fd: i32) {
    let mut b = [0u8; MSG];
    let mut cbuf = [0u8; 64];
    let mut iov = libc::iovec { iov_base: b.as_mut_ptr().cast(), iov_len: MSG };
    let mut h: libc::msghdr = unsafe { std::mem::zeroed() };
    h.msg_iov = &mut iov;
    h.msg_iovlen = 1;
    h.msg_control = cbuf.as_mut_ptr().cast();
    h.msg_controllen = cbuf.len() as _;
    let n = unsafe { libc::recvmsg(fd, &mut h, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        return;
    }
    if n != MSG as isize {
        log("zc: crosvm disconnected, back to copies");
        if let Some((t, _)) = st.zc.conn.take() {
            st.lh.remove(t);
        }
        st.zc.active = false;
        let on_screen: Vec<usize> = (0..MAX).filter(|i| st.zc.bufs[*i].is_some() && st.on_screen_zc(*i)).collect();
        for i in 0..MAX {
            if on_screen.contains(&i) {
                if let Some(b) = st.zc.bufs[i].as_mut() {
                    b.dead = true;
                }
            } else {
                st.zc.free(i, st.kms.as_ref());
            }
        }
        return;
    }
    let mut rfd: Option<OwnedFd> = None;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&h);
        if !c.is_null() && (*c).cmsg_type == libc::SCM_RIGHTS {
            rfd = Some(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const i32)));
        }
    }
    let m = parse(&b);
    let i = (0..MAX).find(|i| st.zc.bufs[*i].as_ref().is_some_and(|b| b.m.id == m.id && !b.dead));
    if m.op == FORGET {
        if let Some(i) = i {
            if st.on_screen_zc(i) {
                st.zc.bufs[i].as_mut().unwrap().dead = true;
            } else {
                st.zc.free(i, st.kms.as_ref());
            }
            st.unqueue_zc(i);
        }
        return;
    }
    if m.op != FRAME {
        return;
    }
    let i = match (i, rfd) {
        (Some(i), _) => i,
        (None, None) => return, // fd never seen: wait for the next one
        (None, Some(fd)) => {
            let Some(i) = (0..MAX).find(|i| st.zc.bufs[*i].is_none()) else {
                log("zc: too many buffers");
                return;
            };
            log(format!("zc: buffer {} {}x{} fourcc {:#x} modifier {:#x}", m.id, m.width, m.height, m.fourcc, m.modifier));
            st.zc.bufs[i] = Some(Buf { m, fd, fb: None, dead: false });
            i
        }
    };
    st.zc.stats.frames += 1;
    st.zc.stats.tick();
    if !st.zc.active {
        log("frames: zero copy (guest buffers scanned out directly)");
    }
    st.zc.active = true;
    st.zc.last = Some(i);
    st.present_zc(i);
}
