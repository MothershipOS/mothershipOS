//! The VM pool at run time (mship_core::pool): every VM may use every CPU and the pool's RAM; what each gets follows
//! whether it is awake (its power vote), not which VM is shown. You can work in one VM while another still answers
//! (Android's notifications): both awake VMs share alike; a VM whose screen is off or that sleeps yields.
//!
//! - CPU: each VM runs in its own cgroup (`vm-<name>`, under mothershipd's delegated cgroup; the daemon itself in
//!   `daemon`): cpu.weight 100 while awake (active, idle, or no vote yet), 20 while its screen is off or it sleeps.
//!   Weights only matter while the CPUs are short: an idle VM leaves its cores to the others either way.
//! - RAM: every pass (2 s) reads each awake or dozing VM's balloon stats and the host's available memory, and moves
//!   balloons ([`plan`]): a VM short of free memory is lent more (awake VMs first) from what the host can spare
//!   above its floor; when the host runs short, VMs give back their spare memory (dozing first), and a dozing VM is
//!   squeezed further (its guest frees caches or ends background apps) before the host would swap. A frozen VM is
//!   left as it is (its guest cannot answer). OOM kills a whole VM (memory.oom.group), never part of one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mship_core::api::{Vote, VoteState};
use mship_core::pool;
use mship_host::util::run;
use mship_host::vm;
use serde_json::Value;

const CGROUP_FS: &str = "/sys/fs/cgroup";
const CROSVM: &str = "/opt/mship-gpu/bin/crosvm";
const MB: u64 = 1 << 20;

/// MB the host keeps available (MemAvailable) before VMs give back memory
const HOST_FLOOR_MB: i64 = 768;
/// MB of free memory an awake / a dozing guest is kept at, when the host can spare it
const WANT_AWAKE_MB: u64 = 1024;
const WANT_DOZING_MB: u64 = 384;
/// a dozing guest squeezed for a short host keeps this much free
const SQUEEZE_FLOOR_MB: u64 = 128;
/// spare memory over its want a guest keeps while the host has room (its caches are host RAM too)
const TRIM_MB: u64 = 1024;
/// the host is comfortable above this (no trimming)
const COMFORT_MB: i64 = 2048;
/// most a balloon moves in one pass
const STEP_MB: u64 = 512;
/// least it moves (smaller changes are noise: page reporting and caches move that much all the time)
const MIN_MOVE_MB: u64 = 64;

const WEIGHT_AWAKE: u32 = 100;
const WEIGHT_DOZING: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Awake,
    Dozing,
    Frozen,
}

pub fn class(v: &Vote) -> Class {
    match v.state {
        VoteState::Suspended => Class::Frozen,
        VoteState::ScreenOff | VoteState::Sleeping => Class::Dozing,
        VoteState::Active | VoteState::Idle | VoteState::Unknown => Class::Awake,
    }
}

/// A guest as the planner sees it (MB).
#[derive(Debug, Clone, PartialEq)]
pub struct Guest {
    pub name: String,
    pub class: Class,
    /// the guest's available memory
    pub avail: u64,
    /// all its RAM (inside and outside the balloon)
    pub size: u64,
    pub balloon: u64,
}

fn want(c: Class) -> u64 {
    if c == Class::Awake { WANT_AWAKE_MB } else { WANT_DOZING_MB }
}

/// One pass, pure: `host_avail` the host's MemAvailable (MB) -> new balloon sizes (MB) of the guests that change.
pub fn plan(host_avail: u64, guests: &[Guest]) -> BTreeMap<String, u64> {
    let mut spare = host_avail as i64 - HOST_FLOOR_MB;
    let mut out = BTreeMap::new();
    let max_balloon = |g: &Guest| g.size.saturating_sub(pool::GUEST_MIN_MB);
    let take = |g: &Guest, mb: u64, out: &mut BTreeMap<String, u64>, spare: &mut i64| {
        let b = out.get(&g.name).copied().unwrap_or(g.balloon);
        let mb = mb.min(STEP_MB).min(max_balloon(g).saturating_sub(b));
        if mb >= MIN_MOVE_MB {
            out.insert(g.name.clone(), b + mb);
            *spare += mb as i64;
        }
    };
    let live: Vec<&Guest> = guests.iter().filter(|g| g.class != Class::Frozen).collect();
    if spare < 0 {
        // the host is short: spare memory back first, dozing VMs before awake ones
        let mut order = live.clone();
        order.sort_by_key(|g| (std::cmp::Reverse(g.class), std::cmp::Reverse(g.avail.saturating_sub(want(g.class)))));
        for g in &order {
            if spare < 0 {
                take(g, g.avail.saturating_sub(want(g.class)).min((-spare) as u64), &mut out, &mut spare);
            }
        }
        // still short: squeeze dozing VMs below their want
        for g in order.iter().filter(|g| g.class == Class::Dozing) {
            if spare < 0 {
                let taken = out.get(&g.name).map_or(0, |b| b - g.balloon);
                let left = g.avail.saturating_sub(taken);
                take(g, left.saturating_sub(SQUEEZE_FLOOR_MB).min((-spare) as u64), &mut out, &mut spare);
            }
        }
        return out;
    }
    // lend to VMs short of free memory, awake ones first, the neediest first
    let mut order = live.clone();
    order.sort_by_key(|g| (g.class, std::cmp::Reverse(want(g.class).saturating_sub(g.avail))));
    for g in &order {
        let short = want(g.class).saturating_sub(g.avail);
        let lend = short.min(STEP_MB).min(g.balloon).min(spare.max(0) as u64);
        if lend >= MIN_MOVE_MB {
            out.insert(g.name.clone(), g.balloon - lend);
            spare -= lend as i64;
        }
    }
    // trim: a guest holding much more than it wants (mostly caches) gives some back before the host gets tight
    if spare < COMFORT_MB - HOST_FLOOR_MB {
        for g in &live {
            let over = g.avail.saturating_sub(want(g.class) + TRIM_MB);
            if over > 0 && !out.contains_key(&g.name) {
                take(g, over, &mut out, &mut spare);
            }
        }
    }
    out
}

struct Seen {
    weight: u32,
    balloon: Option<u64>,
}

pub struct Pool {
    /// mothershipd's cgroup (delegated), with the daemon in `daemon` and each VM in `vm-<name>`
    base: Option<PathBuf>,
    seen: Mutex<BTreeMap<String, Seen>>,
}

fn wr(p: impl AsRef<Path>, v: &str) -> std::io::Result<()> {
    std::fs::write(p, v)
}

impl Pool {
    /// Set up the cgroups (mothershipd.service has Delegate=yes); without them VMs still run, unweighted.
    pub fn new() -> Self {
        let base = Self::setup().map_err(|e| println!("pool: no cgroups ({e}): VMs run unweighted")).ok();
        Self { base, seen: Mutex::default() }
    }

    fn setup() -> std::io::Result<PathBuf> {
        let own = std::fs::read_to_string("/proc/self/cgroup")?;
        let rel = own.lines().find_map(|l| l.strip_prefix("0::")).ok_or(std::io::ErrorKind::NotFound)?;
        let base = Path::new(CGROUP_FS).join(rel.trim_start_matches('/'));
        if base.file_name().is_some_and(|n| n == "daemon") {
            return Ok(base.parent().unwrap().to_path_buf()); // set up already
        }
        // a cgroup with children holds no processes itself: the daemon (and whatever it started) moves to `daemon`
        let daemon = base.join("daemon");
        let _ = std::fs::create_dir(&daemon);
        for pid in std::fs::read_to_string(base.join("cgroup.procs"))?.lines() {
            let _ = wr(daemon.join("cgroup.procs"), pid);
        }
        wr(base.join("cgroup.subtree_control"), "+cpu +memory")?;
        // VMs of an earlier daemon are gone (the unit's processes stop with it): their empty cgroups go too
        for e in std::fs::read_dir(&base)?.flatten() {
            if e.file_name().to_string_lossy().starts_with("vm-") {
                let _ = std::fs::remove_dir(e.path());
            }
        }
        Ok(base)
    }

    /// The cgroup a VM's launcher starts in (created, awake), or None without cgroups.
    pub fn cgroup(&self, vm: &str) -> Option<PathBuf> {
        let d = self.base.as_ref()?.join(format!("vm-{vm}"));
        if let Err(e) = std::fs::create_dir(&d)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            println!("pool: {vm}: no cgroup: {e}");
            return None;
        }
        let _ = wr(d.join("memory.oom.group"), "1");
        let _ = wr(d.join("cpu.weight"), &WEIGHT_AWAKE.to_string());
        self.seen.lock().unwrap().insert(vm.into(), Seen { weight: WEIGHT_AWAKE, balloon: None });
        Some(d)
    }

    /// The 2 s pass: CPU weights and balloons from the power votes.
    pub fn tick(&self, votes: &BTreeMap<String, Vote>) {
        let running: BTreeMap<String, Class> = vm::running_dirs()
            .into_iter()
            .filter_map(|d| {
                let n = d.file_name()?.to_string_lossy().into_owned();
                Some((n.clone(), votes.get(&n).map(class).unwrap_or(Class::Awake)))
            })
            .collect();
        self.weights(&running);
        let guests: Vec<Guest> = running.iter().filter(|(_, c)| **c != Class::Frozen).filter_map(|(n, c)| stats(n, *c)).collect();
        let changes = plan(pool::host_available_mb(), &guests);
        let mut seen = self.seen.lock().unwrap();
        for (n, mb) in changes {
            let g = guests.iter().find(|g| g.name == n).unwrap();
            let sock = Path::new(vm::VMS).join(&n).join("crosvm.sock");
            let o = run("timeout", &["5", CROSVM, "balloon", &(mb * MB).to_string(), &sock.to_string_lossy()]);
            if o.code != 0 {
                println!("pool: {n}: balloon: {}", o.message());
                continue;
            }
            println!("pool: {n} ({:?}, {} MB free): {:.1} -> {:.1} GB of {:.1} GB", g.class, g.avail,
                     (g.size - g.balloon) as f64 / 1024.0, (g.size - mb) as f64 / 1024.0, g.size as f64 / 1024.0);
            if let Some(s) = seen.get_mut(&n) {
                s.balloon = Some(mb);
            }
        }
        seen.retain(|n, _| running.contains_key(n));
        drop(seen);
        self.tidy(&running);
    }

    fn weights(&self, running: &BTreeMap<String, Class>) {
        let Some(base) = &self.base else { return };
        let mut seen = self.seen.lock().unwrap();
        for (n, c) in running {
            let w = if *c == Class::Dozing { WEIGHT_DOZING } else { WEIGHT_AWAKE };
            let s = seen.entry(n.clone()).or_insert(Seen { weight: 0, balloon: None });
            if s.weight != w && wr(base.join(format!("vm-{n}")).join("cpu.weight"), &w.to_string()).is_ok() {
                s.weight = w;
            }
        }
    }

    /// Remove the cgroups of VMs that ended.
    fn tidy(&self, running: &BTreeMap<String, Class>) {
        let Some(base) = &self.base else { return };
        for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(vm) = name.strip_prefix("vm-")
                && !running.contains_key(vm)
                && std::fs::read_to_string(e.path().join("cgroup.procs")).is_ok_and(|p| p.trim().is_empty())
            {
                let _ = std::fs::remove_dir(e.path());
            }
        }
    }
}

/// A VM's balloon stats (crosvm balloon_stats; the guest answers), or None.
fn stats(vm: &str, class: Class) -> Option<Guest> {
    let sock = Path::new(vm::VMS).join(vm).join("crosvm.sock");
    if !sock.exists() {
        return None;
    }
    let size = crosvm_mem_mb(&sock)?;
    let o = run("timeout", &["3", CROSVM, "balloon_stats", &sock.to_string_lossy()]);
    if o.code != 0 {
        return None;
    }
    parse_stats(vm, class, size, &o.stdout)
}

/// The RAM (MB) of the crosvm serving `sock`: its `--mem size=N` (a guest's total may or may not exclude its
/// balloon: Linux's does unless deflate-on-OOM, Android's does not).
fn crosvm_mem_mb(sock: &Path) -> Option<u64> {
    let sock = sock.to_string_lossy();
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let raw = std::fs::read(e.path().join("cmdline")).ok()?;
        let args: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
        if !args.first()?.ends_with(b"crosvm") || !args.contains(&sock.as_bytes()) {
            return None;
        }
        let i = args.iter().position(|a| *a == b"--mem")?;
        let v = std::str::from_utf8(args.get(i + 1)?).ok()?;
        v.split(',').find_map(|kv| kv.strip_prefix("size="))?.parse().ok()
    })
}

fn parse_stats(vm: &str, class: Class, size: u64, out: &str) -> Option<Guest> {
    let start = out.find('{')?;
    let v: Value = serde_json::Deserializer::from_str(&out[start..]).into_iter().next()?.ok()?;
    let r = if v["BalloonStats"].is_object() { &v["BalloonStats"] } else { &v };
    let balloon = r["balloon_actual"].as_u64()? / MB;
    let s = &r["stats"];
    let avail = s["available_memory"].as_u64().or(s["free_memory"].as_u64())? / MB;
    Some(Guest { name: vm.into(), class, avail, size, balloon })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(name: &str, class: Class, avail: u64, balloon: u64) -> Guest {
        Guest { name: name.into(), class, avail, size: 6000, balloon }
    }

    #[test]
    fn lends_to_awake_vms_first() {
        // 1 GB spare above the floor: the awake VM short of 800 MB gets it before the dozing one
        let p = plan(1792, &[g("android", Class::Dozing, 100, 2000), g("ubuntu", Class::Awake, 224, 3000)]);
        assert_eq!(p.get("ubuntu"), Some(&(3000 - 512)));
        assert_eq!(p.get("android"), Some(&(2000 - 284)));
    }

    #[test]
    fn a_short_host_takes_spare_memory_dozing_first() {
        let p = plan(500, &[g("android", Class::Dozing, 900, 2000), g("ubuntu", Class::Awake, 3000, 0)]);
        // 268 MB short: the dozing VM's spare (516 MB over its want) covers it
        assert_eq!(p.get("android"), Some(&(2000 + 268)));
        assert_eq!(p.get("ubuntu"), None);
    }

    #[test]
    fn a_short_host_squeezes_dozing_vms_not_awake_ones() {
        let p = plan(200, &[g("android", Class::Dozing, 384, 2000), g("ubuntu", Class::Awake, 1024, 0)]);
        assert_eq!(p.get("android"), Some(&(2000 + 256)));
        assert_eq!(p.get("ubuntu"), None);
    }

    #[test]
    fn frozen_vms_are_left_alone() {
        assert!(plan(100, &[g("android", Class::Frozen, 3000, 0)]).is_empty());
    }

    #[test]
    fn trims_big_caches_only_when_the_host_is_tight() {
        let guests = [g("ubuntu", Class::Awake, 3000, 0)];
        assert!(plan(4000, &guests).is_empty());
        assert_eq!(plan(1500, &guests).get("ubuntu"), Some(&512));
    }

    #[test]
    fn never_balloons_below_the_minimum() {
        let p = plan(100, &[g("android", Class::Dozing, 5000, 4900)]);
        assert_eq!(p.get("android"), Some(&(6000 - pool::GUEST_MIN_MB)));
    }

    #[test]
    fn reads_crosvm_stats() {
        let out = r#"{"BalloonStats":{"stats":{"swap_in":null,"free_memory":524288000,"total_memory":3221225472,
                     "available_memory":1073741824},"balloon_actual":3221225472}}"#;
        assert_eq!(parse_stats("x", Class::Awake, 6144, out), Some(Guest { name: "x".into(), class: Class::Awake, avail: 1024,
                                                                     size: 6144, balloon: 3072 }));
    }
}
