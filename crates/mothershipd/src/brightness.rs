//! The panel's brightness follows the display: while a VM holds it, the VM's own brightness, else mothershipOS's
//! (the Phone page's Display setting).
//!
//! A VM's brightness comes from its lights HAL's backlight, in Cuttlefish's lights protocol: the guest listens on
//! vsock port 6900, the host connects (vm.json "cid") and reads updates, each a 4-byte little-endian length and
//! JSON `{"event": "VIRTUAL_DEVICE_LIGHTS_UPDATE", "lights": [{"id", "color", "light_type"}]}` (light_type 0 =
//! backlight; its brightness: colour 0x01000000 | level = 16 bits (PixelOS's framework patch), else the colour's
//! low byte, 0..255; written at the panel's full resolution). The guest queues updates while no host is
//! connected and sends them on connect, so the newest one arrives late too. Brightness 0 (Android's screen off) is
//! ignored: the panel's power is mship.display-power's.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mship_host::power as hpower;
use mship_host::util::load_value;
use mship_host::vm;

const LIGHTS_PORT: u32 = 6900;
const BACKLIGHT: u64 = 0;

#[derive(Default)]
pub struct Brightness {
    /// VM directory -> its backlight (0..65535 of the range), as last reported
    vm: Mutex<HashMap<String, u32>>,
    /// the running VMs (their connections run while they do)
    running: Mutex<HashSet<String>>,
    /// VMs with a connection thread
    conns: Mutex<HashSet<String>>,
    /// whom the panel follows now ("host" or a VM directory) and what was written for it
    applied: Mutex<Option<(String, Option<u32>)>>,
    /// the display's holder ("host" or a VM path), as the handover last said
    holder: Mutex<String>,
    /// mothershipOS's brightness when a VM's took over (restored when the display comes back)
    host_pct: Mutex<Option<u32>>,
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl Brightness {
    pub fn new() -> Arc<Self> {
        let b = Arc::new(Self::default());
        *b.holder.lock().unwrap() = "host".into();
        b
    }

    /// The handover changed: the panel follows the display's holder.
    pub fn follow(&self, holders: &BTreeMap<String, String>) {
        *self.holder.lock().unwrap() = holders.get("display").cloned().unwrap_or_else(|| "host".into());
        self.apply();
    }

    /// Every tick: a connection to each running VM's lights.
    pub fn tick(self: &Arc<Self>, running: Vec<String>) {
        let set: HashSet<String> = running.into_iter().collect();
        self.vm.lock().unwrap().retain(|d, _| set.contains(d));
        *self.running.lock().unwrap() = set.clone();
        for d in set {
            let Some(cid) = load_value(Path::new(vm::VMS).join(&d).join("vm.json")).and_then(|c| c["cid"].as_u64()) else { continue };
            if self.conns.lock().unwrap().insert(d.clone()) {
                let me = self.clone();
                std::thread::Builder::new().name(format!("lights-{d}")).spawn(move || me.session(d, cid as u32)).ok();
            }
        }
    }

    /// One VM's lights while it runs (reconnecting: the guest's server starts some time after the VM).
    fn session(self: Arc<Self>, d: String, cid: u32) {
        while self.running.lock().unwrap().contains(&d) {
            if let Ok(mut s) = vsock::VsockStream::connect_with_cid_port(cid, LIGHTS_PORT) {
                println!("brightness: {d}: lights connected");
                loop {
                    let mut len = [0u8; 4];
                    if s.read_exact(&mut len).is_err() {
                        break;
                    }
                    let n = u32::from_le_bytes(len) as usize;
                    if n > 1 << 16 {
                        break;
                    }
                    let mut buf = vec![0u8; n];
                    if s.read_exact(&mut buf).is_err() {
                        break;
                    }
                    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf) else { continue };
                    for l in v["lights"].as_array().into_iter().flatten() {
                        if l["light_type"].as_u64() != Some(BACKLIGHT) {
                            continue;
                        }
                        let c = l["color"].as_u64().unwrap_or(0);
                        let level = if c >> 24 == 0x01 { c & 0xffff } else { (c & 0xff) * 65535 / 255 };
                        if level == 0 {
                            continue;
                        }
                        self.vm.lock().unwrap().insert(d.clone(), level as u32);
                        self.apply();
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(3));
        }
        self.conns.lock().unwrap().remove(&d);
    }

    /// Write the panel's brightness for whom it follows now (only when that changed).
    fn apply(&self) {
        let holder = self.holder.lock().unwrap().clone();
        let follow = if holder == "host" { "host".to_string() } else { base(&holder).to_string() };
        let want = if follow == "host" { None } else { self.vm.lock().unwrap().get(&follow).copied() };
        let mut applied = self.applied.lock().unwrap();
        let next = Some((follow.clone(), want));
        if *applied == next {
            return;
        }
        // a VM that has not said yet keeps what the panel has
        let mut host_pct = self.host_pct.lock().unwrap();
        let r = match want {
            Some(level) => {
                if host_pct.is_none() {
                    *host_pct = Some(hpower::brightness());
                }
                hpower::set_backlight(level as f64 / 65535.0)
            }
            None if follow == "host" => match host_pct.take() {
                Some(pct) => hpower::set_brightness(pct, false).map(|_| ()),
                None => Ok(()),
            },
            None => Ok(()),
        };
        if let Err(e) = r {
            println!("brightness: {e}");
        }
        *applied = next;
    }
}
