//! Zero-copy scanout of gfxstream frames (Cuttlefish's crosvm, the Android VM's gfxstream fallback). Installed as
//! /opt/crosvm/bin/libGLESv2.so (soname libGLESv2.so) and LD_PRELOADed into crosvm.
//!
//! Without it every guest frame costs two copies: crosvm's display flush reads the scanout ColorBuffer back
//! (stream_renderer_transfer_read_iov -> glReadPixels into the wl_shm buffer) and the presenter copies that into a
//! KMS buffer. gfxstream ColorBuffers are freedreno GL textures, which export as UBWC dmabufs the display controller
//! scans out directly. So for crosvm's scanout reads (the only transfer_read calls with a host iovec; guest
//! TRANSFER_FROM_HOST has none) this exports the read framebuffer's texture as a dmabuf (once per texture), hands it
//! to the presenter on $MSHIP_ZC_SOCK and skips the readback. Everything else (and every frame while the presenter
//! is not listening) takes the normal path.
//!
//! gfxstream's GLES translator dlopen()s "libGLESv2.so" and dlsym()s every host GL function from it: the hooks below
//! shadow those, the rest resolve to the real libGLESv2.so.2 (DT_NEEDED).

use std::cell::Cell;
use std::ffi::{CStr, c_void};
use std::sync::{Mutex, OnceLock};

type GLenum = u32;
type GLuint = u32;
type GLint = i32;
type GLsizei = i32;
type Ptr = *mut c_void;

const GL_TEXTURE_2D: GLenum = 0x0DE1;
const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
const GL_READ_FRAMEBUFFER: GLenum = 0x8CA8;
const GL_READ_FRAMEBUFFER_BINDING: GLenum = 0x8CAA;
const GL_DRAW_FRAMEBUFFER_BINDING: GLenum = 0x8CA6;
const EGL_GL_TEXTURE_2D_KHR: u32 = 0x30B1;
const EGL_GL_TEXTURE_LEVEL_KHR: i32 = 0x30BC;
const EGL_NONE: i32 = 0x3038;

/// presenter protocol (SOCK_SEQPACKET): one message per frame, the dmabuf fd attached the first time an id is sent
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Msg {
    op: u32,
    id: u32,
    width: u32,
    height: u32,
    fourcc: u32,
    stride: u32,
    offset: u32,
    pad: u32,
    modifier: u64,
}
const FRAME: u32 = 1;
const FORGET: u32 = 2;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    a as u32 | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
}

#[derive(Clone, Copy, Default)]
struct Fbo {
    ctx: usize,
    fbo: GLuint,
    tex: GLuint,
}

#[derive(Clone, Copy)]
struct Exp {
    tex: GLuint,
    id: u32,
    fd: i32,
    sent: bool,
    dpy: usize,
    img: usize,
    m: Msg,
}

impl Default for Exp {
    fn default() -> Self {
        Exp { tex: 0, id: 0, fd: -1, sent: false, dpy: 0, img: 0, m: Msg::default() }
    }
}

struct Inner {
    sock: i32,
    sock_retry: i64,
    fbos: [Fbo; 256],
    exps: [Exp; 16],
    next_id: u32,
    victim: usize,
}

static INNER: Mutex<Inner> =
    Mutex::new(Inner { sock: -1, sock_retry: 0, fbos: [Fbo { ctx: 0, fbo: 0, tex: 0 }; 256], exps: [EXP0; 16], next_id: 1, victim: 0 });
const EXP0: Exp = Exp { tex: 0, id: 0, fd: -1, sent: false, dpy: 0, img: 0, m: Msg { op: 0, id: 0, width: 0, height: 0, fourcc: 0, stride: 0, offset: 0, pad: 0, modifier: 0 } };

thread_local! {
    /// inside crosvm's display readback
    static SCANOUT_READ: Cell<bool> = const { Cell::new(false) };
}

fn zlog(msg: &str) {
    eprintln!("[mship-zc] {msg}");
}

fn handle(lib: &CStr, cell: &OnceLock<usize>) -> usize {
    *cell.get_or_init(|| unsafe { libc::dlopen(lib.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) } as usize)
}

fn real_gl(name: &CStr) -> Ptr {
    static H: OnceLock<usize> = OnceLock::new();
    let h = handle(c"libGLESv2.so.2", &H);
    if h == 0 { std::ptr::null_mut() } else { unsafe { libc::dlsym(h as Ptr, name.as_ptr()) } }
}

fn real_egl(name: &CStr) -> Ptr {
    static H: OnceLock<usize> = OnceLock::new();
    let h = handle(c"libEGL.so.1", &H);
    if h == 0 {
        return std::ptr::null_mut();
    }
    let f = unsafe { libc::dlsym(h as Ptr, name.as_ptr()) };
    if !f.is_null() {
        return f;
    }
    let gpa = unsafe { libc::dlsym(h as Ptr, c"eglGetProcAddress".as_ptr()) };
    if gpa.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { std::mem::transmute::<Ptr, unsafe extern "C" fn(*const libc::c_char) -> Ptr>(gpa)(name.as_ptr()) }
}

/// A real function by name, cached, as the given function type.
macro_rules! real {
    ($lookup:ident, $name:literal, $ty:ty) => {{
        static F: OnceLock<usize> = OnceLock::new();
        let f = *F.get_or_init(|| $lookup($name) as usize);
        if f == 0 { None } else { Some(unsafe { std::mem::transmute::<usize, $ty>(f) }) }
    }};
}

fn current_context() -> usize {
    real!(real_egl, c"eglGetCurrentContext", unsafe extern "C" fn() -> Ptr).map_or(0, |f| unsafe { f() } as usize)
}

fn current_display() -> usize {
    real!(real_egl, c"eglGetCurrentDisplay", unsafe extern "C" fn() -> Ptr).map_or(0, |f| unsafe { f() } as usize)
}

fn get_integer(pname: GLenum) -> GLint {
    let mut v = 0;
    if let Some(f) = real!(real_gl, c"glGetIntegerv", unsafe extern "C" fn(GLenum, *mut GLint)) {
        unsafe { f(pname, &mut v) };
    }
    v
}

impl Inner {
    fn sock_ok(&mut self) -> bool {
        if self.sock >= 0 {
            return true;
        }
        let Some(path) = std::env::var_os("MSHIP_ZC_SOCK") else { return false };
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        if now == self.sock_retry {
            return false;
        }
        self.sock_retry = now;
        let s = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if s < 0 {
            return false;
        }
        let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        a.sun_family = libc::AF_UNIX as libc::sa_family_t;
        use std::os::unix::ffi::OsStrExt;
        let p = path.as_bytes();
        let n = p.len().min(a.sun_path.len() - 1);
        for (d, s) in a.sun_path.iter_mut().zip(&p[..n]) {
            *d = *s as libc::c_char;
        }
        if unsafe { libc::connect(s, (&a as *const libc::sockaddr_un).cast(), std::mem::size_of::<libc::sockaddr_un>() as u32) } != 0 {
            unsafe { libc::close(s) };
            return false;
        }
        self.sock = s;
        for e in self.exps.iter_mut() {
            e.sent = false; // a new presenter needs every fd again
        }
        zlog(&format!("presenter connected ({})", path.to_string_lossy()));
        true
    }

    fn send(&mut self, m: &Msg, fd: i32) -> bool {
        let mut msg = *m;
        let mut io = libc::iovec { iov_base: (&mut msg as *mut Msg).cast(), iov_len: std::mem::size_of::<Msg>() };
        let mut cbuf = [0u8; 64];
        let mut h: libc::msghdr = unsafe { std::mem::zeroed() };
        h.msg_iov = &mut io;
        h.msg_iovlen = 1;
        if fd >= 0 {
            h.msg_control = cbuf.as_mut_ptr().cast();
            h.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as _;
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&h);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<i32>(), fd);
            }
        }
        if unsafe { libc::sendmsg(self.sock, &h, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) } == std::mem::size_of::<Msg>() as isize {
            return true;
        }
        zlog("presenter gone");
        unsafe { libc::close(self.sock) };
        self.sock = -1;
        false
    }

    fn drop_export(&mut self, i: usize) {
        let e = self.exps[i];
        if e.tex == 0 {
            return;
        }
        if self.sock >= 0 && e.sent {
            self.send(&Msg { op: FORGET, id: e.id, ..Default::default() }, -1);
        }
        if let Some(di) = real!(real_egl, c"eglDestroyImageKHR", unsafe extern "C" fn(Ptr, Ptr) -> u32)
            && e.img != 0
        {
            unsafe { di(e.dpy as Ptr, e.img as Ptr) };
        }
        unsafe { libc::close(e.fd) };
        self.exps[i] = Exp::default();
    }

    /// The texture's export (made once): index into `exps`
    fn export_tex(&mut self, tex: GLuint, w: GLsizei, h: GLsizei) -> Option<usize> {
        if let Some(i) = self.exps.iter().position(|e| e.tex == tex) {
            return Some(i);
        }
        let ci = real!(real_egl, c"eglCreateImageKHR", unsafe extern "C" fn(Ptr, Ptr, u32, Ptr, *const i32) -> Ptr)?;
        let q = real!(real_egl, c"eglExportDMABUFImageQueryMESA", unsafe extern "C" fn(Ptr, Ptr, *mut i32, *mut i32, *mut u64) -> u32)?;
        let ex = real!(real_egl, c"eglExportDMABUFImageMESA", unsafe extern "C" fn(Ptr, Ptr, *mut i32, *mut i32, *mut i32) -> u32)?;
        let dpy = current_display();
        let attr = [EGL_GL_TEXTURE_LEVEL_KHR, 0, EGL_NONE];
        let img = unsafe { ci(dpy as Ptr, current_context() as Ptr, EGL_GL_TEXTURE_2D_KHR, tex as usize as Ptr, attr.as_ptr()) };
        let (mut fmt, mut planes, mut modifier) = (0i32, 0i32, 0u64);
        let (mut fd, mut stride, mut off) = ([-1i32; 4], [0i32; 4], [0i32; 4]);
        if img.is_null()
            || unsafe { q(dpy as Ptr, img, &mut fmt, &mut planes, &mut modifier) } == 0
            || planes != 1
            || unsafe { ex(dpy as Ptr, img, fd.as_mut_ptr(), stride.as_mut_ptr(), off.as_mut_ptr()) } == 0
        {
            let err = real!(real_egl, c"eglGetError", unsafe extern "C" fn() -> i32).map_or(0, |f| unsafe { f() });
            zlog(&format!("texture {tex}: dmabuf export failed ({err:#x})"));
            return None;
        }
        let i = match self.exps.iter().position(|e| e.tex == 0) {
            Some(i) => i,
            None => {
                // table full: recycle round robin
                let i = self.victim % self.exps.len();
                self.victim += 1;
                self.drop_export(i);
                i
            }
        };
        // the panel has no use for alpha: scan out the X variant of the same layout
        let fmt = fmt as u32;
        let f = match fmt {
            x if x == fourcc(b'A', b'B', b'2', b'4') => fourcc(b'X', b'B', b'2', b'4'),
            x if x == fourcc(b'A', b'R', b'2', b'4') => fourcc(b'X', b'R', b'2', b'4'),
            x => x,
        };
        let id = self.next_id;
        self.next_id += 1;
        self.exps[i] = Exp {
            tex,
            id,
            fd: fd[0],
            sent: false,
            dpy,
            img: img as usize,
            m: Msg { op: FRAME, id, width: w as u32, height: h as u32, fourcc: f, stride: stride[0] as u32, offset: off[0] as u32, pad: 0, modifier },
        };
        let name: String = f.to_le_bytes().iter().map(|b| *b as char).collect();
        zlog(&format!("texture {tex} -> id {id}: {w}x{h} {name} modifier {modifier:#x} stride {}", stride[0]));
        Some(i)
    }
}

// ---------------------------------------------------------------- hooks

/// # Safety
/// GLES's glFramebufferTexture2D.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glFramebufferTexture2D(target: GLenum, att: GLenum, textarget: GLenum, tex: GLuint, level: GLint) {
    if let Some(f) = real!(real_gl, c"glFramebufferTexture2D", unsafe extern "C" fn(GLenum, GLenum, GLenum, GLuint, GLint)) {
        unsafe { f(target, att, textarget, tex, level) };
    }
    if att != GL_COLOR_ATTACHMENT0 {
        return;
    }
    let fbo = get_integer(if target == GL_READ_FRAMEBUFFER { GL_READ_FRAMEBUFFER_BINDING } else { GL_DRAW_FRAMEBUFFER_BINDING }) as GLuint;
    let ctx = current_context();
    let mut i = INNER.lock().unwrap();
    let slot = i.fbos.iter().position(|s| s.ctx == ctx && s.fbo == fbo).or_else(|| i.fbos.iter().position(|s| s.fbo == 0));
    if let Some(s) = slot {
        i.fbos[s] = Fbo { ctx, fbo, tex: if textarget == GL_TEXTURE_2D && level == 0 { tex } else { 0 } };
    }
}

/// # Safety
/// GLES's glDeleteFramebuffers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glDeleteFramebuffers(n: GLsizei, names: *const GLuint) {
    let ctx = current_context();
    {
        let names = unsafe { std::slice::from_raw_parts(names, n.max(0) as usize) };
        let mut i = INNER.lock().unwrap();
        for s in i.fbos.iter_mut() {
            if s.ctx == ctx && names.contains(&s.fbo) {
                *s = Fbo::default();
            }
        }
    }
    if let Some(f) = real!(real_gl, c"glDeleteFramebuffers", unsafe extern "C" fn(GLsizei, *const GLuint)) {
        unsafe { f(n, names) };
    }
}

/// # Safety
/// GLES's glDeleteTextures.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glDeleteTextures(n: GLsizei, names: *const GLuint) {
    {
        let names = unsafe { std::slice::from_raw_parts(names, n.max(0) as usize) };
        let mut i = INNER.lock().unwrap();
        for k in 0..i.exps.len() {
            if i.exps[k].tex != 0 && names.contains(&i.exps[k].tex) {
                i.drop_export(k);
            }
        }
    }
    if let Some(f) = real!(real_gl, c"glDeleteTextures", unsafe extern "C" fn(GLsizei, *const GLuint)) {
        unsafe { f(n, names) };
    }
}

/// # Safety
/// GLES's glReadPixels.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glReadPixels(x: GLint, y: GLint, w: GLsizei, h: GLsizei, format: GLenum, typ: GLenum, px: Ptr) {
    if SCANOUT_READ.get() && x == 0 && y == 0 && w >= 640 && h >= 640 {
        SCANOUT_READ.set(false);
        let ctx = current_context();
        let fbo = get_integer(GL_READ_FRAMEBUFFER_BINDING) as GLuint;
        let mut i = INNER.lock().unwrap();
        let tex = i.fbos.iter().find(|s| s.ctx == ctx && s.fbo == fbo).map_or(0, |s| s.tex);
        let mut done = false;
        if tex != 0
            && i.sock_ok()
            && let Some(k) = i.export_tex(tex, w, h)
        {
            // rendering submitted: the dmabuf's implicit fence makes KMS wait for it
            if let Some(f) = real!(real_gl, c"glFlush", unsafe extern "C" fn()) {
                unsafe { f() };
            }
            let e = i.exps[k];
            done = i.send(&e.m, if e.sent { -1 } else { e.fd });
            if done {
                i.exps[k].sent = true;
            }
        }
        if done {
            return; // the presenter shows the texture itself; crosvm's shm copy stays stale and unused
        }
    }
    if let Some(f) = real!(real_gl, c"glReadPixels", unsafe extern "C" fn(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, Ptr)) {
        unsafe { f(x, y, w, h, format, typ, px) };
    }
}

type TransferReadIov = unsafe extern "C" fn(u32, u32, u32, u32, u32, Ptr, u64, *mut libc::iovec, i32) -> i32;

/// crosvm's display flush: transfer_read into a host iovec (guest-initiated transfers have none)
///
/// # Safety
/// gfxstream's stream_renderer_transfer_read_iov.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream_renderer_transfer_read_iov(
    handle: u32,
    ctx_id: u32,
    level: u32,
    stride: u32,
    layer_stride: u32,
    bbox: Ptr,
    offset: u64,
    iov: *mut libc::iovec,
    iovec_cnt: i32,
) -> i32 {
    static REAL: OnceLock<usize> = OnceLock::new();
    let f = *REAL.get_or_init(|| unsafe { libc::dlsym(libc::RTLD_NEXT, c"stream_renderer_transfer_read_iov".as_ptr()) } as usize);
    if f == 0 {
        return -libc::ENOSYS;
    }
    SCANOUT_READ.set(!iov.is_null() && iovec_cnt > 0 && ctx_id == 0);
    let r = unsafe { std::mem::transmute::<usize, TransferReadIov>(f)(handle, ctx_id, level, stride, layer_stride, bbox, offset, iov, iovec_cnt) };
    SCANOUT_READ.set(false);
    r
}

#[cfg(test)]
mod tests {
    #[test]
    fn message_layout_matches_the_presenter() {
        // mship-presenter's zc.rs reads 40 bytes: 8 x u32, then the u64 modifier
        assert_eq!(std::mem::size_of::<super::Msg>(), 40);
        assert_eq!(std::mem::offset_of!(super::Msg, modifier), 32);
    }
}
