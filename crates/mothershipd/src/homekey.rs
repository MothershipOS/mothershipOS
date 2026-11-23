//! The way home: holding the home key (device.json home_key, default KEY_SEARCH: the nio's assist key) for 3 s switches the panel between mothershipOS and
//! the VM: a VM shown -> its screen hardware back to mothershipOS; mothershipOS shown -> lent to the running VM
//! (on the terminals' workspace: back to the shell first). A short press does nothing.
//!
//! The key is read here, not grabbed (sway leaves it unbound; grabbing its device would also take the volume keys
//! on it from the VMs' input bridges), whoever has the screen.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::handover::Handover;
use crate::session;

const EV_KEY: u16 = 1;
const HOLD: Duration = Duration::from_secs(3);

/// The event node of the device with the home key (none on phones without such a key: no way home by key).
fn evdev(key: u16) -> Option<String> {
    mship_core::device::inputs().into_iter().find(|i| i.is_keys() && i.has_key(key)).map(|i| i.node)
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
fn watch(path: &str, key: u16, handover: &Handover) {
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
                println!("home key held {} s: home", HOLD.as_secs());
                home(handover);
            }
            continue;
        }
        if f.read_exact(&mut ev).is_err() {
            return;
        }
        let (ty, code) = (u16::from_ne_bytes([ev[16], ev[17]]), u16::from_ne_bytes([ev[18], ev[19]]));
        let value = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
        if ty == EV_KEY && code == key {
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
        .spawn(move || {
            let key = mship_core::device::home_key();
            if key == 0 {
                println!("no home key (device.json home_key 0)");
                return;
            }
            loop {
                if let Some(p) = evdev(key) {
                    watch(&p, key, &handover);
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .unwrap();
}
