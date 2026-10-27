//! CPU modes (Cool / Balanced / Performance / Custom): governors and clock caps per cluster, and the temperature
//! limit the CPU guard (mothershipd) holds them under.

use std::collections::BTreeMap;
use std::path::Path;

use mship_core::status::{CpuPolicy, CpuStatus, GuardState};
use serde::{Deserialize, Serialize};

use crate::util::{Result, fail, load_value, rd, rd_u64, run, save_json, sleep_s, wr};

pub const CONF: &str = "/etc/mothership/cpu.json";
pub const CPUFREQ: &str = "/sys/devices/system/cpu/cpufreq";

/// A mode: governor, per-cluster caps (kHz; policy0 = A55 ×4, policy4 = A77 ×3, policy7 = prime A77), and the
/// temperature (°C) above which the CPU guard steps the caps down (`None`: only the kernel's 90 °C trip).
pub struct Profile {
    pub name: &'static str,
    pub governor: &'static str,
    pub limit: Option<f64>,
    pub max: &'static [(&'static str, u64)],
}

pub const PROFILES: &[Profile] = &[
    Profile { name: "cool", governor: "conservative", limit: Some(70.0),
              max: &[("policy0", 1516800), ("policy4", 1478400), ("policy7", 1516800)] },
    Profile { name: "balanced", governor: "schedutil", limit: Some(80.0),
              max: &[("policy0", 1804800), ("policy4", 2419200), ("policy7", 2841600)] },
    Profile { name: "performance", governor: "performance", limit: None,
              max: &[("policy0", 1804800), ("policy4", 2419200), ("policy7", 3091200)] },
];
/// names before 2026-10-21
const OLD_PROFILES: &[(&str, &str)] = &[("powersave", "cool")];
/// the conservative governor for Cool: ramp up only under heavy load, in small steps, and drop back early
const CONSERVATIVE: &[(&str, u32)] = &[("up_threshold", 90), ("down_threshold", 50), ("freq_step", 10),
                                       ("sampling_down_factor", 1)];

/// /etc/mothership/cpu.json
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CpuConf {
    pub profile: Option<String>,
    /// policy -> governor
    pub governor: BTreeMap<String, String>,
    /// policy -> cap, kHz
    pub max: BTreeMap<String, u64>,
    /// °C; absent in files written before modes had a limit
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// the file has a "limit" key (null = Performance: no limit)
    #[serde(skip)]
    pub has_limit: bool,
}

pub fn conf() -> Option<CpuConf> {
    let v = load_value(CONF)?;
    let mut c: CpuConf = serde_json::from_value(v.clone()).ok()?;
    c.has_limit = v.get("limit").is_some();
    Some(c)
}

fn save(c: &CpuConf) -> Result<()> {
    let mut v = serde_json::to_value(c).map_err(|e| crate::util::Error(e.to_string()))?;
    v["limit"] = c.limit.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null);
    save_json(CONF, &v)
}

pub fn policies() -> Vec<String> {
    let mut p: Vec<String> = std::fs::read_dir(CPUFREQ)
        .map(|d| d.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    p.retain(|n| n.starts_with("policy"));
    p.sort();
    p
}

/// thermal zone -> °C for the cpu* and gpu* zones
pub fn temps() -> BTreeMap<String, f64> {
    let mut t = BTreeMap::new();
    for z in std::fs::read_dir("/sys/class/thermal").into_iter().flatten().flatten() {
        let p = z.path();
        if !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("thermal_zone")) {
            continue;
        }
        let typ = rd(p.join("type"), "");
        if (typ.starts_with("cpu") || typ.starts_with("gpu"))
            && let Ok(v) = rd(p.join("temp"), "").parse::<f64>()
        {
            t.insert(typ, v / 1000.0);
        }
    }
    t
}

fn split(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

pub fn status(guard: Option<&GuardState>) -> CpuStatus {
    let c = conf().unwrap_or(CpuConf { profile: Some("balanced".into()), ..Default::default() });
    let policies = policies()
        .into_iter()
        .map(|p| {
            let b = Path::new(CPUFREQ).join(&p);
            CpuPolicy {
                cpus: rd(b.join("related_cpus"), ""),
                governor: rd(b.join("scaling_governor"), ""),
                governors: split(&rd(b.join("scaling_available_governors"), "")),
                cur: rd_u64(b.join("scaling_cur_freq")),
                max: rd_u64(b.join("scaling_max_freq")),
                hw_min: rd_u64(b.join("cpuinfo_min_freq")),
                hw_max: rd_u64(b.join("cpuinfo_max_freq")),
                freqs: split(&rd(b.join("scaling_available_frequencies"), "")).iter().filter_map(|f| f.parse().ok()).collect(),
                policy: p,
            }
        })
        .collect();
    let temps = temps();
    let hottest = temps.values().cloned().fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
    let profile = c.profile.map(|p| OLD_PROFILES.iter().find(|(o, _)| *o == p).map(|(_, n)| n.to_string()).unwrap_or(p));
    let throttled = guard.is_some_and(|g| g.throttled && g.limit == c.limit);
    CpuStatus { profile, policies, temps, hottest, limit: c.limit, throttled }
}

/// Write a configuration to cpufreq (governors, caps, the conservative governor's tuning).
pub fn apply_conf(c: &CpuConf) {
    for p in policies() {
        let b = Path::new(CPUFREQ).join(&p);
        let gov = c.governor.get(&p).map(String::as_str).unwrap_or("schedutil");
        let mx = c.max.get(&p).copied().unwrap_or_else(|| rd_u64(b.join("cpuinfo_max_freq")));
        if wr(b.join("scaling_governor"), gov).is_err() && matches!(gov, "powersave" | "conservative") {
            run("modprobe", &[&format!("cpufreq_{gov}")]);
            let _ = wr(b.join("scaling_governor"), gov);
        }
        let _ = wr(b.join("scaling_max_freq"), mx);
        if gov == "conservative" {
            // per policy or global
            if let Some(d) = [b.join("conservative"), Path::new(CPUFREQ).join("conservative")].into_iter().find(|d| d.is_dir()) {
                for (k, v) in CONSERVATIVE {
                    let _ = wr(d.join(k), v);
                }
            }
        }
    }
}

pub fn set_profile(name: &str, guard: Option<&GuardState>) -> Result<CpuStatus> {
    let Some(pr) = PROFILES.iter().find(|p| p.name == name) else { return fail(format!("unknown profile {name}")) };
    let c = CpuConf {
        profile: Some(name.into()),
        governor: policies().into_iter().map(|p| (p, pr.governor.to_string())).collect(),
        max: pr.max.iter().map(|(p, m)| (p.to_string(), *m)).collect(),
        limit: pr.limit,
        has_limit: true,
    };
    apply_conf(&c);
    save(&c)?;
    Ok(status(guard))
}

/// One cluster by hand (the mode becomes Custom: no temperature limit beyond what it had).
pub fn set_policy(policy: &str, governor: &str, max_khz: u64, guard: Option<&GuardState>) -> Result<CpuStatus> {
    if !policies().iter().any(|p| p == policy) {
        return fail(format!("no {policy}"));
    }
    let mut c = conf().unwrap_or_default();
    c.profile = Some("custom".into());
    c.governor.insert(policy.into(), governor.into());
    c.max.insert(policy.into(), max_khz);
    apply_conf(&c);
    save(&c)?;
    Ok(status(guard))
}

/// At boot: apply the saved mode (cpufreq-hw appears once icc_osm_l3 is loaded: wait for it).
pub fn apply_saved() -> Result<CpuStatus> {
    for _ in 0..60 {
        if !policies().is_empty() {
            break;
        }
        sleep_s(0.5);
    }
    let Some(c) = conf() else { return set_profile("balanced", None) };
    if let Some((_, new)) = OLD_PROFILES.iter().find(|(o, _)| Some(*o) == c.profile.as_deref()) {
        return set_profile(new, None);
    }
    if let Some(p) = c.profile.as_deref().filter(|p| PROFILES.iter().any(|x| x.name == *p))
        && !c.has_limit
    {
        return set_profile(p, None); // saved before modes had a temperature limit
    }
    apply_conf(&c);
    Ok(status(None))
}
