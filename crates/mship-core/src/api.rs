//! The mothershipd API: `/run/mothership/mothershipd.sock` (root and group mship), one JSON object per line.
//!
//! - request: `{"id": n, "op": "<op>", ...args}` ([`Request`])
//! - reply: `{"id": n, "ok": true, "result": ...}` or `{"id": n, "ok": false, "error": "..."}` ([`Reply`])
//! - event: `{"event": "<topic>", "data": ...}` ([`Event`]), pushed to subscribers on every change and once on
//!   subscribe
//!
//! Topics: `power` ([`PowerState`]), `vms` ([`VmList`]), `handover` ([`HandoverState`]), `host`
//! ([`crate::status::HostStatus`]), `cpu` ([`crate::status::CpuStatus`]: only read while someone subscribes).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::status::GuardState;

pub const SOCKET: &str = "/run/mothership/mothershipd.sock";
/// the guest channel of mship.power (vsock, host CID 2)
pub const POWER_VSOCK_PORT: u32 = 7720;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Topic {
    Power,
    Vms,
    Handover,
    Host,
    Cpu,
}

pub const TOPICS: &[Topic] = &[Topic::Power, Topic::Vms, Topic::Handover, Topic::Host, Topic::Cpu];

/// Hardware a VM can be given (Settings › Hardware).
pub const ITEMS: &[&str] = &["display", "buttons", "wifi-2g", "wifi-5g", "internet", "bluetooth", "usb", "display-ext", "cellular", "cameras"];
/// lent while the VM is shown; the rest while it runs
pub const SCREEN_ITEMS: &[&str] = &["display", "buttons"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Op {
    #[serde(rename = "hello")]
    Hello,
    #[serde(rename = "subscribe")]
    Subscribe { topics: Vec<Topic> },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { topics: Vec<Topic> },

    // power (mship.power)
    #[serde(rename = "power.get")]
    PowerGet,
    /// a host-side agent's vote (root only)
    #[serde(rename = "power.set_vote")]
    SetVote {
        participant: String,
        #[serde(flatten)]
        vote: VoteRequest,
    },
    #[serde(rename = "cpu.guard")]
    CpuGuard,
    #[serde(rename = "resources")]
    Resources,

    // VMs
    #[serde(rename = "vm.list")]
    VmList,
    #[serde(rename = "vm.start")]
    VmStart { vm: String },
    #[serde(rename = "vm.stop")]
    VmStop { vm: String },
    #[serde(rename = "vm.show")]
    VmShow { vm: String },
    #[serde(rename = "vm.sleep")]
    VmSleep { vm: String },
    #[serde(rename = "vm.wake")]
    VmWake { vm: String },
    /// From a VM's presenter: the guest's display is on (its window is shown) or off (it blanked: crosvm closes the
    /// window when the guest disables its scanout).
    #[serde(rename = "vm.display")]
    VmDisplay { vm: String, on: bool },

    // hardware handover
    #[serde(rename = "handover.status")]
    HandoverStatus,
    #[serde(rename = "handover.assoc")]
    HandoverAssoc,
    #[serde(rename = "handover.give")]
    HandoverGive {
        vm: String,
        #[serde(default)]
        items: Vec<String>,
    },
    #[serde(rename = "handover.take")]
    HandoverTake {
        #[serde(default)]
        items: Vec<String>,
    },
    #[serde(rename = "handover.release")]
    HandoverRelease { vm: String },
    #[serde(rename = "handover.toggle")]
    HandoverToggle,
    #[serde(rename = "handover.screen")]
    HandoverScreen { on: bool },
    #[serde(rename = "handover.assign")]
    HandoverAssign { item: String, owner: String },

    // settings and their status
    #[serde(rename = "wifi.status")]
    WifiStatus,
    #[serde(rename = "wifi.scan")]
    WifiScan,
    #[serde(rename = "wifi.connect")]
    WifiConnect {
        ssid: String,
        #[serde(default)]
        password: String,
    },
    #[serde(rename = "wifi.disconnect")]
    WifiDisconnect,
    #[serde(rename = "wifi.forget")]
    WifiForget { ssid: String },
    #[serde(rename = "wifi.radio")]
    WifiRadio { on: bool },
    #[serde(rename = "owner.set")]
    OwnerSet { device: String, owner: String },
    #[serde(rename = "ssh.status")]
    SshStatus,
    #[serde(rename = "ssh.set")]
    SshSet { key: String, value: String },
    #[serde(rename = "ssh.passwd")]
    SshPasswd { user: String, password: String },
    #[serde(rename = "ssh.addkey")]
    SshAddKey { user: String, key: String },
    #[serde(rename = "cpu.status")]
    CpuStatus,
    #[serde(rename = "cpu.profile")]
    CpuProfile { name: String },
    #[serde(rename = "cpu.set")]
    CpuSet { policy: String, governor: String, max_khz: u64 },
    #[serde(rename = "region.status")]
    RegionStatus,
    #[serde(rename = "region.countries")]
    RegionCountries,
    #[serde(rename = "region.country")]
    RegionCountry { code: String },
    #[serde(rename = "region.timezone")]
    RegionTimezone { tz: String },
    #[serde(rename = "region.clock24")]
    RegionClock24 { on: bool },
    /// the phone's screen (panel, backlight, touch): off also pauses a VM's touch, and lets the phone sleep
    #[serde(rename = "screen.set")]
    ScreenSet { on: bool },
    #[serde(rename = "screen.toggle")]
    ScreenToggle,
    #[serde(rename = "screen.status")]
    ScreenStatus,
    #[serde(rename = "power.status")]
    PowerStatus,
    #[serde(rename = "power.set")]
    PowerSet { key: String, value: String },
    #[serde(rename = "battery.status")]
    BatteryStatus,
    #[serde(rename = "battery.set")]
    BatterySet { key: String, value: String },
    #[serde(rename = "brightness.set")]
    BrightnessSet { percent: u32 },
    #[serde(rename = "bt.status")]
    BtStatus,
    #[serde(rename = "bt.power")]
    BtPower { on: bool },
    #[serde(rename = "bt.discoverable")]
    BtDiscoverable { on: bool },
    #[serde(rename = "bt.scan")]
    BtScan,
    #[serde(rename = "bt.pair")]
    BtPair { mac: String },
    #[serde(rename = "bt.connect")]
    BtConnect { mac: String },
    #[serde(rename = "bt.disconnect")]
    BtDisconnect { mac: String },
    #[serde(rename = "bt.remove")]
    BtRemove { mac: String },
    /// stop every running VM cleanly, then power off / reboot the phone
    #[serde(rename = "system.poweroff")]
    PowerOff,
    #[serde(rename = "system.reboot")]
    Reboot,
}

impl Op {
    /// only root may send these
    pub fn root_only(&self) -> bool {
        matches!(self, Op::SetVote { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: Option<u64>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub event: Topic,
    pub data: serde_json::Value,
}

/// One line from the daemon: a reply to a request, or an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Event(Event),
    Reply(Reply),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub daemon: String,
    pub release: String,
    pub topics: Vec<Topic>,
}

// ---------------------------------------------------------------- power (mship.power)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoteState {
    Active,
    Idle,
    ScreenOff,
    Sleeping,
    Suspended,
    /// a running VM without a fresh vote
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resource {
    Cpu,
    Gpu,
    Codec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Low,
    Balanced,
    High,
}

/// What a participant asks for (a guest's `set`, or a host agent's `power.set_vote`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoteRequest {
    pub state: VoteState,
    #[serde(default)]
    pub may_sleep: bool,
    #[serde(default)]
    pub demand: BTreeMap<Resource, Level>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub seq: u64,
}

/// A participant's vote as the host holds it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vote {
    pub state: VoteState,
    pub may_sleep: bool,
    pub demand: BTreeMap<Resource, Level>,
    pub reason: String,
    #[serde(default)]
    pub seq: u64,
    /// unix time of the last update
    pub updated: Option<f64>,
    /// host | guest | host-agent | none
    pub source: String,
    /// the VM's name (VMs)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SleepVerdict {
    pub allowed: bool,
    pub blockers: Vec<String>,
}

/// Topic `power`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PowerState {
    pub contract: String,
    pub version: String,
    pub sleep: SleepVerdict,
    pub floors: BTreeMap<Resource, Level>,
    /// what the floors set: cpu.policyN (kHz), gpu (Hz), codec (no control: null)
    pub applied: BTreeMap<String, Option<u64>>,
    /// "mothershipOS" and every running VM (by directory name)
    pub participants: BTreeMap<String, Vote>,
    pub guard: GuardState,
}

// ---------------------------------------------------------------- VMs and handover
/// One VM (topic `vms`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmInfo {
    /// directory name under /var/lib/mothership/vms
    pub vm: String,
    pub name: String,
    /// it has a vm.json
    pub manifest: bool,
    pub running: bool,
    /// "starting" | "stopping"
    pub busy: Option<String>,
    pub display_mode: Option<String>,
    pub output: String,
    pub workspace: u32,
    pub memory_mb: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VmList {
    pub vms: Vec<VmInfo>,
}

/// Topic `handover`: who holds each item now ("host" | a VM directory), and who it is assigned to ("host" | a VM
/// name).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HandoverState {
    pub holders: BTreeMap<String, String>,
    pub assoc: BTreeMap<String, String>,
}

/// vm.show's answer when there is nothing to switch.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShowResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r: Request = serde_json::from_str(r#"{"id": 3, "op": "vm.start", "vm": "pixelos"}"#).unwrap();
        assert_eq!(r, Request { id: 3, op: Op::VmStart { vm: "pixelos".into() } });
        let r: Request = serde_json::from_str(r#"{"id": 1, "op": "handover.take"}"#).unwrap();
        assert_eq!(r.op, Op::HandoverTake { items: vec![] });
        let v: Request = serde_json::from_str(
            r#"{"id": 2, "op": "power.set_vote", "participant": "x", "state": "screen_off", "demand": {"cpu": "high"}}"#,
        )
        .unwrap();
        let Op::SetVote { participant, vote } = v.op else { panic!() };
        assert_eq!((participant.as_str(), vote.state, vote.demand[&Resource::Cpu]), ("x", VoteState::ScreenOff, Level::High));
        assert_eq!(serde_json::to_value(Request { id: 1, op: Op::PowerGet }).unwrap(), serde_json::json!({"id": 1, "op": "power.get"}));
        let m: Message = serde_json::from_str(r#"{"event": "vms", "data": {"vms": []}}"#).unwrap();
        assert!(matches!(m, Message::Event(Event { event: Topic::Vms, .. })));
        let m: Message = serde_json::from_str(r#"{"id": 4, "ok": false, "error": "no"}"#).unwrap();
        assert!(matches!(m, Message::Reply(Reply { ok: false, .. })));
        assert!(serde_json::from_str::<Request>(r#"{"id": 1, "op": "nope"}"#).is_err());
    }
}
