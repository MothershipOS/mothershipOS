//! What the device services share: logging, sockets for the session user; kernel uevents and polling (from
//! mship_core::uevent).

pub mod mw;
pub mod qrtr;
pub mod netlink;

use std::path::Path;

pub use mship_core::uevent::{poll, read_uevent, uevents};

pub const RUN: &str = "/run/mothership";
/// the session user's group (crosvm runs as the session user)
pub const GROUP: &str = "mship";

pub fn log(msg: impl AsRef<str>) {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    println!("{:02}:{:02}:{:02} {}", t / 3600 % 24, t / 60 % 60, t % 60, msg.as_ref());
}

pub fn group_id(name: &str) -> Option<u32> {
    let n = std::ffi::CString::new(name).ok()?;
    let g = unsafe { libc::getgrnam(n.as_ptr()) };
    (!g.is_null()).then(|| unsafe { (*g).gr_gid })
}

/// root and the session user's group may use it
pub fn for_session(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    if let Some(g) = group_id(GROUP) {
        let _ = std::os::unix::fs::chown(path, Some(0), Some(g));
    }
}

/// A fresh socket path (any old one removed, its directory made).
pub fn fresh(path: &Path) {
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::remove_file(path);
}

pub fn rd(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}
