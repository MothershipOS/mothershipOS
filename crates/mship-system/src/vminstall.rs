//! `vm-install <source> [--name <id>]`: install a VM, or update an installed one, from its package
//! (docs/vm-packages.md). mothershipOS builds no VM itself: a VM is a repository of its own.
//!
//! `<source>` is a VM repository (a git URL, or a directory) whose `mothership-build` makes the package, or a
//! prebuilt package: an archive (a URL or a file: .tar, .tar.gz, .tar.xz) or a directory with vm.json. The work
//! happens in /var/lib/mothership/build/<n> as the session user and is removed afterwards, failed or not (or by the
//! next run, if the phone lost power meanwhile).
//!
//! The package's manifest must be runnable on this release. Its files then go into
//! /var/lib/mothership/vms/<id>/ (id from mothership-package.json, or --name); on an update the files the package
//! keeps (its `keep` list: the user's disk, a used seed) stay as they are and the rest is replaced. Last, the
//! package's install step runs in the VM's directory (`<install> install`, or `<install> update`). The VM must be
//! stopped.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use mship_core::{contracts, manifest};
use serde_json::Value;

use crate::{Res, run};

const VMS: &str = "/var/lib/mothership/vms";
const BUILD: &str = "/var/lib/mothership/build";
const USER: &str = "mship";
/// the host's key for ssh into VMs (a VM's build can authorize it in the guest)
const VM_KEY: &str = "/root/.ssh/id_ed25519_vm.pub";
const ARCHIVES: &[&str] = &[".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst"];

struct Package {
    id: String,
    keep: Vec<String>,
    install: Option<String>,
}

fn user() -> Res<(u32, u32)> {
    let name = std::ffi::CString::new(USER).unwrap();
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        return Err(format!("no user {USER}"));
    }
    Ok(unsafe { ((*pw).pw_uid, (*pw).pw_gid) })
}

/// Run a step of the VM's (its build, its install step) as the session user, its output on ours.
fn run_as_user(prog: &Path, args: &[&str], dir: &Path) -> Res<()> {
    run_as_user_env(prog, args, dir, &[])
}

fn run_as_user_env(prog: &Path, args: &[&str], dir: &Path, extra: &[(&str, &str)]) -> Res<()> {
    let (uid, gid) = user()?;
    let mut c = Command::new(prog);
    c.args(args).current_dir(dir).env_clear().uid(uid).gid(gid)
        .env("HOME", format!("/home/{USER}")).env("USER", USER).env("LANG", "C.UTF-8")
        .env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin")
        .env("MSHIP_RELEASE", mship_core::RELEASE);
    if let Ok(key) = std::fs::read_to_string(VM_KEY) {
        c.env("MSHIP_VM_SSH_KEY", key.trim());
    }
    // the caller's MSHIP_* settings (MSHIP_VM_DISK, MSHIP_ASSETS, ...) reach the VM's steps
    for (k, v) in std::env::vars().filter(|(k, _)| k.starts_with("MSHIP_")) {
        c.env(k, v);
    }
    c.envs(extra.iter().copied());
    let st = c.status().map_err(|e| format!("{}: {e}", prog.display()))?;
    if st.success() { Ok(()) } else { Err(format!("{} {} failed ({st})", prog.display(), args.join(" "))) }
}

fn chown(p: &Path) -> Res<()> {
    run("chown", &["-R", &format!("{USER}:{USER}"), &p.to_string_lossy()])
}

fn download(url: &str, dest: &Path) -> Res<()> {
    println!("downloading {url}");
    let d = dest.to_string_lossy();
    if run("curl", &["-fsSL", "--retry", "3", "-o", &d, url]).is_ok() {
        return Ok(());
    }
    run("wget", &["-q", "-O", &d, url])
}

/// The source as a directory under `work`: a repository (with mothership-build) or a package (with vm.json).
fn obtain(source: &str, work: &Path) -> Res<PathBuf> {
    let src = work.join("src");
    let is_archive = |s: &str| ARCHIVES.iter().any(|x| s.ends_with(x));
    let local = Path::new(source);
    if local.is_dir() {
        // a copy: the steps run as the session user and must not change the original
        run("cp", &["-a", &local.to_string_lossy(), &src.to_string_lossy()])?;
    } else if local.is_file() || source.starts_with("http://") || source.starts_with("https://") && is_archive(source) {
        let archive = if local.is_file() {
            local.to_path_buf()
        } else {
            let a = work.join("download");
            download(source, &a)?;
            a
        };
        std::fs::create_dir_all(&src).map_err(|e| e.to_string())?;
        run("tar", &["-xSf", &archive.to_string_lossy(), "-C", &src.to_string_lossy()])?;
    } else {
        let (uid, gid) = user()?;
        let st = Command::new("git").args(["clone", "-q", "--depth", "1", source, &src.to_string_lossy()]).uid(uid).gid(gid)
            .env("HOME", format!("/home/{USER}")).status().map_err(|e| format!("git: {e}"))?;
        if !st.success() {
            return Err(format!("git clone {source} failed"));
        }
    }
    // an archive of one top directory: that directory
    let top: Vec<PathBuf> = std::fs::read_dir(&src).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).collect();
    if let [only] = top.as_slice()
        && only.is_dir()
        && !src.join("vm.json").exists()
    {
        return Ok(only.clone());
    }
    Ok(src)
}

fn package(dir: &Path) -> Res<Package> {
    let p = dir.join("mothership-package.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?)
        .map_err(|e| format!("{}: {e}", p.display()))?;
    let id = v["id"].as_str().unwrap_or_default().to_string();
    let keep = v["keep"].as_array().into_iter().flatten().filter_map(|k| k.as_str().map(String::from)).collect();
    let install = v["install"].as_str().map(String::from);
    Ok(Package { id, keep, install })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A path the package names: inside its directory, no way out.
fn inside(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && Path::new(p).components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn running(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("run.pid")).ok().and_then(|p| p.trim().parse::<i32>().ok())
        .is_some_and(|p| unsafe { libc::kill(p, 0) } == 0)
}

/// Move the package's files into the VM's directory; what `keep` names stays where the VM already has it.
fn merge(from: &Path, to: &Path, keep: &[String], rel: &Path) -> Res<()> {
    for e in std::fs::read_dir(from.join(rel)).map_err(|e| e.to_string())?.flatten() {
        let r = rel.join(e.file_name());
        let rs = r.to_string_lossy();
        let dst = to.join(&r);
        if keep.iter().any(|k| rs == k.as_str() || rs.starts_with(&format!("{k}/"))) && dst.exists() {
            println!("kept {rs}");
            continue;
        }
        if e.path().is_dir() {
            std::fs::create_dir_all(&dst).map_err(|e| format!("{}: {e}", dst.display()))?;
            merge(from, to, keep, &r)?;
        } else if std::fs::rename(e.path(), &dst).is_err() {
            std::fs::copy(e.path(), &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        }
    }
    Ok(())
}

fn install_in(source: &str, name: Option<&str>, work: &Path) -> Res<()> {
    let src = obtain(source, work)?;
    let dir = if src.join("mothership-build").is_file() {
        let out = work.join("out");
        std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        chown(work)?;
        // an update of an installed VM: the build may leave out what the package keeps (MSHIP_UPDATE=1)
        let installed = name.map(String::from).or_else(|| package(&src).ok().map(|p| p.id))
            .is_some_and(|id| valid_id(&id) && Path::new(VMS).join(id).join("vm.json").exists());
        println!("building ({source}){}", if installed { ", an update" } else { "" });
        run_as_user_env(&src.join("mothership-build"), &[&out.to_string_lossy()], &src, if installed { &[("MSHIP_UPDATE", "1")] } else { &[] })?;
        out
    } else {
        src
    };
    if !dir.join("vm.json").is_file() {
        return Err(format!("{source}: neither a VM repository (mothership-build) nor a package (vm.json)"));
    }
    let pkg = package(&dir)?;
    let m = manifest::load(&dir).map_err(|e| format!("vm.json: {e}"))?;
    let c = manifest::check(&m, contracts::release());
    if !c.runnable {
        return Err(format!("vm.json does not run on this release: {}", c.errors.join("; ")));
    }
    let id = name.map(String::from).unwrap_or(pkg.id.clone());
    if !valid_id(&id) {
        return Err(format!("VM id {id:?}: lowercase letters, digits and '-' (mothership-package.json \"id\", or --name)"));
    }
    if let Some(bad) = pkg.keep.iter().chain(pkg.install.iter()).find(|p| !inside(p)) {
        return Err(format!("mothership-package.json: {bad:?} is not a path inside the package"));
    }
    let to = Path::new(VMS).join(&id);
    if running(&to) {
        return Err(format!("{id} is running; stop it first"));
    }
    let update = to.join("vm.json").exists();
    std::fs::create_dir_all(&to).map_err(|e| format!("{}: {e}", to.display()))?;
    merge(&dir, &to, if update { &pkg.keep } else { &[] }, Path::new(""))?;
    chown(&to)?;
    if let Some(step) = &pkg.install {
        run_as_user(&to.join(step), &[if update { "update" } else { "install" }], &to)?;
    }
    println!("{id}: {} ({})", if update { "updated" } else { "installed" }, to.display());
    Ok(())
}

/// Builds of earlier runs that never finished (the phone lost power, the process was killed): `<time>-<pid>` of a
/// process that is gone.
fn remove_stale_builds() {
    for e in std::fs::read_dir(BUILD).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let pid = name.split_once('-').filter(|(t, _)| t.parse::<u64>().is_ok()).and_then(|(_, p)| p.parse::<i32>().ok());
        if let Some(pid) = pid
            && unsafe { libc::kill(pid, 0) } != 0
        {
            println!("removing an unfinished build ({name})");
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

pub fn install(source: &str, name: Option<&str>) -> Res<()> {
    remove_stale_builds();
    let work = Path::new(BUILD).join(format!("{}-{}", crate::now(), std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let r = install_in(source, name, &work);
    // the build is not kept, failed or not
    let _ = std::fs::remove_dir_all(&work);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_paths() {
        assert!(valid_id("pixelos") && valid_id("ubuntu-desktop"));
        assert!(!valid_id("") && !valid_id("../x") && !valid_id("Pixel OS"));
        assert!(inside("disk.img") && inside("seed/user-data"));
        assert!(!inside("/etc/passwd") && !inside("../x") && !inside("a/../../b"));
    }

    #[test]
    fn an_update_keeps_what_the_package_keeps() {
        let base = std::env::temp_dir().join(format!("mship-vminstall-{}", std::process::id()));
        let (from, to) = (base.join("pkg"), base.join("vm"));
        for d in [&from.join("seed"), &to.join("seed")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(from.join("vm.json"), "new").unwrap();
        std::fs::write(from.join("hdd.raw"), "fresh disk").unwrap();
        std::fs::write(from.join("seed/user-data"), "fresh seed").unwrap();
        std::fs::write(to.join("vm.json"), "old").unwrap();
        std::fs::write(to.join("hdd.raw"), "the user's disk").unwrap();
        std::fs::write(to.join("seed/used"), "").unwrap();
        std::fs::write(to.join("qemu.log"), "log").unwrap();
        merge(&from, &to, &["hdd.raw".into(), "seed".into()], Path::new("")).unwrap();
        let rd = |p: &str| std::fs::read_to_string(to.join(p)).unwrap();
        assert_eq!(rd("vm.json"), "new");
        assert_eq!(rd("hdd.raw"), "the user's disk");
        assert!(!to.join("seed/user-data").exists(), "the used seed stays as it was");
        assert_eq!(rd("qemu.log"), "log");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
