//! The graphical session (sway, user mship) as seen from the root daemon: its environment, and running things in it
//! as the session user (swayidle, VM launchers).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mship_core::sway::Sway;
use mship_host::util::run;

pub const UI_USER: &str = "mship";
const RUN: &str = "/run/mship-wayland";
/// process names of the host compositor
const SWAY: &[&str] = &["sway-mship", "sway"];
/// sway's unit's, not the session's
const UNIT_VARS: &[&str] = &["INVOCATION_ID", "JOURNAL_STREAM", "SYSTEMD_EXEC_PID", "MEMORY_PRESSURE_WATCH",
                             "MEMORY_PRESSURE_WRITE", "NOTIFY_SOCKET", "RUNTIME_DIRECTORY"];

pub fn user() -> (u32, u32) {
    let name = std::ffi::CString::new(UI_USER).unwrap();
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() { (1000, 1000) } else { unsafe { ((*pw).pw_uid, (*pw).pw_gid) } }
}

static CACHE: Mutex<Option<(Instant, BTreeMap<String, String>)>> = Mutex::new(None);

fn proc_uid(p: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.uid())
}

/// The session's environment: sway's own (what its children, e.g. the shell, inherit) plus the sway IPC and Wayland
/// sockets it sets for them (cached for a few seconds: one handover runs several sway commands).
pub fn env() -> BTreeMap<String, String> {
    if let Some((t, e)) = CACHE.lock().unwrap().as_ref()
        && t.elapsed() < Duration::from_secs(5)
    {
        return e.clone();
    }
    let uid = user().0;
    let mut e = BTreeMap::new();
    for p in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let p = p.path();
        if proc_uid(&p) != Some(uid) {
            continue;
        }
        let comm = std::fs::read_to_string(p.join("comm")).unwrap_or_default();
        if !SWAY.contains(&comm.trim()) {
            continue;
        }
        if let Ok(raw) = std::fs::read(p.join("environ")) {
            e = raw
                .split(|b| *b == 0)
                .filter_map(|kv| {
                    let kv = String::from_utf8_lossy(kv);
                    kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
                })
                .collect();
            break;
        }
    }
    let run_dir = e.get("XDG_RUNTIME_DIR").cloned().unwrap_or_else(|| RUN.into());
    e.entry("XDG_RUNTIME_DIR".into()).or_insert_with(|| run_dir.clone());
    e.entry("HOME".into()).or_insert_with(|| format!("/home/{UI_USER}"));
    e.entry("USER".into()).or_insert_with(|| UI_USER.into());
    e.entry("LOGNAME".into()).or_insert_with(|| UI_USER.into());
    e.entry("PATH".into()).or_insert_with(|| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin".into());
    let mut entries: Vec<String> = std::fs::read_dir(&run_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|x| x.file_name().into_string().ok())
        .collect();
    entries.sort();
    if let Some(s) = entries.iter().find(|n| n.starts_with("sway-ipc.") && n.ends_with(".sock")) {
        e.insert("SWAYSOCK".into(), format!("{run_dir}/{s}"));
    }
    if let Some(w) = entries.iter().find(|n| n.starts_with("wayland-") && !n.ends_with(".lock")) {
        e.insert("WAYLAND_DISPLAY".into(), w.clone());
    }
    for k in UNIT_VARS {
        e.remove(*k);
    }
    *CACHE.lock().unwrap() = Some((Instant::now(), e.clone()));
    e
}

/// A command that runs `cmd` as the session user in the session's environment, in `cgroup` if given (a cgroup
/// directory: the child moves itself there before it drops root, so everything it starts is in it too).
fn command(cmd: &[&str], extra: &[(&str, &str)], cgroup: Option<&Path>) -> Command {
    let (uid, gid) = user();
    let procs = cgroup.and_then(|c| std::ffi::CString::new(c.join("cgroup.procs").into_os_string().into_encoded_bytes()).ok());
    let mut c = Command::new(cmd[0]);
    c.args(&cmd[1..]).env_clear().envs(env()).envs(extra.iter().copied());
    use std::os::unix::process::CommandExt;
    let groups = supplementary_groups(uid, gid);
    // here, not with Command::uid/gid: std drops root before pre_exec runs, and setgroups needs root
    unsafe {
        c.pre_exec(move || {
            // after fork: raw calls only ("0" moves the writer)
            if let Some(procs) = &procs {
                let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd < 0 || libc::write(fd, b"0".as_ptr().cast(), 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(fd);
            }
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    c.current_dir("/");
    c
}

fn supplementary_groups(uid: u32, gid: u32) -> Vec<libc::gid_t> {
    let name = std::ffi::CString::new(UI_USER).unwrap();
    let mut n: libc::c_int = 64;
    let mut groups = vec![0 as libc::gid_t; n as usize];
    let r = unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut n) };
    if r < 0 {
        return vec![gid];
    }
    groups.truncate(n as usize);
    let _ = uid;
    groups
}

/// Start detached (its own session) as the session user, output to `log` (owned by the user) or nowhere.
pub fn spawn(cmd: &[&str], extra: &[(&str, &str)], log: Option<&Path>) -> std::io::Result<u32> {
    spawn_in(cmd, extra, log, None)
}

/// [`spawn`], in `cgroup`.
pub fn spawn_in(cmd: &[&str], extra: &[(&str, &str)], log: Option<&Path>, cgroup: Option<&Path>) -> std::io::Result<u32> {
    let mut c = command(cmd, extra, cgroup);
    match log {
        Some(path) => {
            let f = std::fs::File::create(path)?;
            let (uid, gid) = user();
            std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
            c.stdout(f.try_clone()?).stderr(f);
        }
        None => {
            c.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    c.stdin(Stdio::null());
    use std::os::unix::process::CommandExt;
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = c.spawn()?;
    let pid = child.id();
    // reaped by a thread so it does not linger as a zombie when it exits
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(pid)
}

/// A sway command (words joined), in the session's sway; false if it failed.
pub fn sway(args: &[&str]) -> bool {
    let cmd = args.join(" ");
    let r = sway_conn().and_then(|mut s| s.command(&cmd));
    if let Err(e) = &r {
        println!("sway: {e}");
    }
    r.is_ok()
}

fn sway_conn() -> std::io::Result<Sway> {
    let e = env();
    let sock = e.get("SWAYSOCK").ok_or_else(|| std::io::Error::other("sway is not running"))?;
    Sway::connect(Path::new(sock))
}

/// sway's outputs (names), or None if sway does not answer.
pub fn outputs() -> Option<Vec<String>> {
    let o = sway_conn().and_then(|mut s| s.outputs()).ok()?;
    Some(o.iter().filter_map(|o| o["name"].as_str().map(String::from)).collect())
}

/// Is this output on (DPMS)?
pub fn output_power(name: &str) -> Option<bool> {
    sway_conn().and_then(|mut s| s.output_power(name)).ok().flatten()
}

/// Kill the session user's processes named `name`.
pub fn pkill(name: &str) {
    run("pkill", &["-u", &user().0.to_string(), "-x", name]);
}
