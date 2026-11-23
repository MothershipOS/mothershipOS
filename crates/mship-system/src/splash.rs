//! The boot logo's fade out and the shell's fade in, around the compositor (mship-sway.service): the logo was faded
//! in by mship-init (initramfs) to the saved brightness.
//! - `splash-out` (ExecStartPre): the backlight to 0, before sway takes the panel from the boot logo
//! - `splash-in` (ExecStartPost): once the shell's window is on screen (at most 15 s), the backlight to the saved
//!   brightness. Also after a compositor restart: the panel never stays dark.

use std::time::{Duration, Instant};

use mship_core::backlight;
use mship_core::sway::Sway;

use crate::Res;

/// the shell's window (its app_id is the program's name; the GTK application id as a fallback)
const SHELL: &[&str] = &["mship-shell", "os.mothership.Shell"];

pub fn fade_out() -> Res<()> {
    backlight::fade(0, 450).map_err(|e| format!("backlight: {e}"))
}

/// Does the tree have a visible window with one of these app_ids?
fn shown(node: &serde_json::Value, ids: &[&str]) -> bool {
    (node["app_id"].as_str().is_some_and(|a| ids.contains(&a)) && node["visible"] == true)
        || ["nodes", "floating_nodes"].iter().any(|k| node[*k].as_array().is_some_and(|a| a.iter().any(|n| shown(n, ids))))
}

pub fn fade_in() -> Res<()> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(15) {
        if Sway::session().and_then(|mut s| s.tree()).is_ok_and(|tree| shown(&tree, SHELL)) {
            // its first frame
            std::thread::sleep(Duration::from_millis(150));
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    backlight::fade(backlight::saved(), 600).map_err(|e| format!("backlight: {e}"))
}
