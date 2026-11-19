//! The VM store: a vmstore server (github.com/MothershipOS/vmstore) whose VMs the shell lists and installs.
//!
//! The store's address is a setting (/etc/mothership/store.json). Its list (index.json) and icons are fetched here,
//! the icons cached where the shell can read them. An install downloads the package (resumable, with progress) and
//! hands it to `mship-system vm-install`, which gives a package no root (docs/vm-packages.md). Only the configured
//! store's packages are installed, by id: the shell names no URL.
//!
//! Updates: an installed VM's version is its package's (mothership-package.json "version", in the VM's directory),
//! or the store's it was installed from. The store is checked at start, every 6 hours and whenever the shell lists
//! it; a VM whose store version differs from the installed one has an update.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use mship_core::api::{StoreJob, StoreList, StoreStatus, StoreVm};
use mship_host::util::{Result, fail, load_json, save_json};
use mship_host::vm;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const CONF: &str = "/etc/mothership/store.json";
/// installed VMs: id -> the store version (root's: not in the VM's directory, which the session user owns)
const INSTALLED: &str = "/var/lib/mothership/store-installed.json";
const ICONS: &str = "/var/cache/mothership/store";
const DOWNLOADS: &str = "/var/lib/mothership/build";
/// the store when none is set
const DEFAULT_URL: &str = "https://vmstore.buzz-ltd.com";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const ARCHIVES: &[&str] = &[".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst"];
const ICON_TYPES: &[&str] = &[".png", ".svg", ".jpg", ".jpeg", ".webp"];

static JOBS: Mutex<BTreeMap<String, StoreJob>> = Mutex::new(BTreeMap::new());
/// the last read of the store: (when, id -> version)
static LAST: Mutex<(u64, BTreeMap<String, String>)> = Mutex::new((0, BTreeMap::new()));

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Conf {
    url: String,
}

pub fn url() -> String {
    let c: Conf = load_json(CONF);
    if c.url.is_empty() { DEFAULT_URL.into() } else { c.url }
}

pub fn set_url(url: &str) -> Result<StoreStatus> {
    let url = url.trim().trim_end_matches('/');
    if !url.is_empty() && !valid_url(url) {
        return fail("the store's address: http://host[:port] or https://host[:port], optionally with a path");
    }
    save_json(CONF, &Conf { url: url.into() })?;
    Ok(status())
}

fn valid_url(u: &str) -> bool {
    let rest = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://"));
    rest.is_some_and(|r| {
        !r.is_empty() && r.len() < 256 && r.chars().all(|c| c.is_ascii_alphanumeric() || "-._~:/[]%".contains(c))
            && !r.starts_with('/')
    })
}

/// a store id is a VM directory name (as mship-system vm-install takes it)
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
}

/// `<id>/<name>` of the store: the paths index.json gives, nothing else
fn store_path(id: &str, p: &str, stem: &str, types: &[&str]) -> bool {
    p.strip_prefix(id).and_then(|r| r.strip_prefix('/')).and_then(|f| f.strip_prefix(stem))
        .is_some_and(|ext| types.contains(&ext))
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let o = Command::new("curl").args(["-fsSL", "--proto", "=http,https", "--max-redirs", "3"]).args(args)
        .stdin(Stdio::null()).output()?;
    if !o.status.success() {
        return fail(String::from_utf8_lossy(&o.stderr).trim().trim_start_matches("curl: ").to_string());
    }
    Ok(o.stdout)
}

/// index.json: (id, package path, the VM) for each valid entry
fn index(base: &str) -> Result<Vec<(String, StoreVm, Option<String>)>> {
    if base.is_empty() {
        return fail("no store set");
    }
    let raw = curl(&["--max-time", "20", "--max-filesize", "4000000", &format!("{base}/index.json")])?;
    let v: Value = serde_json::from_slice(&raw).map_err(|e| mship_host::util::Error(format!("the store's index.json: {e}")))?;
    let mut out = vec![];
    for e in v.as_array().into_iter().flatten() {
        let id = e["id"].as_str().unwrap_or_default().to_string();
        let package = e["package"].as_str().unwrap_or_default().to_string();
        if !valid_id(&id) || !store_path(&id, &package, "package", ARCHIVES) {
            continue;
        }
        let icon = e["icon"].as_str().filter(|i| store_path(&id, i, "icon", ICON_TYPES)).map(String::from);
        let text = |k: &str, max: usize| e[k].as_str().unwrap_or_default().chars().take(max).collect::<String>();
        let vm = StoreVm {
            name: Some(text("name", 80)).filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone()),
            version: text("version", 40),
            summary: text("summary", 300),
            description: text("description", 4000),
            size: e["size"].as_u64().unwrap_or(0),
            icon: None,
            id: id.clone(),
        };
        out.push((package, vm, icon));
    }
    Ok(out)
}

/// The store's VMs, their icons fetched into the cache.
pub fn list() -> StoreList {
    let base = url();
    match index(&base) {
        Ok(entries) => {
            let _ = std::fs::create_dir_all(ICONS);
            let vms: Vec<StoreVm> = entries.into_iter().map(|(_, mut vm, icon)| {
                vm.icon = icon.and_then(|i| {
                    let ext = i.rsplit('.').next().unwrap_or("png");
                    let file = Path::new(ICONS).join(format!("{}.{ext}", vm.id));
                    let tmp = file.with_extension("part");
                    let got = curl(&["--max-time", "10", "--max-filesize", "1000000", "-o", &tmp.to_string_lossy(), &format!("{base}/{i}")]);
                    if got.is_ok() {
                        let _ = std::fs::rename(&tmp, &file);
                    } else {
                        let _ = std::fs::remove_file(&tmp);
                    }
                    file.exists().then(|| file.to_string_lossy().into_owned())
                });
                vm
            }).collect();
            remember(&vms);
            StoreList { url: base, vms, error: None }
        }
        Err(e) => StoreList { url: base, vms: vec![], error: Some(e.0) },
    }
}

fn remember(vms: &[StoreVm]) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    *LAST.lock().unwrap() = (now, vms.iter().map(|v| (v.id.clone(), v.version.clone())).collect());
}

/// an installed VM's version: its package's, else the store's it came from
fn version(id: &str, recorded: &BTreeMap<String, String>) -> String {
    let pkg: Value = load_json(Path::new(vm::VMS).join(id).join("mothership-package.json"));
    pkg["version"].as_str().map(String::from).filter(|v| !v.is_empty())
        .or_else(|| recorded.get(id).cloned()).unwrap_or_default()
}

fn installed() -> BTreeMap<String, String> {
    let recorded: BTreeMap<String, String> = load_json(INSTALLED);
    vm::names().into_iter()
        .filter(|n| Path::new(vm::VMS).join(n).join("vm.json").exists())
        .map(|n| { let v = version(&n, &recorded); (n, v) })
        .collect()
}

pub fn status() -> StoreStatus {
    let installed = installed();
    let (checked, available) = LAST.lock().unwrap().clone();
    let updates = installed.iter().filter(|(id, have)| available.get(*id).is_some_and(|v| v != *have)).map(|(id, _)| id.clone()).collect();
    StoreStatus { url: url(), jobs: JOBS.lock().unwrap().clone(), installed, available, updates, checked }
}

/// Check the store for updates now and every 6 hours.
pub fn watch() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(60)); // after the boot's rush, then every CHECK_EVERY
        let l = list();
        if let Some(e) = l.error {
            println!("store: {e}");
        } else {
            let s = status();
            if !s.updates.is_empty() {
                println!("store: updates for {}", s.updates.join(", "));
            }
        }
        std::thread::sleep(CHECK_EVERY.saturating_sub(Duration::from_secs(60)));
    });
}

fn set_job(id: &str, phase: &str, done: u64, total: u64, detail: &str) {
    JOBS.lock().unwrap().insert(id.into(), StoreJob { phase: phase.into(), done, total, detail: detail.into() });
}

fn free_bytes(p: &str) -> u64 {
    let c = std::ffi::CString::new(p).unwrap();
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return u64::MAX;
    }
    s.f_bavail as u64 * s.f_frsize as u64
}

/// Start installing `id` from the store: download, then mship-system vm-install. Progress in [`status`].
pub fn install(id: &str) -> Result<StoreStatus> {
    if !valid_id(id) {
        return fail("not a store VM id");
    }
    if JOBS.lock().unwrap().get(id).is_some_and(|j| j.phase != "failed") {
        return fail(format!("{id} is being installed already"));
    }
    if vm::running(&Path::new(vm::VMS).join(id)) {
        return fail(format!("{id} is running: stop it first"));
    }
    let base = url();
    let entries = index(&base)?;
    remember(&entries.iter().map(|(_, v, _)| v.clone()).collect::<Vec<_>>());
    let Some((package, vm, _)) = entries.into_iter().find(|(_, v, _)| v.id == id) else {
        return fail(format!("the store has no {id}"));
    };
    // the archive, its unpacked copy and the VM's disk made from it
    std::fs::create_dir_all(DOWNLOADS)?;
    let need = vm.size.saturating_mul(3);
    if free_bytes(DOWNLOADS) < need {
        return fail(format!("not enough space: {id} needs about {} GB free", need.div_ceil(1 << 30)));
    }
    set_job(id, "downloading", 0, vm.size, "");
    let id = id.to_string();
    std::thread::spawn(move || {
        let ext = ARCHIVES.iter().find(|x| package.ends_with(*x)).copied().unwrap_or(".tar");
        let file = PathBuf::from(DOWNLOADS).join(format!("store-{id}{ext}"));
        // a failed download stays, to resume on the next try; a downloaded package goes once it was installed (or not)
        let r = download(&id, &format!("{base}/{package}"), &file, vm.size).and_then(|_| {
            let r = run_install(&id, &file);
            let _ = std::fs::remove_file(&file);
            r
        });
        match r {
            Ok(()) => {
                let mut rec: BTreeMap<String, String> = load_json(INSTALLED);
                rec.insert(id.clone(), vm.version.clone());
                let _ = save_json(INSTALLED, &rec);
                // the store's version is what is installed now (also for a package that states none)
                LAST.lock().unwrap().1.insert(id.clone(), vm.version.clone());
                JOBS.lock().unwrap().remove(&id);
                println!("store: {id} {} installed", vm.version);
            }
            Err(e) => {
                println!("store: {id}: {}", e.0);
                set_job(&id, "failed", 0, 0, &e.0);
            }
        }
    });
    Ok(status())
}

/// curl into `file` (a broken download resumes, also on a later try), its size as the progress
fn download(id: &str, url: &str, file: &Path, total: u64) -> Result<()> {
    println!("store: downloading {url}");
    let mut child = Command::new("curl")
        .args(["-fsSL", "--proto", "=http,https", "--max-redirs", "3", "--retry", "5", "--retry-all-errors",
               "--speed-limit", "1000", "--speed-time", "60", "-C", "-", "-o"])
        .arg(file).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    loop {
        if let Some(st) = child.try_wait()? {
            if st.success() {
                return Ok(());
            }
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut err);
            }
            return fail(format!("download failed: {}", err.trim().trim_start_matches("curl: ")));
        }
        let done = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        set_job(id, "downloading", done, total, "");
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn run_install(id: &str, file: &Path) -> Result<()> {
    use std::io::BufRead;
    set_job(id, "installing", 0, 0, "unpacking");
    let mut child = Command::new("/usr/local/sbin/mship-system").arg("vm-install").arg(file).args(["--name", id])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    // stderr into its own thread (a full pipe would stall the installer); stdout lines are the progress
    let err = child.stderr.take().map(|e| std::thread::spawn(move || {
        std::io::BufReader::new(e).lines().map_while(|l| l.ok()).collect::<Vec<_>>()
    }));
    let mut last = String::new();
    if let Some(out) = child.stdout.take() {
        for line in std::io::BufReader::new(out).lines().map_while(|l| l.ok()) {
            let line = line.trim().to_string();
            if !line.is_empty() {
                set_job(id, "installing", 0, 0, &line.chars().take(120).collect::<String>());
                last = line;
            }
        }
    }
    let st = child.wait()?;
    let errs = err.and_then(|t| t.join().ok()).unwrap_or_default();
    if st.success() {
        return Ok(());
    }
    fail(errs.last().cloned().filter(|e| !e.is_empty()).unwrap_or(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_ids_and_paths() {
        assert!(valid_url("http://192.0.2.1:8080") && valid_url("https://vms.example.org/store"));
        assert!(!valid_url("file:///etc/passwd") && !valid_url("http://") && !valid_url("http://a b") && !valid_url("http:///x"));
        assert!(valid_id("pixelos") && valid_id("ubuntu-desktop"));
        assert!(!valid_id("../x") && !valid_id("Pixel") && !valid_id("") && !valid_id("-x"));
        assert!(store_path("pixelos", "pixelos/package.tar.xz", "package", ARCHIVES));
        assert!(!store_path("pixelos", "other/package.tar.xz", "package", ARCHIVES));
        assert!(!store_path("pixelos", "pixelos/../../x/package.tar.xz", "package", ARCHIVES));
        assert!(!store_path("pixelos", "pixelos/package.sh", "package", ARCHIVES));
        assert!(store_path("pixelos", "pixelos/icon.svg", "icon", ICON_TYPES));
    }
}
