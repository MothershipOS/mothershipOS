//! The way home: holding the assist key (gpio-keys KEY_SEARCH) for 3 s switches the panel between mothershipOS and
//! the VM: a VM shown -> its screen hardware back to mothershipOS; mothershipOS shown -> lent to the running VM
//! (on the terminals' workspace: back to the shell first). A short press does nothing.
//!
//! The key is read here, not grabbed (sway leaves it unbound; grabbing gpio-keys would also take volume up from
//! the VMs' input bridges), whoever has the screen.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::handover::Handover;
use crate::session;

const DEVICE: &str = "gpio-keys";
const KEY_SEARCH: u16 = 217;
const EV_KEY: u16 = 1;
const HOLD: Duration = Duration::from_secs(3);

/// The event node of the input device named `name`.
fn evdev(name: &str) -> Option<String> {
    std::fs::read_dir("/sys/class/input").ok()?.flatten().find_map(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        (n.starts_with("event") && std::fs::read_to_string(e.path().join("device/name")).is_ok_and(|s| s.trim() == name))
            .then(|| format!("/dev/input/{n}"))
    })
}

fn home(handover: &Handover) {
    if handover.holders().get("display").is_none_or(|h| h == "host") && session::focused_workspace().as_deref() == Some("terminal") {
        session::sway(&["workspace", "number", "1"]);
        return;
    }
    handover.toggle();
}

/// Wait for readable input at most `ms` (negative: no limit); false on timeout.
fn readable(f: &File, ms: i32) -> bool {
    let mut p = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut p, 1, ms) > 0 }
}

/// One open of the device: until it goes away (its driver rebound).
fn watch(path: &str, handover: &Handover) {
    let Ok(mut f) = File::open(path) else { return };
    let mut down: Option<Instant> = None;
    let mut fired = false;
    let mut ev = [0u8; 24]; // struct input_event (64-bit): timeval, type u16, code u16, value i32
    loop {
        let wait = match down {
            Some(t) if !fired => HOLD.saturating_sub(t.elapsed()).as_millis() as i32,
            _ => -1,
        };
        if !readable(&f, wait) {
            if down.is_some() && !fired {
                fired = true;
                println!("assist key held {} s: home", HOLD.as_secs());
                home(handover);
            }
            continue;
        }
        if f.read_exact(&mut ev).is_err() {
            return;
        }
        let (ty, code) = (u16::from_ne_bytes([ev[16], ev[17]]), u16::from_ne_bytes([ev[18], ev[19]]));
        let value = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
        if ty == EV_KEY && code == KEY_SEARCH {
            match value {
                1 => {
                    down = Some(Instant::now());
                    fired = false;
                }
                0 => down = None,
                _ => {} // autorepeat
            }
        }
    }
}

pub fn start(handover: Arc<Handover>) {
    std::thread::Builder::new()
        .name("homekey".into())
        .spawn(move || loop {
            match evdev(DEVICE) {
                Some(p) => watch(&p, &handover),
                None => {}
            }
            std::thread::sleep(Duration::from_secs(2));
        })
        .unwrap();
}
