//! mship-presenter: the mothershipOS VM display presenter. It shows a VM on a display through a DRM lease.
//!
//! `mship-presenter <socket path>`
//! - MSHIP_VM_OUTPUT=<connector>: the display to lease (default DSI-1, the panel; DP-1 = the USB-C external display)
//! - MSHIP_VM_MODE=<W>x<H>: preferred mode on it (highest refresh of that size), when the display has it
//! - MSHIP_VM_PRESENT=copy: scan out a GPU copy of each guest frame instead of the guest's buffer: a guest whose
//!   flips complete at once (virtio-gpu has no real vblank) overwrites the buffer still being scanned out, which
//!   tears; the copy can't be touched
//!
//! crosvm (`--wayland-sock=<socket path>`) sees a minimal Wayland compositor (compositor, subcompositor, shm,
//! linux-dmabuf, xdg-shell, seat) and draws its virtio-gpu scanout into one toplevel, configured fullscreen at the
//! leased mode's size. The presenter is also a client of sway's wp_drm_lease_device_v1: whenever sway offers the
//! display (`swaymsg lease_output DSI-1 on`, done by mothershipd when the VM gets it) it takes the lease and
//! page-flips the VM's frames on the leased CRTC/plane with atomic KMS, pacing the guest with real vsync (frame
//! callbacks complete on page flip). When sway revokes the lease the display goes back to sway and frames are just
//! dropped. The guest's display power drives the display: SIGUSR1 = off (CRTC off: panel and backlight off, frames
//! dropped), SIGUSR2 = on again (modeset + the current frame); the launcher sends these on Android's display power
//! changes, mothershipd when a VM's mship.power vote says its screen is off or the VM sleeps.
//!
//! Frames: client dmabufs are scanned out directly (or copied, MSHIP_VM_PRESENT=copy), wl_shm frames are copied
//! into dumb buffers. With the gfxstream GL shim the guest's scanout buffers arrive as dmabufs over <socket>-zc
//! ([`zc`]) and crosvm's own frames only pace.

mod kms;
mod lease;
mod zc;

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use drm::control::framebuffer;
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Buffer as _, Fourcc, Modifier};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::{GlesError, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, Frame as _, ImportDma, ImportMemWl, Renderer};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::signals::{Signal, Signals};
use smithay::reexports::calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, ListeningSocket};
use smithay::utils::{Rectangle, Serial, Size, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes, with_states};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf};
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState, XdgToplevelSurfaceData};
use smithay::wayland::shm::{ShmHandler, ShmState, with_buffer_contents};
use smithay::{delegate_compositor, delegate_dmabuf, delegate_seat, delegate_shm, delegate_xdg_shell};

use kms::{Commit, Kms, Planes};

pub fn log(msg: impl AsRef<str>) {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    eprintln!("{:02}:{:02}:{:02} {}", t / 3600 % 24, t / 60 % 60, t % 60, msg.as_ref());
}

/// copy mode: own scanout buffers the guest frames are drawn into
const COPIES: usize = 3;

struct CopySlot {
    _bo: GbmBuffer,
    dmabuf: Dmabuf,
    fb: Option<framebuffer::Handle>,
}

/// What is (or goes) on screen.
enum Scan {
    /// a dumb buffer (copied shm frame) or nothing held
    None,
    /// a client dmabuf scanned out directly (held until replaced)
    Client(WlBuffer, framebuffer::Handle),
    Copy(usize),
    Zc(usize),
}

/// A frame waiting for the flip in progress (the newest wins).
enum Queued {
    Client(WlBuffer),
    Copy(usize),
    Zc(usize),
}

#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

pub struct State {
    dh: DisplayHandle,
    lh: LoopHandle<'static, State>,
    stop: LoopSignal,
    pub output: String,
    want_mode: Option<(u16, u16)>,
    compositor: CompositorState,
    xdg: XdgShellState,
    shm: ShmState,
    dmabuf: DmabufState,
    _dmabuf_global: DmabufGlobal,
    seat_state: SeatState<State>,
    _seat: Seat<State>,
    renderer: GlesRenderer,
    /// copy mode: the allocator and its buffers (at the leased mode's size)
    copy: Option<(GbmAllocator<OwnedFd>, Vec<CopySlot>)>,
    pub lease: lease::Lease,
    pub kms: Option<Kms>,
    kms_token: Option<RegistrationToken>,
    shown: Scan,
    flipping: Scan,
    queued: Option<Queued>,
    /// the newest guest frame that arrived before the lease (the lease follows the guest's first frame, so a boot
    /// splash drawn once would otherwise never be shown): presented when the lease is granted
    early: Option<WlBuffer>,
    /// surfaces waiting for a frame callback
    pending_frame: Vec<WlSurface>,
    pub toplevel: Option<ToplevelSurface>,
    /// the guest's screen is off
    pub panel_off: bool,
    /// the guest blanked its display (crosvm closed its window)
    blanked: bool,
    /// the VM (its directory's name), told to mothershipd with its display state
    vm: Option<String>,
    pub zc: zc::Zc,
    start: Instant,
}

impl State {
    fn send_frame_done(&mut self) {
        let t = self.start.elapsed().as_millis() as u32;
        for s in self.pending_frame.drain(..) {
            with_states(&s, |states| {
                for cb in states.cached_state.get::<SurfaceAttributes>().current().frame_callbacks.drain(..) {
                    cb.done(t);
                }
            });
        }
    }

    fn release(&mut self, s: Scan) {
        match s {
            Scan::Client(b, fb) => {
                if let Some(k) = &self.kms {
                    k.rm_fb(fb);
                }
                b.release();
            }
            Scan::Zc(i) => self.zc.off_screen(i, self.kms.as_ref()),
            Scan::None | Scan::Copy(_) => {}
        }
    }

    /// Is this zero-copy buffer shown or flipping?
    pub fn on_screen_zc(&self, i: usize) -> bool {
        matches!(self.shown, Scan::Zc(j) if j == i) || matches!(self.flipping, Scan::Zc(j) if j == i)
    }

    pub fn unqueue_zc(&mut self, i: usize) {
        if matches!(self.queued, Some(Queued::Zc(j)) if j == i) {
            self.queued = None;
        }
    }

    fn drop_queued(&mut self) {
        if let Some(Queued::Client(b)) = self.queued.take() {
            b.release();
        }
    }

    /// A page flip finished: the previous frame is off screen.
    fn on_flip(&mut self) {
        if let Some(k) = self.kms.as_mut() {
            k.flip_pending = false;
        }
        let old = std::mem::replace(&mut self.shown, std::mem::replace(&mut self.flipping, Scan::None));
        self.release(old);
        self.zc.stats.flip();
        self.send_frame_done();
        match self.queued.take() {
            Some(Queued::Client(b)) => self.present_buffer(b),
            Some(Queued::Copy(i)) => self.present_copy(i),
            Some(Queued::Zc(i)) => self.present_zc(i),
            None => {}
        }
    }

    fn commit(&mut self, fb: framebuffer::Handle, w: u32, h: u32) -> bool {
        if self.panel_off {
            return false;
        }
        let Some(k) = self.kms.as_mut() else { return false };
        match k.commit(fb, w, h) {
            Commit::Ok => true,
            Commit::Failed => false,
            Commit::Drop => {
                // the display came back after an unplug: give the lease back, once this frame is done with its buffers
                self.lh.insert_idle(|st| {
                    if st.lease.lease.is_some() {
                        log("lease dropped: commits keep failing");
                        st.lease_finished();
                    }
                });
                false
            }
        }
    }

    /// Show one client buffer (it is released once off screen, or now if it cannot be shown).
    fn present_buffer(&mut self, b: WlBuffer) {
        let Some(k) = self.kms.as_ref() else { return b.release() };
        if self.panel_off {
            return b.release();
        }
        if k.flip_pending {
            if self.queued.is_some() {
                self.zc.stats.dropped += 1;
            }
            self.drop_queued();
            self.queued = Some(Queued::Client(b));
            return;
        }
        if let Ok(d) = get_dmabuf(&b) {
            // zero copy: scan the guest's buffer out directly
            let planes = planes_of(d);
            let (w, h) = planes.size;
            self.zc.log_path(&format!("frames: dmabuf {w}x{h} format {:?} modifier {:#x} (zero copy)", d.format().code, planes.modifier));
            match k.add_fb(&planes) {
                Ok(fb) => {
                    if self.commit(fb, w, h) {
                        self.flipping = Scan::Client(b, fb);
                    } else {
                        if let Some(k) = &self.kms {
                            k.rm_fb(fb);
                        }
                        b.release();
                    }
                }
                Err(e) => {
                    self.zc.warn_once(&format!("kms: dmabuf not scanout-capable ({e})"));
                    b.release();
                }
            }
            return;
        }
        // shm: one copy into a dumb buffer
        let k = self.kms.as_mut().unwrap();
        let fb = with_buffer_contents(&b, |ptr, len, d| {
            let data = unsafe { std::slice::from_raw_parts(ptr.add(d.offset as usize), len - d.offset as usize) };
            k.copy(data, d.width as u32, d.height as u32, d.stride as usize)
        })
        .ok()
        .flatten();
        b.release();
        self.zc.log_path("frames: shm (one copy)");
        if let Some(fb) = fb {
            let (w, h) = self.kms.as_ref().unwrap().size();
            if self.commit(fb, w, h) {
                self.flipping = Scan::None;
            }
        }
    }

    fn present_copy(&mut self, i: usize) {
        let Some(k) = self.kms.as_ref() else { return };
        if self.panel_off {
            return;
        }
        if k.flip_pending {
            if self.queued.is_some() {
                self.zc.stats.dropped += 1;
            }
            self.drop_queued();
            self.queued = Some(Queued::Copy(i));
            return;
        }
        let (w, h) = k.size();
        let Some((_, slots)) = self.copy.as_mut() else { return };
        if slots[i].fb.is_none() {
            slots[i].fb = k.add_fb(&planes_of(&slots[i].dmabuf)).map_err(|e| log(format!("copy: addfb: {e}"))).ok();
        }
        if let Some(fb) = slots[i].fb
            && self.commit(fb, w, h)
        {
            self.flipping = Scan::Copy(i);
        }
    }

    pub fn present_zc(&mut self, i: usize) {
        let Some(k) = self.kms.as_ref() else { return };
        if self.zc.dead(i) || self.panel_off {
            return;
        }
        if k.flip_pending {
            if matches!(self.queued, Some(Queued::Zc(_))) {
                self.zc.stats.dropped += 1;
            }
            self.drop_queued();
            self.queued = Some(Queued::Zc(i));
            return;
        }
        let Some((fb, w, h)) = self.zc.fb(i, k) else { return };
        if self.commit(fb, w, h) {
            self.flipping = Scan::Zc(i);
        }
    }

    /// copy mode: the guest frame drawn by the GPU into one of our scanout buffers (taken now: the guest may reuse
    /// its buffer right away); None if no buffer is free or the copy failed
    fn copy_frame(&mut self, b: &WlBuffer) -> Option<usize> {
        let (w, h) = self.kms.as_ref()?.size();
        let (alloc, slots) = self.copy.as_mut()?;
        if slots.first().is_some_and(|s| s.dmabuf.size() != Size::from((w as i32, h as i32))) {
            slots.clear();
        }
        if slots.is_empty() {
            for _ in 0..COPIES {
                // the display reads linear XRGB at any size
                let bo = alloc.create_buffer_with_flags(w, h, Fourcc::Xrgb8888, &[Modifier::Linear], GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT).ok()?;
                let dmabuf = bo.export().ok()?;
                slots.push(CopySlot { _bo: bo, dmabuf, fb: None });
            }
            log(format!("copy: guest frames are copied into own {w}x{h} scanout buffers"));
        }
        // shown, flipping or queued: busy
        let mut used = vec![];
        for s in [&self.shown, &self.flipping] {
            if let Scan::Copy(j) = s {
                used.push(*j);
            }
        }
        let queued = match &self.queued {
            Some(Queued::Copy(j)) => Some(*j),
            _ => None,
        };
        // a free slot, else the queued copy's (not on screen yet: this newer frame replaces it)
        let i = match (0..slots.len()).find(|i| !used.contains(i) && Some(*i) != queued) {
            Some(i) => i,
            None => match queued {
                Some(j) => {
                    self.queued = None;
                    self.zc.stats.dropped += 1;
                    j
                }
                None => {
                    self.zc.stats.no_slot += 1;
                    return None;
                }
            },
        };
        let t0 = Instant::now();
        // the guest frame as a texture; crosvm passes native-context buffers without a modifier, and EGL won't
        // import those: they are linear (the guest KMS only offers linear)
        let tex = match get_dmabuf(b) {
            Ok(d) if matches!(d.format().modifier, Modifier::Invalid | Modifier::Linear) => import_linear(&mut self.renderer, d),
            Ok(d) => self.renderer.import_dmabuf(d, None),
            Err(_) => self.renderer.import_shm_buffer(b, None, &[]),
        }
        .map_err(|e| log(format!("copy: import: {e}")))
        .ok()?;
        let mut target = slots[i].dmabuf.clone();
        let size = Size::from((w as i32, h as i32));
        let mut fb = self.renderer.bind(&mut target).ok()?;
        let tex_size = smithay::backend::renderer::Texture::size(&tex);
        let sync = (|| {
            let mut frame = self.renderer.render(&mut fb, size, Transform::Normal)?;
            smithay::backend::renderer::Frame::render_texture_from_to(
                &mut frame, &tex, Rectangle::from_size(tex_size.to_f64()), Rectangle::from_size(size), &[Rectangle::from_size(size)], &[],
                Transform::Normal, 1.0,
            )?;
            frame.finish()
        })()
        .map_err(|e| log(format!("copy: render: {e}")))
        .ok()?;
        let _ = sync.wait();
        let us = t0.elapsed().as_micros() as u64;
        self.zc.stats.copies += 1;
        self.zc.stats.copy_us += us;
        self.zc.stats.copy_max_us = self.zc.stats.copy_max_us.max(us);
        Some(i)
    }

    /// The VM committed a surface.
    fn on_commit(&mut self, s: &WlSurface) {
        if let Some(tl) = self.toplevel.as_ref().filter(|t| t.wl_surface() == s && !t.is_initial_configure_sent()) {
            // the VM's toplevel: fullscreen at the leased mode's size (no client-side title bar or shadow)
            let size = self.kms.as_ref().map_or((1080, 2520), |k| k.size());
            configure(tl, size);
        }
        let buffer = with_states(s, |states| match states.cached_state.get::<SurfaceAttributes>().current().buffer.take() {
            Some(BufferAssignment::NewBuffer(b)) => Some(b),
            _ => None,
        });
        if self.pending_frame.len() < 8 {
            self.pending_frame.push(s.clone());
        }
        self.zc.stats.commits += 1;
        let not_flipping = self.kms.as_ref().is_none_or(|k| !k.flip_pending);
        if self.zc.active {
            // the frame itself arrived (or arrives) as a dmabuf from the shim
            if let Some(b) = buffer {
                b.release();
            }
            if not_flipping {
                self.send_frame_done();
            }
            return;
        }
        // the VM's frame (not a cursor or popup): about the leased mode's size
        let big = |b: &WlBuffer| {
            let (bw, bh) = buffer_size(b);
            self.kms.as_ref().is_some_and(|k| {
                let (w, h) = k.size();
                bw >= w * 3 / 4 && bh >= h * 3 / 4
            })
        };
        if let Some(b) = &buffer {
            let (bw, bh) = buffer_size(b);
            let kind = if get_dmabuf(b).is_ok() { "dmabuf" } else { "shm" };
            let shown = if self.kms.is_none() { "not shown: no lease" } else if big(b) { "shown" } else { "not shown: smaller than the display" };
            self.zc.log_frame_kind(format!("{kind} {bw}x{bh}, {shown}"));
        }
        match buffer {
            Some(b) if big(&b) => {
                if self.copy.is_some()
                    && let Some(i) = self.copy_frame(&b)
                {
                    b.release();
                    return self.present_copy(i);
                }
                self.present_buffer(b); // frame callbacks complete on page flip
            }
            Some(b) if self.kms.is_none() => {
                if let Some(old) = self.early.replace(b) {
                    old.release();
                }
                if not_flipping {
                    self.send_frame_done();
                }
            }
            other => {
                if let Some(b) = other {
                    b.release();
                }
                if not_flipping {
                    self.send_frame_done(); // not on screen: let the guest keep rendering
                }
            }
        }
    }

    /// A lease fd from sway: KMS on it, the newest frame on screen.
    pub fn lease_granted(&mut self, fd: OwnedFd) {
        log(format!("lease granted (fd {})", fd.as_raw_fd()));
        match Kms::open(fd, self.want_mode) {
            Ok(k) => {
                let src = Generic::new(k.card.0.try_clone().expect("dup lease fd"), Interest::READ, Mode::Level);
                self.kms_token = self
                    .lh
                    .insert_source(src, |_, _, st: &mut State| {
                        let n = st.kms.as_ref().map_or(0, |k| k.flips());
                        for _ in 0..n.min(1) {
                            st.on_flip();
                        }
                        Ok(PostAction::Continue)
                    })
                    .ok();
                self.kms = Some(k);
                let early = self.early.take();
                if self.panel_off {
                    self.kms.as_mut().unwrap().power_off();
                    if let Some(b) = early {
                        b.release();
                    }
                } else if let Some(i) = self.zc.last {
                    self.present_zc(i); // the newest guest frame right away
                } else if let Some(b) = early {
                    // the frame drawn before the lease (a boot splash): on screen now
                    if self.copy.is_some()
                        && let Some(i) = self.copy_frame(&b)
                    {
                        b.release();
                        self.present_copy(i);
                    } else {
                        self.present_buffer(b);
                    }
                }
                if let (Some(tl), Some(k)) = (&self.toplevel, &self.kms) {
                    configure(tl, k.size()); // the VM's window follows the leased display's mode
                }
            }
            Err(e) => {
                log(e);
                self.lease.destroy();
            }
        }
    }

    pub fn lease_finished(&mut self) {
        log(format!("lease finished: {} back with sway", self.output));
        self.kms_close();
        self.lease.destroy();
    }

    pub fn kms_close(&mut self) {
        if let Some(t) = self.kms_token.take() {
            self.lh.remove(t);
        }
        let shown = std::mem::replace(&mut self.shown, Scan::None);
        let flipping = std::mem::replace(&mut self.flipping, Scan::None);
        self.release(shown);
        self.release(flipping);
        self.drop_queued();
        if let Some((_, slots)) = self.copy.as_mut() {
            for s in slots {
                s.fb = None;
            }
        }
        self.zc.forget_fbs();
        if let Some(k) = self.kms.take() {
            k.close();
        }
        self.send_frame_done();
    }

    /// mothershipd builds the power vote of a VM without a guest adapter from its display state.
    fn tell_display(&self, on: bool) {
        let Some(vm) = self.vm.clone() else { return };
        std::thread::spawn(move || {
            if let Err(e) = mship_core::client::call(mship_core::api::Op::VmDisplay { vm, on }) {
                log(format!("mothershipd: vm.display: {e}"));
            }
        });
    }

    /// The guest's display power: CRTC off (panel + backlight off) / back on with the current frame.
    fn power(&mut self, off: bool) {
        if off == self.panel_off || (!off && self.blanked) {
            return; // (on again once the guest shows a frame: a new window)
        }
        log(format!("guest display {}", if off { "off: panel off" } else { "on: panel on" }));
        if off {
            self.drop_queued();
            if let Some(k) = self.kms.as_mut() {
                k.power_off();
            }
            // the frame in flight is not shown: give it back with the rest
            let flipping = std::mem::replace(&mut self.flipping, Scan::None);
            self.release(flipping);
            self.panel_off = true;
            self.send_frame_done();
            return;
        }
        self.panel_off = false;
        if self.kms.is_none() {
            return;
        }
        if let Some(i) = self.zc.last {
            return self.present_zc(i);
        }
        // the last frame again at once: a guest that only stopped blanking (no damage) may not send a new one
        match std::mem::replace(&mut self.shown, Scan::None) {
            Scan::Client(b, fb) => {
                let (w, h) = buffer_size(&b);
                if self.commit(fb, w, h) {
                    self.flipping = Scan::Client(b, fb);
                } else {
                    self.release(Scan::Client(b, fb));
                }
            }
            Scan::Copy(i) => self.present_copy(i),
            other => self.shown = other,
        }
    }
}

fn configure(tl: &ToplevelSurface, (w, h): (u32, u32)) {
    tl.with_pending_state(|s| {
        s.size = Some((w as i32, h as i32).into());
        s.states.set(xdg_toplevel::State::Fullscreen);
        s.states.set(xdg_toplevel::State::Activated);
    });
    tl.send_configure();
}

fn buffer_size(b: &WlBuffer) -> (u32, u32) {
    if let Ok(d) = get_dmabuf(b) {
        return (d.width(), d.height());
    }
    with_buffer_contents(b, |_, _, d| (d.width as u32, d.height as u32)).unwrap_or((0, 0))
}

fn planes_of(d: &Dmabuf) -> Planes {
    Planes {
        size: (d.width(), d.height()),
        format: d.format().code as u32,
        modifier: u64::from(d.format().modifier),
        fds: d
            .handles()
            .zip(d.offsets())
            .zip(d.strides())
            .filter_map(|((fd, o), s)| Some((fd.try_clone_to_owned().ok()?, o, s)))
            .collect(),
    }
}

// ------------------------------------------------------------------ the compositor crosvm sees
impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.on_commit(surface);
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let title = with_states(surface.wl_surface(), |s| {
            s.data_map.get::<XdgToplevelSurfaceData>().and_then(|d| d.lock().unwrap().title.clone())
        });
        log(format!("crosvm window: {}", title.as_deref().unwrap_or("(untitled)"))); // the launcher waits for this line
        self.toplevel = Some(surface);
        if self.blanked {
            self.blanked = false;
            self.power(false);
        }
        self.tell_display(true);
    }

    /// crosvm closes its window when the guest disables its scanout (it blanked its screen): the output goes off
    /// until the guest shows a frame again (a new window).
    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if self.toplevel.as_ref() == Some(&surface) {
            if let Some(b) = self.early.take() {
                b.release();
            }
            self.toplevel = None;
            self.blanked = true;
            self.power(true);
            self.tell_display(false);
        }
    }

    fn new_popup(&mut self, _: PopupSurface, _: PositionerState) {}

    fn grab(&mut self, _: PopupSurface, _: WlSeat, _: Serial) {}

    fn reposition_request(&mut self, _: PopupSurface, _: PositionerState, _: u32) {}
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _: &WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }

    fn dmabuf_imported(&mut self, _: &DmabufGlobal, _: Dmabuf, notifier: ImportNotifier) {
        let _ = notifier.successful::<State>();
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }
}

delegate_compositor!(State);
delegate_xdg_shell!(State);
delegate_shm!(State);
delegate_dmabuf!(State);
delegate_seat!(State);

fn render_node() -> Option<PathBuf> {
    let mut n: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|f| f.to_string_lossy().starts_with("renderD")))
        .collect();
    n.sort();
    n.into_iter().next()
}

fn run(sock: PathBuf) -> Result<(), String> {
    let output = std::env::var("MSHIP_VM_OUTPUT").ok().filter(|o| !o.is_empty()).unwrap_or_else(|| "DSI-1".into());
    let want_mode = std::env::var("MSHIP_VM_MODE").ok().and_then(|m| {
        let (w, h) = m.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });
    let copy_mode = std::env::var("MSHIP_VM_PRESENT").is_ok_and(|p| p == "copy");

    let mut ev: EventLoop<'static, State> = EventLoop::try_new().map_err(|e| e.to_string())?;
    let display: Display<State> = Display::new().map_err(|e| e.to_string())?;
    let dh = display.handle();

    // the GPU (render node): linux-dmabuf formats, and the copies
    let node = render_node().ok_or("no render node")?;
    let gpu: OwnedFd = std::fs::OpenOptions::new().read(true).write(true).open(&node).map_err(|e| format!("{}: {e}", node.display()))?.into();
    let gbm = |fd: &OwnedFd| -> Result<GbmDevice<OwnedFd>, String> {
        GbmDevice::new(fd.try_clone().map_err(|e| e.to_string())?).map_err(|e| format!("gbm: {e}"))
    };
    let egl = unsafe { EGLDisplay::new(gbm(&gpu)?) }.map_err(|e| format!("egl: {e}"))?;
    let renderer = unsafe { GlesRenderer::new(EGLContext::new(&egl).map_err(|e| format!("egl context: {e}"))?) }.map_err(|e| format!("gles: {e}"))?;
    let copy = if copy_mode { Some((GbmAllocator::new(gbm(&gpu)?, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT), vec![])) } else { None };

    let mut dmabuf = DmabufState::new();
    let dmabuf_global = dmabuf.create_global::<State>(&dh, renderer.dmabuf_formats());
    let mut seat_state = SeatState::new();
    let seat = seat_state.new_wl_seat(&dh, "seat0");

    // sway: its drm lease device
    let (lease, queue) = lease::connect().map_err(|e| format!("cannot connect to sway: {e}"))?;
    let mut st = State {
        dh: dh.clone(),
        lh: ev.handle(),
        stop: ev.get_signal(),
        output: output.clone(),
        want_mode,
        compositor: CompositorState::new::<State>(&dh),
        xdg: XdgShellState::new::<State>(&dh),
        shm: ShmState::new::<State>(&dh, vec![]),
        dmabuf,
        _dmabuf_global: dmabuf_global,
        seat_state,
        _seat: seat,
        renderer,
        copy,
        lease,
        kms: None,
        kms_token: None,
        shown: Scan::None,
        flipping: Scan::None,
        queued: None,
        early: None,
        pending_frame: vec![],
        toplevel: None,
        panel_off: false,
        blanked: false,
        vm: std::env::var_os("MSHIP_VM_DIR").and_then(|d| PathBuf::from(d).file_name().map(|n| n.to_string_lossy().into_owned())),
        zc: zc::Zc::new(std::env::var_os("MSHIP_VM_DISPLAY_STATS").is_some()),
        start: Instant::now(),
    };
    lease::start(&mut st, queue)?;

    // crosvm's socket
    let _ = std::fs::remove_file(&sock);
    let listener = ListeningSocket::bind_absolute(sock.clone()).map_err(|e| format!("cannot listen on {}: {e}", sock.display()))?;
    ev.handle()
        .insert_source(Generic::new(listener, Interest::READ, Mode::Level), |_, l, st: &mut State| {
            while let Ok(Some(stream)) = l.accept() {
                let _ = st.dh.insert_client(stream, Arc::new(ClientState::default()));
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| e.to_string())?;
    ev.handle()
        .insert_source(Generic::new(display, Interest::READ, Mode::Level), |_, d, st: &mut State| {
            unsafe {
                let _ = d.get_mut().dispatch_clients(st);
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| e.to_string())?;
    zc::listen(&mut st, &format!("{}-zc", sock.display()));
    ev.handle()
        .insert_source(Signals::new(&[Signal::SIGUSR1, Signal::SIGUSR2, Signal::SIGTERM, Signal::SIGINT]).map_err(|e| e.to_string())?, |e, _, st: &mut State| {
            match e.signal() {
                // the guest's own display power (Android's power mode): mothershipd hears of it too, as of a blank
                // (its power vote, the VM pool's share)
                Signal::SIGUSR1 => {
                    st.power(true);
                    st.tell_display(false);
                }
                Signal::SIGUSR2 => {
                    st.power(false);
                    st.tell_display(true);
                }
                _ => st.stop.stop(),
            }
        })
        .map_err(|e| e.to_string())?;
    log(format!("presenting on {output} via DRM lease; crosvm socket {}", sock.display()));
    ev.run(Some(Duration::from_secs(1)), &mut st, |st| {
        let _ = st.dh.flush_clients();
        st.lease.flush();
        if st.lease.lost {
            log("sway connection lost");
            st.stop.stop();
        }
    })
    .map_err(|e| e.to_string())?;
    st.kms_close();
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(format!("{}-zc", sock.display()));
    Ok(())
}

fn main() -> std::process::ExitCode {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 2 {
        eprintln!("usage: mship-presenter <socket path>");
        return std::process::ExitCode::from(2);
    }
    match run(PathBuf::from(&a[1])) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            log(e);
            std::process::ExitCode::FAILURE
        }
    }
}

/// A linear dmabuf as a texture, with the modifier stated: smithay leaves LINEAR out of the EGL attributes (it is
/// "no modifier" to it), and Mesa then cannot import the guest's buffers.
fn import_linear(renderer: &mut GlesRenderer, d: &Dmabuf) -> Result<GlesTexture, GlesError> {
    use smithay::backend::egl::ffi::egl;
    use smithay::backend::renderer::gles::ffi;
    const PLANES: [[u32; 5]; 4] = [
        [egl::DMA_BUF_PLANE0_FD_EXT, egl::DMA_BUF_PLANE0_OFFSET_EXT, egl::DMA_BUF_PLANE0_PITCH_EXT, egl::DMA_BUF_PLANE0_MODIFIER_LO_EXT, egl::DMA_BUF_PLANE0_MODIFIER_HI_EXT],
        [egl::DMA_BUF_PLANE1_FD_EXT, egl::DMA_BUF_PLANE1_OFFSET_EXT, egl::DMA_BUF_PLANE1_PITCH_EXT, egl::DMA_BUF_PLANE1_MODIFIER_LO_EXT, egl::DMA_BUF_PLANE1_MODIFIER_HI_EXT],
        [egl::DMA_BUF_PLANE2_FD_EXT, egl::DMA_BUF_PLANE2_OFFSET_EXT, egl::DMA_BUF_PLANE2_PITCH_EXT, egl::DMA_BUF_PLANE2_MODIFIER_LO_EXT, egl::DMA_BUF_PLANE2_MODIFIER_HI_EXT],
        [egl::DMA_BUF_PLANE3_FD_EXT, egl::DMA_BUF_PLANE3_OFFSET_EXT, egl::DMA_BUF_PLANE3_PITCH_EXT, egl::DMA_BUF_PLANE3_MODIFIER_LO_EXT, egl::DMA_BUF_PLANE3_MODIFIER_HI_EXT],
    ];
    let mut attrs = vec![egl::WIDTH as i32, d.width() as i32, egl::HEIGHT as i32, d.height() as i32, egl::LINUX_DRM_FOURCC_EXT as i32, d.format().code as i32];
    for (n, ((fd, off), stride)) in PLANES.iter().zip(d.handles().zip(d.offsets()).zip(d.strides())) {
        // DRM_FORMAT_MOD_LINEAR = 0 (both halves)
        attrs.extend([n[0] as i32, fd.as_raw_fd(), n[1] as i32, off as i32, n[2] as i32, stride as i32, n[3] as i32, 0, n[4] as i32, 0]);
    }
    attrs.push(egl::NONE as i32);
    let dpy = renderer.egl_context().display().get_display_handle();
    let image = unsafe { egl::CreateImageKHR(**dpy, egl::NO_CONTEXT, egl::LINUX_DMA_BUF_EXT, std::ptr::null(), attrs.as_ptr()) };
    if image == egl::NO_IMAGE_KHR {
        return Err(GlesError::BindBufferEGLError(smithay::backend::egl::Error::EGLImageCreationFailed));
    }
    let tex = renderer.with_context(|gl| unsafe {
        let mut tex = 0;
        gl.GenTextures(1, &mut tex);
        gl.BindTexture(ffi::TEXTURE_2D, tex);
        gl.EGLImageTargetTexture2DOES(ffi::TEXTURE_2D, image);
        gl.BindTexture(ffi::TEXTURE_2D, 0);
        tex
    });
    // the texture keeps the buffer
    unsafe { egl::DestroyImageKHR(**dpy, image) };
    Ok(unsafe { GlesTexture::from_raw(renderer, Some(ffi::RGBA8), true, tex?, d.size()) })
}
