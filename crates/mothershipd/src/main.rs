//! mothershipd: the mothershipOS host daemon (root). It owns the host's live state in memory and serves it over one
//! socket (mship_core::api); nothing is published as files.
//!
//! Domains: power ([`power`]: mship.power votes and the guest channel, VM sleep/wake and display power, frequency
//! floors, phone suspend, critical battery), the CPU temperature guard ([`cpuguard`]), the hardware handover
//! ([`handover`]), VM lifecycle ([`vms`]), the VMs' shared CPUs and RAM ([`pool`]), host status and settings (mship-host). At start it applies the saved
//! charge limit (the battery driver starts at 100 %), CPU mode and brightness.

mod api;
mod cpuguard;
mod handover;
mod hub;
mod pool;
mod power;
mod screen;
mod session;
mod vms;

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use mship_core::api::Topic;
use mship_host::{cpu, power as hpower};

/// the policy pass
const TICK: Duration = Duration::from_secs(2);
/// host status (Wi-Fi, CPU, battery, SSH) is read this often, or at once after a settings change
const HOST_POLL: Duration = Duration::from_secs(4);

pub struct Daemon {
    pub hub: Arc<hub::Hub>,
    pub guard: Mutex<cpuguard::CpuGuard>,
    pub power: Arc<power::Power>,
    pub handover: Arc<handover::Handover>,
    pub vms: Arc<vms::Vms>,
    pub pool: Arc<pool::Pool>,
    host_kick: (Mutex<bool>, Condvar),
}

/// The power state without timestamps: what decides whether it changed.
fn power_key(v: &serde_json::Value) -> serde_json::Value {
    let mut k = v.clone();
    if let Some(p) = k["participants"].as_object_mut() {
        for e in p.values_mut() {
            if let Some(e) = e.as_object_mut() {
                e.remove("updated");
            }
        }
    }
    k
}

impl Daemon {
    fn new() -> Arc<Self> {
        let hub = Arc::new(hub::Hub::default());
        let h = hub.clone();
        let power = power::Power::new(move |st| {
            let v = serde_json::to_value(st).unwrap_or_default();
            h.set_keyed(Topic::Power, v.clone(), power_key(&v));
        });
        let h = hub.clone();
        let handover = Arc::new(handover::Handover::new(move |st| {
            h.set(Topic::Handover, serde_json::to_value(st).unwrap_or_default());
        }));
        let pool = Arc::new(pool::Pool::new());
        let h = hub.clone();
        let vms = vms::Vms::new(power.clone(), handover.clone(), pool.clone(), move |st| {
            h.set(Topic::Vms, serde_json::to_value(st).unwrap_or_default());
        });
        hub.set(Topic::Handover, serde_json::to_value(handover.state()).unwrap_or_default());
        Arc::new(Self { hub, guard: Mutex::default(), power, handover, vms, pool, host_kick: (Mutex::new(false), Condvar::new()) })
    }

    /// Read host status again now (a setting changed, or someone subscribed).
    pub fn kick_host(&self) {
        *self.host_kick.0.lock().unwrap() = true;
        self.host_kick.1.notify_one();
    }

    fn host_loop(self: Arc<Self>) {
        loop {
            let guard = self.guard.lock().unwrap().state.clone();
            let st = mship_host::host_status(Some(&guard));
            self.hub.set(Topic::Host, serde_json::to_value(st).unwrap_or_default());
            if self.hub.wanted(Topic::Cpu) {
                // the CPU page's detail, only while it is open
                self.hub.set(Topic::Cpu, serde_json::to_value(cpu::status(Some(&guard))).unwrap_or_default());
            }
            let mut kicked = self.host_kick.0.lock().unwrap();
            if !*kicked {
                kicked = self.host_kick.1.wait_timeout(kicked, HOST_POLL).unwrap().0;
            }
            *kicked = false;
        }
    }

    fn run(self: Arc<Self>) {
        let me = self.clone();
        std::thread::Builder::new().name("api".into()).spawn(move || me.api_loop()).unwrap();
        // boot: the saved charge limit (the battery driver starts at 100 %), CPU mode and brightness
        std::thread::spawn(|| {
            if let Err(e) = hpower::battery_apply() {
                println!("charge limit: {e}");
            }
            if let Err(e) = cpu::apply_saved() {
                println!("cpu mode: {e}");
            }
            if let Err(e) = hpower::display_apply() {
                println!("brightness: {e}");
            }
        });
        self.power.start();
        let me = self.clone();
        std::thread::Builder::new().name("host-status".into()).spawn(move || me.host_loop()).unwrap();
        println!("mothershipd {} running", mship_core::RELEASE);
        loop {
            let guard = {
                let mut g = self.guard.lock().unwrap();
                if g.tick() {
                    println!("cpu guard: limit {:?} °C, hottest {} °C{}", g.state.limit, g.state.hottest,
                             if g.state.throttled { ", throttling" } else { "" });
                }
                g.state.clone()
            };
            self.vms.tick();
            self.handover.tick();
            self.power.tick(&guard);
            self.pool.tick(&self.power.state().participants);
            std::thread::sleep(TICK);
        }
    }
}

fn main() {
    Daemon::new().run();
}
