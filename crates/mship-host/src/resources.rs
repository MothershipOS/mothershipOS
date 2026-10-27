//! Counters only root can read, for Settings › Resources (the shell turns them into rates): GPU busy time of all
//! msm DRM clients (fdinfo drm-engine-gpu, counted once per client), the Venus core clock, and per VM the CPU
//! ticks, PSS and GPU time of the VMM's process tree.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use mship_core::status::{Resources, VmResources};

use crate::util::{load_value, rd, rd_u64};
use crate::vm;

/// /proc/<pid>/stat -> (ppid, utime + stime). Pure.
pub fn parse_stat(stat: &str) -> Option<(i32, u64)> {
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(1)?.parse().ok()?, f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?))
}

/// One DRM fd's fdinfo -> (client id, GPU busy ns) if it is an msm client. Pure.
pub fn parse_fdinfo(text: &str) -> Option<(String, u64)> {
    let field = |k: &str| text.lines().find_map(|l| l.split_once(':').filter(|(a, _)| a.trim() == k).map(|(_, v)| v.trim()));
    if field("drm-driver")? != "msm" {
        return None;
    }
    let cid = field("drm-client-id").filter(|c| !c.is_empty())?;
    let ns = field("drm-engine-gpu").and_then(|v| v.split_whitespace().next()?.parse().ok()).unwrap_or(0);
    Some((cid.to_string(), ns))
}

fn monotonic() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

pub fn collect() -> Resources {
    let pids: Vec<i32> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    let mut ppid = HashMap::new();
    let mut ticks = HashMap::new();
    for &p in &pids {
        if let Some((pp, t)) = parse_stat(&rd(format!("/proc/{p}/stat"), "")) {
            ppid.insert(p, pp);
            ticks.insert(p, t);
        }
    }
    let mut gpu: HashMap<i32, u64> = HashMap::new();
    let mut seen = BTreeSet::new();
    for &p in &pids {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{p}/fdinfo")) else { continue };
        for fd in fds.flatten() {
            let name = fd.file_name();
            let link = std::fs::read_link(format!("/proc/{p}/fd/{}", name.to_string_lossy()));
            if !link.is_ok_and(|l| l.starts_with("/dev/dri/")) {
                continue;
            }
            if let Some((cid, ns)) = parse_fdinfo(&rd(fd.path(), ""))
                && seen.insert(cid)
            {
                *gpu.entry(p).or_default() += ns;
            }
        }
    }
    let mut children: BTreeMap<i32, Vec<i32>> = BTreeMap::new();
    for (&p, &pp) in &ppid {
        children.entry(pp).or_default().push(p);
    }
    let vms = vm::running_dirs()
        .into_iter()
        .filter_map(|d| {
            let pid = vm::pid(&d)?;
            let mut tree = vec![];
            let mut todo = vec![pid];
            while let Some(p) = todo.pop() {
                tree.push(p);
                todo.extend(children.get(&p).cloned().unwrap_or_default());
            }
            let pss = tree
                .iter()
                .filter_map(|p| {
                    rd(format!("/proc/{p}/smaps_rollup"), "").lines().find_map(|l| {
                        l.strip_prefix("Pss:").and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                    })
                })
                .sum();
            let name = load_value(d.join("vm.json"))
                .and_then(|c| c["name"].as_str().map(String::from))
                .unwrap_or_else(|| d.file_name().unwrap().to_string_lossy().into_owned());
            Some(VmResources {
                dir: d.to_string_lossy().into_owned(),
                name,
                ticks: tree.iter().filter_map(|p| ticks.get(p)).sum(),
                pss_kb: pss,
                gpu_ns: tree.iter().filter_map(|p| gpu.get(p)).sum(),
            })
        })
        .collect();
    Resources {
        t: monotonic(),
        gpu_ns: gpu.values().sum(),
        venus_hz: rd_u64(Path::new("/sys/kernel/debug/clk/video_cc_mvs0_clk/clk_rate")),
        vms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_and_fdinfo() {
        let stat = "1234 (crosvm (vm)) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 9 0 100 0 0";
        assert_eq!(parse_stat(stat), Some((1, 300)));
        let fi = "pos:\t0\nflags:\t02\ndrm-driver:\tmsm\ndrm-client-id:\t34\ndrm-engine-gpu:\t1500 ns\n";
        assert_eq!(parse_fdinfo(fi), Some(("34".into(), 1500)));
        assert_eq!(parse_fdinfo("drm-driver:\tvirtio_gpu\ndrm-client-id:\t1\n"), None);
    }
}
