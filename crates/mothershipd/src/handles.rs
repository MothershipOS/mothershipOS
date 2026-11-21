//! The handle registry: every handle the device services publish (mship_core::handle, /run/mothership/handles) and
//! mothershipd's own (the screen, the buttons, the external display, the GPU), grouped into what the
//! user assigns. Re-read every policy tick; a change is published on the `handles` topic.

use std::collections::BTreeMap;
use std::sync::Mutex;

use mship_core::api::{GroupInfo, HandleInfo, HandleList};
use mship_core::handle::{self, Direction, Handle, Lend};
use serde_json::json;

pub const SELF: &str = "mothershipd";

/// What mothershipd lends itself (the session's devices).
fn builtins() -> Vec<Handle> {
    vec![
        Handle::new("display:panel0", "mship.display", "Display & touchscreen", "video-display-symbolic", Direction::Out, "display")
            .while_shown()
            .caps(json!({"width": 1080, "height": 2520, "refresh_hz": 90, "dpi": 420})),
        Handle::new("display-power:panel0", "mship.display-power", "Screen power", "video-display-symbolic", Direction::Out, "display").while_shown(),
        Handle::new("touch:panel0", "mship.touch", "Touchscreen", "input-touchpad-symbolic", Direction::In, "display")
            .while_shown()
            .caps(json!({"width": 1080, "height": 2520, "max_contacts": 10})),
        Handle::new("keys:power", "mship.keys", "Power key", "system-shutdown-symbolic", Direction::In, "display")
            .while_shown()
            .caps(json!({"keys": [116, 143]})),
        Handle::new("keys:volume", "mship.keys", "Volume buttons", "audio-volume-high-symbolic", Direction::In, "buttons")
            .while_shown()
            .caps(json!({"keys": [114, 115]})),
        Handle::new("display:dp0", "mship.display", "External display (USB-C)", "video-joined-displays-symbolic", Direction::Out, "display-ext"),
        Handle::new("gpu:0", "mship.gpu", "GPU", "video-display-symbolic", Direction::Both, "").every_vm(None)
            .caps(json!({"context_types": ["drm", "cross-domain", "virgl", "gfxstream-gles", "gfxstream-composer"]})),
    ]
}

#[derive(Default)]
pub struct Registry {
    list: Mutex<HandleList>,
    /// each publishing service's process
    pids: Mutex<BTreeMap<String, u32>>,
}

/// What a refresh found.
#[derive(Default)]
pub struct Changes {
    /// the list changed
    pub list: bool,
    /// services that appeared or restarted (a new process): their groups are lent (again) to the VMs that hold or
    /// are assigned them
    pub started: Vec<String>,
}

/// The groups in a stable order: screen groups first, then by label.
fn groups(handles: &[HandleInfo]) -> Vec<GroupInfo> {
    let mut by: BTreeMap<String, Vec<&Handle>> = BTreeMap::new();
    for h in handles.iter().map(|i| &i.handle) {
        if let Some(g) = &h.group {
            by.entry(g.clone()).or_default().push(h);
        }
    }
    let mut out: Vec<GroupInfo> = by
        .into_iter()
        .map(|(name, hs)| {
            // the group's face: its handle whose class names the group, else its first
            let main = hs.iter().find(|h| name.starts_with(h.class())).copied().unwrap_or(hs[0]);
            GroupInfo {
                name,
                label: main.label.clone(),
                icon: main.icon.clone(),
                lend: main.lend,
                summary: main.summary(),
                handles: hs.iter().map(|h| h.id.clone()).collect(),
                present: hs.iter().any(|h| h.present),
            }
        })
        .collect();
    out.sort_by_key(|g| (g.lend != Lend::WhileShown, g.label.to_lowercase()));
    out
}

impl Registry {
    /// Read the published handles again.
    pub fn refresh(&self) -> Changes {
        let mut handles: Vec<HandleInfo> = builtins().into_iter().map(|h| HandleInfo { service: SELF.into(), handle: h }).collect();
        let mut pids: BTreeMap<String, u32> = BTreeMap::new();
        for (service, pid, h) in handle::discover() {
            pids.insert(service.clone(), pid);
            if handles.iter().any(|x| x.handle.id == h.id) {
                continue; // one owner per id: the first wins
            }
            handles.push(HandleInfo { service, handle: h });
        }
        let started: Vec<String> = {
            let mut old = self.pids.lock().unwrap();
            let s = pids.iter().filter(|(svc, pid)| old.get(*svc) != Some(*pid)).map(|(svc, _)| svc.clone()).collect();
            *old = pids;
            s
        };
        handles.sort_by(|a, b| a.handle.id.cmp(&b.handle.id));
        let new = HandleList { groups: groups(&handles), handles };
        let mut l = self.list.lock().unwrap();
        let list = *l != new;
        *l = new;
        Changes { list, started }
    }

    /// The services whose handles make up a group.
    pub fn services(&self, group: &str) -> Vec<String> {
        let l = self.list.lock().unwrap();
        let mut s: Vec<String> = l.handles.iter().filter(|i| i.handle.group.as_deref() == Some(group)).map(|i| i.service.clone()).collect();
        s.dedup();
        s
    }

    pub fn list(&self) -> HandleList {
        self.list.lock().unwrap().clone()
    }

    pub fn groups(&self) -> Vec<GroupInfo> {
        self.list.lock().unwrap().groups.clone()
    }

    pub fn group(&self, name: &str) -> Option<GroupInfo> {
        self.list.lock().unwrap().groups.iter().find(|g| g.name == name).cloned()
    }

    /// The groups lent with the screen (they move with the shown VM).
    pub fn screen_groups(&self) -> Vec<String> {
        self.groups().into_iter().filter(|g| g.lend == Lend::WhileShown).map(|g| g.name).collect()
    }

    /// How a group is lent: the control sockets of its handles (with each one's argument).
    pub fn controls(&self, group: &str) -> Vec<(String, Option<String>)> {
        let l = self.list.lock().unwrap();
        let mut out: Vec<(String, Option<String>)> = vec![];
        for h in l.handles.iter().map(|i| &i.handle).filter(|h| h.group.as_deref() == Some(group)) {
            if let Some(c) = &h.control
                && !out.iter().any(|(s, a)| *s == c.socket && *a == c.arg)
            {
                out.push((c.socket.clone(), c.arg.clone()));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_groups() {
        let hs: Vec<HandleInfo> = builtins().into_iter().map(|h| HandleInfo { service: SELF.into(), handle: h }).collect();
        for h in &hs {
            h.handle.check().unwrap();
        }
        let g = groups(&hs);
        let names: Vec<&str> = g.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(&names[..2], ["display", "buttons"]); // screen groups first
        let d = g.iter().find(|g| g.name == "display").unwrap();
        assert_eq!(d.label, "Display & touchscreen");
        assert_eq!(d.handles.len(), 4);
        assert!(!names.contains(&"gpu")); // shared by every VM: not assignable
    }
}
