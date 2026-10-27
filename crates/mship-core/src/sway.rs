//! sway's IPC (the i3 protocol): commands, outputs, the window tree and events, without swaymsg.
//!
//! A message is "i3-ipc", the payload length and the message type (u32, native byte order), then the payload.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const GET_WORKSPACES: u32 = 1;
const SUBSCRIBE: u32 = 2;
const GET_OUTPUTS: u32 = 3;
const GET_TREE: u32 = 4;
/// events have the high bit set; window events are type 3
pub const EVENT_WINDOW: u32 = 0x8000_0003;
const GET_INPUTS: u32 = 100;

/// The mothershipOS session's runtime directory (sway-mship).
pub const RUNTIME_DIR: &str = "/run/mship-wayland";

/// sway's socket: $SWAYSOCK, or the first sway-ipc.*.sock in `runtime_dir`.
pub fn socket(runtime_dir: Option<&Path>) -> Option<PathBuf> {
    if runtime_dir.is_none()
        && let Some(s) = std::env::var_os("SWAYSOCK")
    {
        return Some(s.into());
    }
    let dir = runtime_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
        .unwrap_or_else(|| RUNTIME_DIR.into());
    let mut socks: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("sway-ipc.") && n.ends_with(".sock")))
        .collect();
    socks.sort();
    socks.into_iter().next()
}

pub struct Sway(UnixStream);

impl Sway {
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(Self(s))
    }

    /// The session's sway.
    pub fn session() -> std::io::Result<Self> {
        Self::connect(&socket(None).ok_or_else(|| std::io::Error::other("sway is not running"))?)
    }

    fn request(&mut self, kind: u32, payload: &str) -> std::io::Result<Value> {
        let mut m = Vec::with_capacity(14 + payload.len());
        m.extend_from_slice(MAGIC);
        m.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        m.extend_from_slice(&kind.to_ne_bytes());
        m.extend_from_slice(payload.as_bytes());
        self.0.write_all(&m)?;
        Ok(self.message()?.1)
    }

    /// The next message: its type and payload.
    fn message(&mut self) -> std::io::Result<(u32, Value)> {
        let mut h = [0u8; 14];
        self.0.read_exact(&mut h)?;
        if &h[..6] != MAGIC {
            return Err(std::io::Error::other("not sway"));
        }
        let mut b = vec![0u8; u32::from_ne_bytes(h[6..10].try_into().unwrap()) as usize];
        self.0.read_exact(&mut b)?;
        Ok((u32::from_ne_bytes(h[10..14].try_into().unwrap()), serde_json::from_slice(&b).map_err(std::io::Error::other)?))
    }

    /// Subscribe to events (e.g. ["window"]); then read them with [`Sway::event`]. The connection is for events only
    /// from then on, and waits for them without a timeout.
    pub fn subscribe(&mut self, events: &[&str]) -> std::io::Result<()> {
        let r = self.request(SUBSCRIBE, &serde_json::to_string(events).unwrap_or_default())?;
        if r["success"] != true {
            return Err(std::io::Error::other("subscribe failed"));
        }
        self.0.set_read_timeout(None)
    }

    /// The next event: its type (EVENT_*) and payload.
    pub fn event(&mut self) -> std::io::Result<(u32, Value)> {
        self.message()
    }

    /// The focused workspace's name.
    pub fn focused_workspace(&mut self) -> std::io::Result<Option<String>> {
        let ws = self.request(GET_WORKSPACES, "")?;
        Ok(ws.as_array().into_iter().flatten().find(|w| w["focused"] == true).and_then(|w| w["name"].as_str()).map(String::from))
    }

    pub fn tree(&mut self) -> std::io::Result<Value> {
        self.request(GET_TREE, "")
    }

    /// Run a command; Err with sway's message if it failed.
    pub fn command(&mut self, cmd: &str) -> std::io::Result<()> {
        let r = self.request(RUN_COMMAND, cmd)?;
        let failed: Vec<String> = r
            .as_array()
            .into_iter()
            .flatten()
            .filter(|x| x["success"] != true)
            .map(|x| x["error"].as_str().unwrap_or("failed").to_string())
            .collect();
        if failed.is_empty() { Ok(()) } else { Err(std::io::Error::other(format!("{cmd}: {}", failed.join("; ")))) }
    }

    pub fn outputs(&mut self) -> std::io::Result<Vec<Value>> {
        Ok(self.request(GET_OUTPUTS, "")?.as_array().cloned().unwrap_or_default())
    }

    pub fn inputs(&mut self) -> std::io::Result<Vec<Value>> {
        Ok(self.request(GET_INPUTS, "")?.as_array().cloned().unwrap_or_default())
    }

    /// Is this output on (DPMS)? None if sway has no such output.
    pub fn output_power(&mut self, name: &str) -> std::io::Result<Option<bool>> {
        Ok(self.outputs()?.iter().find(|o| o["name"] == name).map(|o| o["power"].as_bool().unwrap_or(true)))
    }
}
