//! Autofocus by contrast, on the host (the software ISP has no focus algorithm), driven by what the guest asks
//! (the camera node's standard focus controls) and helped by a laser ranger when the camera has one.
//!
//! A camera whose sensor has a lens (`lens-focus` in the device tree: a V4L2 lens subdev, V4L2_CID_FOCUS_ABSOLUTE)
//! is focused from the processed frames the service already reads: a coarse sweep over the lens range, a fine
//! sweep around the sharpest position, then the best position. Modes, from the guest:
//! - continuous: a found focus is searched again when the scene changes (sharpness or brightness away from the
//!   found values for a while, or the ranger's distance moving by a fifth);
//! - one-shot: a guest's trigger runs one search, then the lens stays;
//! - hold: the lens stays where it is (a locked continuous focus);
//! - manual: the guest's lens position.
//!
//! With a ranger, each found focus is remembered against the distance (in diopters, where a voice coil's position
//! is close to linear); with two of them a search starts at the predicted position with a fine sweep only.
//!
//! The lens subdev is held open while the camera runs, which powers the actuator; closing it parks the lens.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// <linux/videodev2.h>
const VIDIOC_S_CTRL: libc::c_ulong = 0xc008_561c; // _IOWR('V', 28, struct v4l2_control: 8)
const CID_FOCUS_ABSOLUTE: u32 = 0x009a_090a;

/// frames to wait after a move before measuring (lens travel + the pipeline's frames in flight)
const SETTLE: u32 = 3;
const COARSE: i32 = 96;
const FINE: i32 = 16;
/// the scene changed: sharpness below this share of the found value, or brightness off by this share ...
const DROP: f64 = 0.6;
const LIGHT: f64 = 0.3;
/// ... for this many frames in a row
const CHANGED_FOR: u32 = 10;
/// a ranger distance this much away (share) from the found one is a new scene
const RANGE_MOVED: f64 = 0.2;
/// V4L2_AUTO_FOCUS_STATUS_*
pub const STATUS_BUSY: u32 = 1;
pub const STATUS_REACHED: u32 = 2;
pub const STATUS_FAILED: u32 = 4;

/// The lens subdev of a libcamera camera (`/base/...` device-tree path of its sensor), if it has one.
pub fn lens_of(camera_id: &str) -> Option<PathBuf> {
    let node = Path::new("/sys/firmware/devicetree").join(camera_id.trim_start_matches('/'));
    let want = std::fs::read(node.join("lens-focus")).ok()?;
    for e in std::fs::read_dir("/sys/class/video4linux").ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("v4l-subdev") && std::fs::read(e.path().join("device/of_node/phandle")).ok().as_ref() == Some(&want) {
            return Some(Path::new("/dev").join(name));
        }
    }
    None
}

/// Relative contrast of the frame's centre: mean absolute step between neighbouring green pixels over mean
/// green, sampled at about 160 x 90 points whatever the size. `frame` is BGRx.
#[cfg(test)]
pub fn sharpness(frame: &[u8], width: usize, height: usize) -> (f64, f64) {
    sharpness_stride(frame, width, height, width * 4)
}

/// The same for a frame with `stride` bytes per line.
pub fn sharpness_stride(frame: &[u8], width: usize, height: usize, stride: usize) -> (f64, f64) {
    if frame.len() < stride * height || stride < width * 4 {
        return (0.0, 0.0);
    }
    let (x0, x1, y0, y1) = (width / 4, 3 * width / 4, height / 4, 3 * height / 4);
    let (sx, sy) = (((x1 - x0) / 160).max(2), ((y1 - y0) / 90).max(1));
    let (mut steps, mut light, mut n) = (0u64, 0u64, 0u64);
    for y in (y0..y1).step_by(sy) {
        let row = &frame[y * stride..y * stride + width * 4];
        for x in (x0 + 1..x1).step_by(sx) {
            let g = row[x * 4 + 1] as i32;
            steps += (g - row[(x - 1) * 4 + 1] as i32).unsigned_abs() as u64;
            light += g as u64;
            n += 1;
        }
    }
    if n == 0 {
        return (0.0, 0.0);
    }
    let mean = light as f64 / n as f64;
    (if mean < 1.0 { 0.0 } else { steps as f64 / n as f64 / mean }, mean)
}

/// What the guest wants from the lens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Continuous,
    OneShot,
    Hold,
    Manual(i32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    /// sweeping from `lo` to `hi` in `step`s; the best so far
    Sweep { lo: i32, hi: i32, step: i32, best: (i32, f64), fine: bool },
    /// a search ended: the scene's sharpness and brightness once settled there, how long they're off, whether
    /// the search found a peak
    Found { base: Option<(f64, f64)>, off: u32, ok: bool },
    /// not searching (one-shot before its trigger, hold, manual)
    Idle,
}

/// A laser ranger (IIO distance channel), read in its own thread: the latest distance in meters.
pub struct Ranger {
    latest: Arc<Mutex<Option<(f64, Instant)>>>,
}

impl Ranger {
    /// The first IIO device named `name` with a distance channel; read while the Ranger lives.
    pub fn find(name: &str) -> Option<Self> {
        let dev = std::fs::read_dir("/sys/bus/iio/devices").ok()?.flatten()
            .find(|e| std::fs::read_to_string(e.path().join("name")).is_ok_and(|n| n.trim() == name))?
            .path();
        let raw = dev.join("in_distance_raw");
        let scale: f64 = std::fs::read_to_string(dev.join("in_distance_scale")).ok()?.trim().parse().ok()?;
        let latest = Arc::new(Mutex::new(None));
        let l = latest.clone();
        std::thread::Builder::new().name("ranger".into()).spawn(move || {
            // each read waits for one ranging (50 ms); no target: no value
            while Arc::strong_count(&l) > 1 {
                if let Some(v) = std::fs::read_to_string(&raw).ok().and_then(|s| s.trim().parse::<f64>().ok()) {
                    *l.lock().unwrap() = Some((v * scale, Instant::now()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }).ok()?;
        Some(Self { latest })
    }

    /// The latest valid distance (meters), when recent.
    pub fn distance(&self) -> Option<f64> {
        self.latest.lock().unwrap().filter(|(_, t)| t.elapsed() < Duration::from_secs(1)).map(|(d, _)| d)
    }
}

pub struct Af {
    lens: File,
    path: PathBuf,
    max: i32,
    pos: i32,
    wait: u32,
    want: Want,
    state: State,
    ranger: Option<Ranger>,
    /// the ranger's distance when the current focus was found
    found_at: Option<f64>,
    /// found focus positions against the ranger's distance in diopters
    learned: Vec<(f64, i32)>,
}

impl Af {
    pub fn open(path: &Path, ranger: Option<Ranger>) -> std::io::Result<Self> {
        let lens = File::options().read(true).write(true).open(path)?;
        let mut af = Self {
            lens, path: path.into(), max: 1023, pos: 0, wait: 0, want: Want::Continuous, state: State::Idle,
            ranger, found_at: None, learned: vec![],
        };
        af.search();
        Ok(af)
    }

    fn set(&mut self, pos: i32) {
        let pos = pos.clamp(0, self.max);
        let mut c: [u32; 2] = [CID_FOCUS_ABSOLUTE, pos as u32];
        if unsafe { libc::ioctl(self.lens.as_raw_fd(), VIDIOC_S_CTRL, c.as_mut_ptr()) } == 0 {
            self.pos = pos;
        }
        self.wait = SETTLE;
    }

    /// The lens position the ranger's distance predicts (a line through the remembered focuses, in diopters).
    fn predicted(&self) -> Option<i32> {
        let d = 1.0 / self.ranger.as_ref()?.distance()?.max(0.03);
        if self.learned.len() < 2 {
            return None;
        }
        let n = self.learned.len() as f64;
        let (mx, my) = self.learned.iter().fold((0.0, 0.0), |a, (x, y)| (a.0 + x / n, a.1 + *y as f64 / n));
        let (sxy, sxx) = self.learned.iter().fold((0.0, 0.0), |a, (x, y)| (a.0 + (x - mx) * (*y as f64 - my), a.1 + (x - mx) * (x - mx)));
        if sxx < 1e-6 {
            return None;
        }
        Some((my + sxy / sxx * (d - mx)).round() as i32)
    }

    fn search(&mut self) {
        if let Some(p) = self.predicted() {
            // a fine sweep around where the distance says
            let (lo, hi) = ((p - 2 * COARSE).max(0), (p + 2 * COARSE).min(self.max));
            self.state = State::Sweep { lo, hi, step: FINE * 2, best: (lo, -1.0), fine: true };
            self.set(lo);
        } else {
            self.state = State::Sweep { lo: 0, hi: self.max, step: COARSE, best: (0, -1.0), fine: false };
            self.set(0);
        }
    }

    /// What the guest asks for now.
    pub fn want(&mut self, want: Want) {
        if want == self.want {
            return;
        }
        self.want = want;
        match want {
            Want::Continuous => self.search(),
            // the lens stays: a found focus (a continuous one locked) or wherever it is
            Want::OneShot | Want::Hold => {
                if !matches!(self.state, State::Found { .. }) {
                    self.state = State::Idle;
                }
            }
            Want::Manual(p) => {
                self.state = State::Idle;
                self.set(p);
            }
        }
    }

    /// A guest's autofocus trigger: one search (one-shot mode).
    pub fn trigger(&mut self) {
        if self.want == Want::OneShot {
            self.search();
        }
    }

    /// V4L2_AUTO_FOCUS_STATUS bits.
    pub fn status(&self) -> u32 {
        match self.state {
            State::Sweep { .. } => STATUS_BUSY,
            State::Found { ok: true, .. } => STATUS_REACHED,
            State::Found { ok: false, .. } => STATUS_FAILED,
            State::Idle => 0,
        }
    }

    /// One processed frame.
    /// A frame (BGRx, `stride` bytes per line) seen: the next step.
    #[allow(dead_code)]
    pub fn frame_stride(&mut self, frame: &[u8], width: usize, height: usize, stride: usize) {
        self.frame_with(|| Some(sharpness_stride(frame, width, height, stride)));
    }

    /// A frame seen, its sharpness and light measured by `measure` (only when this step needs them).
    pub fn frame_with(&mut self, measure: impl FnOnce() -> Option<(f64, f64)>) {
        if self.wait > 0 {
            self.wait -= 1;
            return;
        }
        let Some((sharp, light)) = measure() else { return };
        match self.state {
            State::Sweep { lo, hi, step, mut best, fine } => {
                if sharp > best.1 {
                    best = (self.pos, sharp);
                }
                if self.pos < hi {
                    self.state = State::Sweep { lo, hi, step, best, fine };
                    self.set((self.pos + step).min(hi));
                } else if !fine {
                    let (lo, hi) = ((best.0 - COARSE).max(0), (best.0 + COARSE).min(self.max));
                    self.state = State::Sweep { lo, hi, step: FINE, best: (best.0, -1.0), fine: true };
                    self.set(lo);
                } else {
                    let ok = best.1 > 0.0;
                    self.set(best.0);
                    self.state = State::Found { base: None, off: 0, ok };
                    self.found_at = self.ranger.as_ref().and_then(|r| r.distance());
                    if let Some(d) = self.found_at
                        && ok
                    {
                        let diopters = 1.0 / d.max(0.03);
                        self.learned.retain(|(x, _)| (x - diopters).abs() > 0.2);
                        self.learned.push((diopters, best.0));
                        if self.learned.len() > 16 {
                            self.learned.remove(0);
                        }
                    }
                }
            }
            State::Found { base: None, ok, .. } => self.state = State::Found { base: Some((sharp, light)), off: 0, ok },
            State::Found { base: Some((s0, l0)), off, ok } => {
                if self.want != Want::Continuous {
                    return;
                }
                let moved = match (self.found_at, self.ranger.as_ref().and_then(|r| r.distance())) {
                    (Some(a), Some(b)) => (b - a).abs() > a * RANGE_MOVED,
                    _ => false,
                };
                let changed = moved || sharp < s0 * DROP || (light - l0).abs() > l0.max(8.0) * LIGHT;
                let off = if changed { off + 1 } else { 0 };
                if off >= CHANGED_FOR || (moved && off >= 3) {
                    self.search();
                } else {
                    self.state = State::Found { base: Some((s0, l0)), off, ok };
                }
            }
            State::Idle => {}
        }
    }

    pub fn status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "lens": self.path,
            "position": self.pos,
            "mode": format!("{:?}", self.want).to_lowercase(),
            "state": match self.state {
                State::Sweep { fine: false, .. } => "searching",
                State::Sweep { .. } => "refining",
                State::Found { ok: true, .. } => "focused",
                State::Found { ok: false, .. } => "not focused",
                State::Idle => "idle",
            },
            "distance_m": self.ranger.as_ref().and_then(|r| r.distance()),
            "learned": self.learned.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, edge: bool) -> Vec<u8> {
        let mut f = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                f[(y * w + x) * 4 + 1] = if edge { if x % 2 == 0 { 200 } else { 50 } } else { 125 };
            }
        }
        f
    }

    #[test]
    fn contrast() {
        let (s, l) = sharpness(&frame(640, 360, true), 640, 360);
        assert!(s > 1.0 && (l - 125.0).abs() < 80.0, "{s} {l}");
        assert_eq!(sharpness(&frame(640, 360, false), 640, 360).0, 0.0);
    }
}
