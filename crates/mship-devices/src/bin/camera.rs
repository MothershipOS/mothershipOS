//! mship-camera: the phone's cameras for VMs (root), the mship.camera 1.x contract (docs/contracts/mship.camera-1.md).
//!
//! The cameras run on the host: sensor drivers, CAMSS and libcamera's software ISP (on the GPU) turn raw sensor data
//! into processed frames. A VM's camera slot is one V4L2 capture device per camera (virtio-media: crosvm proxies a
//! host node into the guest). The host node is a v4l2loopback device per VM and camera, created here; this service is
//! its writer: the format is set from the start (so the guest's camera stack finds a working device at any time),
//! and frames are written only while the VM holds the handle and a reader in the guest streams. Only then does the
//! camera run: one libcamera pipeline (`gst-launch-1.0 libcamerasrc`, raw BGRx frames on a pipe) per camera in use.
//!
//! Cameras are found with libcamera (`cam -l`), in its order; those the software ISP cannot process (the mono depth
//! sensor) are left out. Names follow the facing: back, back 2, front, front 2. Configuration (optional):
//! /etc/mothership/camera.json `{"cameras": {"<libcamera id>": {"name": "back ultrawide", "width": 1280,
//! "height": 720}}}`.
//!
//! Nodes: /run/mothership/camera/<vm>/<n>-<name> -> /dev/videoN, for VMs whose manifest has a `camera` slot (the
//! launcher proxies them in that order). Control: datagrams on /run/mothership/camera.sock, one reply each:
//! `attach <vm>`, `detach <vm> [user|screen|stop|revoked|gone]`, `status`.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mship_devices::{for_session, fresh, log};
use serde_json::{Value, json};

const CONF: &str = "/etc/mothership/camera.json";
const CTL: &str = "/run/mothership/camera.sock";
const NODES: &str = "/run/mothership/camera";
const VMS: &str = "/var/lib/mothership/vms";
const LOOPBACK_CTL: &str = "/dev/v4l2loopback";
const REASONS: &[&str] = &["user", "screen", "stop", "revoked", "gone"];
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 30;
/// the frame pipe from a camera pipeline (fs.pipe-max-size allows 1 MiB to root)
const PIPE_SIZE: libc::c_int = 1 << 20;

/// <linux/videodev2.h>, v4l2loopback.h (arm64 sizes)
mod v4l2 {
    pub const VIDIOC_S_FMT: libc::c_ulong = 0xc0d0_5605; // _IOWR('V', 5, struct v4l2_format: 208)
    pub const VIDIOC_SUBSCRIBE_EVENT: libc::c_ulong = 0x4020_565a; // _IOW('V', 90, struct v4l2_event_subscription: 32)
    pub const VIDIOC_DQEVENT: libc::c_ulong = 0x8088_5659; // _IOR('V', 89, struct v4l2_event: 136)
    pub const LOOPBACK_CTL_ADD: libc::c_ulong = 0x4048_7e01; // _IOW('~', 1, struct v4l2_loopback_config: 72)
    pub const LOOPBACK_CTL_REMOVE: libc::c_ulong = 0x4004_7e02; // _IOW('~', 2, __u32)
    pub const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
    pub const FIELD_NONE: u32 = 1;
    pub const COLORSPACE_SRGB: u32 = 8;
    /// 'XR24': B, G, R, X in memory (GStreamer BGRx, libcamera XRGB8888)
    pub const PIX_FMT_XBGR32: u32 = u32::from_le_bytes(*b"XR24");
    /// V4L2_EVENT_PRIVATE_START + 0x08E00000 + 1: whether a reader streams
    pub const EVENT_CLIENT_USAGE: u32 = 0x0800_0000 + 0x08e0_0000 + 1;
}

fn ioctl(fd: RawFd, req: libc::c_ulong, arg: *mut u8) -> std::io::Result<i32> {
    let r = unsafe { libc::ioctl(fd, req, arg) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(r) }
}

/// A new loopback device: its number.
fn loopback_add(label: &str, width: u32, height: u32) -> std::io::Result<i32> {
    let ctl = std::fs::OpenOptions::new().read(true).write(true).open(LOOPBACK_CTL)?;
    let mut cfg = [0u8; 72];
    cfg[0..4].copy_from_slice(&(-1i32).to_ne_bytes()); // output_nr: any
    cfg[4..8].copy_from_slice(&(-1i32).to_ne_bytes());
    let l = label.as_bytes();
    cfg[8..8 + l.len().min(31)].copy_from_slice(&l[..l.len().min(31)]);
    for (i, v) in [width, width, height, height].iter().enumerate() {
        cfg[40 + i * 4..44 + i * 4].copy_from_slice(&v.to_ne_bytes()); // min/max width, min/max height
    }
    cfg[56..60].copy_from_slice(&4i32.to_ne_bytes()); // max_buffers
    cfg[60..64].copy_from_slice(&4i32.to_ne_bytes()); // max_openers
    // announce_all_caps 0: capture only, while we (the writer) stream
    ioctl(ctl.as_raw_fd(), v4l2::LOOPBACK_CTL_ADD, cfg.as_mut_ptr())
}

fn loopback_remove(nr: i32) {
    if let Ok(ctl) = std::fs::OpenOptions::new().read(true).write(true).open(LOOPBACK_CTL) {
        let mut n = (nr as u32).to_ne_bytes();
        let _ = ioctl(ctl.as_raw_fd(), v4l2::LOOPBACK_CTL_REMOVE, n.as_mut_ptr());
    }
}

/// One VM's view of one camera: the loopback device we write.
struct Node {
    nr: i32,
    /// None only while dropping: closed before the device is removed
    fd: Option<OwnedFd>,
    frame: usize,
    /// a reader in the guest streams
    streaming: bool,
}

impl Node {
    fn new(label: &str, width: u32, height: u32) -> std::io::Result<Self> {
        let nr = loopback_add(label, width, height)?;
        // non-blocking: VIDIOC_DQEVENT must not wait (write() copies regardless)
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
        let fd = OwnedFd::from(f.inspect_err(|_| loopback_remove(nr))?);
        // struct v4l2_format: type, then v4l2_pix_format at offset 8
        let mut fmt = [0u8; 208];
        let frame = (width * height * 4) as usize;
        for (off, v) in [(0, v4l2::BUF_TYPE_VIDEO_OUTPUT), (8, width), (12, height), (16, v4l2::PIX_FMT_XBGR32),
                         (20, v4l2::FIELD_NONE), (24, width * 4), (28, frame as u32), (32, v4l2::COLORSPACE_SRGB)] {
            fmt[off..off + 4].copy_from_slice(&v.to_ne_bytes());
        }
        ioctl(fd.as_raw_fd(), v4l2::VIDIOC_S_FMT, fmt.as_mut_ptr())?;
        let mut sub = [0u8; 32];
        sub[0..4].copy_from_slice(&v4l2::EVENT_CLIENT_USAGE.to_ne_bytes());
        sub[8..12].copy_from_slice(&1u32.to_ne_bytes()); // V4L2_EVENT_SUB_FL_SEND_INITIAL
        ioctl(fd.as_raw_fd(), v4l2::VIDIOC_SUBSCRIBE_EVENT, sub.as_mut_ptr())?;
        let n = Self { nr, fd: Some(fd), frame, streaming: false };
        // the first frame starts our output stream: from now on readers can stream (and see black until frames flow)
        n.black();
        Ok(n)
    }

    fn raw(&self) -> RawFd {
        self.fd.as_ref().map_or(-1, |f| f.as_raw_fd())
    }

    fn write(&self, frame: &[u8]) {
        let r = unsafe { libc::write(self.raw(), frame.as_ptr().cast(), frame.len()) };
        if r < 0 {
            log(format!("video{}: {}", self.nr, std::io::Error::last_os_error()));
        }
    }

    /// Nothing of the camera's last image stays in the device.
    fn black(&self) {
        let mut f = vec![0u8; self.frame];
        f.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 0xff);
        self.write(&f);
    }

    /// Pending usage events: the reader state now.
    fn events(&mut self) {
        let mut ev = [0u8; 136];
        while ioctl(self.raw(), v4l2::VIDIOC_DQEVENT, ev.as_mut_ptr()).is_ok() {
            if u32::from_ne_bytes(ev[0..4].try_into().unwrap()) == v4l2::EVENT_CLIENT_USAGE {
                self.streaming = u32::from_ne_bytes(ev[8..12].try_into().unwrap()) != 0;
            }
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        // the device can only go once its last opener (ours) is closed
        self.fd.take();
        loopback_remove(self.nr);
    }
}

struct Cam {
    id: String,
    /// "back", "front 2", or the configured name
    name: String,
    width: u32,
    height: u32,
}

/// A running camera: libcamera's frames on a pipe.
struct Pipeline {
    child: Child,
    out: std::process::ChildStdout,
    /// the frame being read, and how much of it has arrived
    frame: Vec<u8>,
    filled: usize,
    started: Instant,
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
        if !probe.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("XRGB8888")) {
            log(format!("{id}: no processed output, left out"));
            continue;
        }
        let n = cams.iter().filter(|c| c.name.starts_with(facing)).count();
        let name = c["name"].as_str().map(String::from).unwrap_or(if n == 0 { facing.into() } else { format!("{facing} {}", n + 1) });
        cams.push(Cam { id: id.into(), name, width: w, height: h });
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
                log(format!("cameras: {}", self.cams.iter().map(|c| format!("{} ({}x{})", c.name, c.width, c.height)).collect::<Vec<_>>().join(", ")));
            }
        }
        if self.cams.is_empty() {
            return;
        }
        let want = camera_vms();
        let gone: Vec<String> = self.vms.keys().filter(|v| !want.contains(v)).cloned().collect();
        for vm in gone {
            self.vms.remove(&vm);
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
                match Node::new(&format!("{} camera", c.name), c.width, c.height) {
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

    /// Run exactly the cameras the holder's guest streams from.
    fn reconcile(&mut self) {
        let want: Vec<usize> = self.lent.as_ref().and_then(|vm| self.vms.get(vm))
            .map(|nodes| nodes.iter().enumerate().filter(|(_, n)| n.streaming).map(|(i, _)| i).collect())
            .unwrap_or_default();
        let stop: Vec<usize> = self.pipes.keys().filter(|i| !want.contains(i)).copied().collect();
        for i in stop {
            if let Some(mut p) = self.pipes.remove(&i) {
                let _ = p.child.kill();
                let _ = p.child.wait();
                log(format!("{} camera off", self.cams[i].name));
            }
        }
        for i in want {
            if self.pipes.contains_key(&i) {
                continue;
            }
            let c = &self.cams[i];
            let caps = format!("video/x-raw,width={},height={},framerate={FPS}/1,format=BGRx", c.width, c.height);
            let child = Command::new("gst-launch-1.0")
                .args(["-q", "libcamerasrc", &format!("camera-name={}", c.id), "!", &caps, "!", "fdsink", "fd=1", "sync=false"])
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
                    log(format!("{} camera on ({}x{} for {})", c.name, c.width, c.height, self.lent.as_deref().unwrap_or("?")));
                    let frame = vec![0u8; (c.width * c.height * 4) as usize];
                    self.pipes.insert(i, Pipeline { child, out, frame, filled: 0, started: Instant::now() });
                }
                Err(e) => log(format!("{} camera: gst-launch-1.0: {e}", c.name)),
            }
        }
    }

    /// Frames from a camera to the holder's node.
    fn frames(&mut self, i: usize) {
        let Some(p) = self.pipes.get_mut(&i) else { return };
        let node = self.lent.as_ref().and_then(|vm| self.vms.get(vm)).and_then(|n| n.get(i));
        loop {
            let filled = p.filled;
            match p.out.read(&mut p.frame[filled..]) {
                Ok(0) => {
                    let ran = p.started.elapsed();
                    let _ = p.child.wait();
                    self.pipes.remove(&i);
                    log(format!("{} camera stopped after {:.1} s", self.cams[i].name, ran.as_secs_f32()));
                    return;
                }
                Ok(n) => {
                    p.filled += n;
                    if p.filled == p.frame.len() {
                        if let Some(node) = node {
                            node.write(&p.frame);
                        }
                        p.filled = 0;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(_) => return,
            }
        }
    }

    fn release(&mut self, vm: &str, reason: &str) {
        if self.lent.as_deref() != Some(vm) {
            return;
        }
        self.lent = None;
        self.reconcile();
        // abort: the guest keeps nothing of what the camera saw
        for n in self.vms.get(vm).into_iter().flatten() {
            n.black();
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
                "cameras": self.cams.iter().map(|c| json!({"name": c.name, "id": c.id, "width": c.width, "height": c.height})).collect::<Vec<_>>(),
                "lent": self.lent,
                "running": self.pipes.keys().map(|i| self.cams[*i].name.clone()).collect::<Vec<_>>(),
                "vms": self.vms.iter().map(|(vm, nodes)| (vm.clone(), nodes.iter().zip(&self.cams)
                    .map(|(n, c)| json!({"camera": c.name, "node": format!("/dev/video{}", n.nr), "streaming": n.streaming})).collect::<Value>()))
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
    let _ = Command::new("modprobe").args(["v4l2loopback", "devices=0"]).status();
    std::fs::create_dir_all(NODES)?;
    for_session(Path::new(NODES), 0o755);
    let mut s = Service { conf, cams: vec![], vms: BTreeMap::new(), pipes: BTreeMap::new(), lent: None };
    s.scan();
    fresh(Path::new(CTL));
    let ctl = UnixDatagram::bind(CTL)?;
    for_session(Path::new(CTL), 0o660);
    let mut last = Instant::now();
    loop {
        let mut fds: Vec<(RawFd, i16)> = vec![(ctl.as_raw_fd(), libc::POLLIN)];
        let mut nodes = vec![];
        for (vm, ns) in &s.vms {
            for (i, n) in ns.iter().enumerate() {
                fds.push((n.raw(), libc::POLLPRI));
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
        for (fd, vm, i) in nodes {
            if ready.contains(&fd)
                && let Some(n) = s.vms.get_mut(&vm).and_then(|ns| ns.get_mut(i))
            {
                let was = n.streaming;
                n.events();
                changed |= was != n.streaming;
            }
        }
        if changed {
            s.reconcile();
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
        assert_eq!(v4l2::VIDIOC_S_FMT, ioc(3, b'V', 5, 208));
        assert_eq!(v4l2::VIDIOC_SUBSCRIBE_EVENT, ioc(1, b'V', 90, 32));
        assert_eq!(v4l2::VIDIOC_DQEVENT, ioc(2, b'V', 89, 136));
        assert_eq!(v4l2::LOOPBACK_CTL_ADD, ioc(1, b'~', 1, 72));
        assert_eq!(v4l2::LOOPBACK_CTL_REMOVE, ioc(1, b'~', 2, 4));
        assert_eq!(v4l2::PIX_FMT_XBGR32, 0x3432_5258);
    }
}
