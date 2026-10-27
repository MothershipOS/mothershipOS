//! mothershipOS core: what every part of mothershipOS agrees on.
//!
//! - [`version`]: contract versions (MAJOR.MINOR) and mothershipOS releases (YYYY.N)
//! - [`contracts`]: every contract, platform and manifest schema mothershipOS has shipped since V1
//! - [`manifest`]: VM manifests (`vm.json`)
//! - [`status`]: host status and settings as mothershipd reports them
//! - [`api`]: the mothershipd API (requests, replies, events, topics), [`client`]: a client for it
//! - [`sway`]: sway's IPC
//! - [`ctl`]: the device services' control sockets

pub mod api;
pub mod client;
pub mod contracts;
pub mod ctl;
pub mod manifest;
pub mod pool;
pub mod status;
pub mod sway;
pub mod uevent;
pub mod version;

/// This mothershipOS release (the repo's `VERSION` file; a test keeps them equal).
pub const RELEASE: &str = "2026.1";
