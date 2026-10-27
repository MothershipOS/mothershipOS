//! Host status and settings as mothershipd reports them (the shell and the CLI read these). Field names match the
//! JSON the earlier tools produced, so the shapes stay stable on the wire.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- Wi-Fi
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WifiStatus {
    pub radio: bool,
    /// "host" | "vm:<vm dir>": who uses the phone's Wi-Fi uplink as its internet
    pub owner: String,
    pub owner_name: String,
    pub present: bool,
    /// NetworkManager's device state (connected, disconnected, unavailable, ...)
    pub state: String,
    pub ssid: Option<String>,
    pub ip: Option<String>,
    /// dBm
    pub signal: Option<i32>,
    /// MHz
    pub freq: Option<f64>,
    /// saved connections (names)
    pub saved: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WifiNetwork {
    pub ssid: String,
    /// %
    pub signal: i32,
    /// "" = open
    pub security: String,
    pub band: String,
    pub active: bool,
    pub saved: bool,
}

// ---------------------------------------------------------------- SSH
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshConf {
    pub enabled: bool,
    /// "usb" | "all"
    pub listen: String,
    #[serde(rename = "password-auth")]
    pub password_auth: bool,
    #[serde(rename = "root-login")]
    pub root_login: bool,
}

impl Default for SshConf {
    fn default() -> Self {
        Self { enabled: true, listen: "all".into(), password_auth: false, root_login: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SshStatus {
    #[serde(flatten)]
    pub conf: SshConf,
    pub active: bool,
    /// user -> key comments (or key prefixes)
    pub keys: BTreeMap<String, Vec<String>>,
    /// "TYPE SHA256:..." per host key
    pub hostkeys: Vec<String>,
    pub addresses: Vec<String>,
}

// ---------------------------------------------------------------- CPU
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuPolicy {
    pub policy: String,
    pub cpus: String,
    pub governor: String,
    pub governors: Vec<String>,
    /// kHz
    pub cur: u64,
    pub max: u64,
    pub hw_min: u64,
    pub hw_max: u64,
    pub freqs: Vec<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuStatus {
    /// cool | balanced | performance | custom
    pub profile: Option<String>,
    pub policies: Vec<CpuPolicy>,
    /// thermal zone -> °C (cpu* and gpu* zones)
    pub temps: BTreeMap<String, f64>,
    pub hottest: Option<f64>,
    /// the CPU mode's temperature limit, °C
    pub limit: Option<f64>,
    /// the CPU guard is holding the clocks below the mode's caps
    pub throttled: bool,
}

/// The CPU guard's state (mothershipd).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuardState {
    pub limit: Option<f64>,
    pub hottest: f64,
    pub throttled: bool,
    /// policy -> cap, kHz
    pub caps: BTreeMap<String, u64>,
}

// ---------------------------------------------------------------- region
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RegionConf {
    /// ISO 3166 code, "00" = world
    pub country: String,
    pub clock24: bool,
}

impl Default for RegionConf {
    fn default() -> Self {
        Self { country: "00".into(), clock24: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RegionStatus {
    #[serde(flatten)]
    pub conf: RegionConf,
    pub timezone: String,
    pub wifi_regdom: Option<String>,
    pub time: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Country {
    pub code: String,
    pub name: String,
    pub zones: Vec<String>,
}

// ---------------------------------------------------------------- screen & sleep
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerConf {
    /// s; 0 = never
    pub screen_timeout: u64,
    pub sleep: bool,
    /// s the screen stays off before the phone suspends
    pub sleep_delay: u64,
    pub stay_awake_usb: bool,
}

impl Default for PowerConf {
    fn default() -> Self {
        Self { screen_timeout: 60, sleep: true, sleep_delay: 10, stay_awake_usb: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PowerStatus {
    #[serde(flatten)]
    pub conf: PowerConf,
    /// "on" | "off": the phone's own screen
    pub screen: String,
    /// the daemon's last sleep decision
    pub sleepd: String,
    pub suspends: u64,
}

// ---------------------------------------------------------------- battery
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BatteryConf {
    /// %
    pub end_threshold: u32,
    pub charging: bool,
}

impl Default for BatteryConf {
    fn default() -> Self {
        Self { end_threshold: 100, charging: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Charger {
    pub online: bool,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub input_limit_ma: i64,
    /// V
    pub voltage: f64,
    pub charge_current_ma: i64,
    pub charge_type: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BatteryStatus {
    pub present: bool,
    #[serde(default)]
    pub capacity: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub health: Option<String>,
    /// V
    #[serde(default)]
    pub voltage: f64,
    /// mA, positive = into the battery
    #[serde(default)]
    pub current: f64,
    /// W (signed as the driver reports it)
    #[serde(default)]
    pub power: f64,
    /// °C
    #[serde(default)]
    pub temp: f64,
    #[serde(default)]
    pub full_design_mah: i64,
    #[serde(default)]
    pub end_threshold: Option<i64>,
    #[serde(default)]
    pub behaviour: Option<String>,
    #[serde(default)]
    pub charger: Option<Charger>,
    #[serde(default)]
    pub conf: BatteryConf,
}

// ---------------------------------------------------------------- Bluetooth
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BtDevice {
    pub mac: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub paired: Option<String>,
    #[serde(default)]
    pub bonded: Option<String>,
    #[serde(default)]
    pub trusted: Option<String>,
    #[serde(default)]
    pub connected: Option<String>,
    #[serde(default)]
    pub rssi: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BtStatus {
    pub present: bool,
    pub powered: Option<String>,
    pub discoverable: Option<String>,
    pub pairable: Option<String>,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub address: Option<String>,
    pub paired: Vec<BtDevice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BtFound {
    pub mac: String,
    pub name: String,
    /// a real name (not the address)
    pub named: bool,
    pub rssi: Option<i32>,
}

// ---------------------------------------------------------------- summary + resources
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SshSummary {
    pub enabled: bool,
    pub active: bool,
    pub listen: String,
    pub addresses: Vec<String>,
}

/// The home screen's host status (topic "host").
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostStatus {
    pub wifi: WifiStatus,
    pub owners: BTreeMap<String, String>,
    pub cpu: CpuStatus,
    /// %
    pub brightness: u32,
    pub battery: BatteryStatus,
    pub ssh: SshSummary,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VmResources {
    pub dir: String,
    pub name: String,
    /// CPU time of the VMM's process tree, clock ticks
    pub ticks: u64,
    pub pss_kb: u64,
    /// GPU busy time, ns
    pub gpu_ns: u64,
}

/// Root-only counters for the Resources page (the shell turns them into rates).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Resources {
    /// monotonic seconds
    pub t: f64,
    /// GPU busy time of all msm DRM clients, ns
    pub gpu_ns: u64,
    pub venus_hz: u64,
    pub vms: Vec<VmResources>,
}
