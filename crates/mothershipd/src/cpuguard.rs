//! CPU temperature guard: keeps the CPUs under the CPU mode's temperature limit (/etc/mothership/cpu.json "limit").
//!
//! Every pass (2 s): while the hottest CPU zone is above the limit, lower the clock caps by one frequency step, plus
//! one per 5 °C over (at most 4 per pass); the big cores first, the A55s (policy0) only once the big cores are at
//! their floor (a third up their table). Once it is 5 °C under the limit, raise them a step per pass back to the
//! mode's caps. Writes scaling_max_freq only; the mode itself (cpu.json) stays as set.

use std::collections::BTreeMap;
use std::path::Path;

use mship_core::status::GuardState;
use mship_host::cpu::{self, CPUFREQ};
use mship_host::util::{rd, rd_u64, wr};

/// °C under the limit before clocks go back up
const HYSTERESIS: f64 = 5.0;
/// the A55 cluster: throttled last
const SMALL: &str = "policy0";

pub type Tables = BTreeMap<String, Vec<u64>>;

/// One guard pass, pure: caps {policy: kHz} -> new caps. `freqs`: each policy's sorted table; `mode_max`: the CPU
/// mode's cap (missing = the table's top).
pub fn step(caps: &BTreeMap<String, u64>, limit: f64, hot: f64, freqs: &Tables, mode_max: &BTreeMap<String, u64>)
            -> BTreeMap<String, u64> {
    let top: BTreeMap<&String, u64> =
        freqs.iter().map(|(p, t)| (p, mode_max.get(p).copied().unwrap_or(t[t.len() - 1]).min(t[t.len() - 1]))).collect();
    let floor: BTreeMap<&String, u64> = freqs.iter().map(|(p, t)| (p, t[t.len() / 3].min(top[p]))).collect();
    let big_done = freqs.keys().filter(|q| *q != SMALL).all(|q| caps.get(q).copied().unwrap_or(top[q]) <= floor[q]);
    let mut new = BTreeMap::new();
    for (p, table) in freqs {
        let cur = caps.get(p).copied().unwrap_or(top[p]);
        let v = if hot > limit && (p != SMALL || big_done) {
            let steps = (1 + ((hot - limit) as u64) / 5).min(4) as usize;
            let lower: Vec<u64> = table.iter().copied().filter(|&f| f < cur).collect();
            let v = if lower.len() >= steps { lower[lower.len() - steps] } else { lower.first().copied().unwrap_or(cur) };
            if cur > floor[p] { v.max(floor[p]) } else { cur }
        } else if hot < limit - HYSTERESIS {
            table.iter().copied().find(|&f| f > cur).unwrap_or(cur).min(top[p])
        } else {
            cur
        };
        new.insert(p.clone(), v);
    }
    new
}

fn cpu_temps() -> Vec<f64> {
    cpu::temps().into_iter().filter(|(k, _)| k.starts_with("cpu")).map(|(_, v)| v).collect()
}

#[derive(Default)]
pub struct CpuGuard {
    caps: BTreeMap<String, u64>,
    conf: Option<cpu::CpuConf>,
    pub state: GuardState,
}

impl CpuGuard {
    /// One pass; true when the published state changed.
    pub fn tick(&mut self) -> bool {
        let conf = cpu::conf().unwrap_or_default();
        if self.conf.as_ref() != Some(&conf) {
            self.caps.clear(); // mode changed: start from its caps
            self.conf = Some(conf.clone());
        }
        let temps = cpu_temps();
        let hot = temps.iter().cloned().fold(0.0, f64::max);
        let freqs: Tables = cpu::policies()
            .into_iter()
            .map(|p| {
                let t = rd(Path::new(CPUFREQ).join(&p).join("scaling_available_frequencies"), "");
                let mut t: Vec<u64> = t.split_whitespace().filter_map(|f| f.parse().ok()).collect();
                t.sort();
                (p, t)
            })
            .filter(|(_, t)| !t.is_empty())
            .collect();
        let mut throttled = false;
        match conf.limit {
            Some(limit) if !freqs.is_empty() => {
                let new = step(&self.caps, limit, hot, &freqs, &conf.max);
                for (p, v) in &new {
                    let f = Path::new(CPUFREQ).join(p).join("scaling_max_freq");
                    if rd_u64(&f) != *v {
                        let _ = wr(&f, v);
                    }
                }
                throttled = new.iter().any(|(p, v)| {
                    let t = &freqs[p];
                    *v < conf.max.get(p).copied().unwrap_or(t[t.len() - 1]).min(t[t.len() - 1])
                });
                self.caps = new;
            }
            _ => self.caps.clear(),
        }
        let old = std::mem::replace(&mut self.state, GuardState {
            limit: conf.limit,
            hottest: (hot * 10.0).round() / 10.0,
            throttled,
            caps: self.caps.clone(),
        });
        (old.limit, old.throttled, &old.caps) != (self.state.limit, self.state.throttled, &self.state.caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables() -> Tables {
        [("policy0", vec![300, 691, 1075, 1305, 1516, 1804]),
         ("policy4", vec![710, 1056, 1382, 1612, 1862, 2073, 2419]),
         ("policy7", vec![844, 1171, 1478, 1862, 2246, 2841, 3091])]
            .into_iter()
            .map(|(p, t)| (p.to_string(), t))
            .collect()
    }

    fn caps(v: &[(&str, u64)]) -> BTreeMap<String, u64> {
        v.iter().map(|(p, c)| (p.to_string(), *c)).collect()
    }

    fn max() -> BTreeMap<String, u64> {
        caps(&[("policy0", 1804), ("policy4", 2419), ("policy7", 2841)])
    }

    #[test]
    fn big_cores_first() {
        let c = step(&BTreeMap::new(), 80.0, 85.0, &tables(), &max());
        assert_eq!(c["policy0"], 1804); // the A55s wait for the big cores
        assert!(c["policy4"] < 2419 && c["policy7"] < 2841);
    }

    #[test]
    fn floors_then_small_cores() {
        let mut c = BTreeMap::new();
        for _ in 0..10 {
            c = step(&c, 80.0, 85.0, &tables(), &max());
        }
        assert_eq!(c, caps(&[("policy0", 1075), ("policy4", 1382), ("policy7", 1478)])); // a third up each table
    }

    #[test]
    fn hysteresis_and_recovery() {
        let low = caps(&[("policy0", 1075), ("policy4", 1382), ("policy7", 1478)]);
        assert_eq!(step(&low, 80.0, 78.0, &tables(), &max()), low); // within 5 °C: hold
        let mut c = low;
        for _ in 0..10 {
            c = step(&c, 80.0, 70.0, &tables(), &max());
        }
        assert_eq!(c, max()); // back to the mode's caps, not the hardware's top
    }

    #[test]
    fn far_over_steps_faster() {
        assert_eq!(step(&BTreeMap::new(), 80.0, 81.0, &tables(), &max())["policy7"], 2246);
        assert_eq!(step(&BTreeMap::new(), 80.0, 96.0, &tables(), &max())["policy7"], 1478); // 4 steps, at the floor
    }
}
