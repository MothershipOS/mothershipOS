//! mship-camera: the phone's cameras for VMs (root), the mship.camera 1.x contract (docs/contracts/mship.camera-1.md).
//!
//! The cameras run on the host: sensor drivers, CAMSS and libcamera's software ISP (on the GPU) turn raw sensor data
//! into processed frames. A VM's camera slot is one V4L2 capture device per camera (virtio-media: crosvm proxies a
//! host node into the guest). The host node is an mship-vcam camera per VM and camera (the kernel's mship-vcam
//! module), created here; this service is its writer. Frames flow only while the VM holds the handle and a reader in
//! the guest streams; only then does the camera run: one libcamera pipeline per camera in use (`gst-launch-1.0
//! libcamerasrc ! v4l2sink` into the camera's source node: the software ISP's buffers as they are). Each frame is
//! rendered on the GPU straight into the reader's buffer (the VM's own buffer, in the format and size it asked
//! for: gpu.rs); no CPU copies a frame.
//!
//! Cameras are found with libcamera (`cam -l`), in its order; those the software ISP cannot process (the mono depth
//! sensor) are left out. Names follow the facing: back, back 2, front, front 2. Configuration (optional):
//! /etc/mothership/camera.json `{"cameras": {"<libcamera id>": {"name": "back ultrawide", "width": 1280,
//! "height": 720}}}`.
//!
//! Nodes: /run/mothership/camera/<vm>/<n>-<name> -> /dev/videoN, for VMs whose manifest has a `camera` slot (the
//! launcher proxies them in that order). Control: datagrams on /run/mothership/camera.sock, one reply each:
//! `attach <vm>`, `detach <vm> [user|screen|stop|revoked|gone]`, `status`.
//!
//! Cameras with a focus lens are focused continuously while they run (af.rs); `"autofocus": false` in a camera's
//! configuration leaves the lens alone.

mod af;
mod gpu;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mship_core::handle::{Direction, Handle};
use mship_devices::{for_session, fresh, log, publish_handles};
use serde_json::{Value, json};

const CONF: &str = "/etc/mothership/camera.json";
const CTL: &str = "/run/mothership/camera.sock";
const NODES: &str = "/run/mothership/camera";
const VMS: &str = "/var/lib/mothership/vms";
const VCAM_CTL: &str = "/dev/mship-vcam";
const REASONS: &[&str] = &["user", "screen", "stop", "revoked", "gone"];
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 30;
/// the frame pipe from a raw camera (fs.pipe-max-size allows 1 MiB to root)
const PIPE_SIZE: libc::c_int = 1 << 20;

/// <linux/videodev2.h>, <linux/mship-vcam.h> (arm64 sizes)
mod v4l2 {
    pub const VIDIOC_S_CTRL: libc::c_ulong = 0xc008_561c; // _IOWR('V', 28, struct v4l2_control: 8)
    pub const VIDIOC_S_EXT_CTRLS: libc::c_ulong = 0xc020_5648; // _IOWR('V', 72, struct v4l2_ext_controls: 32)
    pub const VIDIOC_SUBDEV_ENUM_MBUS_CODE: libc::c_ulong = 0xc030_5602; // _IOWR('V', 2, 48)
    pub const VIDIOC_SUBDEV_ENUM_FRAME_SIZE: libc::c_ulong = 0xc040_564a; // _IOWR('V', 74, 64)
    /// mship-vcam: the modes the writer offers ("WxH@FPS,..."), and the event of a reader's choice
    pub const CID_MODES: u32 = 0x0098_f904;
    pub const EVENT_MODE: u32 = 0x0800_0000 + 0x08e0_0000 + 2;
    /// which of the camera's focus / light controls are real (bit 0 focus, bit 1 light), and a reader's AF trigger
    pub const CID_FEATURES: u32 = 0x0098_f905;
    pub const EVENT_AF_START: u32 = 0x0800_0000 + 0x08e0_0000 + 3;
    /// standard controls the guest's camera stack sets (V4L2_EVENT_CTRL = 3 on change) and the AF status we report
    pub const EVENT_CTRL: u32 = 3;
    pub const CID_FOCUS_AUTO: u32 = 0x009a_090c;
    pub const CID_FOCUS_ABSOLUTE: u32 = 0x009a_090a;
    pub const CID_AUTO_FOCUS_STATUS: u32 = 0x009a_090e;
    pub const CID_FLASH_LED_MODE: u32 = 0x009c_0901;
    pub const CID_EXPOSURE: u32 = 0x0098_0911;
    pub const CID_ANALOGUE_GAIN: u32 = 0x009e_0903;
    pub const FLASH_LED_MODE_TORCH: i32 = 2;
    /// the camera's facing (0 front, 1 back, 2 external) and mounting (degrees), standard camera controls
    pub const CID_CAMERA_ORIENTATION: u32 = 0x009a_0922;
    pub const CID_CAMERA_SENSOR_ROTATION: u32 = 0x009a_0923;
    pub const VIDIOC_SUBSCRIBE_EVENT: libc::c_ulong = 0x4020_565a; // _IOW('V', 90, struct v4l2_event_subscription: 32)
    pub const VIDIOC_DQEVENT: libc::c_ulong = 0x8088_5659; // _IOR('V', 89, struct v4l2_event: 136)
    /// V4L2_EVENT_PRIVATE_START + 0x08E00000 + 1: whether a reader streams
    pub const EVENT_CLIENT_USAGE: u32 = 0x0800_0000 + 0x08e0_0000 + 1;
    /// mship-vcam: cameras (struct mship_vcam_config: 64), the writer's claim, jobs (struct mship_vcam_job: 136),
    /// buffer exports (struct mship_vcam_export: 16), completions (struct mship_vcam_done: 32)
    pub const VCAM_ADD: libc::c_ulong = 0xc040_7e40; // _IOWR('~', 0x40, 64)
    pub const VCAM_REMOVE: libc::c_ulong = 0x4004_7e41; // _IOW('~', 0x41, 4)
    pub const VCAM_CLAIM: libc::c_ulong = 0x4004_56e0; // _IOW('V', BASE_VIDIOC_PRIVATE + 0x20, 4)
    pub const VCAM_GET_JOB: libc::c_ulong = 0x8088_56e1; // _IOR('V', + 0x21, 136)
    pub const VCAM_EXPORT: libc::c_ulong = 0xc010_56e2; // _IOWR('V', + 0x22, 16)
    pub const VCAM_PUT_JOB: libc::c_ulong = 0x4020_56e3; // _IOW('V', + 0x23, 32)
    pub const CLAIM_NO_SOURCE: u32 = 1;
    pub const JOB_SOURCE: u32 = 1;
    pub const QUEUE_CAPTURE: u32 = 0;
    pub const QUEUE_SOURCE: u32 = 1;
    pub const DONE_ERROR: u32 = 1;
    pub const DONE_DROP: u32 = 2;
    /// <linux/dma-buf.h>: DMA_BUF_IOCTL_SYNC (_IOW('b', 0, u64)), START / END, READ / WRITE
    pub const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
    pub const DMA_BUF_SYNC_END: u64 = 4;
    pub const DMA_BUF_SYNC_READ: u64 = 1;
    pub const DMA_BUF_SYNC_WRITE: u64 = 2;
}

fn ioctl(fd: RawFd, req: libc::c_ulong, arg: *mut u8) -> std::io::Result<i32> {
    let r = unsafe { libc::ioctl(fd, req, arg) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(r) }
}

/// A new mship-vcam camera (frames up to width x height): its capture and source node numbers.
fn vcam_add(label: &str, width: u32, height: u32) -> std::io::Result<(i32, i32)> {
    let ctl = std::fs::OpenOptions::new().read(true).write(true).open(VCAM_CTL)?;
    let mut cfg = [0u8; 64];
    let l = label.as_bytes();
    cfg[..l.len().min(31)].copy_from_slice(&l[..l.len().min(31)]);
    cfg[32..36].copy_from_slice(&width.to_ne_bytes());
    cfg[36..40].copy_from_slice(&height.to_ne_bytes());
    ioctl(ctl.as_raw_fd(), v4l2::VCAM_ADD, cfg.as_mut_ptr())?;
    let word = |o: usize| i32::from_ne_bytes(cfg[o..o + 4].try_into().unwrap());
    Ok((word(40), word(44)))
}

fn vcam_remove(nr: i32) {
    if let Ok(ctl) = std::fs::OpenOptions::new().read(true).write(true).open(VCAM_CTL) {
        let mut n = nr.to_ne_bytes();
        let _ = ioctl(ctl.as_raw_fd(), v4l2::VCAM_REMOVE, n.as_mut_ptr());
    }
}

/// A frame of a job (struct mship_vcam_frame).
#[derive(Clone, Copy, Debug, Default)]
struct Frame {
    index: u32,
    width: u32,
    height: u32,
    fourcc: u32,
    bytesperline: u32,
    offset1: u32,
    cookie: u64,
}

impl Frame {
    fn parse(b: &[u8]) -> Self {
        let w = |i: usize| u32::from_ne_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        Frame { index: w(0), width: w(1), height: w(2), fourcc: w(3), bytesperline: w(4), offset1: w(6),
                cookie: u64::from_ne_bytes(b[32..40].try_into().unwrap()) }
    }
}

/// A job: the reader's next buffer, and the newest source frame (when the camera runs).
struct Job {
    dst: Frame,
    src: Option<Frame>,
}

/// A mapping of a dma-buf (CPU access, synced as the dma-buf API says).
struct Map {
    ptr: *mut u8,
    len: usize,
    fd: RawFd,
}

impl Map {
    fn new(fd: RawFd, write: bool) -> std::io::Result<Self> {
        let len = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
        if len <= 0 {
            return Err(std::io::Error::other("dma-buf without a size"));
        }
        let prot = libc::PROT_READ | if write { libc::PROT_WRITE } else { 0 };
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len as usize, prot, libc::MAP_SHARED, fd, 0) };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        // our own descriptor (for the syncs): the buffer's export may be closed first
        Ok(Map { ptr: ptr.cast(), len: len as usize, fd: unsafe { libc::dup(fd) } })
    }

    fn sync(&self, flags: u64) {
        let mut f = flags;
        let _ = ioctl(self.fd, v4l2::DMA_BUF_IOCTL_SYNC, (&raw mut f).cast());
    }

    fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
            libc::close(self.fd);
        }
    }
}

/// Fill a buffer on the CPU from a BGRx frame (nearest neighbour, centre crop; None: black): the raw camera's
/// frames, and every frame when the GPU is not available.
fn cpu_fill(out: &mut [u8], d: &Frame, src: Option<(&[u8], u32, u32, u32)>) {
    let (dw, dh, dbpl) = (d.width as usize, d.height as usize, d.bytesperline as usize);
    let nv12 = d.fourcc == gpu::FOURCC_NV12;
    let need = if nv12 { d.offset1 as usize + dbpl * dh.div_ceil(2) } else { dbpl * dh };
    if out.len() < need || dw == 0 || dh == 0 {
        return;
    }
    // the source pixel for each target pixel (crop to the target's aspect ratio)
    let pick = src.map(|(_, sw, sh, _)| {
        let (sw, sh) = (sw as usize, sh as usize);
        let (cw, ch) = if sw * dh > dw * sh { (sh * dw / dh, sh) } else { (sw, sw * dh / dw) };
        let (x0, y0) = ((sw - cw) / 2, (sh - ch) / 2);
        let xs: Vec<usize> = (0..dw).map(|x| x0 + x * cw / dw).collect();
        let ys: Vec<usize> = (0..dh).map(|y| y0 + y * ch / dh).collect();
        (xs, ys)
    });
    let px = |x: usize, y: usize| -> (i32, i32, i32) {
        match (&src, &pick) {
            (Some((s, _, _, sbpl)), Some((xs, ys))) => {
                let i = ys[y] * *sbpl as usize + xs[x] * 4;
                (s[i + 2] as i32, s[i + 1] as i32, s[i] as i32)
            }
            _ => (0, 0, 0),
        }
    };
    for y in 0..dh {
        for x in 0..dw {
            let (r, g, b) = px(x, y);
            if nv12 {
                out[y * dbpl + x] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
            } else {
                out[y * dbpl + x * 4..y * dbpl + x * 4 + 4].copy_from_slice(&[b as u8, g as u8, r as u8, 0xff]);
            }
        }
    }
    if nv12 {
        for y in 0..dh / 2 {
            for x in 0..dw / 2 {
                let (r, g, b) = px(x * 2, y * 2);
                let o = d.offset1 as usize + y * dbpl + x * 2;
                out[o] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                out[o + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            }
        }
    }
}

/// A camera mode: size and frames per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mode {
    width: u32,
    height: u32,
    fps: u32,
}

impl Mode {
    fn frame(&self) -> usize {
        (self.width * self.height * 4) as usize
    }
}

/// Set an integer control.
fn set_ctrl(fd: RawFd, id: u32, value: i32) -> bool {
    let mut c: [u32; 2] = [id, value as u32];
    ioctl(fd, v4l2::VIDIOC_S_CTRL, c.as_mut_ptr().cast()).is_ok()
}

/// Set a string control (VIDIOC_S_EXT_CTRLS with one control).
fn set_string_ctrl(fd: RawFd, id: u32, value: &str) -> std::io::Result<()> {
    let mut text = value.as_bytes().to_vec();
    text.push(0);
    // struct v4l2_ext_control (packed, 20 bytes): id, size, reserved2, union { char *string }
    let mut ctrl = [0u8; 20];
    ctrl[0..4].copy_from_slice(&id.to_ne_bytes());
    ctrl[4..8].copy_from_slice(&(text.len() as u32).to_ne_bytes());
    ctrl[12..20].copy_from_slice(&(text.as_ptr() as u64).to_ne_bytes());
    // struct v4l2_ext_controls (32 bytes): which, count, error_idx, request_fd, reserved, pad, controls
    let mut ctrls = [0u8; 32];
    ctrls[4..8].copy_from_slice(&1u32.to_ne_bytes());
    ctrls[24..32].copy_from_slice(&(ctrl.as_ptr() as u64).to_ne_bytes());
    ioctl(fd, v4l2::VIDIOC_S_EXT_CTRLS, ctrls.as_mut_ptr()).map(|_| ())
}

/// The frame sizes of a camera's sensor (its V4L2 subdev, found by the libcamera id = device tree path).
fn sensor_sizes(camera_id: &str) -> Vec<(u32, u32)> {
    let node = Path::new("/sys/firmware/devicetree").join(camera_id.trim_start_matches('/'));
    let Ok(want) = std::fs::canonicalize(&node) else { return vec![] };
    let mut out = vec![];
    for e in std::fs::read_dir("/sys/class/video4linux").into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("v4l-subdev") || std::fs::canonicalize(e.path().join("device/of_node")).ok().as_ref() != Some(&want) {
            continue;
        }
        let Ok(f) = std::fs::File::open(Path::new("/dev").join(&name)) else { continue };
        // struct v4l2_subdev_mbus_code_enum: pad, index, code, which (1 = active), ...
        let mut code = [0u8; 48];
        code[12..16].copy_from_slice(&1u32.to_ne_bytes());
        if ioctl(f.as_raw_fd(), v4l2::VIDIOC_SUBDEV_ENUM_MBUS_CODE, code.as_mut_ptr()).is_err() {
            continue;
        }
        let mbus = u32::from_ne_bytes(code[8..12].try_into().unwrap());
        for index in 0..32u32 {
            // struct v4l2_subdev_frame_size_enum: index, pad, code, min_w, max_w, min_h, max_h, which, ...
            let mut fse = [0u8; 64];
            fse[0..4].copy_from_slice(&index.to_ne_bytes());
            fse[8..12].copy_from_slice(&mbus.to_ne_bytes());
            fse[28..32].copy_from_slice(&1u32.to_ne_bytes());
            if ioctl(f.as_raw_fd(), v4l2::VIDIOC_SUBDEV_ENUM_FRAME_SIZE, fse.as_mut_ptr()).is_err() {
                break;
            }
            let w = u32::from_ne_bytes(fse[16..20].try_into().unwrap());
            let h = u32::from_ne_bytes(fse[24..28].try_into().unwrap());
            out.push((w, h));
        }
    }
    out
}

/// The modes offered for a camera whose processed output goes up to `max`: the full size, then common sizes
/// below it (16:9 for video, and 4:3 sizes just large enough to hold them: Android's camera HAL captures one size
/// that covers every stream, e.g. a 4:3 preview or photo stream with 4K video). The rate follows the software
/// ISP's throughput, at most 30; 60 fps where the sensor has a binned mode at most 1920x1088 (its high-speed mode;
/// the rate still depends on the light).
fn modes_for(max: (u32, u32), sensor: &[(u32, u32)]) -> Vec<Mode> {
    // the software ISP on the GPU processes about 270 Mpixel/s (measured: 16 MP at 17 fps, 4K at 30)
    let rate = |w: u32, h: u32| (270_000_000 / ((w as u64) * (h as u64)).max(1)).clamp(1, 30) as u32;
    // the full size with whole 64-pixel groups per line: libcamera's frame lines are padded to 256 bytes, and the
    // camera's source node takes frames of exactly the width's line (4600 -> 4608 bytes too many: QBUF fails)
    // (the height follows, so the aspect ratio stays)
    let w = max.0 & !63;
    let max = (w, ((max.1 as u64 * w as u64 / max.0.max(1) as u64) as u32) & !1);
    let mut sizes = vec![max];
    for s in [(3840, 2880), (3840, 2160), (2560, 1920), (1920, 1440), (1920, 1080), (1280, 960), (1280, 720), (640, 480)] {
        if s.0 <= max.0 && s.1 <= max.1 && s != max {
            sizes.push(s);
        }
    }
    // a binned sensor mode that covers the size
    let fast = |w: u32, h: u32| sensor.iter().any(|&(sw, sh)| sw <= 1920 && sh <= 1088 && sw >= w && sh >= h);
    let mut out = vec![];
    for (w, h) in sizes {
        out.push(Mode { width: w, height: h, fps: rate(w, h) });
        if fast(w, h) {
            out.push(Mode { width: w, height: h, fps: 60 });
        }
    }
    out
}

/// The size of a mono sensor's raw stream (`cam -I`'s R10_CSI2P format), for cameras without processed output.
fn raw_size(id: &str) -> Option<(u32, u32)> {
    let out = Command::new("cam").args(["-c", id, "-I", "-s", "role=raw"]).env("LIBCAMERA_LOG_LEVELS", "*:FATAL").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // " * Pixelformat: R10_CSI2P (1600x1200)-(1600x1200)/(+1,+1)"
    let line = text.lines().find(|l| l.contains("Pixelformat: R10_CSI2P"))?;
    let r = line.split("-(").nth(1)?.split(')').next()?;
    let (w, h) = r.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// A camera's sensor subdev (its V4L2 subdev whose device tree node is the camera's).
fn sensor_subdev(camera_id: &str) -> Option<PathBuf> {
    let want = std::fs::canonicalize(Path::new("/sys/firmware/devicetree").join(camera_id.trim_start_matches('/'))).ok()?;
    std::fs::read_dir("/sys/class/video4linux").ok()?.flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("v4l-subdev")
            && std::fs::canonicalize(e.path().join("device/of_node")).ok().as_ref() == Some(&want))
        .map(|e| Path::new("/dev").join(e.file_name()))
}

/// The largest processed size libcamera offers for a camera (`cam -I`'s XRGB8888 range).
fn max_size(id: &str) -> Option<(u32, u32)> {
    let out = Command::new("cam").args(["-c", id, "-I", "-s", "role=video"]).env("LIBCAMERA_LOG_LEVELS", "*:FATAL").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // " * Pixelformat: XRGB8888 (4x2)-(4616x3472)/(+4,+2)"
    let line = text.lines().find(|l| l.contains("Pixelformat: XRGB8888"))?;
    let r = line.split("-(").nth(1)?.split(')').next()?;
    let (w, h) = r.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// One VM's view of one camera: the mship-vcam camera we write.
struct Node {
    /// its capture node (the VM's) and source node (the camera pipeline's)
    nr: i32,
    src_nr: i32,
    /// None only while dropping: closed before the device is removed
    fd: Option<OwnedFd>,
    /// the mode the guest chose (the writer follows): width, height, frames per second
    mode: Mode,
    /// a reader in the guest streams
    streaming: bool,
    /// what the guest wants from the lens and the light (the node's focus and flash controls)
    focus: af::Want,
    /// AF triggers not yet carried out
    triggers: u32,
    light: bool,
    /// the AF status last reported
    af_status: u32,
    /// how jobs come: with source frames (a camera pipeline), or when we fill buffers ourselves (CLAIM_NO_SOURCE)
    claim: u32,
    /// a raw camera: we fill every frame
    raw: bool,
    /// one black frame is due (the camera was taken away while the guest streams)
    black: bool,
    /// the buffers exported to us: (queue, index) -> (cookie, dma-buf)
    exports: HashMap<(u32, u32), (u64, OwnedFd)>,
}

/// What changed in a node's events.
#[derive(Default)]
struct Changes {
    mode: bool,
    streaming: bool,
    focus: bool,
    light: bool,
}

impl Node {
    fn new(label: &str, cam: &Cam) -> std::io::Result<Self> {
        let (facing, rotation) = (cam.facing, cam.rotation);
        let Mode { width, height, .. } = cam.default;
        let max = cam.modes.iter().fold((width, height), |m, x| (m.0.max(x.width), m.1.max(x.height)));
        let (nr, src_nr) = vcam_add(label, max.0, max.1)?;
        // non-blocking: VIDIOC_DQEVENT must not wait
        let open = || {
            std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(format!("/dev/video{nr}"))
        };
        // udev may still be creating the node
        let mut f = open();
        for _ in 0..50 {
            if f.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            f = open();
        }
        let fd = OwnedFd::from(f.inspect_err(|_| vcam_remove(nr))?);
        // we are its writer: jobs with source frames, or (a raw camera) jobs we fill ourselves
        let claim = if cam.raw.is_some() { v4l2::CLAIM_NO_SOURCE } else { 0 };
        let mut c = claim.to_ne_bytes();
        ioctl(fd.as_raw_fd(), v4l2::VCAM_CLAIM, c.as_mut_ptr())?;
        // which camera this is, for the guest's camera stack (the node keeps them for its readers)
        for (id, v) in [(v4l2::CID_CAMERA_ORIENTATION, facing), (v4l2::CID_CAMERA_SENSOR_ROTATION, rotation)] {
            let mut c: [u32; 2] = [id, v];
            if let Err(e) = ioctl(fd.as_raw_fd(), v4l2::VIDIOC_S_CTRL, c.as_mut_ptr().cast()) {
                log(format!("video{nr}: control {id:#x}: {e}"));
            }
        }
        let mut sub = [0u8; 32];
        sub[0..4].copy_from_slice(&v4l2::EVENT_CLIENT_USAGE.to_ne_bytes());
        sub[8..12].copy_from_slice(&1u32.to_ne_bytes()); // V4L2_EVENT_SUB_FL_SEND_INITIAL
        ioctl(fd.as_raw_fd(), v4l2::VIDIOC_SUBSCRIBE_EVENT, sub.as_mut_ptr())?;
        // the modes the guest may choose from
        let list = cam.modes.iter().map(|m| format!("{}x{}@{}", m.width, m.height, m.fps)).collect::<Vec<_>>().join(",");
        if let Err(e) = set_string_ctrl(fd.as_raw_fd(), v4l2::CID_MODES, &list) {
            log(format!("video{nr}: modes: {e}"));
        } else {
            let mut sub = [0u8; 32];
            sub[0..4].copy_from_slice(&v4l2::EVENT_MODE.to_ne_bytes());
            ioctl(fd.as_raw_fd(), v4l2::VIDIOC_SUBSCRIBE_EVENT, sub.as_mut_ptr())?;
        }
        // the camera's focus and light controls, and the guest's changes of them
        let features = u32::from(cam.lens.is_some()) | (u32::from(!cam.lights.is_empty()) << 1);
        set_ctrl(fd.as_raw_fd(), v4l2::CID_FEATURES, features as i32);
        for (ty, id) in [(v4l2::EVENT_CTRL, v4l2::CID_FOCUS_AUTO), (v4l2::EVENT_CTRL, v4l2::CID_FOCUS_ABSOLUTE),
                         (v4l2::EVENT_CTRL, v4l2::CID_FLASH_LED_MODE), (v4l2::EVENT_AF_START, 0)] {
            let mut sub = [0u8; 32];
            sub[0..4].copy_from_slice(&ty.to_ne_bytes());
            sub[4..8].copy_from_slice(&id.to_ne_bytes());
            let _ = ioctl(fd.as_raw_fd(), v4l2::VIDIOC_SUBSCRIBE_EVENT, sub.as_mut_ptr());
        }
        Ok(Self { nr, src_nr, fd: Some(fd), mode: cam.default, streaming: false, focus: af::Want::Continuous, triggers: 0,
                  light: false, af_status: 0, claim, raw: cam.raw.is_some(), black: false, exports: HashMap::new() })
    }

    fn raw(&self) -> RawFd {
        self.fd.as_ref().map_or(-1, |f| f.as_raw_fd())
    }

    fn set_claim(&mut self, flags: u32) {
        if flags != self.claim {
            let mut c = flags.to_ne_bytes();
            if ioctl(self.raw(), v4l2::VCAM_CLAIM, c.as_mut_ptr()).is_ok() {
                self.claim = flags;
            }
        }
    }

    /// The next job, if one is ready.
    fn job(&self) -> Option<Job> {
        let mut j = [0u8; 136];
        ioctl(self.raw(), v4l2::VCAM_GET_JOB, j.as_mut_ptr()).ok()?;
        let flags = u32::from_ne_bytes(j[0..4].try_into().unwrap());
        Some(Job { dst: Frame::parse(&j[8..56]), src: (flags & v4l2::JOB_SOURCE != 0).then(|| Frame::parse(&j[56..104])) })
    }

    /// Complete the open job (flags: DONE_*).
    fn done(&self, flags: u32) {
        let mut d = [0u8; 32];
        d[0..4].copy_from_slice(&flags.to_ne_bytes());
        if let Err(e) = ioctl(self.raw(), v4l2::VCAM_PUT_JOB, d.as_mut_ptr()) {
            log(format!("video{}: job: {e}", self.nr));
        }
    }

    /// A buffer of the camera as a dma-buf (exported once per memory).
    fn export(&mut self, queue: u32, f: &Frame) -> Option<RawFd> {
        if let Some((cookie, fd)) = self.exports.get(&(queue, f.index))
            && *cookie == f.cookie
        {
            return Some(fd.as_raw_fd());
        }
        let mut e = [0u8; 16];
        e[0..4].copy_from_slice(&queue.to_ne_bytes());
        e[4..8].copy_from_slice(&f.index.to_ne_bytes());
        if let Err(err) = ioctl(self.raw(), v4l2::VCAM_EXPORT, e.as_mut_ptr()) {
            log(format!("video{}: export {queue}/{}: {err}", self.nr, f.index));
            return None;
        }
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(i32::from_ne_bytes(e[8..12].try_into().unwrap())) };
        let raw = fd.as_raw_fd();
        self.exports.insert((queue, f.index), (f.cookie, fd));
        Some(raw)
    }

    /// The job's buffer as the GPU takes it.
    fn gpu_buf(&mut self, queue: u32, f: &Frame) -> Option<gpu::Buf> {
        let fd = self.export(queue, f)?;
        Some(gpu::Buf { fd, width: f.width, height: f.height, fourcc: f.fourcc, bytesperline: f.bytesperline,
                        offset1: f.offset1, key: (self.nr, queue, f.index), cookie: f.cookie })
    }

    /// Fill the open job's buffer on the CPU (from a BGRx frame; None: black) and complete the job.
    fn fill(&mut self, dst: &Frame, src: Option<(&[u8], u32, u32, u32)>) {
        let ok = self.export(v4l2::QUEUE_CAPTURE, dst).and_then(|fd| Map::new(fd, true).ok()).map(|mut m| {
            m.sync(v4l2::DMA_BUF_SYNC_WRITE);
            cpu_fill(m.bytes(), dst, src);
            m.sync(v4l2::DMA_BUF_SYNC_END | v4l2::DMA_BUF_SYNC_WRITE);
        });
        self.done(if ok.is_some() { 0 } else { v4l2::DONE_ERROR });
    }

    /// Pending events: whether a reader streams, the mode it chose, its focus and light.
    fn events(&mut self) -> Changes {
        let mut ev = [0u8; 136];
        let mut c = Changes::default();
        let word = |ev: &[u8], i: usize| u32::from_ne_bytes(ev[8 + i * 4..12 + i * 4].try_into().unwrap());
        while ioctl(self.raw(), v4l2::VIDIOC_DQEVENT, ev.as_mut_ptr()).is_ok() {
            match u32::from_ne_bytes(ev[0..4].try_into().unwrap()) {
                v4l2::EVENT_CLIENT_USAGE => {
                    let s = word(&ev, 0) != 0;
                    c.streaming |= s != self.streaming;
                    self.streaming = s;
                }
                v4l2::EVENT_MODE => {
                    let m = Mode { width: word(&ev, 0), height: word(&ev, 1), fps: word(&ev, 2).max(1) };
                    if m.width > 0 && m.height > 0 && m != self.mode {
                        self.mode = m;
                        c.mode = true;
                    }
                }
                v4l2::EVENT_AF_START => {
                    self.triggers += 1;
                    c.focus = true;
                }
                v4l2::EVENT_CTRL => {
                    // struct v4l2_event_ctrl at 8: changes, type, value (s32); the control id at 96
                    let id = u32::from_ne_bytes(ev[96..100].try_into().unwrap());
                    let value = i32::from_ne_bytes(ev[16..20].try_into().unwrap());
                    match id {
                        v4l2::CID_FOCUS_AUTO => {
                            let f = if value != 0 { af::Want::Continuous } else if self.focus == af::Want::Continuous { af::Want::Hold } else { self.focus };
                            c.focus |= f != self.focus;
                            self.focus = f;
                        }
                        v4l2::CID_FOCUS_ABSOLUTE => {
                            // manual focus: the guest sets the position with continuous AF off
                            if self.focus != af::Want::Continuous {
                                self.focus = af::Want::Manual(value);
                                c.focus = true;
                            }
                        }
                        v4l2::CID_FLASH_LED_MODE => {
                            let on = value == v4l2::FLASH_LED_MODE_TORCH;
                            c.light |= on != self.light;
                            self.light = on;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        // a trigger: one-shot autofocus (the guest's AF mode AUTO)
        if self.triggers > 0 && matches!(self.focus, af::Want::Hold | af::Want::Manual(_)) {
            self.focus = af::Want::OneShot;
        }
        c
    }

    /// Report the autofocus status to the guest (when it changes).
    fn report_af(&mut self, status: u32) {
        if status != self.af_status {
            self.af_status = status;
            set_ctrl(self.raw(), v4l2::CID_AUTO_FOCUS_STATUS, status as i32);
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.exports.clear();
        self.fd.take();
        vcam_remove(self.nr);
    }
}

struct Cam {
    id: String,
    /// "back", "front 2", or the configured name
    name: String,
    width: u32,
    height: u32,
    /// its lens subdev, when it has one and autofocus is on
    lens: Option<PathBuf>,
    /// V4L2_CAMERA_ORIENTATION_*: 0 front, 1 back, 2 external
    facing: u32,
    /// the modes VMs may choose from, and the one a device starts with (the configured size, 30 fps)
    modes: Vec<Mode>,
    default: Mode,
    /// its lights (LED class devices: flash LEDs, a ring light), driven as the guest's flash / torch
    lights: Vec<PathBuf>,
    /// the IIO name of a laser ranger that helps its autofocus
    ranger: Option<String>,
    /// raw mono frames only (no software ISP output): unpacked here, exposed by us
    raw: Option<RawSensor>,
    /// the sensor's mounting rotation in degrees (the device tree's `rotation`, as libcamera reports it)
    rotation: u32,
}

/// A raw-only mono camera (RAW10 packed frames from libcamera): its size and sensor subdev (exposure, gain).
#[derive(Clone)]
struct RawSensor {
    width: u32,
    height: u32,
    subdev: Option<PathBuf>,
}

/// Exposure control for a raw camera: mean brightness towards a target with the sensor's exposure, then gain.
struct RawAe {
    subdev: Option<File>,
    exposure: i32,
    gain: i32,
    /// RAW10 -> 8-bit lookup with the black level (64) removed
    lut: Vec<u8>,
}

impl RawAe {
    const EXPOSURE: (i32, i32) = (4, 1219);
    const GAIN: (i32, i32) = (16, 255);

    fn new(subdev: Option<&Path>) -> Self {
        let lut = (0..1024u32).map(|v| (v.saturating_sub(64) * 255 / (1023 - 64)).min(255) as u8).collect();
        let ae = Self { subdev: subdev.and_then(|p| File::options().read(true).write(true).open(p).ok()), exposure: 600, gain: 16, lut };
        ae.apply();
        ae
    }

    fn apply(&self) {
        if let Some(f) = &self.subdev {
            set_ctrl(f.as_raw_fd(), v4l2::CID_EXPOSURE, self.exposure);
            set_ctrl(f.as_raw_fd(), v4l2::CID_ANALOGUE_GAIN, self.gain);
        }
    }

    /// Unpack a RAW10 (CSI-2 packed) frame into grey BGRx, and adjust the exposure from its brightness.
    fn convert(&mut self, raw: &[u8], out: &mut [u8], width: usize, height: usize) {
        let stride = width * 5 / 4;
        let mut sum = 0u64;
        for y in 0..height {
            let row = &raw[y * stride..(y + 1) * stride];
            let o = &mut out[y * width * 4..(y + 1) * width * 4];
            for (g, px) in row.as_chunks::<5>().0.iter().enumerate() {
                for k in 0..4 {
                    let v = ((px[k] as usize) << 2) | ((px[4] as usize >> (2 * k)) & 3);
                    let b = self.lut[v];
                    let i = (g * 4 + k) * 4;
                    o[i] = b;
                    o[i + 1] = b;
                    o[i + 2] = b;
                    o[i + 3] = 0xff;
                    sum += b as u64;
                }
            }
        }
        // towards a mean of 100: exposure first, then gain (10 % steps, a dead band of 15 %)
        let mean = sum as f64 / (width * height) as f64;
        let ratio = 100.0 / mean.max(1.0);
        if !(0.85..=1.15).contains(&ratio) {
            let step = ratio.clamp(0.9, 1.1);
            let (exp, gain) = (self.exposure, self.gain);
            if step > 1.0 && self.exposure < Self::EXPOSURE.1 || step < 1.0 && self.gain <= Self::GAIN.0 {
                self.exposure = ((self.exposure as f64 * step) as i32).clamp(Self::EXPOSURE.0, Self::EXPOSURE.1);
            } else {
                self.gain = ((self.gain as f64 * step) as i32).clamp(Self::GAIN.0, Self::GAIN.1);
            }
            if (exp, gain) != (self.exposure, self.gain) {
                self.apply();
            }
        }
    }
}

/// A running camera: a libcamera pipeline queueing its frames on the holder's source node (its stdout ends when it
/// does), or (a raw camera) raw frames on its stdout.
struct Pipeline {
    child: Child,
    out: std::process::ChildStdout,
    /// a raw camera: the converted frame, the raw frame being read and how much of it has arrived, its exposure
    frame: Vec<u8>,
    filled: usize,
    raw: Option<(Vec<u8>, RawAe)>,
    started: Instant,
    af: Option<af::Af>,
    /// the mode it runs in
    mode: Mode,
    /// source frames mapped for the autofocus' look at them: (index, cookie) -> mapping
    maps: HashMap<(u32, u64), Map>,
}

/// The cameras libcamera finds and its software ISP can process, named by facing.
fn discover(conf: &Value) -> Vec<Cam> {
    let out = Command::new("cam").arg("-l").env("LIBCAMERA_LOG_LEVELS", "*:ERROR").output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let mut cams: Vec<Cam> = vec![];
    for l in text.lines() {
        // "1: Internal back camera (/base/soc@0/.../camera@21)"
        let Some((desc, id)) = l.split_once(" (").and_then(|(d, rest)| rest.strip_suffix(')').map(|i| (d, i))) else { continue };
        let facing = if desc.contains("front") { "front" } else if desc.contains("back") { "back" } else { "external" };
        let c = &conf["cameras"][id];
        let (w, h) = (c["width"].as_u64().unwrap_or(WIDTH as u64) as u32, c["height"].as_u64().unwrap_or(HEIGHT as u64) as u32);
        let probe = Command::new("cam").args(["-c", id, "-I", "-s", &format!("role=video,width={w},height={h}")])
            .env("LIBCAMERA_LOG_LEVELS", "*:FATAL").output();
        let mut raw = None;
        if !probe.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("XRGB8888")) {
            // a mono sensor the software ISP doesn't process: its raw frames, unpacked here
            match raw_size(id) {
                Some((rw, rh)) => raw = Some(RawSensor { width: rw, height: rh, subdev: sensor_subdev(id) }),
                None => {
                    log(format!("{id}: no processed or raw mono output, left out"));
                    continue;
                }
            }
        }
        let n = cams.iter().filter(|c| c.name.starts_with(facing)).count();
        let name = c["name"].as_str().map(String::from).unwrap_or(if n == 0 { facing.into() } else { format!("{facing} {}", n + 1) });
        let lens = if c["autofocus"].as_bool() == Some(false) { None } else { af::lens_of(id) };
        let rotation = std::fs::read(Path::new("/sys/firmware/devicetree").join(id.trim_start_matches('/')).join("rotation")).ok()
            .and_then(|b| b.get(..4).map(|b| u32::from_be_bytes(b.try_into().unwrap())))
            .unwrap_or(0);
        let facing = match facing { "front" => 0, "back" => 1, _ => 2 };
        let (modes, default) = match &raw {
            Some(r) => (vec![Mode { width: r.width, height: r.height, fps: FPS }], Mode { width: r.width, height: r.height, fps: FPS }),
            None => {
                let default = Mode { width: w, height: h, fps: FPS };
                let mut modes = max_size(id).map(|m| modes_for(m, &sensor_sizes(id))).unwrap_or_default();
                if !modes.contains(&default) {
                    modes.push(default);
                }
                (modes, default)
            }
        };
        let lights = c["lights"].as_array().into_iter().flatten().filter_map(|l| l.as_str())
            .map(|l| Path::new("/sys/class/leds").join(l)).filter(|p| p.exists()).collect();
        let ranger = c["ranger"].as_str().map(String::from);
        let lens = if raw.is_some() { None } else { lens };
        cams.push(Cam { id: id.into(), name, width: w, height: h, lens, facing, rotation, modes, default, lights, ranger, raw });
    }
    cams
}

/// The VM directories whose manifest has a camera slot.
fn camera_vms() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(VMS).into_iter().flatten().flatten()
        .filter(|e| {
            std::fs::read_to_string(e.path().join("vm.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .is_some_and(|m| m["slots"].as_object().is_some_and(|s| s.values().any(|x| x["contract"] == "mship.camera")))
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

struct Service {
    conf: Value,
    cams: Vec<Cam>,
    /// vm -> its nodes, one per camera (same order)
    vms: BTreeMap<String, Vec<Node>>,
    pipes: BTreeMap<usize, Pipeline>,
    lent: Option<String>,
    /// the GPU that renders the frames (None: the CPU does, slowly)
    gpu: Option<gpu::Gpu>,
}

impl Service {
    fn links(vm: &str) -> PathBuf {
        Path::new(NODES).join(vm)
    }

    /// Nodes for every VM with a camera slot (cameras found once they have probed).
    fn scan(&mut self) {
        if self.cams.is_empty() {
            self.cams = discover(&self.conf);
            if !self.cams.is_empty() {
                log(format!("cameras: {}", self.cams.iter().map(|c| format!("{} ({}x{}{})", c.name, c.width, c.height, if c.lens.is_some() { ", autofocus" } else { "" })).collect::<Vec<_>>().join(", ")));
            }
        }
        if self.cams.is_empty() {
            return;
        }
        let want = camera_vms();
        let gone: Vec<String> = self.vms.keys().filter(|v| !want.contains(v)).cloned().collect();
        for vm in gone {
            if let (Some(nodes), Some(g)) = (self.vms.remove(&vm), self.gpu.as_mut()) {
                nodes.iter().for_each(|n| g.forget(n.nr));
            }
            let _ = std::fs::remove_dir_all(Self::links(&vm));
        }
        for vm in want {
            if self.vms.contains_key(&vm) {
                continue;
            }
            let dir = Self::links(&vm);
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::create_dir_all(&dir);
            let mut nodes = vec![];
            for (i, c) in self.cams.iter().enumerate() {
                match Node::new(&format!("{} camera", c.name), c) {
                    Ok(n) => {
                        let _ = std::os::unix::fs::symlink(format!("/dev/video{}", n.nr), dir.join(format!("{i}-{}", c.name.replace(' ', "-"))));
                        nodes.push(n);
                    }
                    Err(e) => log(format!("{vm}: {} camera: {e}", c.name)),
                }
            }
            for_session(&dir, 0o755);
            log(format!("{vm}: {}", nodes.iter().zip(&self.cams).map(|(n, c)| format!("{} = video{}", c.name, n.nr)).collect::<Vec<_>>().join(", ")));
            self.vms.insert(vm, nodes);
        }
    }

    /// Run exactly the cameras the holder's guest streams from, in the mode it chose.
    fn reconcile(&mut self) {
        let want: Vec<(usize, Mode)> = self.lent.as_ref().and_then(|vm| self.vms.get(vm))
            .map(|nodes| nodes.iter().enumerate().filter(|(_, n)| n.streaming).map(|(i, n)| (i, n.mode)).collect())
            .unwrap_or_default();
        // a camera whose mode changed starts again in the new one
        let stop: Vec<usize> = self.pipes.iter()
            .filter(|(i, p)| !want.iter().any(|(j, m)| j == *i && *m == p.mode))
            .map(|(i, _)| *i).collect();
        for i in stop {
            if let Some(mut p) = self.pipes.remove(&i) {
                let _ = p.child.kill();
                let _ = p.child.wait();
                log(format!("{} camera off", self.cams[i].name));
            }
        }
        for (i, mode) in want {
            if self.pipes.contains_key(&i) {
                continue;
            }
            let c = &self.cams[i];
            let Some(src_nr) = self.lent.as_ref().and_then(|vm| self.vms.get(vm)).and_then(|n| n.get(i)).map(|n| n.src_nr) else { continue };
            let mut cmd = if c.raw.is_some() {
                // raw mono frames (no software ISP for them), unpacked here
                let mut cmd = Command::new("cam");
                cmd.args(["-c", &c.id, "--capture", "-s", "role=raw", "--file=/dev/stdout"]);
                cmd
            } else {
                // the software ISP's buffers queued on the source node as they are (dma-bufs)
                let caps = format!("video/x-raw,width={},height={},framerate={}/1,format=BGRx,pixel-aspect-ratio=1/1", mode.width, mode.height, mode.fps);
                let mut cmd = Command::new("gst-launch-1.0");
                cmd.args(["-q", "libcamerasrc", &format!("camera-name={}", c.id), "!", &caps, "!", "v4l2sink",
                          &format!("device=/dev/video{src_nr}"), "io-mode=dmabuf-import", "sync=false"]);
                cmd
            };
            let child = cmd
                .env("LIBCAMERA_LOG_LEVELS", "*:ERROR")
                .env("LIBCAMERA_SOFTISP_MODE", "gpu")
                .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
                .spawn();
            match child {
                Ok(mut child) => {
                    let out = child.stdout.take().unwrap();
                    unsafe {
                        libc::fcntl(out.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
                        // a frame in few reads (the default 64 KiB pipe needs ~60 per 720p frame)
                        libc::fcntl(out.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_SIZE);
                    }
                    log(format!("{} camera on ({}x{}@{} for {})", c.name, mode.width, mode.height, mode.fps, self.lent.as_deref().unwrap_or("?")));
                    let frame = if c.raw.is_some() { vec![0u8; mode.frame()] } else { vec![] };
                    let ranger = c.ranger.as_deref().and_then(af::Ranger::find);
                    let mut af = c.lens.as_deref().and_then(|l| af::Af::open(l, ranger).map_err(|e| log(format!("{} camera: {}: {e}", c.name, l.display()))).ok());
                    // what the holder's guest asks of the lens
                    if let (Some(af), Some(node)) = (af.as_mut(), self.lent.as_ref().and_then(|vm| self.vms.get(vm)).and_then(|n| n.get(i))) {
                        af.want(node.focus);
                    }
                    let raw = c.raw.as_ref().map(|r| (vec![0u8; (r.width * 5 / 4 * r.height) as usize], RawAe::new(r.subdev.as_deref())));
                    self.pipes.insert(i, Pipeline { child, out, frame, filled: 0, raw, started: Instant::now(), af, mode, maps: HashMap::new() });
                }
                Err(e) => log(format!("{} camera: gst-launch-1.0: {e}", c.name)),
            }
        }
    }

    /// A camera's output: a raw camera's frames to the holder's node; the end of a pipeline.
    fn frames(&mut self, i: usize) {
        let Some(p) = self.pipes.get_mut(&i) else { return };
        let mut node = self.lent.as_ref().and_then(|vm| self.vms.get_mut(vm)).and_then(|n| n.get_mut(i));
        let mut sink = [0u8; 256];
        loop {
            let filled = p.filled;
            let buf = match p.raw.as_mut() {
                Some((raw, _)) => &mut raw[filled..],
                None => &mut sink[..],
            };
            match p.out.read(buf) {
                Ok(0) => {
                    let ran = p.started.elapsed();
                    let _ = p.child.wait();
                    self.pipes.remove(&i);
                    log(format!("{} camera stopped after {:.1} s", self.cams[i].name, ran.as_secs_f32()));
                    return;
                }
                Ok(n) => {
                    let Some((raw, ae)) = p.raw.as_mut() else { continue };
                    p.filled += n;
                    if p.filled == raw.len() {
                        let (w, h) = (p.mode.width as usize, p.mode.height as usize);
                        ae.convert(raw, &mut p.frame, w, h);
                        // into the reader's next buffer (none queued: the frame is dropped)
                        if let Some(node) = node.as_deref_mut()
                            && let Some(job) = node.job()
                        {
                            node.fill(&job.dst, Some((&p.frame, w as u32, h as u32, w as u32 * 4)));
                        }
                        p.filled = 0;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(_) => return,
            }
        }
    }

    /// A node's next job: the newest source frame rendered into the reader's buffer (or one black frame).
    fn job(&mut self, vm: &str, i: usize) {
        let held = self.lent.as_deref() == Some(vm);
        let Some(node) = self.vms.get_mut(vm).and_then(|n| n.get_mut(i)) else { return };
        let Some(job) = node.job() else { return };
        if node.black {
            node.black = false;
            let dst = node.gpu_buf(v4l2::QUEUE_CAPTURE, &job.dst);
            if self.gpu.as_mut().zip(dst).is_some_and(|(g, d)| g.convert(None, &d).is_ok()) {
                node.done(0);
            } else {
                node.fill(&job.dst, None);
            }
            node.set_claim(if node.raw { v4l2::CLAIM_NO_SOURCE } else { 0 });
            return;
        }
        let Some(src) = job.src else {
            node.done(v4l2::DONE_DROP);
            return;
        };
        let (dst_buf, src_buf) = (node.gpu_buf(v4l2::QUEUE_CAPTURE, &job.dst), node.gpu_buf(v4l2::QUEUE_SOURCE, &src));
        let ok = match (self.gpu.as_mut(), dst_buf, src_buf) {
            (Some(g), Some(d), Some(s)) => g.convert(Some(&s), &d).map_err(|e| log(format!("video{}: {e}", node.nr))).is_ok(),
            (None, Some(_), Some(s)) => {
                // no GPU: on the CPU (slow)
                match Map::new(s.fd, false) {
                    Ok(mut m) => {
                        m.sync(v4l2::DMA_BUF_SYNC_READ);
                        let frame = m.bytes();
                        node.fill(&job.dst, Some((frame, src.width, src.height, src.bytesperline)));
                        m.sync(v4l2::DMA_BUF_SYNC_END | v4l2::DMA_BUF_SYNC_READ);
                        return;
                    }
                    Err(_) => false,
                }
            }
            _ => false,
        };
        node.done(if ok { 0 } else { v4l2::DONE_ERROR });
        // the autofocus looks at the frame (measured on the GPU; without one, a few thousand pixels on the CPU)
        if held
            && let Some(p) = self.pipes.get_mut(&i)
            && let Some(af) = p.af.as_mut()
            && let Some(sb) = src_buf
        {
            match self.gpu.as_mut() {
                Some(g) => af.frame_with(|| g.focus_measure(&sb)),
                None => {
                    if !p.maps.contains_key(&(src.index, src.cookie)) {
                        p.maps.retain(|k, _| k.0 != src.index);
                        if let Ok(m) = Map::new(sb.fd, false) {
                            p.maps.insert((src.index, src.cookie), m);
                        }
                    }
                    if let Some(m) = p.maps.get_mut(&(src.index, src.cookie)) {
                        m.sync(v4l2::DMA_BUF_SYNC_READ);
                        af.frame_stride(m.bytes(), src.width as usize, src.height as usize, src.bytesperline as usize);
                        m.sync(v4l2::DMA_BUF_SYNC_END | v4l2::DMA_BUF_SYNC_READ);
                    }
                }
            }
            node.report_af(af.status());
        }
    }

    /// The holder's focus wishes to a running camera.
    fn focus(&mut self, i: usize) {
        let Some(node) = self.lent.as_ref().and_then(|vm| self.vms.get_mut(vm)).and_then(|n| n.get_mut(i)) else { return };
        let Some(af) = self.pipes.get_mut(&i).and_then(|p| p.af.as_mut()) else { return };
        af.want(node.focus);
        while node.triggers > 0 {
            node.triggers -= 1;
            af.trigger();
        }
    }

    /// Each camera's lights as its holder's guest wants them (off when no VM holds the cameras).
    fn lights(&self) {
        for (i, c) in self.cams.iter().enumerate() {
            let on = self.lent.as_ref().and_then(|vm| self.vms.get(vm)).and_then(|n| n.get(i)).is_some_and(|n| n.light);
            for l in &c.lights {
                let value = if on { std::fs::read_to_string(l.join("max_brightness")).map(|m| m.trim().to_string()).unwrap_or("1".into()) } else { "0".into() };
                if std::fs::read_to_string(l.join("brightness")).map(|b| b.trim().to_string()).ok().as_deref() != Some(value.as_str())
                    && let Err(e) = std::fs::write(l.join("brightness"), &value)
                {
                    log(format!("{}: {e}", l.display()));
                }
            }
        }
    }

    fn release(&mut self, vm: &str, reason: &str) {
        if self.lent.as_deref() != Some(vm) {
            return;
        }
        self.lent = None;
        self.reconcile();
        self.lights();
        // abort: the guest's next frame is black (it keeps nothing of what the camera saw after this)
        for n in self.vms.get_mut(vm).into_iter().flatten().filter(|n| n.streaming) {
            n.black = true;
            n.set_claim(v4l2::CLAIM_NO_SOURCE);
        }
        log(format!("{vm}: released ({reason})"));
    }

    fn control(&mut self, msg: &str) -> String {
        self.scan();
        let w: Vec<&str> = msg.split_whitespace().collect();
        match w.as_slice() {
            ["attach", vm] if self.vms.contains_key(*vm) => {
                if let Some(old) = self.lent.clone().filter(|o| o != vm) {
                    self.release(&old, "revoked");
                }
                self.lent = Some(vm.to_string());
                self.reconcile();
                log(format!("{vm}: attached"));
                "ok".into()
            }
            ["attach", vm] => format!("error: {vm} has no camera slot (or no cameras found)"),
            ["detach", vm, rest @ ..] => {
                let reason = rest.first().filter(|r| REASONS.contains(r)).copied().unwrap_or("user");
                self.release(vm, reason);
                "ok".into()
            }
            ["status", ..] => json!({
                "cameras": self.cams.iter().map(|c| json!({"name": c.name, "id": c.id, "lens": c.lens,
                    "modes": c.modes.iter().map(|m| format!("{}x{}@{}", m.width, m.height, m.fps)).collect::<Vec<_>>()})).collect::<Vec<_>>(),
                "focus": self.pipes.iter().filter_map(|(i, p)| p.af.as_ref().map(|a| (self.cams[*i].name.clone(), a.status_json()))).collect::<serde_json::Map<_, _>>(),
                "lent": self.lent,
                "running": self.pipes.keys().map(|i| self.cams[*i].name.clone()).collect::<Vec<_>>(),
                "vms": self.vms.iter().map(|(vm, nodes)| (vm.clone(), nodes.iter().zip(&self.cams)
                    .map(|(n, c)| json!({"camera": c.name, "node": format!("/dev/video{}", n.nr), "streaming": n.streaming,
                        "mode": format!("{}x{}@{}", n.mode.width, n.mode.height, n.mode.fps)})).collect::<Value>()))
                    .collect::<serde_json::Map<_, _>>(),
            })
            .to_string(),
            _ => "error: attach <vm> | detach <vm> [reason] | status".into(),
        }
    }
}

/// Wait for input (or, for the nodes, a V4L2 event); the ready descriptors.
fn wait(fds: &[(RawFd, i16)], timeout: Duration) -> Vec<RawFd> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&(fd, events)| libc::pollfd { fd, events, revents: 0 }).collect();
    if unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout.as_millis() as i32) } <= 0 {
        return vec![];
    }
    p.iter().filter(|x| x.revents != 0).map(|x| x.fd).collect()
}

fn serve() -> std::io::Result<()> {
    let conf: Value = std::fs::read_to_string(CONF).ok().and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
    let _ = Command::new("modprobe").arg("mship-vcam").status();
    std::fs::create_dir_all(NODES)?;
    for_session(Path::new(NODES), 0o755);
    let gpu = gpu::Gpu::new().map_err(|e| log(format!("no GPU for the frames ({e}): the CPU renders them"))).ok();
    let mut s = Service { conf, cams: vec![], vms: BTreeMap::new(), pipes: BTreeMap::new(), lent: None, gpu };
    s.scan();
    fresh(Path::new(CTL));
    let ctl = UnixDatagram::bind(CTL)?;
    for_session(Path::new(CTL), 0o660);
    publish_handles("camera", &[Handle::new("camera:all", "mship.camera", "Cameras", "camera-photo-symbolic", Direction::In, "cameras")
        .present(!s.cams.is_empty())
        .control(CTL, None)]);
    let mut last = Instant::now();
    loop {
        let mut fds: Vec<(RawFd, i16)> = vec![(ctl.as_raw_fd(), libc::POLLIN)];
        let mut nodes = vec![];
        for (vm, ns) in &s.vms {
            for (i, n) in ns.iter().enumerate() {
                // jobs: as source frames come (a raw camera's when its frames come; a due black frame at once)
                let jobs = if !n.raw || n.black { libc::POLLIN } else { 0 };
                fds.push((n.raw(), libc::POLLPRI | jobs));
                nodes.push((n.raw(), vm.clone(), i));
            }
        }
        let pipes: Vec<(RawFd, usize)> = s.pipes.iter().map(|(i, p)| (p.out.as_raw_fd(), *i)).collect();
        fds.extend(pipes.iter().map(|(fd, _)| (*fd, libc::POLLIN)));
        let ready = wait(&fds, Duration::from_secs(10));
        if ready.contains(&ctl.as_raw_fd()) {
            let mut m = [0u8; 512];
            if let Ok((n, peer)) = ctl.recv_from(&mut m) {
                let reply = s.control(&String::from_utf8_lossy(&m[..n]));
                if let Some(p) = peer.as_pathname() {
                    let _ = ctl.send_to(reply.as_bytes(), p);
                }
            }
        }
        let mut changed = false;
        let mut focus = vec![];
        let mut lights = false;
        for (fd, vm, i) in &nodes {
            if ready.contains(fd) {
                s.job(vm, *i);
            }
        }
        for (fd, vm, i) in nodes {
            if ready.contains(&fd)
                && let Some(n) = s.vms.get_mut(&vm).and_then(|ns| ns.get_mut(i))
            {
                let c = n.events();
                if c.mode {
                    log(format!("{vm}: {} camera: {}x{}@{}", s.cams[i].name, n.mode.width, n.mode.height, n.mode.fps));
                }
                changed |= c.streaming || c.mode;
                if c.focus {
                    focus.push(i);
                }
                lights |= c.light;
            }
        }
        if changed {
            s.reconcile();
        }
        for i in focus {
            s.focus(i);
        }
        if lights {
            s.lights();
        }
        for (fd, i) in pipes {
            if ready.contains(&fd) {
                s.frames(i);
            }
        }
        if last.elapsed() >= Duration::from_secs(10) {
            s.scan();
            // a camera that died while wanted starts again
            s.reconcile();
            last = Instant::now();
        }
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: mship-camera (control: {CTL})");
        return std::process::ExitCode::from(2);
    }
    if let Err(e) = serve() {
        eprintln!("mship-camera: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers() {
        // _IOC(dir, type, nr, size) = dir << 30 | size << 16 | type << 8 | nr
        let ioc = |dir: u64, t: u8, nr: u64, size: u64| (dir << 30 | size << 16 | (t as u64) << 8 | nr) as libc::c_ulong;
        assert_eq!(v4l2::VIDIOC_SUBSCRIBE_EVENT, ioc(1, b'V', 90, 32));
        assert_eq!(v4l2::VIDIOC_DQEVENT, ioc(2, b'V', 89, 136));
        // BASE_VIDIOC_PRIVATE = 192
        assert_eq!(v4l2::VCAM_ADD, ioc(3, b'~', 0x40, 64));
        assert_eq!(v4l2::VCAM_REMOVE, ioc(1, b'~', 0x41, 4));
        assert_eq!(v4l2::VCAM_CLAIM, ioc(1, b'V', 192 + 0x20, 4));
        assert_eq!(v4l2::VCAM_GET_JOB, ioc(2, b'V', 192 + 0x21, 136));
        assert_eq!(v4l2::VCAM_EXPORT, ioc(3, b'V', 192 + 0x22, 16));
        assert_eq!(v4l2::VCAM_PUT_JOB, ioc(1, b'V', 192 + 0x23, 32));
        assert_eq!(v4l2::DMA_BUF_IOCTL_SYNC, ioc(1, b'b', 0, 8));
        assert_eq!(gpu::FOURCC_XR24, 0x3432_5258);
        assert_eq!(v4l2::VIDIOC_S_EXT_CTRLS, ioc(3, b'V', 72, 32));
        assert_eq!(v4l2::VIDIOC_SUBDEV_ENUM_MBUS_CODE, ioc(3, b'V', 2, 48));
        assert_eq!(v4l2::VIDIOC_SUBDEV_ENUM_FRAME_SIZE, ioc(3, b'V', 74, 64));
    }

    #[test]
    fn cpu_fill_nv12() {
        // a white 4x2 BGRx frame into a 4x2 NV12 buffer padded to 4 luma rows
        let src = vec![0xffu8; 4 * 2 * 4];
        let d = Frame { width: 4, height: 2, fourcc: gpu::FOURCC_NV12, bytesperline: 8, offset1: 32, ..Default::default() };
        let mut out = vec![0u8; 48];
        cpu_fill(&mut out, &d, Some((&src, 4, 2, 16)));
        assert_eq!(&out[0..4], &[235, 235, 235, 235]);
        assert_eq!(&out[32..36], &[128, 128, 128, 128]);
    }

    #[test]
    fn modes() {
        let names = |m: Vec<Mode>| m.iter().map(|m| format!("{}x{}@{}", m.width, m.height, m.fps)).collect::<Vec<_>>().join(",");
        // one sensor mode (the full size): no high-speed modes
        assert_eq!(names(modes_for((4616, 3472), &[(4624, 3472)])),
            "4608x3464@16,3840x2880@24,3840x2160@30,2560x1920@30,1920x1440@30,1920x1080@30,1280x960@30,1280x720@30,640x480@30");
        // a binned 1080p mode: 60 fps up to 1080p
        assert_eq!(names(modes_for((3256, 2448), &[(3264, 2448), (1920, 1080)])),
            "3200x2404@30,2560x1920@30,1920x1440@30,1920x1080@30,1920x1080@60,1280x960@30,1280x960@60,1280x720@30,1280x720@60,640x480@30,640x480@60");
    }
}
