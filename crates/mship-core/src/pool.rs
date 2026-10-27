//! The VM resource pool: every VM may use every CPU and the pool's RAM, instead of a fixed share each. What a VM
//! actually gets is decided while it runs (mothershipd), by which VMs are awake, not by which one is shown.
//!
//! - CPU: a VM gets one vCPU per host CPU, each mirroring its host CPU's type (capacity, cluster) and free to run
//!   anywhere in that CPU's cluster, so guests schedule big.LITTLE like the host does and an idle VM leaves its cores
//!   to the others. `resources.cpus` caps the count (the fastest CPUs are kept).
//! - RAM: a VM sees the pool (the host's RAM less what mothershipOS needs) and boots with part of it; the rest sits
//!   in its balloon and is lent and taken back as it runs. `resources.memory_mb` caps what it sees.

use std::path::Path;

/// MB the host keeps for itself (sway, shell, services, GPU buffers)
pub const HOST_RESERVE_MB: u64 = 1536;
/// MB a VM is never ballooned below
pub const GUEST_MIN_MB: u64 = 1024;
/// MB outside the balloon at boot, at most (more is lent as the guest needs it)
pub const BOOT_MB: u64 = 3072;

fn meminfo(key: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':')?.split_whitespace().next()?.parse::<u64>().ok())
}

/// The host's RAM in MB.
pub fn host_total_mb() -> u64 {
    meminfo("MemTotal").unwrap_or(0) / 1024
}

/// What the host could hand out without swapping, in MB.
pub fn host_available_mb() -> u64 {
    meminfo("MemAvailable").unwrap_or(0) / 1024
}

/// The RAM a VM sees (MB): the pool, or less with a cap.
pub fn vm_size_mb(total_mb: u64, cap: Option<u64>) -> u64 {
    let pool = total_mb.saturating_sub(HOST_RESERVE_MB).max(GUEST_MIN_MB);
    cap.map_or(pool, |c| c.clamp(GUEST_MIN_MB, pool))
}

/// The RAM outside the balloon at boot (MB).
pub fn vm_boot_mb(size_mb: u64) -> u64 {
    size_mb.min(BOOT_MB.max(size_mb / 2))
}

/// A host CPU: its number, relative capacity (1024 = the fastest) and the CPUs of its cluster (its cpufreq policy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCpu {
    pub cpu: usize,
    pub capacity: u32,
    pub cluster: Vec<usize>,
}

pub fn host_cpus() -> Vec<HostCpu> {
    let base = Path::new("/sys/devices/system/cpu");
    let rd = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let mut out = vec![];
    for cpu in 0.. {
        let d = base.join(format!("cpu{cpu}"));
        if !d.is_dir() {
            break;
        }
        let capacity = rd(&d.join("cpu_capacity")).trim().parse().unwrap_or(1024);
        let mut cluster: Vec<usize> =
            rd(&d.join("cpufreq/related_cpus")).split_whitespace().filter_map(|c| c.parse().ok()).collect();
        if cluster.is_empty() {
            cluster = vec![cpu];
        }
        out.push(HostCpu { cpu, capacity, cluster });
    }
    out
}

fn list(cpus: &[usize]) -> String {
    cpus.iter().map(usize::to_string).collect::<Vec<_>>().join(",")
}

/// crosvm's CPU arguments for a VM: vCPU i mirrors host CPU i (of the kept ones), runs anywhere in that CPU's
/// cluster, and the guest sees the clusters and capacities.
pub fn crosvm_cpu_args(host: &[HostCpu], cap: Option<usize>) -> Vec<String> {
    let mut kept: Vec<&HostCpu> = host.iter().collect();
    kept.sort_by(|a, b| b.capacity.cmp(&a.capacity).then(a.cpu.cmp(&b.cpu)));
    kept.truncate(cap.unwrap_or(host.len()).clamp(1, host.len().max(1)));
    kept.sort_by_key(|c| c.cpu);
    let ids: Vec<usize> = kept.iter().map(|c| c.cpu).collect();
    let mut clusters: Vec<Vec<usize>> = vec![];
    let mut affinity = vec![];
    let mut capacity = vec![];
    for (v, c) in kept.iter().enumerate() {
        // its cluster, of the kept CPUs: never a CPU of another type
        let set: Vec<usize> = c.cluster.iter().copied().filter(|h| ids.contains(h)).collect();
        affinity.push(format!("{v}={}", list(&set)));
        capacity.push(format!("{v}={}", c.capacity));
        let mine: Vec<usize> = kept.iter().enumerate().filter(|(_, o)| o.cluster == c.cluster).map(|(i, _)| i).collect();
        if !clusters.contains(&mine) {
            clusters.push(mine);
        }
    }
    let clusters = clusters.iter().map(|c| format!("[{}]", list(c))).collect::<Vec<_>>().join(",");
    vec![
        "--cpus".into(), format!("num-cores={},clusters=[{clusters}]", kept.len()),
        "--cpu-affinity".into(), affinity.join(":"),
        "--cpu-capacity".into(), capacity.join(","),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the nio (SM8250): 4 A55, 3 A77, 1 A77 prime
    fn nio() -> Vec<HostCpu> {
        (0..8)
            .map(|cpu| match cpu {
                0..=3 => HostCpu { cpu, capacity: 261, cluster: vec![0, 1, 2, 3] },
                4..=6 => HostCpu { cpu, capacity: 801, cluster: vec![4, 5, 6] },
                _ => HostCpu { cpu, capacity: 1024, cluster: vec![7] },
            })
            .collect()
    }

    #[test]
    fn mirrors_every_cpu() {
        assert_eq!(crosvm_cpu_args(&nio(), None), vec![
            "--cpus", "num-cores=8,clusters=[[0,1,2,3],[4,5,6],[7]]",
            "--cpu-affinity", "0=0,1,2,3:1=0,1,2,3:2=0,1,2,3:3=0,1,2,3:4=4,5,6:5=4,5,6:6=4,5,6:7=7",
            "--cpu-capacity", "0=261,1=261,2=261,3=261,4=801,5=801,6=801,7=1024",
        ]);
    }

    #[test]
    fn a_cap_keeps_the_fastest() {
        assert_eq!(crosvm_cpu_args(&nio(), Some(4)), vec![
            "--cpus", "num-cores=4,clusters=[[0,1,2],[3]]",
            "--cpu-affinity", "0=4,5,6:1=4,5,6:2=4,5,6:3=7",
            "--cpu-capacity", "0=801,1=801,2=801,3=1024",
        ]);
    }

    #[test]
    fn sizes() {
        assert_eq!(vm_size_mb(7570, None), 7570 - HOST_RESERVE_MB);
        assert_eq!(vm_size_mb(7570, Some(4096)), 4096);
        assert_eq!(vm_size_mb(7570, Some(512)), GUEST_MIN_MB);
        assert_eq!(vm_size_mb(7570, Some(99999)), 7570 - HOST_RESERVE_MB);
        assert_eq!(vm_boot_mb(6034), 3072);
        assert_eq!(vm_boot_mb(2048), 2048);
    }
}
