//! The host terminal (foot) with the on-screen keyboard. sway puts terminals on a workspace of their own ("terminal":
//! no tab bar over them, see the sway config); the keyboard shows while one is focused and the terminal shrinks above
//! it (wvkbd reserves its height). Closing the last one goes back to the shell. sway's window events drive it, from
//! a thread: the keyboard is a signal to wvkbd, nothing in GTK.

use std::time::Duration;

use mship_core::sway::{EVENT_WINDOW, Sway};
use serde_json::Value;

use crate::ui;

const APP_ID: &str = "foot";
const SHELL_APP_ID: &str = "mship-shell";

/// Show the terminal: the open one, or a new one.
pub fn open() {
    let focused = Sway::session().and_then(|mut s| s.command(&format!("[app_id=\"^{APP_ID}$\"] focus"))).is_ok();
    if !focused {
        ui::spawn(APP_ID);
    }
}

fn any_open(tree: &Value) -> bool {
    tree["app_id"] == APP_ID
        || ["nodes", "floating_nodes"].iter().any(|k| tree[k].as_array().is_some_and(|n| n.iter().any(any_open)))
}

/// Follow sway's focus for as long as the shell runs (reconnecting if sway restarts).
pub fn watch(osk: &'static str) {
    std::thread::spawn(move || {
        loop {
            if let Err(e) = follow(osk) {
                eprintln!("terminal: sway events: {e}");
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

fn follow(osk: &str) -> std::io::Result<()> {
    let mut ev = Sway::session()?;
    ev.subscribe(&["window"])?;
    let mut on_terminal = false;
    loop {
        let (kind, v) = ev.event()?;
        if kind != EVENT_WINDOW {
            continue;
        }
        let app = v["container"]["app_id"].as_str().unwrap_or_default();
        match v["change"].as_str() {
            Some("focus") => {
                if app == APP_ID {
                    ui::signal_named(osk, libc::SIGUSR2);
                } else if on_terminal && app != SHELL_APP_ID {
                    // the shell decides for itself (its text fields), anything else gets no keyboard
                    ui::signal_named(osk, libc::SIGUSR1);
                }
                on_terminal = app == APP_ID;
            }
            Some("close") if app == APP_ID => {
                let mut s = Sway::session()?;
                if !any_open(&s.tree()?) {
                    ui::signal_named(osk, libc::SIGUSR1);
                    on_terminal = false;
                    let _ = s.command("workspace number 1");
                }
            }
            _ => {}
        }
    }
}
