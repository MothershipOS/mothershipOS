//! Hardware handles: the universal description of a piece of hardware mothershipOS can give VMs
//! (docs/hardware-handles.md §2b). Every handle has the same structure, whatever it is; the classes (display, gnss,
//! audio, ...) only differ in their contract and caps.
//!
//! Discovery: each device service publishes the handles it serves in /run/mothership/handles/<service>.json
//! ([`HandleFile`], schema [`SCHEMA`]) and rewrites it when its hardware changes; mothershipd reads that directory
//! (files of services that are no longer running are ignored), adds its own built-in handles, and lends the groups.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The directory device services publish their handles in.
pub const DIR: &str = "/run/mothership/handles";
/// The handle file schema. Newer schemas may add fields; a reader ignores what it doesn't know.
pub const SCHEMA: u32 = 1;

/// Which way the data goes, seen from the VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// the VM receives (sensors, location, cameras, microphones, battery)
    In,
    /// the VM sends (display, speaker, earpiece)
    Out,
    /// both ways (Wi-Fi, Bluetooth, USB, modem, GPU, buttons, touch)
    Both,
}

/// How many VMs may use it at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum Sharing {
    /// one holder at a time (mothershipOS or one VM)
    Exclusive,
    /// several VMs at once; `max` None = any number
    Shared {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u32>,
    },
}

impl Sharing {
    pub fn exclusive(self) -> bool {
        self == Sharing::Exclusive
    }
}

/// When an assigned VM gets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lend {
    /// while the VM is shown (it moves with the screen)
    WhileShown,
    /// while the VM runs
    WhileRunning,
    /// every running VM whose manifest has the slot, without assignment (shared handles)
    EveryVm,
}

/// One handle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handle {
    /// `<class>:<name>`; names are generic (panel0, 5g, speaker), never part names
    pub id: String,
    /// the slot contract (`mship.gnss`) and the versions served
    pub contract: String,
    pub versions: Vec<String>,
    /// what the user sees
    pub label: String,
    /// a symbolic icon name
    #[serde(default)]
    pub icon: String,
    pub direction: Direction,
    pub sharing: Sharing,
    pub lend: Lend,
    /// the unit the user assigns (several handles may form one group: display = panel + touch + power key);
    /// None: not assignable (shared with every VM)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// per class facts (resolution, bands, parts, ...)
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub caps: Value,
    /// how the broker lends it: `attach <vm dir name> [arg]`, `detach <vm dir name> <reason> [arg]` on this service's
    /// control socket (datagram, "ok" or "error: ..."). None: mothershipd lends it itself, or nothing to lend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<Control>,
    /// false while the hardware is missing (a monitor unplugged, the modem down)
    #[serde(default = "yes")]
    pub present: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Control {
    pub socket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg: Option<String>,
}

impl Handle {
    /// An exclusive handle lent while its VM runs, version 1.0 of `contract`, in group `group`; the builder methods
    /// change the rest.
    pub fn new(id: &str, contract: &str, label: &str, icon: &str, direction: Direction, group: &str) -> Self {
        Self {
            id: id.into(),
            contract: contract.into(),
            versions: vec!["1.0".into()],
            label: label.into(),
            icon: icon.into(),
            direction,
            sharing: Sharing::Exclusive,
            lend: Lend::WhileRunning,
            group: Some(group.into()),
            caps: Value::Null,
            control: None,
            present: true,
        }
    }

    /// Every running VM with the slot gets it (shared by up to `max` VMs, None = any number); no group.
    pub fn every_vm(mut self, max: Option<u32>) -> Self {
        self.sharing = Sharing::Shared { max };
        self.lend = Lend::EveryVm;
        self.group = None;
        self
    }

    pub fn while_shown(mut self) -> Self {
        self.lend = Lend::WhileShown;
        self
    }

    pub fn control(mut self, socket: &str, arg: Option<&str>) -> Self {
        self.control = Some(Control { socket: socket.into(), arg: arg.map(String::from) });
        self
    }

    pub fn caps(mut self, caps: Value) -> Self {
        self.caps = caps;
        self
    }

    pub fn present(mut self, present: bool) -> Self {
        self.present = present;
        self
    }

    pub fn class(&self) -> &str {
        self.id.split(':').next().unwrap_or(&self.id)
    }

    /// One line on what it is and how it is shared, for people.
    pub fn summary(&self) -> String {
        let dir = match self.direction {
            Direction::In => "to the VM",
            Direction::Out => "from the VM",
            Direction::Both => "both ways",
        };
        let share = match (self.sharing, self.lend) {
            (_, Lend::EveryVm) => match self.sharing {
                Sharing::Shared { max: Some(n) } => format!("shared by up to {n} VMs"),
                _ => "shared by every VM".into(),
            },
            (Sharing::Exclusive, Lend::WhileShown) => "one owner, lent while shown".into(),
            (Sharing::Exclusive, _) => "one owner, lent while it runs".into(),
            (Sharing::Shared { max: Some(n) }, _) => format!("up to {n} VMs"),
            (Sharing::Shared { max: None }, _) => "any number of VMs".into(),
        };
        format!("{share} · {dir}")
    }

    /// The structure every handle must have.
    pub fn check(&self) -> Result<(), String> {
        let (class, name) = self.id.split_once(':').ok_or(format!("{}: id must be <class>:<name>", self.id))?;
        let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !word(class) || !word(name) {
            return Err(format!("{}: id must be <class>:<name> (letters, digits, - and _)", self.id));
        }
        if !self.contract.starts_with("mship.") || self.versions.is_empty() {
            return Err(format!("{}: needs a contract (mship.*) and its versions", self.id));
        }
        if self.versions.iter().any(|v| v.parse::<crate::version::Version>().is_err()) {
            return Err(format!("{}: versions are MAJOR.MINOR", self.id));
        }
        match (self.lend, &self.group) {
            (Lend::EveryVm, Some(_)) => Err(format!("{}: a handle every VM gets has no group", self.id)),
            (Lend::WhileShown | Lend::WhileRunning, None) => Err(format!("{}: an assigned handle needs a group", self.id)),
            (Lend::EveryVm, None) if self.sharing.exclusive() => Err(format!("{}: an exclusive handle can't go to every VM", self.id)),
            (_, Some(g)) if !word(g) => Err(format!("{}: group {g:?} is not a name", self.id)),
            _ => Ok(()),
        }
    }
}

/// A device service's handle file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandleFile {
    pub schema: u32,
    /// the publisher (its file name)
    pub service: String,
    /// its process: a file whose process is gone is stale
    pub pid: u32,
    pub handles: Vec<Handle>,
}

fn path(service: &str) -> PathBuf {
    Path::new(DIR).join(format!("{service}.json"))
}

/// Publish (replace) `service`'s handles, atomically. Invalid handles are refused.
pub fn publish(service: &str, handles: &[Handle]) -> Result<(), String> {
    for h in handles {
        h.check()?;
    }
    let f = HandleFile { schema: SCHEMA, service: service.into(), pid: std::process::id(), handles: handles.to_vec() };
    std::fs::create_dir_all(DIR).map_err(|e| format!("{DIR}: {e}"))?;
    let p = path(service);
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&f).unwrap()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Every published handle of running services, with its service and the service's process (a new process = the
/// service restarted); files of newer schemas are read for what this release knows.
pub fn discover() -> Vec<(String, u32, Handle)> {
    let mut out = vec![];
    for e in std::fs::read_dir(DIR).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(f) = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str::<HandleFile>(&s).ok()) else { continue };
        if !Path::new(&format!("/proc/{}", f.pid)).exists() {
            continue; // stale: its service is gone
        }
        out.extend(f.handles.into_iter().filter(|h| h.check().is_ok()).map(|h| (f.service.clone(), f.pid, h)));
    }
    out.sort_by(|a, b| a.2.id.cmp(&b.2.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gnss() -> Handle {
        Handle::new("gnss:0", "mship.gnss", "Location (GPS)", "find-location-symbolic", Direction::In, "location")
            .caps(serde_json::json!({"constellations": ["gps"]}))
            .control("/run/mothership/gnss.sock", None)
    }

    #[test]
    fn structure() {
        assert!(gnss().check().is_ok());
        let mut h = gnss();
        h.group = None;
        assert!(h.check().is_err()); // assigned, but no group
        h.lend = Lend::EveryVm;
        assert!(h.check().is_err()); // exclusive for every VM
        h.sharing = Sharing::Shared { max: None };
        assert!(h.check().is_ok());
        h.id = "gnss".into();
        assert!(h.check().is_err());
        assert_eq!(gnss().summary(), "one owner, lent while it runs · to the VM");
        let sensors = Handle::new("sensors:0", "mship.sensors", "Sensors", "", Direction::In, "x").every_vm(None);
        assert!(sensors.check().is_ok());
        assert_eq!(sensors.summary(), "shared by every VM · to the VM");
    }

    #[test]
    fn wire_format() {
        let j = serde_json::to_value(gnss()).unwrap();
        assert_eq!(j["sharing"], serde_json::json!({"mode": "exclusive"}));
        assert_eq!(j["lend"], "while_running");
        assert_eq!(j["direction"], "in");
        let shared: Sharing = serde_json::from_value(serde_json::json!({"mode": "shared", "max": 2})).unwrap();
        assert_eq!(shared, Sharing::Shared { max: Some(2) });
        // a newer publisher's extra fields are ignored; present defaults to true
        let mut v = j.clone();
        v["future_field"] = 1.into();
        v.as_object_mut().unwrap().remove("present");
        let h: Handle = serde_json::from_value(v).unwrap();
        assert!(h.present);
    }
}
