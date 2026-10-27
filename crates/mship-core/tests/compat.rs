//! Compatibility promises, checked against the repository: the frozen V1 manifests (tests/compat/manifests) load with
//! the same meaning, the manifests of the VMs next to mothershipOS run on this release, docs/compat.md agrees with the registry,
//! every contract and platform major has its spec, and VERSION is this release.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mship_core::contracts::{self, CONTRACTS, MANIFEST_SCHEMAS, PLATFORMS, Table};
use mship_core::manifest::{self, Manifest};
use mship_core::version::{Release, Version};
use serde_json::Value;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures() -> PathBuf {
    root().join("tests/compat/manifests")
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// A VM directory holding `src` as its vm.json.
struct VmDir(PathBuf);

impl VmDir {
    fn with(src: &Path) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("mship-compat-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::copy(src, d.join("vm.json")).unwrap();
        VmDir(d)
    }
}

impl Drop for VmDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    v.sort();
    v
}

fn name(p: &Path) -> String {
    p.file_name().unwrap().to_string_lossy().into_owned()
}

// ---------------------------------------------------------------- frozen fixtures

#[test]
fn v1_fixtures_keep_their_meaning() {
    let expected = read_json(&fixtures().join("expected.json"));
    let files = json_files(&fixtures().join("v1"));
    let names: Vec<String> = files.iter().map(|f| name(f)).collect();
    let mut want: Vec<String> = expected.as_object().unwrap().keys().cloned().collect();
    want.sort();
    assert_eq!(names, want, "every frozen fixture needs an expected.json entry");
    for f in files {
        let e = &expected[name(&f)];
        let d = VmDir::with(&f);
        let m = manifest::load(&d.0).unwrap_or_else(|e| panic!("{}: {e}", name(&f)));
        let c = manifest::check(&m, contracts::release());
        assert_eq!(m.source_schema, 1, "{}", name(&f));
        assert_eq!(c.runnable, e["runnable"].as_bool().unwrap(), "{}: {:?}", name(&f), c.errors);
        let empty: Vec<String> = serde_json::from_value(e["empty_slots"].clone()).unwrap();
        assert_eq!(c.empty_slots, empty, "{}", name(&f));
        let lent: Vec<String> = serde_json::from_value(e["lent"].clone()).unwrap();
        assert_eq!(m.lent_groups(), lent, "{}", name(&f));
        assert_eq!(m.data["platform"]["id"], e["platform"], "{}", name(&f));
    }
}

#[test]
fn invalid_fixtures_are_refused() {
    for f in json_files(&fixtures().join("invalid")) {
        assert!(manifest::parse(&read_json(&f)).is_err(), "{} was accepted", name(&f));
    }
}

#[test]
fn refusals_say_why() {
    let err = |f: &str| manifest::parse(&read_json(&fixtures().join("invalid").join(f))).unwrap_err().to_string();
    assert!(err("future-schema.json").contains("2031.2"));
    assert!(err("pre-v1.json").contains("pre-V1"));
}

#[test]
fn shipped_manifests_run_on_this_release() {
    let mut found = vec![];
    // the VMs checked out next to mothershipOS (each its own repository): every vm.json in them runs
    let parent = root().join("..");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&parent).unwrap().flatten().map(|e| e.path())
        .filter(|p| p.is_dir() && !p.ends_with("mothershipOS") && !p.ends_with("motorola_nio")).collect();
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.') {
                dirs.push(e.path());
            }
        }
        if d.join("vm.json").is_file() {
            let m = manifest::load(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display()));
            let c = manifest::check(&m, contracts::release());
            assert!(c.runnable, "{}: {:?}", d.display(), c.errors);
            found.push(d);
        }
    }
    // a mothershipOS checkout alone has none to check
    println!("VM manifests checked: {found:?}");
}

#[test]
fn handover_write_back_keeps_everything_else() {
    let d = VmDir::with(&fixtures().join("v1/forward-compatible.json"));
    let before = read_json(&d.0.join("vm.json"));
    let mut m = manifest::load(&d.0).unwrap();
    manifest::update_handover(&mut m, &["buttons"], &["internet"]).unwrap();
    let after = read_json(&d.0.join("vm.json"));
    // a group from a newer release survives, the known ones follow the user
    assert_eq!(after["handover"], serde_json::json!({"when_shown": ["buttons", "camera"], "while_running": ["internet"]}));
    for (k, v) in before.as_object().unwrap() {
        if k != "handover" {
            assert_eq!(&after[k], v, "{k}");
        }
    }
    assert_eq!(after["schema"], 1);
    assert_eq!(manifest::load(&d.0).unwrap().lent_groups(), ["buttons", "internet"]);
}

#[test]
fn min_mothership_is_checked() {
    let data = serde_json::json!({"schema": 1, "name": "x", "min_mothership": "2027.3",
                                  "platform": {"id": "efi-virt-arm64", "version": "1.0"}});
    let m = Manifest { path: PathBuf::from("/x"), data: data.as_object().unwrap().clone(), source_schema: 1, raw: Default::default() };
    let c = manifest::check(&m, "2026.1".parse::<Release>().unwrap());
    assert!(!c.runnable);
    assert!(c.errors[0].contains("2027.3"), "{:?}", c.errors);
}

// ---------------------------------------------------------------- docs/compat.md and the specs

/// The rows of the table under `## heading` (without its header row).
fn table(md: &str, heading: &str) -> Vec<Vec<String>> {
    let sec = md.split(&format!("## {heading}\n")).nth(1).unwrap_or_else(|| panic!("docs/compat.md: no ## {heading}"));
    let sec = sec.split("\n## ").next().unwrap();
    let rows: Vec<Vec<String>> = sec
        .lines()
        .filter(|r| r.starts_with('|') && !r.trim_start_matches('|').trim_start().starts_with('-'))
        .map(|r| r.trim().trim_matches('|').split('|').map(|c| c.trim().to_string()).collect())
        .collect();
    rows.into_iter().skip(1).collect()
}

fn dash(v: &str) -> Option<String> {
    (!matches!(v, "—" | "-" | "")).then(|| v.to_string())
}

type Rows = BTreeMap<(String, String), (String, Option<String>, Option<String>)>;

fn registry_rows(t: Table) -> Rows {
    t.iter()
        .flat_map(|(n, es)| {
            es.iter().map(move |e| {
                ((n.to_string(), e.version.to_string()), (e.introduced.to_string(), e.deprecated.map(String::from), e.removed.map(String::from)))
            })
        })
        .collect()
}

fn doc_rows(md: &str, heading: &str) -> Rows {
    table(md, heading).into_iter().map(|r| ((r[0].clone(), r[1].clone()), (r[2].clone(), dash(&r[3]), dash(&r[4])))).collect()
}

#[test]
fn compat_doc_matches_the_registry() {
    let md = std::fs::read_to_string(root().join("docs/compat.md")).unwrap();
    assert_eq!(doc_rows(&md, "Contracts"), registry_rows(CONTRACTS));
    assert_eq!(doc_rows(&md, "Platforms"), registry_rows(PLATFORMS));
    let schemas: BTreeMap<u32, String> = table(&md, "Manifest schemas").into_iter().map(|r| (r[0].parse().unwrap(), r[1].clone())).collect();
    let code: BTreeMap<u32, String> = MANIFEST_SCHEMAS.iter().map(|(s, r)| (*s, r.to_string())).collect();
    assert_eq!(schemas, code);
}

#[test]
fn every_major_has_a_spec() {
    for (folder, t) in [("contracts", CONTRACTS), ("platforms", PLATFORMS)] {
        for (name, entries) in t {
            for e in *entries {
                let major = e.version.parse::<Version>().unwrap().major;
                let f = root().join("docs").join(folder).join(format!("{name}-{major}.md"));
                assert!(f.is_file(), "missing spec {}", f.display());
            }
        }
    }
}

#[test]
fn version_file_is_this_release() {
    assert_eq!(std::fs::read_to_string(root().join("VERSION")).unwrap().trim(), mship_core::RELEASE);
}
