//! The phone's parts as the running kernel shows them: the panel, the input devices, the CPU clusters, the GPU, the
//! battery, the backlight and the Wi-Fi interface, found in sysfs. Nothing in mothershipOS names a phone's parts; what
//! can't be found comes from the port's `/etc/mothership/device.json` (docs/ports.md):
//!
//! ```json
//! { "panel": { "refresh_hz": 90, "dpi": 420, "scale": 2 }, "home_key": 217, "boot": { "partition": "boot_b" } }
//! ```

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const CONF: &str = "/etc/mothership/device.json";

/// device.json (empty when absent or broken), read once per process.
pub fn conf() -> &'static serde_json::Value {
    static C: OnceLock<serde_json::Value> = OnceLock::new();
    C.get_or_init(|| std::fs::read_to_string(CONF).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(serde_json::Value::Null))
}

fn read(p: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim_end_matches(['\n', '\0']).to_string())
}

fn entries(dir: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// The phone's name: the device tree's model.
pub fn model() -> String {
    read("/sys/firmware/devicetree/base/model").unwrap_or_else(|| "unknown".into())
}

/// Linux started at EL2: KVM is there and VMs can run.
pub fn has_kvm() -> bool {
    Path::new("/dev/kvm").exists()
}

// ---------------------------------------------------------------- display

/// The built-in panel: the connected DSI / eDP / DPI / LVDS connector.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    /// the connector as DRM and sway name it, e.g. "DSI-1"
    pub output: String,
    /// its card, e.g. "/dev/dri/card1"
    pub card: String,
    pub width: u32,
    pub height: u32,
    /// device.json panel.refresh_hz (sysfs doesn't tell), default 60
    pub refresh_hz: u32,
    /// device.json panel.dpi, default 400
    pub dpi: u32,
    /// the host session's scale, device.json panel.scale, default 2
    pub scale: f64,
}

const BUILT_IN: &[&str] = &["DSI", "eDP", "DPI", "LVDS"];

pub fn panel() -> Option<Panel> {
    let c = &conf()["panel"];
    entries("/sys/class/drm").into_iter().find_map(|p| {
        let name = p.file_name()?.to_string_lossy().into_owned();
        let (card, output) = name.split_once('-')?;
        if !card.starts_with("card") || !BUILT_IN.iter().any(|t| output.starts_with(&format!("{t}-"))) {
            return None;
        }
        // a panel lists its mode whether or not it is on
        let (w, h) = read(p.join("modes"))?.lines().next()?.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))?;
        Some(Panel {
            output: output.into(),
            card: format!("/dev/dri/{card}"),
            width: w,
            height: h,
            refresh_hz: c["refresh_hz"].as_u64().unwrap_or(60) as u32,
            dpi: c["dpi"].as_u64().unwrap_or(400) as u32,
            scale: c["scale"].as_f64().unwrap_or(2.0),
        })
    })
}

/// The panel's connector name, "DSI-1" when none is found (sway then ignores commands for it).
pub fn panel_output() -> String {
    panel().map(|p| p.output).unwrap_or_else(|| "DSI-1".into())
}

/// A VM's vm.json "output" as a connector name: "panel" (the built-in panel), "external" (the first connected other
/// connector, e.g. a USB-C DisplayPort monitor; "DP-1" when none), or a connector name.
pub fn output(name: &str) -> String {
    match name {
        "panel" => panel_output(),
        "external" => entries("/sys/class/drm")
            .into_iter()
            .find_map(|p| {
                let n = p.file_name()?.to_string_lossy().into_owned();
                let (card, out) = n.split_once('-')?;
                (card.starts_with("card") && !BUILT_IN.iter().any(|t| out.starts_with(&format!("{t}-"))) && !out.starts_with("Writeback")
                    && read(p.join("status")).as_deref() == Some("connected"))
                    .then(|| out.to_string())
            })
            .unwrap_or_else(|| "DP-1".into()),
        n => n.into(),
    }
}

/// vm.json "output" names the built-in panel ("panel" or the panel's connector).
pub fn is_panel(name: &str) -> bool {
    match name {
        "panel" => true,
        "external" => false,
        n => n == panel_output(),
    }
}

/// The backlight: device.json backlight (a /sys/class/backlight name), else the first one.
pub fn backlight() -> Option<PathBuf> {
    if let Some(n) = conf()["backlight"].as_str() {
        return Some(Path::new("/sys/class/backlight").join(n));
    }
    entries("/sys/class/backlight").into_iter().next()
}

// ---------------------------------------------------------------- input

pub const KEY_VOLUMEDOWN: u16 = 114;
pub const KEY_VOLUMEUP: u16 = 115;
pub const KEY_POWER: u16 = 116;
pub const KEY_WAKEUP: u16 = 143;
pub const KEY_SEARCH: u16 = 217;
const ABS_MT_POSITION_X: u16 = 0x35;
const INPUT_PROP_DIRECT: u16 = 1;

/// An input device (/sys/class/input/inputN) and its event node.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub name: String,
    /// "/dev/input/eventN"
    pub node: String,
    pub vendor: u16,
    pub product: u16,
    keys: Vec<u64>,
    abs: Vec<u64>,
    props: Vec<u64>,
    sw: Vec<u64>,
}

/// A sysfs capability bitmap ("8000 0 0": 64-bit words, the highest first) as words, the lowest first.
fn bitmap(s: &str) -> Vec<u64> {
    s.split_whitespace().rev().map(|w| u64::from_str_radix(w, 16).unwrap_or(0)).collect()
}

fn bit(map: &[u64], n: u16) -> bool {
    map.get(n as usize / 64).is_some_and(|w| w >> (n % 64) & 1 == 1)
}

impl Input {
    pub fn has_key(&self, code: u16) -> bool {
        bit(&self.keys, code)
    }

    /// A touchscreen: multi-touch positions on the display itself.
    pub fn is_touchscreen(&self) -> bool {
        bit(&self.abs, ABS_MT_POSITION_X) && bit(&self.props, INPUT_PROP_DIRECT)
    }

    /// Plain keys: no axes and no switches (a headset jack reports volume keys too, and switches).
    pub fn is_keys(&self) -> bool {
        self.abs.iter().all(|w| *w == 0) && self.sw.iter().all(|w| *w == 0) && self.keys.iter().any(|w| *w != 0)
    }

    /// sway's identifier: "vendor:product:name" with spaces as underscores.
    pub fn sway_id(&self) -> String {
        format!("{}:{}:{}", self.vendor, self.product, self.name.replace(' ', "_"))
    }
}

/// Every input device with an event node.
pub fn inputs() -> Vec<Input> {
    entries("/sys/class/input")
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("input")))
        .filter_map(|p| {
            let ev = std::fs::read_dir(&p).ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|n| n.starts_with("event"))?;
            let cap = |n: &str| bitmap(&read(p.join("capabilities").join(n)).unwrap_or_default());
            let hex = |n: &str| read(p.join("id").join(n)).and_then(|v| u16::from_str_radix(&v, 16).ok()).unwrap_or(0);
            Some(Input {
                name: read(p.join("name")).unwrap_or_default(),
                node: format!("/dev/input/{ev}"),
                vendor: hex("vendor"),
                product: hex("product"),
                keys: cap("key"),
                abs: cap("abs"),
                props: bitmap(&read(p.join("properties")).unwrap_or_default()),
                sw: cap("sw"),
            })
        })
        .collect()
}

pub fn touchscreen() -> Option<Input> {
    inputs().into_iter().find(Input::is_touchscreen)
}

/// The volume keys' devices (also carrying other buttons, e.g. an assist key).
pub fn volume_keys() -> Vec<Input> {
    inputs().into_iter().filter(|i| i.is_keys() && (i.has_key(KEY_VOLUMEUP) || i.has_key(KEY_VOLUMEDOWN))).collect()
}

/// What wakes the phone: the power key and wake gestures (KEY_WAKEUP), on devices without volume keys.
pub fn power_keys() -> Vec<Input> {
    inputs()
        .into_iter()
        .filter(|i| i.is_keys() && (i.has_key(KEY_POWER) || i.has_key(KEY_WAKEUP)) && !i.has_key(KEY_VOLUMEUP) && !i.has_key(KEY_VOLUMEDOWN))
        .collect()
}

/// The key that, held, switches the screen between mothershipOS and the VM: device.json home_key (a key code),
/// default KEY_SEARCH; 0: none (a phone without a spare key).
pub fn home_key() -> u16 {
    conf()["home_key"].as_u64().map_or(KEY_SEARCH, |k| k as u16)
}

/// Event nodes joined by commas (mship-inputd's arguments).
pub fn nodes(devs: &[Input]) -> String {
    devs.iter().map(|d| d.node.as_str()).collect::<Vec<_>>().join(",")
}

// ---------------------------------------------------------------- CPU, GPU, battery, network

/// A CPU cluster (a cpufreq policy).
#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    /// "policy4"
    pub policy: String,
    pub cpus: Vec<u32>,
    /// the core's name from its MIDR, e.g. "A77"
    pub core: String,
    /// available frequencies, kHz, ascending
    pub freqs: Vec<u64>,
}

impl Cluster {
    pub fn max(&self) -> u64 {
        self.freqs.last().copied().unwrap_or(0)
    }

    /// The highest frequency at most `khz` (the lowest one when all are higher).
    pub fn at_most(&self, khz: u64) -> u64 {
        self.freqs.iter().copied().filter(|f| *f <= khz).max().or(self.freqs.first().copied()).unwrap_or(khz)
    }
}

/// The core name of an arm64 MIDR (implementer, part).
pub fn core_name(midr: u64) -> String {
    let (imp, part) = ((midr >> 24) & 0xff, (midr >> 4) & 0xfff);
    let name = match (imp, part) {
        (0x41, 0xd03) => "A53",
        (0x41, 0xd04) => "A35",
        (0x41, 0xd05) | (0x51, 0x803) | (0x51, 0x805) => "A55",
        (0x41, 0xd07) => "A57",
        (0x41, 0xd08) => "A72",
        (0x41, 0xd09) => "A73",
        (0x41, 0xd0a) | (0x51, 0x802) => "A75",
        (0x41, 0xd0b) | (0x51, 0x804) => "A76",
        (0x41, 0xd0d) => "A77",
        (0x41, 0xd41) => "A78",
        (0x41, 0xd44) => "X1",
        (0x41, 0xd46) => "A510",
        (0x41, 0xd47) => "A710",
        (0x41, 0xd48) => "X2",
        (0x41, 0xd4d) => "A715",
        (0x41, 0xd4e) => "X3",
        (0x41, 0xd80) => "A520",
        (0x41, 0xd81) => "A720",
        (0x41, 0xd82) => "X4",
        _ => return format!("core {imp:#x}:{part:#x}"),
    };
    name.into()
}

pub const CPUFREQ: &str = "/sys/devices/system/cpu/cpufreq";

/// The clusters, slowest first.
pub fn clusters() -> Vec<Cluster> {
    let mut v: Vec<Cluster> = entries(CPUFREQ)
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("policy")))
        .map(|p| {
            let cpus: Vec<u32> = read(p.join("related_cpus")).unwrap_or_default().split_whitespace().filter_map(|c| c.parse().ok()).collect();
            let mut freqs: Vec<u64> = read(p.join("scaling_available_frequencies")).unwrap_or_default().split_whitespace().filter_map(|f| f.parse().ok()).collect();
            if freqs.is_empty() {
                freqs = ["cpuinfo_min_freq", "cpuinfo_max_freq"].iter().filter_map(|n| read(p.join(n))?.parse().ok()).collect();
            }
            freqs.sort();
            freqs.dedup();
            let midr = cpus.first().and_then(|c| read(format!("/sys/devices/system/cpu/cpu{c}/regs/identification/midr_el1")))
                .and_then(|m| u64::from_str_radix(m.trim_start_matches("0x"), 16).ok());
            Cluster { policy: p.file_name().unwrap().to_string_lossy().into_owned(), cpus, core: midr.map_or("core".into(), core_name), freqs }
        })
        .collect();
    v.sort_by_key(|c| (c.max(), c.cpus.first().copied()));
    v
}

/// What the cluster is for, by its place among the clusters (slowest first): "Efficiency cores (A55 ×4)".
pub fn cluster_label(clusters: &[Cluster], i: usize) -> String {
    let c = &clusters[i];
    let kind = match (i, clusters.len()) {
        (_, 1) => "Cores",
        (0, _) => "Efficiency cores",
        (i, n) if i == n - 1 && n >= 3 && c.cpus.len() <= 2 => "Prime core",
        _ => "Performance cores",
    };
    if c.cpus.len() == 1 { format!("{kind} ({})", c.core).replace("cores (", "core (") } else { format!("{kind} ({} ×{})", c.core, c.cpus.len()) }
}

/// The GPU's devfreq node (`/sys/class/devfreq/<addr>.gpu`).
pub fn gpu_devfreq() -> Option<PathBuf> {
    entries("/sys/class/devfreq").into_iter().find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".gpu")))
}

/// The GPU's name from its compatible: "qcom,adreno-650.2" -> "Adreno 650".
pub fn gpu_name() -> String {
    let compat = gpu_devfreq().and_then(|p| read(p.join("device/of_node/compatible"))).unwrap_or_default();
    compat.split('\0').find_map(|c| c.strip_prefix("qcom,adreno-")).and_then(|v| v.split('.').next()).map_or("GPU".into(), |n| format!("Adreno {n}"))
}

/// A power supply of the given type ("Battery", "USB", "Mains"); for USB the charger (not a USB-C port's source).
pub fn power_supply(kind: &str) -> Option<PathBuf> {
    entries("/sys/class/power_supply").into_iter().find(|p| {
        read(p.join("type")).as_deref() == Some(kind) && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("tcpm-source"))
    })
}

/// The phone's own Wi-Fi station (NetworkManager's): the first wireless interface that isn't a mothershipOS band
/// interface (mshw*).
pub fn wifi_iface() -> Option<String> {
    entries("/sys/class/net").into_iter().filter_map(|p| {
        let n = p.file_name()?.to_string_lossy().into_owned();
        (p.join("phy80211").exists() && !n.starts_with("mshw")).then_some(n)
    }).next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmaps() {
        // the nio's wake gesture: KEY_WAKEUP; its power key; gpio-keys: volume up + KEY_SEARCH
        assert!(bit(&bitmap("8000 0 0"), KEY_WAKEUP));
        assert!(bit(&bitmap("10000000000000 0"), KEY_POWER));
        let g = bitmap("2000000 0 8000000000000 0");
        assert!(bit(&g, KEY_VOLUMEUP) && bit(&g, KEY_SEARCH) && !bit(&g, KEY_POWER));
        // its touchscreen: MT positions, INPUT_PROP_DIRECT
        assert!(bit(&bitmap("661800001000003"), ABS_MT_POSITION_X) && bit(&bitmap("2"), INPUT_PROP_DIRECT));
    }

    #[test]
    fn cores_and_labels() {
        assert_eq!(core_name(0x51df805e), "A55");
        assert_eq!(core_name(0x411fd0d0), "A77");
        let c = |p: &str, n: u32, core: &str, max: u64| Cluster { policy: p.into(), cpus: (0..n).collect(), core: core.into(), freqs: vec![300000, max] };
        let nio = [c("policy0", 4, "A55", 1804800), c("policy4", 3, "A77", 2419200), c("policy7", 1, "A77", 3091200)];
        assert_eq!((0..3).map(|i| cluster_label(&nio, i)).collect::<Vec<_>>(),
                   ["Efficiency cores (A55 ×4)", "Performance cores (A77 ×3)", "Prime core (A77)"]);
        let sdm845 = [c("policy0", 4, "A55", 1766400), c("policy4", 4, "A75", 2803200)];
        assert_eq!(cluster_label(&sdm845, 1), "Performance cores (A75 ×4)");
        assert_eq!(nio[2].at_most(2000000), 300000);
    }
}
