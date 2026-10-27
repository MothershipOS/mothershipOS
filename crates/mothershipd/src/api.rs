//! The API socket: requests from the shell and tools, replies, and subscriptions (mship_core::api).

use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

use mship_core::api::{self, Hello, Message, Op, Reply, Request, TOPICS};
use mship_host::util::Error;
use mship_host::{bt, cpu, power as hpower, region, resources, ssh, vm, wifi};
use serde::Serialize;
use serde_json::Value;

use crate::Daemon;
use crate::hub::Client;
use crate::{screen, session};

fn peer_uid(s: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len)
    };
    (r == 0).then_some(cred.uid)
}

fn ok(v: impl Serialize) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

fn host<T: Serialize>(r: Result<T, Error>) -> Result<Value, String> {
    r.map_err(|e| e.0).and_then(ok)
}

/// A VM name from a request (a directory name, or a path under the VM directory).
fn vm_name(v: &str) -> Result<String, String> {
    vm::vm_dir(v).map(|d| d.file_name().unwrap().to_string_lossy().into_owned()).map_err(|e| e.0)
}

impl Daemon {
    pub fn dispatch(self: &Arc<Self>, c: Option<&Arc<Client>>, op: Op) -> Result<Value, String> {
        if op.root_only() && c.is_some_and(|c| c.uid != 0) {
            return Err("this is for root".into());
        }
        let guard = self.guard.lock().unwrap().state.clone();
        let g = Some(&guard);
        let refresh = matches!(
            op,
            Op::WifiConnect { .. } | Op::WifiDisconnect | Op::WifiForget { .. } | Op::WifiRadio { .. } | Op::OwnerSet { .. }
                | Op::SshSet { .. } | Op::CpuProfile { .. } | Op::CpuSet { .. } | Op::BatterySet { .. }
                | Op::BrightnessSet { .. }
        );
        let r = match op {
            Op::Hello => ok(Hello { daemon: "mothershipd".into(), release: mship_core::RELEASE.into(), topics: TOPICS.to_vec() }),
            Op::Subscribe { topics } | Op::Unsubscribe { topics } if c.is_none() => {
                let _ = topics;
                Err("subscribe needs a connection".into())
            }
            Op::Subscribe { topics } => {
                let c = c.unwrap();
                c.topics.lock().unwrap().extend(topics.iter().copied());
                for t in &topics {
                    // the current state right away
                    let _ = c.send(&api::Event { event: *t, data: self.hub.get(*t) });
                }
                self.kick_host();
                ok(c.topics.lock().unwrap().clone())
            }
            Op::Unsubscribe { topics } => {
                let c = c.unwrap();
                c.topics.lock().unwrap().retain(|t| !topics.contains(t));
                ok(c.topics.lock().unwrap().clone())
            }

            Op::PowerGet => ok(self.power.state()),
            Op::SetVote { participant, vote } => {
                self.power.set_vote(&participant, vote);
                ok(serde_json::json!({}))
            }
            Op::CpuGuard => ok(guard.clone()),
            Op::Resources => ok(resources::collect()),

            Op::VmList => ok(self.vms.list()),
            Op::VmStart { vm } => vm_name(&vm).and_then(|n| host(self.vms.start(&n))),
            Op::VmStop { vm } => vm_name(&vm).and_then(|n| host(self.vms.stop(&n))),
            Op::VmShow { vm } => vm_name(&vm).and_then(|n| host(self.vms.show(&n))),
            Op::VmSleep { vm } => vm_name(&vm).and_then(|n| host(self.power.freeze(&n, None))),
            Op::VmDisplay { vm, on } => vm_name(&vm).and_then(|n| {
                self.power.set_display(&n, on);
                ok(serde_json::json!({}))
            }),
            Op::VmWake { vm } => vm_name(&vm).and_then(|n| {
                if self.power.thaw(&n, "Wake") { ok(serde_json::json!({})) } else { Err(format!("{n} is not asleep")) }
            }),

            Op::HandoverStatus => ok(self.handover.holders()),
            Op::HandoverAssoc => ok(self.handover.assoc()),
            Op::HandoverGive { vm, items } => vm::vm_dir(&vm).map_err(|e| e.0).and_then(|d| {
                ok(self.handover.give(&d, (!items.is_empty()).then_some(items)))
            }),
            Op::HandoverTake { items } => {
                let items: Vec<&str> = if items.is_empty() {
                    api::ITEMS.to_vec()
                } else {
                    api::ITEMS.iter().copied().filter(|i| items.iter().any(|x| x == i)).collect()
                };
                ok(self.handover.take(&items))
            }
            Op::HandoverRelease { vm } => vm::vm_dir(&vm).map_err(|e| e.0).and_then(|d| ok(self.handover.release(&d))),
            Op::HandoverToggle => ok(self.handover.toggle()),
            Op::HandoverScreen { on } => ok(self.handover.screen(on)),
            Op::HandoverAssign { item, owner } => host(self.handover.assign(&item, &owner)),

            Op::WifiStatus => ok(wifi::status()),
            Op::WifiScan => host(wifi::scan()),
            Op::WifiConnect { ssid, password } => host(wifi::connect(&ssid, &password)),
            Op::WifiDisconnect => host(wifi::disconnect()),
            Op::WifiForget { ssid } => ok(wifi::forget(&ssid)),
            Op::WifiRadio { on } => host(wifi::radio(on)),
            Op::OwnerSet { device, owner } => host(wifi::owner_set(&device, &owner)),
            Op::SshStatus => ok(ssh::status()),
            Op::SshSet { key, value } => host(ssh::set(&key, &value)),
            Op::SshPasswd { user, password } => host(ssh::passwd(&user, &password)),
            Op::SshAddKey { user, key } => host(ssh::add_key(&user, &key)),
            Op::CpuStatus => ok(cpu::status(g)),
            Op::CpuProfile { name } => host(cpu::set_profile(&name, g)),
            Op::CpuSet { policy, governor, max_khz } => host(cpu::set_policy(&policy, &governor, max_khz, g)),
            Op::RegionStatus => ok(region::status()),
            Op::RegionCountries => ok(region::countries()),
            Op::RegionCountry { code } => host(region::set_country(&code)),
            Op::RegionTimezone { tz } => host(region::set_timezone(&tz)),
            Op::RegionClock24 { on } => host(region::set_clock24(on)),
            Op::ScreenSet { on } => host(screen::set(&self.handover, on)),
            Op::ScreenToggle => host(screen::toggle(&self.handover)),
            Op::ScreenStatus => ok(screen::is_on()),
            Op::PowerStatus => ok(hpower::power_status(&self.power.last_decision.lock().unwrap())),
            Op::PowerSet { key, value } => {
                let last = self.power.last_decision.lock().unwrap().clone();
                let r = host(hpower::set_power(&key, &value, &last));
                if r.is_ok() && key == "screen_timeout" && self.handover.holders()["display"] == "host" {
                    crate::handover::idle(true); // the new timeout (a VM with the display keeps it off)
                }
                r
            }
            Op::BatteryStatus => ok(hpower::battery_status()),
            Op::BatterySet { key, value } => host(hpower::set_battery(&key, &value)),
            Op::BrightnessSet { percent } => host(hpower::set_brightness(percent, true)),
            Op::BtStatus => ok(bt::status()),
            Op::BtPower { on } => ok(bt::power(on)),
            Op::BtDiscoverable { on } => ok(bt::discoverable(on)),
            Op::BtScan => ok(bt::scan()),
            Op::BtPair { mac } => host(bt::pair(&mac)),
            Op::BtConnect { mac } => host(bt::connect(&mac, true)),
            Op::BtDisconnect { mac } => host(bt::connect(&mac, false)),
            Op::BtRemove { mac } => host(bt::remove(&mac)),
            Op::PowerOff | Op::Reboot => {
                let reboot = matches!(op, Op::Reboot);
                let me = self.clone();
                std::thread::spawn(move || {
                    me.vms.stop_all();
                    hpower::shutdown(reboot);
                });
                ok(serde_json::json!({}))
            }
        };
        if refresh {
            self.kick_host();
        }
        r
    }

    fn serve_client(self: Arc<Self>, c: Arc<Client>, stream: UnixStream) {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Request>(&line) {
                // each request on its own thread: a slow one (Wi-Fi connect, Bluetooth scan) holds up no other;
                // replies carry their id
                Ok(Request { id, op }) => {
                    let (me, c) = (self.clone(), c.clone());
                    std::thread::spawn(move || {
                        let reply = match me.dispatch(Some(&c), op) {
                            Ok(result) => Reply { id: Some(id), ok: true, result: Some(result), error: None },
                            Err(e) => Reply { id: Some(id), ok: false, result: None, error: Some(e) },
                        };
                        let _ = c.send(&Message::Reply(reply));
                    });
                }
                Err(e) => {
                    let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| v["id"].as_u64());
                    if c.send(&Message::Reply(Reply { id, ok: false, result: None, error: Some(format!("bad request: {e}")) })).is_err() {
                        break;
                    }
                }
            }
        }
        self.hub.drop_client(&c);
    }

    pub fn api_loop(self: Arc<Self>) {
        let _ = std::fs::create_dir_all("/run/mothership");
        let _ = std::fs::remove_file(api::SOCKET);
        let l = match UnixListener::bind(api::SOCKET) {
            Ok(l) => l,
            Err(e) => {
                println!("api: cannot listen on {}: {e}", api::SOCKET);
                std::process::exit(1);
            }
        };
        let (ui_uid, ui_gid) = session::user();
        let _ = std::os::unix::fs::chown(api::SOCKET, Some(0), Some(ui_gid));
        let _ = mship_host::util::set_mode(api::SOCKET, 0o660); // root and the shell's group
        println!("api: {}", api::SOCKET);
        for s in l.incoming().flatten() {
            let uid = peer_uid(&s).unwrap_or(u32::MAX);
            if uid != 0 && uid != ui_uid {
                continue;
            }
            let Ok(w) = s.try_clone() else { continue };
            let c = Arc::new(Client::new(w, uid));
            self.hub.add(c.clone());
            let me = self.clone();
            std::thread::spawn(move || me.serve_client(c, s));
        }
    }
}
