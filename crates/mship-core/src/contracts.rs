//! Registry of every contract, platform and manifest schema mothershipOS has shipped since V1 (2026.1).
//!
//! Entries are never deleted: a removed version keeps its row with `removed` set, so mothershipOS can tell a VM
//! *why* it cannot run ("uses mship.touch 1.x, removed in 2028.1") instead of "unknown". `docs/compat.md` is the
//! human-readable copy; tests keep both in sync.

use crate::RELEASE;
use crate::version::{Release, Version};

/// One major version of a contract or platform, as shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// MAJOR.MINOR: the highest minor of that major this release implements
    pub version: &'static str,
    pub introduced: &'static str,
    pub deprecated: Option<&'static str>,
    /// the first release that no longer implements it
    pub removed: Option<&'static str>,
}

impl Entry {
    pub const fn new(version: &'static str, introduced: &'static str) -> Self {
        Self { version, introduced, deprecated: None, removed: None }
    }
}

pub type Table = &'static [(&'static str, &'static [Entry])];

/// contract -> one entry per major (the version is the newest minor of that major)
pub const CONTRACTS: Table = &[
    ("mship.display", &[Entry::new("1.0", "2026.1")]),
    ("mship.display-power", &[Entry::new("1.0", "2026.1")]),
    ("mship.touch", &[Entry::new("1.0", "2026.1")]),
    ("mship.keys", &[Entry::new("1.0", "2026.1")]),
    ("mship.net-shared", &[Entry::new("1.0", "2026.1")]),
    ("mship.gpu", &[Entry::new("1.0", "2026.1")]),
    ("mship.wifi", &[Entry::new("1.0", "2026.1")]),
    ("mship.bt", &[Entry::new("1.0", "2026.1")]),
    ("mship.battery", &[Entry::new("1.0", "2026.1")]),
    ("mship.usb", &[Entry::new("1.0", "2026.1")]),
    ("mship.modem", &[Entry::new("1.0", "2026.1")]),
    ("mship.camera", &[Entry::new("1.0", "2026.1")]),
];

pub const PLATFORMS: Table = &[
    ("cuttlefish-arm64", &[Entry::new("1.0", "2026.1")]),
    ("efi-virt-arm64", &[Entry::new("1.0", "2026.1")]),
];

/// manifest schema -> the release that introduced it (every schema stays readable forever). V1 = schema 1.
pub const MANIFEST_SCHEMAS: &[(u32, &str)] = &[(1, "2026.1")];
pub const CURRENT_SCHEMA: u32 = 1;

/// Can a release serve a wanted contract/platform version?
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Support {
    /// the release can serve it
    pub ok: bool,
    pub deprecated: bool,
    /// for the UI / logs when not ok or deprecated
    pub reason: String,
    pub implemented: Option<Version>,
}

impl Support {
    fn no(reason: String) -> Self {
        Self { ok: false, deprecated: false, reason, implemented: None }
    }
}

fn rel(s: &str) -> Release {
    s.parse().expect("registry releases are valid")
}

fn ver(s: &str) -> Version {
    s.parse().expect("registry versions are valid")
}

pub fn check(table: Table, kind: &str, name: &str, wanted: &str, release: Release) -> Support {
    let wanted: Version = match wanted.parse() {
        Ok(v) => v,
        Err(e) => return Support::no(format!("{kind} {name}: {e}")),
    };
    let Some((_, entries)) = table.iter().find(|(n, _)| *n == name) else {
        return Support::no(format!("unknown {kind} {name} (needs a newer mothershipOS?)"));
    };
    for e in entries.iter() {
        let v = ver(e.version);
        if v.major != wanted.major {
            continue;
        }
        if rel(e.introduced) > release {
            return Support::no(format!("{kind} {name} {}.x needs mothershipOS >= {}", v.major, e.introduced));
        }
        if let Some(removed) = e.removed.filter(|r| rel(r) <= release) {
            return Support::no(format!("{kind} {name} {}.x was removed in mothershipOS {removed}", v.major));
        }
        if !v.satisfies(wanted) {
            return Support::no(format!("{kind} {name} {wanted} is newer than this mothershipOS implements ({v})"));
        }
        let deprecated = e.deprecated.is_some_and(|d| rel(d) <= release);
        let reason = if deprecated {
            format!(
                "{kind} {name} {}.x is deprecated since {}{}",
                v.major,
                e.deprecated.unwrap(),
                e.removed.map(|r| format!(", removed in {r}")).unwrap_or_default()
            )
        } else {
            String::new()
        };
        return Support { ok: true, deprecated, reason, implemented: Some(v) };
    }
    Support::no(format!("{kind} {name} {}.x is not known to this mothershipOS", wanted.major))
}

pub fn release() -> Release {
    rel(RELEASE)
}

pub fn contract_support(name: &str, wanted: &str, release: Release) -> Support {
    check(CONTRACTS, "contract", name, wanted, release)
}

pub fn platform_support(name: &str, wanted: &str, release: Release) -> Support {
    check(PLATFORMS, "platform", name, wanted, release)
}

/// Versions of a contract a release implements (for channel hellos).
pub fn implemented(table: Table, name: &str, release: Release) -> Vec<Version> {
    table
        .iter()
        .filter(|(n, _)| *n == name)
        .flat_map(|(_, es)| es.iter())
        .filter(|e| rel(e.introduced) <= release && !e.removed.is_some_and(|r| rel(r) <= release))
        .map(|e| ver(e.version))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Release {
        s.parse().unwrap()
    }

    #[test]
    fn v1_baseline() {
        for (name, _) in CONTRACTS {
            let s = contract_support(name, "1.0", r("2026.1"));
            assert!(s.ok, "{}", s.reason);
        }
        for (name, _) in PLATFORMS {
            assert!(platform_support(name, "1.0", r("2026.1")).ok);
        }
        assert_eq!(CURRENT_SCHEMA, 1);
    }

    #[test]
    fn reasons() {
        assert!(contract_support("mship.teleport", "1.0", release()).reason.contains("unknown contract"));
        assert!(contract_support("mship.touch", "1.5", r("2026.1")).reason.contains("newer than this"));
        assert!(contract_support("mship.touch", "3.0", r("2026.1")).reason.contains("not known"));
    }

    #[test]
    fn lifecycle() {
        const X: Table = &[(
            "mship.x",
            &[
                Entry { version: "1.4", introduced: "2026.1", deprecated: Some("2027.1"), removed: Some("2028.1") },
                Entry::new("2.0", "2027.1"),
            ],
        )];
        assert!(!check(X, "contract", "mship.x", "2.0", r("2026.3")).ok); // not yet
        let s = check(X, "contract", "mship.x", "1.2", r("2027.2"));
        assert!(s.ok && s.deprecated);
        assert!(s.reason.contains("removed in 2028.1"));
        let s = check(X, "contract", "mship.x", "1.0", r("2028.1"));
        assert!(!s.ok);
        assert!(s.reason.contains("removed in mothershipOS 2028.1"));
        let names = |rel| implemented(X, "mship.x", rel).iter().map(|v| v.to_string()).collect::<Vec<_>>();
        assert_eq!(names(r("2027.5")), ["1.4", "2.0"]);
        assert_eq!(names(r("2028.1")), ["2.0"]);
    }

    #[test]
    fn registry_is_append_only_shape() {
        for table in [CONTRACTS, PLATFORMS] {
            for (name, entries) in table {
                let mut majors: Vec<u32> = entries.iter().map(|e| ver(e.version).major).collect();
                majors.sort();
                majors.dedup();
                assert_eq!(majors.len(), entries.len(), "{name}: one entry per major");
                for e in entries.iter() {
                    let seq: Vec<Release> =
                        [Some(e.introduced), e.deprecated, e.removed].into_iter().flatten().map(rel).collect();
                    let mut sorted = seq.clone();
                    sorted.sort();
                    assert_eq!(seq, sorted, "{name} {}: introduced <= deprecated <= removed", e.version);
                }
            }
        }
    }
}
