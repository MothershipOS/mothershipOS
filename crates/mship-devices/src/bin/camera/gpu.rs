//! The frames' last step, on the host GPU: libcamera's processed frame (XRGB8888, a dma-buf the producer queued on
//! the camera's source node) rendered into the reader's buffer (a dma-buf: the VM's own buffer or a node buffer),
//! cropped to its aspect ratio, scaled, converted to its format: NV12 (BT.601 limited range, the coefficients of
//! libyuv's ARGBToI420 like the guest's camera stack) or XBGR32. Both are imported as EGL images (Mesa,
//! EGL_EXT_image_dma_buf_import); the NV12 planes as an R8 and an RG88 render target. No CPU touches the pixels.
//!
//! EGL and GLES are loaded at run time (libEGL.so.1, libGLESv2.so.2: Mesa on the phone), surfaceless.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::fd::RawFd;

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglImage = *mut c_void;
type EglInt = i32;

const EGL_NONE: EglInt = 0x3038;
const EGL_EXTENSIONS: EglInt = 0x3055;
const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31dd;
const EGL_OPENGL_ES_API: u32 = 0x30a0;
const EGL_CONTEXT_MAJOR_VERSION: EglInt = 0x3098;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_WIDTH: EglInt = 0x3057;
const EGL_HEIGHT: EglInt = 0x3056;
const EGL_LINUX_DRM_FOURCC_EXT: EglInt = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: EglInt = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EglInt = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: EglInt = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EglInt = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EglInt = 0x3444;

const GL_TEXTURE_2D: u32 = 0x0de1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812f;
const GL_FRAMEBUFFER: u32 = 0x8d40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8ce0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8cd5;
const GL_VERTEX_SHADER: u32 = 0x8b31;
const GL_FRAGMENT_SHADER: u32 = 0x8b30;
const GL_COMPILE_STATUS: u32 = 0x8b81;
const GL_LINK_STATUS: u32 = 0x8b82;
const GL_TRIANGLES: u32 = 4;
const GL_TEXTURE0: u32 = 0x84c0;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_NEAREST: i32 = 0x2600;

/// the autofocus' sample grid over the frame's centre (af::sharpness's)
const FOCUS_W: usize = 160;
const FOCUS_H: usize = 90;

/// DRM / V4L2 fourccs ('XR24' is the same layout in both: B, G, R, X in memory)
pub const FOURCC_XR24: u32 = u32::from_le_bytes(*b"XR24");
pub const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
const DRM_R8: u32 = u32::from_le_bytes(*b"R8  ");
const DRM_GR88: u32 = u32::from_le_bytes(*b"GR88");

macro_rules! fns {
    ($table:ident { $($name:ident : fn($($arg:ty),*) $(-> $ret:ty)?;)* }) => {
        #[allow(non_snake_case)]
        struct $table { $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)* }
        impl $table {
            unsafe fn load(lib: *mut c_void) -> Result<Self, String> {
                Ok(Self { $($name: {
                    let p = unsafe { libc::dlsym(lib, concat!(stringify!($name), "\0").as_ptr().cast()) };
                    if p.is_null() { return Err(format!("no {}", stringify!($name))); }
                    unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(p) }
                },)* })
            }
        }
    };
}

fns!(Egl {
    eglGetPlatformDisplay: fn(u32, *mut c_void, *const isize) -> EglDisplay;
    eglInitialize: fn(EglDisplay, *mut EglInt, *mut EglInt) -> u32;
    eglTerminate: fn(EglDisplay) -> u32;
    eglQueryString: fn(EglDisplay, EglInt) -> *const c_char;
    eglBindAPI: fn(u32) -> u32;
    eglCreateContext: fn(EglDisplay, *mut c_void, EglContext, *const EglInt) -> EglContext;
    eglDestroyContext: fn(EglDisplay, EglContext) -> u32;
    eglMakeCurrent: fn(EglDisplay, *mut c_void, *mut c_void, EglContext) -> u32;
    eglGetError: fn() -> EglInt;
    eglGetProcAddress: fn(*const c_char) -> *mut c_void;
});

fns!(Gl {
    glGenTextures: fn(i32, *mut u32);
    glDeleteTextures: fn(i32, *const u32);
    glBindTexture: fn(u32, u32);
    glTexParameteri: fn(u32, u32, i32);
    glActiveTexture: fn(u32);
    glGenFramebuffers: fn(i32, *mut u32);
    glDeleteFramebuffers: fn(i32, *const u32);
    glBindFramebuffer: fn(u32, u32);
    glFramebufferTexture2D: fn(u32, u32, u32, u32, i32);
    glCheckFramebufferStatus: fn(u32) -> u32;
    glViewport: fn(i32, i32, i32, i32);
    glClearColor: fn(f32, f32, f32, f32);
    glClear: fn(u32);
    glCreateShader: fn(u32) -> u32;
    glShaderSource: fn(u32, i32, *const *const c_char, *const i32);
    glCompileShader: fn(u32);
    glGetShaderiv: fn(u32, u32, *mut i32);
    glGetShaderInfoLog: fn(u32, i32, *mut i32, *mut c_char);
    glDeleteShader: fn(u32);
    glCreateProgram: fn() -> u32;
    glAttachShader: fn(u32, u32);
    glLinkProgram: fn(u32);
    glGetProgramiv: fn(u32, u32, *mut i32);
    glUseProgram: fn(u32);
    glGetUniformLocation: fn(u32, *const c_char) -> i32;
    glUniform1i: fn(i32, i32);
    glUniform4f: fn(i32, f32, f32, f32, f32);
    glGenVertexArrays: fn(i32, *mut u32);
    glBindVertexArray: fn(u32);
    glDrawArrays: fn(u32, i32, i32);
    glFinish: fn();
    glTexImage2D: fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
    glReadPixels: fn(i32, i32, i32, i32, u32, u32, *mut c_void);
});

type CreateImage = unsafe extern "C" fn(EglDisplay, EglContext, u32, *mut c_void, *const EglInt) -> EglImage;
type DestroyImage = unsafe extern "C" fn(EglDisplay, EglImage) -> u32;
type ImageTargetTexture = unsafe extern "C" fn(u32, EglImage);

const VERTEX: &str = "#version 300 es
uniform vec4 uCrop;
out highp vec2 vUv;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    vUv = uCrop.xy + p * uCrop.zw;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
";

/// fragment shaders: luma, chroma (U in R, V in G: NV12's byte order in an RG88 target), RGB copy, and the
/// autofocus' samples (green and its step from the left neighbour on a grid over the centre)
const FRAGMENTS: [&str; 4] = [
    "#version 300 es
precision highp float;
uniform sampler2D uTex;
in highp vec2 vUv;
out vec4 o;
void main() {
    vec3 c = texture(uTex, vUv).rgb;
    o = vec4(dot(c, vec3(0.2578125, 0.50390625, 0.09765625)) + 16.0 / 255.0, 0.0, 0.0, 1.0);
}
",
    "#version 300 es
precision highp float;
uniform sampler2D uTex;
in highp vec2 vUv;
out vec4 o;
void main() {
    vec3 c = texture(uTex, vUv).rgb;
    o = vec4(dot(c, vec3(-0.1484375, -0.2890625, 0.4375)) + 0.5,
             dot(c, vec3(0.4375, -0.3671875, -0.0703125)) + 0.5, 0.0, 1.0);
}
",
    "#version 300 es
precision highp float;
uniform sampler2D uTex;
in highp vec2 vUv;
out vec4 o;
void main() {
    o = vec4(texture(uTex, vUv).rgb, 1.0);
}
",
    "#version 300 es
precision highp float;
uniform sampler2D uTex;
uniform vec4 uGrid;
in highp vec2 vUv;
out vec4 o;
void main() {
    ivec2 p = ivec2(uGrid.xy + floor(gl_FragCoord.xy) * uGrid.zw);
    float g = texelFetch(uTex, p, 0).g;
    o = vec4(abs(g - texelFetch(uTex, p - ivec2(1, 0), 0).g), g, 0.0, 1.0);
}
",
];

/// A buffer to render from or into.
#[derive(Clone, Copy, Debug)]
pub struct Buf {
    pub fd: RawFd,
    pub width: u32,
    pub height: u32,
    /// V4L2 fourcc: XR24 or NV12
    pub fourcc: u32,
    pub bytesperline: u32,
    /// NV12: the chroma plane's offset
    pub offset1: u32,
    /// which buffer it is (the imports are kept per key), and the identity of its memory
    pub key: (i32, u32, u32),
    pub cookie: u64,
}

struct Plane {
    image: EglImage,
    tex: u32,
    fbo: u32,
}

struct Import {
    cookie: u64,
    planes: Vec<Plane>,
}

pub struct Gpu {
    egl: Egl,
    gl: Gl,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_target: ImageTargetTexture,
    display: EglDisplay,
    context: EglContext,
    programs: [u32; 4],
    crop: [i32; 4],
    grid: i32,
    /// the autofocus samples' target
    focus: Plane,
    imports: HashMap<(i32, u32, u32), Import>,
}

impl Gpu {
    pub fn new() -> Result<Self, String> {
        let open = |name: &str| {
            let n = CString::new(name).unwrap();
            let h = unsafe { libc::dlopen(n.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if h.is_null() { Err(format!("{name}: cannot load")) } else { Ok(h) }
        };
        let egl = unsafe { Egl::load(open("libEGL.so.1")?)? };
        let gl = unsafe { Gl::load(open("libGLESv2.so.2")?)? };
        unsafe {
            let display = (egl.eglGetPlatformDisplay)(EGL_PLATFORM_SURFACELESS_MESA, std::ptr::null_mut(), std::ptr::null());
            if display.is_null() || (egl.eglInitialize)(display, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                return Err(format!("no surfaceless EGL display ({:#x})", (egl.eglGetError)()));
            }
            let ext = CStr::from_ptr((egl.eglQueryString)(display, EGL_EXTENSIONS)).to_string_lossy().into_owned();
            for need in ["EGL_EXT_image_dma_buf_import", "EGL_KHR_surfaceless_context", "EGL_KHR_no_config_context"] {
                if !ext.split(' ').any(|e| e == need) {
                    (egl.eglTerminate)(display);
                    return Err(format!("EGL lacks {need}"));
                }
            }
            let proc = |name: &str| {
                let n = CString::new(name).unwrap();
                (egl.eglGetProcAddress)(n.as_ptr())
            };
            let (ci, di, it) = (proc("eglCreateImageKHR"), proc("eglDestroyImageKHR"), proc("glEGLImageTargetTexture2DOES"));
            if ci.is_null() || di.is_null() || it.is_null() {
                (egl.eglTerminate)(display);
                return Err("no EGL image functions".into());
            }
            (egl.eglBindAPI)(EGL_OPENGL_ES_API);
            let attrs = [EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE];
            let context = (egl.eglCreateContext)(display, std::ptr::null_mut(), std::ptr::null_mut(), attrs.as_ptr());
            if context.is_null() || (egl.eglMakeCurrent)(display, std::ptr::null_mut(), std::ptr::null_mut(), context) == 0 {
                let e = (egl.eglGetError)();
                (egl.eglTerminate)(display);
                return Err(format!("no GLES 3 context ({e:#x})"));
            }
            let mut g = Gpu {
                egl, gl, display, context,
                create_image: std::mem::transmute::<*mut c_void, CreateImage>(ci),
                destroy_image: std::mem::transmute::<*mut c_void, DestroyImage>(di),
                image_target: std::mem::transmute::<*mut c_void, ImageTargetTexture>(it),
                programs: [0; 4], crop: [-1; 4], grid: -1, imports: HashMap::new(),
                focus: Plane { image: std::ptr::null_mut(), tex: 0, fbo: 0 },
            };
            for (i, f) in FRAGMENTS.iter().enumerate() {
                g.programs[i] = g.program(VERTEX, f)?;
                (g.gl.glUseProgram)(g.programs[i]);
                (g.gl.glUniform1i)((g.gl.glGetUniformLocation)(g.programs[i], c"uTex".as_ptr()), 0);
                g.crop[i] = (g.gl.glGetUniformLocation)(g.programs[i], c"uCrop".as_ptr());
            }
            g.grid = (g.gl.glGetUniformLocation)(g.programs[3], c"uGrid".as_ptr());
            (g.gl.glGenTextures)(1, &mut g.focus.tex);
            (g.gl.glBindTexture)(GL_TEXTURE_2D, g.focus.tex);
            (g.gl.glTexImage2D)(GL_TEXTURE_2D, 0, GL_RGBA as i32, FOCUS_W as i32, FOCUS_H as i32, 0, GL_RGBA, GL_UNSIGNED_BYTE, std::ptr::null());
            (g.gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (g.gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (g.gl.glGenFramebuffers)(1, &mut g.focus.fbo);
            (g.gl.glBindFramebuffer)(GL_FRAMEBUFFER, g.focus.fbo);
            (g.gl.glFramebufferTexture2D)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, g.focus.tex, 0);
            if (g.gl.glCheckFramebufferStatus)(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
                return Err("no autofocus sample target".into());
            }
            // gl_VertexID drawing needs a vertex array bound (GLES 3)
            let mut vao = 0;
            (g.gl.glGenVertexArrays)(1, &mut vao);
            (g.gl.glBindVertexArray)(vao);
            Ok(g)
        }
    }

    unsafe fn shader(&self, kind: u32, src: &str) -> Result<u32, String> {
        let gl = &self.gl;
        unsafe {
            let s = (gl.glCreateShader)(kind);
            let text = CString::new(src).unwrap();
            let p = text.as_ptr();
            (gl.glShaderSource)(s, 1, &p, std::ptr::null());
            (gl.glCompileShader)(s);
            let mut ok = 0;
            (gl.glGetShaderiv)(s, GL_COMPILE_STATUS, &mut ok);
            if ok == 0 {
                let mut buf = [0 as c_char; 512];
                let mut n = 0;
                (gl.glGetShaderInfoLog)(s, buf.len() as i32, &mut n, buf.as_mut_ptr());
                return Err(format!("shader: {}", CStr::from_ptr(buf.as_ptr()).to_string_lossy()));
            }
            Ok(s)
        }
    }

    unsafe fn program(&self, vs: &str, fs: &str) -> Result<u32, String> {
        let gl = &self.gl;
        unsafe {
            let (v, f) = (self.shader(GL_VERTEX_SHADER, vs)?, self.shader(GL_FRAGMENT_SHADER, fs)?);
            let p = (gl.glCreateProgram)();
            (gl.glAttachShader)(p, v);
            (gl.glAttachShader)(p, f);
            (gl.glLinkProgram)(p);
            (gl.glDeleteShader)(v);
            (gl.glDeleteShader)(f);
            let mut ok = 0;
            (gl.glGetProgramiv)(p, GL_LINK_STATUS, &mut ok);
            if ok == 0 { Err("program does not link".into()) } else { Ok(p) }
        }
    }

    /// One plane of a dma-buf as a texture (and a framebuffer when it is a target).
    #[allow(clippy::too_many_arguments)]
    unsafe fn plane(&self, fd: RawFd, drm: u32, w: u32, h: u32, offset: u32, pitch: u32, target: bool) -> Option<Plane> {
        let gl = &self.gl;
        unsafe {
            let attrs = [
                EGL_WIDTH, w as EglInt, EGL_HEIGHT, h as EglInt, EGL_LINUX_DRM_FOURCC_EXT, drm as EglInt,
                EGL_DMA_BUF_PLANE0_FD_EXT, fd, EGL_DMA_BUF_PLANE0_OFFSET_EXT, offset as EglInt,
                EGL_DMA_BUF_PLANE0_PITCH_EXT, pitch as EglInt,
                // linear (DRM_FORMAT_MOD_LINEAR)
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, 0, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, 0, EGL_NONE,
            ];
            let image = (self.create_image)(self.display, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attrs.as_ptr());
            if image.is_null() {
                return None;
            }
            let mut tex = 0;
            (gl.glGenTextures)(1, &mut tex);
            (gl.glBindTexture)(GL_TEXTURE_2D, tex);
            (self.image_target)(GL_TEXTURE_2D, image);
            for (k, v) in [(GL_TEXTURE_MIN_FILTER, GL_LINEAR), (GL_TEXTURE_MAG_FILTER, GL_LINEAR),
                           (GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE), (GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE)] {
                (gl.glTexParameteri)(GL_TEXTURE_2D, k, v);
            }
            let mut fbo = 0;
            if target {
                (gl.glGenFramebuffers)(1, &mut fbo);
                (gl.glBindFramebuffer)(GL_FRAMEBUFFER, fbo);
                (gl.glFramebufferTexture2D)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
                if (gl.glCheckFramebufferStatus)(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
                    let p = Plane { image, tex, fbo };
                    self.release(&p);
                    return None;
                }
            }
            Some(Plane { image, tex, fbo })
        }
    }

    unsafe fn release(&self, p: &Plane) {
        unsafe {
            if p.fbo != 0 {
                (self.gl.glDeleteFramebuffers)(1, &p.fbo);
            }
            (self.gl.glDeleteTextures)(1, &p.tex);
            (self.destroy_image)(self.display, p.image);
        }
    }

    /// The buffer's imports (made now if its memory is new).
    fn import(&mut self, b: &Buf, target: bool) -> bool {
        if self.imports.get(&b.key).is_some_and(|i| i.cookie == b.cookie) {
            return true;
        }
        if let Some(old) = self.imports.remove(&b.key) {
            old.planes.iter().for_each(|p| unsafe { self.release(p) });
        }
        let planes: Option<Vec<Plane>> = unsafe {
            if b.fourcc == FOURCC_NV12 {
                let y = self.plane(b.fd, DRM_R8, b.width, b.height, 0, b.bytesperline, target);
                let c = self.plane(b.fd, DRM_GR88, b.width / 2, b.height / 2, b.offset1, b.bytesperline, target);
                match (y, c) {
                    (Some(y), Some(c)) => Some(vec![y, c]),
                    (y, c) => {
                        y.iter().chain(c.iter()).for_each(|p| self.release(p));
                        None
                    }
                }
            } else {
                self.plane(b.fd, FOURCC_XR24, b.width, b.height, 0, b.bytesperline, target).map(|p| vec![p])
            }
        };
        match planes {
            Some(planes) => {
                self.imports.insert(b.key, Import { cookie: b.cookie, planes });
                true
            }
            None => false,
        }
    }

    /// Render src (None: black) into dst; done when this returns. Err: a buffer cannot be imported.
    pub fn convert(&mut self, src: Option<&Buf>, dst: &Buf) -> Result<(), String> {
        if let Some(s) = src
            && !self.import(s, false)
        {
            return Err(format!("cannot import the source frame ({}x{} pitch {}) ({:#x})", s.width, s.height, s.bytesperline, unsafe { (self.egl.eglGetError)() }));
        }
        if !self.import(dst, true) {
            return Err(format!("cannot import the buffer ({}x{} pitch {}) ({:#x})", dst.width, dst.height, dst.bytesperline, unsafe { (self.egl.eglGetError)() }));
        }
        // centre crop of the source to the buffer's aspect ratio
        let crop = src.map_or([0.0, 0.0, 1.0, 1.0], |s| {
            let (sw, sh, dw, dh) = (s.width as f32, s.height as f32, dst.width as f32, dst.height as f32);
            if sw * dh > dw * sh {
                let cw = sh * dw / dh / sw;
                [(1.0 - cw) / 2.0, 0.0, cw, 1.0]
            } else {
                let ch = sw * dh / dw / sh;
                [0.0, (1.0 - ch) / 2.0, 1.0, ch]
            }
        });
        let gl = &self.gl;
        let src_tex = src.map(|s| self.imports[&s.key].planes[0].tex);
        let planes = &self.imports[&dst.key].planes;
        unsafe {
            (gl.glActiveTexture)(GL_TEXTURE0);
            if let Some(t) = src_tex {
                (gl.glBindTexture)(GL_TEXTURE_2D, t);
            }
            for (n, p) in planes.iter().enumerate() {
                let (w, h) = if n == 0 { (dst.width, dst.height) } else { (dst.width / 2, dst.height / 2) };
                (gl.glBindFramebuffer)(GL_FRAMEBUFFER, p.fbo);
                (gl.glViewport)(0, 0, w as i32, h as i32);
                let prog = if dst.fourcc != FOURCC_NV12 { 2 } else { n };
                if src_tex.is_none() {
                    let (r, g) = match (dst.fourcc == FOURCC_NV12, n) {
                        (true, 0) => (16.0 / 255.0, 0.0),
                        (true, _) => (0.5, 0.5),
                        _ => (0.0, 0.0),
                    };
                    (gl.glClearColor)(r, g, 0.0, 1.0);
                    (gl.glClear)(GL_COLOR_BUFFER_BIT);
                    continue;
                }
                (gl.glUseProgram)(self.programs[prog]);
                (gl.glUniform4f)(self.crop[prog], crop[0], crop[1], crop[2], crop[3]);
                (gl.glDrawArrays)(GL_TRIANGLES, 0, 3);
            }
            (gl.glFinish)();
        }
        Ok(())
    }

    /// The autofocus' measure of a source frame (imported by convert): sharpness and mean light, as
    /// af::sharpness computes them from the frame's centre.
    pub fn focus_measure(&mut self, src: &Buf) -> Option<(f64, f64)> {
        let tex = self.imports.get(&src.key).filter(|i| i.cookie == src.cookie)?.planes[0].tex;
        let (w, h) = (src.width as usize, src.height as usize);
        let grid = [(w / 4 + 1) as f32, (h / 4) as f32, (w / 2 / FOCUS_W).max(2) as f32, (h / 2 / FOCUS_H).max(1) as f32];
        let mut px = vec![0u8; FOCUS_W * FOCUS_H * 4];
        let gl = &self.gl;
        unsafe {
            (gl.glBindTexture)(GL_TEXTURE_2D, tex);
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, self.focus.fbo);
            (gl.glViewport)(0, 0, FOCUS_W as i32, FOCUS_H as i32);
            (gl.glUseProgram)(self.programs[3]);
            (gl.glUniform4f)(self.grid, grid[0], grid[1], grid[2], grid[3]);
            (gl.glDrawArrays)(GL_TRIANGLES, 0, 3);
            (gl.glReadPixels)(0, 0, FOCUS_W as i32, FOCUS_H as i32, GL_RGBA, GL_UNSIGNED_BYTE, px.as_mut_ptr().cast());
        }
        let (steps, light) = px.as_chunks::<4>().0.iter().fold((0u64, 0u64), |(s, l), p| (s + p[0] as u64, l + p[1] as u64));
        let n = (FOCUS_W * FOCUS_H) as f64;
        let mean = light as f64 / n;
        Some((if mean < 1.0 { 0.0 } else { steps as f64 / n / mean }, mean))
    }

    /// Drop the imports of a node's buffers (the node is gone).
    pub fn forget(&mut self, node: i32) {
        let keys: Vec<_> = self.imports.keys().filter(|k| k.0 == node).copied().collect();
        for k in keys {
            if let Some(i) = self.imports.remove(&k) {
                i.planes.iter().for_each(|p| unsafe { self.release(p) });
            }
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        let all: Vec<_> = self.imports.drain().collect();
        for (_, i) in all {
            i.planes.iter().for_each(|p| unsafe { self.release(p) });
        }
        unsafe {
            (self.gl.glDeleteFramebuffers)(1, &self.focus.fbo);
            (self.gl.glDeleteTextures)(1, &self.focus.tex);
            (self.egl.eglMakeCurrent)(self.display, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            (self.egl.eglDestroyContext)(self.display, self.context);
            (self.egl.eglTerminate)(self.display);
        }
    }
}
