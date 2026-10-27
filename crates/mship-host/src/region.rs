//! Region & time: country (Wi-Fi regulatory domain), time zone, 24-hour clock.

use std::collections::BTreeMap;
use std::path::Path;

use mship_core::status::{Country, RegionConf, RegionStatus};

use crate::util::{Result, fail, load_json, run, save_json};

const CONF: &str = "/etc/mothership/region.json";
const REGDOM_MODPROBE: &str = "/etc/modprobe.d/mothership-regdom.conf";
const ZONEINFO: &str = "/usr/share/zoneinfo";

pub fn conf() -> RegionConf {
    load_json(CONF)
}

/// Countries (ISO 3166, by name) with their time zones (zone.tab).
pub fn countries() -> Vec<Country> {
    let names = std::fs::read_to_string(format!("{ZONEINFO}/iso3166.tab")).unwrap_or_default();
    let tab = std::fs::read_to_string(format!("{ZONEINFO}/zone.tab")).unwrap_or_default();
    parse_countries(&names, &tab)
}

/// iso3166.tab + zone.tab -> countries. Pure.
pub fn parse_countries(iso3166: &str, zone_tab: &str) -> Vec<Country> {
    let rows = |text: &str| -> Vec<Vec<String>> {
        text.lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .map(|l| l.split('\t').map(String::from).collect())
            .collect()
    };
    let mut zones: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in rows(zone_tab) {
        if f.len() >= 3 {
            zones.entry(f[0].clone()).or_default().push(f[2].clone());
        }
    }
    let mut out: Vec<Country> = rows(iso3166)
        .into_iter()
        .filter(|f| f.len() >= 2)
        .map(|f| {
            let mut z = zones.get(&f[0]).cloned().unwrap_or_default();
            z.sort();
            Country { code: f[0].clone(), name: f[1].clone(), zones: z }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn status() -> RegionStatus {
    let tz = std::fs::canonicalize("/etc/localtime")
        .ok()
        .and_then(|p| p.strip_prefix(ZONEINFO).ok().map(|z| z.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "Etc/UTC".into());
    let reg = run("iw", &["reg", "get"]).stdout;
    let wifi_regdom = reg
        .find("country ")
        .map(|i| &reg[i + 8..])
        .filter(|r| r.as_bytes().get(2) == Some(&b':'))
        .map(|r| r[..2].to_string());
    RegionStatus {
        conf: conf(),
        timezone: tz,
        wifi_regdom,
        time: chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z").to_string(),
    }
}

pub fn set_country(cc: &str) -> Result<RegionStatus> {
    let cc = cc.to_uppercase();
    if cc != "00" && !countries().iter().any(|c| c.code == cc) {
        return fail(format!("unknown country {cc}"));
    }
    // Wi-Fi regulatory domain: now (cfg80211) and on every boot (module option)
    run("iw", &["reg", "set", &cc]);
    std::fs::write(REGDOM_MODPROBE,
                   format!("# mothershipOS region (Settings > Region & time)\noptions cfg80211 ieee80211_regdom={cc}\n"))?;
    let mut c = conf();
    c.country = cc;
    save_json(CONF, &c)?;
    Ok(status())
}

pub fn set_timezone(tz: &str) -> Result<RegionStatus> {
    let path = Path::new(ZONEINFO).join(tz);
    let real = std::fs::canonicalize(&path).ok();
    if tz.contains("..") || !real.as_ref().is_some_and(|p| p.starts_with(ZONEINFO) && p.is_file()) {
        return fail(format!("unknown time zone {tz}"));
    }
    if !run("timedatectl", &["set-timezone", tz]).ok() {
        let _ = std::fs::remove_file("/etc/localtime.new");
        std::os::unix::fs::symlink(&path, "/etc/localtime.new")?;
        std::fs::rename("/etc/localtime.new", "/etc/localtime")?;
        std::fs::write("/etc/timezone", format!("{tz}\n"))?;
    }
    Ok(status())
}

pub fn set_clock24(on: bool) -> Result<RegionStatus> {
    let mut c = conf();
    c.clock24 = on;
    save_json(CONF, &c)?;
    Ok(status())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countries_with_zones() {
        let iso = "# comment\nDE\tGermany\nCZ\tCzech Republic\nAQ\tAntarctica\n";
        let tab = "#cc\tcoord\tTZ\nDE\t+5230+01322\tEurope/Berlin\nDE\t+4742+00841\tEurope/Busingen\nCZ\t+5005+01426\tEurope/Prague\n";
        let c = parse_countries(iso, tab);
        assert_eq!(c.iter().map(|c| c.code.as_str()).collect::<Vec<_>>(), ["AQ", "CZ", "DE"]); // by name
        assert_eq!(c[2].zones, ["Europe/Berlin", "Europe/Busingen"]);
        assert!(c[0].zones.is_empty());
    }
}
