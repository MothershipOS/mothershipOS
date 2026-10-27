//! Atomic KMS on the leased display: one connector, its CRTC and primary plane.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use drm::buffer::{DrmFourcc, DrmModifier, PlanarBuffer};
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{AtomicCommitFlags, Device as _, Mode, ResourceHandle, atomic, connector, crtc, framebuffer, plane, property};
use drm::{ClientCapability, Device as _};

use crate::log;

/// dumb buffers for copied (shm) frames
const NBUF: usize = 3;
const PLANE_TYPE_PRIMARY: u64 = 1;

pub struct Card(pub OwnedFd);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl drm::control::Device for Card {}

/// A dmabuf as a framebuffer source.
pub struct Planes {
    pub size: (u32, u32),
    pub format: u32,
    pub modifier: u64,
    pub fds: Vec<(OwnedFd, u32, u32)>,
}

struct Spec {
    size: (u32, u32),
    format: DrmFourcc,
    modifier: DrmModifier,
    handles: [Option<drm::buffer::Handle>; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
}

impl PlanarBuffer for Spec {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> DrmFourcc {
        self.format
    }
    fn modifier(&self) -> Option<DrmModifier> {
        Some(self.modifier)
    }
    fn pitches(&self) -> [u32; 4] {
        self.pitches
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        self.handles
    }
    fn offsets(&self) -> [u32; 4] {
        self.offsets
    }
}

struct Props {
    fb: property::Handle,
    crtc: property::Handle,
    src_x: property::Handle,
    src_y: property::Handle,
    src_w: property::Handle,
    src_h: property::Handle,
    crtc_x: property::Handle,
    crtc_y: property::Handle,
    crtc_w: property::Handle,
    crtc_h: property::Handle,
    active: property::Handle,
    mode_id: property::Handle,
    conn_crtc: property::Handle,
}

pub enum Commit {
    Ok,
    Failed,
    /// commits keep failing: give the lease back
    Drop,
}

pub struct Kms {
    pub card: Card,
    conn: connector::Handle,
    crtc: crtc::Handle,
    plane: plane::Handle,
    pub mode: Mode,
    mode_blob: u64,
    props: Props,
    dumb: Vec<(DumbBuffer, framebuffer::Handle)>,
    next: usize,
    pub flip_pending: bool,
    modeset_done: bool,
    fails: u32,
}

fn prop(card: &Card, h: impl ResourceHandle, name: &str) -> io::Result<(property::Handle, u64)> {
    let props = card.get_properties(h)?;
    let (ids, vals) = props.as_props_and_values();
    for (id, v) in ids.iter().zip(vals) {
        if card.get_property(*id)?.name().to_bytes() == name.as_bytes() {
            return Ok((*id, *v));
        }
    }
    Err(io::Error::other(format!("no property {name}")))
}

fn raw(v: u64) -> property::Value<'static> {
    property::Value::UnsignedRange(v)
}

impl Kms {
    pub fn open(fd: OwnedFd, want: Option<(u16, u16)>) -> Result<Kms, String> {
        let card = Card(fd);
        fn e(what: &'static str) -> impl Fn(io::Error) -> String {
            move |err| format!("lease: {what}: {err}")
        }
        let _ = card.set_client_capability(ClientCapability::UniversalPlanes, true);
        card.set_client_capability(ClientCapability::Atomic, true).map_err(e("no atomic KMS"))?;
        let res = card.resource_handles().map_err(e("resources"))?;
        let (Some(&conn), Some(&crtc)) = (res.connectors().first(), res.crtcs().first()) else { return Err("lease: missing objects".into()) };
        let plane = card
            .plane_handles()
            .map_err(e("planes"))?
            .into_iter()
            .find(|p| prop(&card, *p, "type").is_ok_and(|(_, v)| v == PLANE_TYPE_PRIMARY))
            .ok_or("lease: no primary plane")?;
        let c = card.get_connector(conn, false).map_err(e("connector"))?;
        let modes = c.modes();
        if modes.is_empty() {
            return Err("lease: no connector mode".into());
        }
        // MSHIP_VM_MODE if the display has it, else the display's preferred mode (the size the launcher gives the
        // guest's display); then the highest refresh of that size
        let mut mode = modes.iter().find(|m| m.mode_type().contains(drm::control::ModeTypeFlags::PREFERRED)).copied().unwrap_or(modes[0]);
        if let Some(m) = want.and_then(|w| modes.iter().find(|m| m.size() == w)) {
            mode = *m;
        }
        for m in modes {
            if m.size() == mode.size() && m.vrefresh() > mode.vrefresh() {
                mode = *m;
            }
        }
        let mode_blob = match card.create_property_blob(&mode).map_err(e("mode blob"))? {
            property::Value::Blob(b) => b,
            _ => return Err("lease: mode blob".into()),
        };
        let p = |h: plane::Handle, n: &str| prop(&card, h, n).map(|x| x.0).map_err(|err| format!("lease: {err}"));
        let props = Props {
            fb: p(plane, "FB_ID")?,
            crtc: p(plane, "CRTC_ID")?,
            src_x: p(plane, "SRC_X")?,
            src_y: p(plane, "SRC_Y")?,
            src_w: p(plane, "SRC_W")?,
            src_h: p(plane, "SRC_H")?,
            crtc_x: p(plane, "CRTC_X")?,
            crtc_y: p(plane, "CRTC_Y")?,
            crtc_w: p(plane, "CRTC_W")?,
            crtc_h: p(plane, "CRTC_H")?,
            active: prop(&card, crtc, "ACTIVE").map_err(|err| format!("lease: {err}"))?.0,
            mode_id: prop(&card, crtc, "MODE_ID").map_err(|err| format!("lease: {err}"))?.0,
            conn_crtc: prop(&card, conn, "CRTC_ID").map_err(|err| format!("lease: {err}"))?.0,
        };
        let (w, h) = mode.size();
        log(format!("lease: connector {conn:?} crtc {crtc:?} plane {plane:?}, mode {w}x{h}@{}", mode.vrefresh()));
        let mut dumb = vec![];
        for _ in 0..NBUF {
            let mut db = card.create_dumb_buffer((w as u32, h as u32), DrmFourcc::Xrgb8888, 32).map_err(e("dumb buffer"))?;
            card.map_dumb_buffer(&mut db).map_err(e("map"))?.as_mut().fill(0);
            let fb = card.add_framebuffer(&db, 24, 32).map_err(e("addfb"))?;
            dumb.push((db, fb));
        }
        Ok(Kms { card, conn, crtc, plane, mode, mode_blob, props, dumb, next: 0, flip_pending: false, modeset_done: false, fails: 0 })
    }

    pub fn size(&self) -> (u32, u32) {
        let (w, h) = self.mode.size();
        (w as u32, h as u32)
    }

    /// Page-flip `fb` (w x h, scaled to the mode); the first commit (and after a power off) sets the mode.
    pub fn commit(&mut self, fb: framebuffer::Handle, w: u32, h: u32) -> Commit {
        let (mw, mh) = self.size();
        let pr = &self.props;
        let mut req = atomic::AtomicModeReq::new();
        let mut flags = AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK;
        if !self.modeset_done {
            req.add_property(self.crtc, pr.mode_id, property::Value::Blob(self.mode_blob));
            req.add_property(self.crtc, pr.active, raw(1));
            req.add_property(self.conn, pr.conn_crtc, raw(u32::from(self.crtc) as u64));
            flags |= AtomicCommitFlags::ALLOW_MODESET;
        }
        req.add_property(self.plane, pr.fb, raw(u32::from(fb) as u64));
        req.add_property(self.plane, pr.crtc, raw(u32::from(self.crtc) as u64));
        req.add_property(self.plane, pr.src_x, raw(0));
        req.add_property(self.plane, pr.src_y, raw(0));
        req.add_property(self.plane, pr.src_w, raw((w as u64) << 16));
        req.add_property(self.plane, pr.src_h, raw((h as u64) << 16));
        req.add_property(self.plane, pr.crtc_x, raw(0));
        req.add_property(self.plane, pr.crtc_y, raw(0));
        req.add_property(self.plane, pr.crtc_w, raw(mw as u64));
        req.add_property(self.plane, pr.crtc_h, raw(mh as u64));
        if let Err(e) = self.card.atomic_commit(flags, req) {
            // the display was unplugged and plugged in again (or reset): first a full modeset again, then the lease
            // goes back so a fresh one starts from scratch (the launcher offers the display again)
            self.fails += 1;
            if self.fails < 3 || self.fails.is_multiple_of(100) {
                log(format!("kms: commit failed: {e}"));
            }
            if self.modeset_done {
                self.modeset_done = false;
            } else if self.fails >= 5 {
                self.fails = 0;
                return Commit::Drop;
            }
            return Commit::Failed;
        }
        self.fails = 0;
        self.modeset_done = true;
        self.flip_pending = true;
        Commit::Ok
    }

    /// A dmabuf as a framebuffer on the lease (no modifier: linear; the display rejects implicit ones).
    pub fn add_fb(&self, p: &Planes) -> io::Result<framebuffer::Handle> {
        let format = DrmFourcc::try_from(p.format).map_err(|_| io::Error::other(format!("format {:#x}", p.format)))?;
        let modifier = DrmModifier::from(if p.modifier == u64::from(DrmModifier::Invalid) { u64::from(DrmModifier::Linear) } else { p.modifier });
        let mut spec = Spec { size: p.size, format, modifier, handles: [None; 4], pitches: [0; 4], offsets: [0; 4] };
        for (i, (fd, offset, stride)) in p.fds.iter().enumerate().take(4) {
            spec.handles[i] = Some(self.card.prime_fd_to_buffer(fd.as_fd())?);
            spec.offsets[i] = *offset;
            spec.pitches[i] = *stride;
        }
        let r = self.card.add_planar_framebuffer(&spec, drm::control::FbCmd2Flags::MODIFIERS);
        // the framebuffer holds its own reference to the buffer
        for h in spec.handles.iter().flatten() {
            let _ = self.card.close_buffer(*h);
        }
        r
    }

    /// Copy a CPU frame (XRGB/ARGB rows) into the next dumb buffer; its framebuffer.
    pub fn copy(&mut self, data: &[u8], w: u32, h: u32, stride: usize) -> Option<framebuffer::Handle> {
        let (mw, mh) = self.size();
        let (w, h) = (w.min(mw) as usize, h.min(mh) as usize);
        let i = self.next;
        self.next = (self.next + 1) % NBUF;
        let fb = self.dumb[i].1;
        let mut m = self.card.map_dumb_buffer(&mut self.dumb[i].0).ok()?;
        let pitch = m.as_ref().len() / mh as usize;
        let dst = m.as_mut();
        for y in 0..h {
            let s = &data.get(y * stride..y * stride + w * 4)?;
            dst[y * pitch..y * pitch + w * 4].copy_from_slice(s);
        }
        Some(fb)
    }

    /// Android's screen off: CRTC off (panel + backlight off); the next frame turns it on again.
    pub fn power_off(&mut self) {
        let pr = &self.props;
        let mut req = atomic::AtomicModeReq::new();
        req.add_property(self.plane, pr.fb, raw(0));
        req.add_property(self.plane, pr.crtc, raw(0));
        req.add_property(self.crtc, pr.active, raw(0));
        // blocking: after any flip
        if let Err(e) = self.card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req) {
            log(format!("kms: panel off failed: {e}"));
        }
        self.modeset_done = false;
        self.flip_pending = false;
    }

    /// Before giving the lease back: the pipe off (plane, CRTC with no mode, connector). Left lit, it keeps its
    /// display resources after the lease ends and the display's next modeset by sway fails. Harmless when the
    /// compositor has already revoked the lease.
    pub fn close(self) {
        let pr = &self.props;
        let mut req = atomic::AtomicModeReq::new();
        req.add_property(self.plane, pr.fb, raw(0));
        req.add_property(self.plane, pr.crtc, raw(0));
        req.add_property(self.crtc, pr.active, raw(0));
        req.add_property(self.crtc, pr.mode_id, raw(0));
        req.add_property(self.conn, pr.conn_crtc, raw(0));
        if self.card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req).is_ok() {
            log("kms: pipe switched off");
        }
        for (db, fb) in self.dumb {
            let _ = self.card.destroy_framebuffer(fb);
            let _ = self.card.destroy_dumb_buffer(db);
        }
    }

    pub fn rm_fb(&self, fb: framebuffer::Handle) {
        let _ = self.card.destroy_framebuffer(fb);
    }

    /// Page flips done since the last call.
    pub fn flips(&self) -> usize {
        self.card.receive_events().map(|ev| ev.filter(|e| matches!(e, drm::control::Event::PageFlip(_))).count()).unwrap_or(0)
    }
}
