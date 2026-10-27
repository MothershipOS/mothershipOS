//! VM manifests (`vm.json`). Schema 1 = V1 (mothershipOS 2026.1), the compatibility baseline.
//!
//! Every schema from 1 on stays readable forever. When a schema N+1 appears, add a pure function to
//! [`MIGRATIONS`] that turns an N file into the N+1 shape (in memory); writing back keeps the file in its own
//! schema so the VM still works on the release it was made for. Files older than V1 are not supported (convert
//! them once).

use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};

use crate::contracts::{CURRENT_SCHEMA, MANIFEST_SCHEMAS, contract_support, platform_support};
use crate::version::Release;

/// What the user lends (the handover checklist). wifi-any = single-band radios (no per-band handles).
pub const GROUPS: &[&str] =
    &["display", "buttons", "wifi-2g", "wifi-5g", "wifi-any", "internet", "bluetooth", "usb", "display-ext", "cellular", "cameras"];

/// schema N -> a function turning an N manifest into the N+1 shape. Empty while schema 1 is the only one.
pub type Migration = fn(Value) -> Value;
pub const MIGRATIONS: &[(u32, Migration)] = &[];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ManifestError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, ManifestError> {
    Err(ManifestError(msg.into()))
}

#[derive(Debug, Clone)]
pub struct Manifest {
    /// the VM directory (absolute)
    pub path: PathBuf,
    /// the current-schema view
    pub data: Map<String, Value>,
    /// the schema of the file on disk
    pub source_schema: u32,
    /// the file as read (written back in its own schema)
    pub raw: Map<String, Value>,
}

impl Manifest {
    pub fn name(&self) -> &str {
        self.data["name"].as_str().unwrap_or_default()
    }

    /// (when_shown, while_running): the groups this release knows
    pub fn handover(&self) -> (Vec<String>, Vec<String>) {
        let h = self.data.get("handover").and_then(Value::as_object);
        let list = |k: &str| -> Vec<String> {
            h.and_then(|h| h.get(k))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).filter(|g| GROUPS.contains(g)).map(String::from).collect())
                .unwrap_or_default()
        };
        (list("when_shown"), list("while_running"))
    }

    pub fn lent_groups(&self) -> Vec<String> {
        let (shown, running) = self.handover();
        GROUPS
            .iter()
            .filter(|g| shown.iter().any(|s| s == *g) || running.iter().any(|s| s == *g))
            .map(|g| g.to_string())
            .collect()
    }

    /// a path from the manifest's `boot`, resolved against the VM directory
    pub fn file(&self, key: &str) -> Option<PathBuf> {
        self.data.get("boot")?.get(key)?.as_str().map(|v| self.path.join(v))
    }

    pub fn platform(&self) -> (&str, &str) {
        let p = &self.data["platform"];
        (p["id"].as_str().unwrap_or_default(), p["version"].as_str().unwrap_or_default())
    }
}

fn inside(path: &str) -> bool {
    let p = Path::new(path);
    !p.is_absolute() && !p.components().any(|c| matches!(c, Component::ParentDir))
}

fn validate(d: &Map<String, Value>) -> Result<(), ManifestError> {
    if !d.get("name").and_then(Value::as_str).is_some_and(|n| !n.trim().is_empty()) {
        return err(r#"manifest needs a "name""#);
    }
    let p = d.get("platform").and_then(Value::as_object);
    if !p.is_some_and(|p| p.get("id").is_some_and(Value::is_string) && p.contains_key("version")) {
        return err(r#"manifest needs "platform": {"id": ..., "version": ...}"#);
    }
    match d.get("slots") {
        None => {}
        Some(Value::Object(slots)) => {
            for (slot, s) in slots {
                if !s.get("contract").is_some_and(Value::is_string) || s.get("version").is_none() {
                    return err(format!(r#"slot "{slot}" needs "contract" and "version""#));
                }
            }
        }
        Some(_) => return err(r#""slots" must be an object"#),
    }
    match d.get("handover") {
        None => {}
        Some(Value::Object(h)) if ["when_shown", "while_running"].iter().all(|k| h.get(*k).is_none_or(Value::is_array)) => {}
        Some(_) => return err(r#""handover" must be {"when_shown": [...], "while_running": [...]}"#),
    }
    for key in ["boot", "resources"] {
        if d.get(key).is_some_and(|v| !v.is_object()) {
            return err(format!(r#""{key}" must be an object"#));
        }
    }
    // debugging: variables for the VM's launcher and everything it starts (crosvm, the presenter)
    if d.get("env").is_some_and(|e| !e.as_object().is_some_and(|e| e.values().all(Value::is_string))) {
        return err(r#""env" must be {"NAME": "value", ...}"#);
    }
    if let Some(boot) = d.get("boot").and_then(Value::as_object) {
        for (k, v) in boot {
            if !v.as_str().is_some_and(inside) {
                return err(format!("boot.{k} must be a path inside the VM directory"));
            }
        }
    }
    Ok(())
}

/// File content -> (current-schema view, source schema). Pure.
pub fn parse(raw: &Value) -> Result<(Map<String, Value>, u32), ManifestError> {
    let Some(obj) = raw.as_object() else { return err("vm.json is not an object") };
    let Some(schema) = obj.get("schema").and_then(Value::as_u64).map(|s| s as u32) else {
        return err(r#"vm.json has no "schema" (pre-V1 manifest? convert it to schema 1)"#);
    };
    if schema > CURRENT_SCHEMA {
        let need = obj.get("min_mothership").and_then(Value::as_str).unwrap_or("newer than this");
        return err(format!("vm.json schema {schema} needs mothershipOS {need}"));
    }
    if !MANIFEST_SCHEMAS.iter().any(|(s, _)| *s == schema) {
        return err(format!("unknown vm.json schema {schema}"));
    }
    let mut d = raw.clone();
    for n in schema..CURRENT_SCHEMA {
        let migrate = MIGRATIONS.iter().find(|(s, _)| *s == n).map(|(_, f)| f).expect("a migration per schema");
        d = migrate(d);
    }
    let mut d = d.as_object().cloned().unwrap_or_default();
    d.insert("schema".into(), CURRENT_SCHEMA.into());
    validate(&d)?;
    Ok((d, schema))
}

pub fn is_vm_dir(vm_dir: &Path) -> bool {
    vm_dir.join("vm.json").is_file()
}

pub fn load(vm_dir: &Path) -> Result<Manifest, ManifestError> {
    let f = vm_dir.join("vm.json");
    let text = std::fs::read_to_string(&f)
        .map_err(|e| ManifestError(format!("{}: no vm.json ({e})", vm_dir.display())))?;
    let raw: Value = serde_json::from_str(&text).map_err(|e| ManifestError(format!("{}: {e}", f.display())))?;
    let (data, source_schema) = parse(&raw)?;
    Ok(Manifest {
        path: vm_dir.canonicalize().unwrap_or_else(|_| vm_dir.to_path_buf()),
        data,
        source_schema,
        raw: raw.as_object().cloned().unwrap_or_default(),
    })
}

fn write(path: &Path, obj: &Map<String, Value>) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(obj)? + "\n")?;
    std::fs::rename(tmp, path)
}

/// The user's checklist changed. Rewrites only `handover`, in the file's own schema; groups this release
/// doesn't know (written by a newer one) are kept.
pub fn update_handover(m: &mut Manifest, when_shown: &[&str], while_running: &[&str]) -> std::io::Result<()> {
    let mut raw = m.raw.clone();
    let h = raw.entry("handover").or_insert_with(|| Value::Object(Map::new()));
    let h = h.as_object_mut().expect("validated: handover is an object");
    for (k, new) in [("when_shown", when_shown), ("while_running", while_running)] {
        let mut list: Vec<Value> = GROUPS.iter().filter(|g| new.contains(g)).map(|g| Value::from(*g)).collect();
        if let Some(old) = h.get(k).and_then(Value::as_array) {
            list.extend(old.iter().filter(|g| g.as_str().is_some_and(|g| !GROUPS.contains(&g))).cloned());
        }
        h.insert(k.into(), Value::Array(list));
    }
    write(&m.path.join("vm.json"), &raw)?;
    m.data.insert("handover".into(), raw["handover"].clone());
    m.raw = raw;
    Ok(())
}

/// Can a release run the VM?
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Check {
    pub runnable: bool,
    /// why it cannot start
    pub errors: Vec<String>,
    /// deprecations, optional slots that stay empty
    pub warnings: Vec<String>,
    /// optional slots this release cannot serve
    pub empty_slots: Vec<String>,
}

/// Can `release` run the VM? Pure apart from the registry.
pub fn check(m: &Manifest, release: Release) -> Check {
    let mut c = Check::default();
    if let Some(need) = m.data.get("min_mothership").and_then(Value::as_str) {
        match need.parse::<Release>() {
            Ok(r) if r > release => c.errors.push(format!("needs mothershipOS >= {need} (this is {release})")),
            Ok(_) => {}
            Err(_) => c.errors.push(format!("bad min_mothership {need:?}")),
        }
    }
    let (pid, pver) = m.platform();
    let ps = platform_support(pid, pver, release);
    if !ps.ok {
        c.errors.push(ps.reason);
    } else if ps.deprecated {
        c.warnings.push(ps.reason);
    }
    let mut slots: Vec<(&String, &Value)> =
        m.data.get("slots").and_then(Value::as_object).map(|s| s.iter().collect()).unwrap_or_default();
    slots.sort_by(|a, b| a.0.cmp(b.0));
    for (slot, s) in slots {
        let version = match &s["version"] {
            Value::String(v) => v.clone(),
            other => other.to_string(),
        };
        let cs = contract_support(s["contract"].as_str().unwrap_or_default(), &version, release);
        if !cs.ok {
            if s.get("required").and_then(Value::as_bool).unwrap_or(true) {
                c.errors.push(format!("slot {slot}: {}", cs.reason));
            } else {
                c.empty_slots.push(slot.clone());
                c.warnings.push(format!("slot {slot} stays empty: {}", cs.reason));
            }
        } else if cs.deprecated {
            c.warnings.push(format!("slot {slot}: {}", cs.reason));
        }
    }
    c.runnable = c.errors.is_empty();
    c
}
