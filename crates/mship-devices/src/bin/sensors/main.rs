//! mship-sensors: the phone's sensors (root).
//!
//! The sensors hang off a DSP (on the nio: the SLPI) whose sensor process (Qualcomm SEE) reads its registry, config
//! and libraries from the host over FastRPC and keeps the registry up to date there. This service:
//! - **prepares** the files it is served under /var/lib/mothership/sensors: the DSP libraries and the registry copied
//!   once from the phone's stock partitions (mounted read-only; the registry stays a writable copy, the partition is
//!   never written), the platform's socinfo written at every start, the port's registry overrides applied;
//! - **starts** the DSP if it is off and attaches the file server (`hexagonrpcd -s`, host/hexagonrpc) once per DSP
//!   boot, for the whole life of its sensor process: attaching a second time to a sensor process whose server went
//!   away crashes it, so when the server exits the DSP is restarted and attached afresh;
//! - **serves** the sensors (the mship.sensors 1.x contract, docs/contracts/mship.sensors-1.md) to every VM whose
//!   manifest has the slot, all at once: /run/mothership/sensors/<vm>.sock, which crosvm connects the slot's console
//!   to (cuttlefish-arm64: hvc13, the emulator's sensor protocol, `goldfish`), and the split layout of Android 17's
//!   HAL: requests and replies on /run/mothership/sensors/<vm>.ctl.sock (hvc18), events on <vm>.data.sock (hvc19). The DSP's sensors (`see`) are streamed
//!   while some VM wants them, each at the highest rate any VM asks for; every VM gets its own rate;
//! - answers `status` on /run/mothership/sensors.sock (datagram, one reply).
//!
//! Configuration (the port's): /etc/mothership/sensors.json `{"remoteproc", "fastrpc", "dsp", "dsp_partition",
//! "persist_partition", "registry_overrides": {group: content}}`.

mod goldfish;
mod see;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use mship_devices::qrtr::{PORT_CTRL, Qrtr, ctrl, ctrl_packet};
use mship_core::handle::{Direction, Handle};
use mship_devices::{for_session, fresh, log, poll, publish_handles};
use serde_json::{Value, json};

const CONF: &str = "/etc/mothership/sensors.json";
const CTL: &str = "/run/mothership/sensors.sock";
const STATE: &str = "/var/lib/mothership/sensors";
const SERVER: &str = "/usr/local/lib/mothership/hexagonrpcd";
const SOCKS: &str = "/run/mothership/sensors";
/// a VM's split-layout slot: `<vm>+split`
const SPLIT: &str = "+split";
const VMS: &str = "/var/lib/mothership/vms";
const CONTRACT: &str = "mship.sensors";
/// the SEE sensor service (QMI)
const SNS: u32 = see::SERVICE;
/// a guest that reads nothing loses events beyond this
const OUT_MAX: usize = 64 * 1024;
/// sample interval bounds a guest may ask for (the slot's sensors: 100 Hz at most)
const DELAY_MS: (u32, u32) = (10, 1000);
const DEFAULT_DELAY_MS: u32 = 200;
/// restarts of the DSP after which the service waits (a broken setup must not loop)
const BURST: usize = 5;
const BURST_WINDOW: Duration = Duration::from_secs(600);

/// Qualcomm's generic sns_reg_config (the same on every SEE device): where the registry and the platform facts are
const SNS_REG_CONF: &str = "version=1
file=hw_platform=/sys/devices/soc0/hw_platform
file=platform_subtype=/sys/devices/soc0/platform_subtype
file=platform_subtype_id=/sys/devices/soc0/platform_subtype_id
file=platform_version=/sys/devices/soc0/platform_version
file=soc_id=/sys/devices/soc0/soc_id
file=revision=/sys/devices/soc0/revision
file=output=/mnt/vendor/persist/sensors/registry/registry
property=persist.vendor.sensors.enable.property=/mnt/vendor/persist/sensors/registry/file1
property=persist.vendor.sensors.enable.property1=/mnt/vendor/persist/sensors/registry/file2
";

struct Conf {
    remoteproc: String,
    fastrpc: String,
    dsp: String,
    dsp_partition: String,
    persist_partition: String,
    overrides: Vec<(String, String)>,
}

impl Conf {
    fn load() -> Result<Self, String> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(CONF).map_err(|e| format!("{CONF}: {e}"))?)
            .map_err(|e| format!("{CONF}: {e}"))?;
        let s = |k: &str| v[k].as_str().map(String::from).ok_or(format!("{CONF}: no {k}"));
        Ok(Self {
            remoteproc: s("remoteproc")?,
            fastrpc: s("fastrpc")?,
            dsp: s("dsp")?,
            dsp_partition: s("dsp_partition")?,
            persist_partition: s("persist_partition")?,
            overrides: v["registry_overrides"].as_object().into_iter().flatten()
                .filter_map(|(k, c)| Some((k.clone(), c.as_str()?.to_string()))).collect(),
        })
    }
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let o = Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if o.status.success() { Ok(()) } else { Err(format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim())) }
}

fn rd(p: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// Copy `from` (inside a stock partition, mounted read-only for it) to `to`, once: an existing `to` is kept.
fn copy_from_partition(partition: &str, from: &str, to: &Path) -> Result<(), String> {
    if to.exists() {
        return Ok(());
    }
    let mnt = Path::new(STATE).join(".mnt");
    std::fs::create_dir_all(&mnt).map_err(|e| e.to_string())?;
    let dev = format!("/dev/disk/by-partlabel/{partition}");
    run("mount", &["-o", "ro,noexec,nosuid,nodev", &dev, &mnt.to_string_lossy()])?;
    let tmp = to.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    let r = run("cp", &["-a", &mnt.join(from).to_string_lossy(), &tmp.to_string_lossy()])
        .and_then(|_| std::fs::rename(&tmp, to).map_err(|e| e.to_string()));
    let _ = run("umount", &[&mnt.to_string_lossy()]);
    log(format!("{}: copied from {partition}:/{from}", to.display()));
    r
}

/// The platform facts the sensor process selects its registry by, as Android's socinfo shows them (mainline keeps
/// the platform fields in debugfs).
fn socinfo(dir: &Path) -> Result<(), String> {
    const PLATFORMS: &[(u32, &str)] = &[(0, "Unknown"), (1, "Surf"), (2, "FFA"), (3, "Fluid"), (8, "MTP"), (9, "Liquid"), (10, "Dragon"),
                                        (11, "QRD"), (13, "HRD"), (14, "DTV"), (21, "RCM"), (23, "STP"), (24, "SBC"), (25, "ADP"),
                                        (26, "CHI"), (28, "SA"), (30, "HDK"), (34, "IDP")];
    let dbg = Path::new("/sys/kernel/debug/qcom_socinfo");
    let num = |f: &str| rd(dbg.join(f)).and_then(|v| v.parse::<u32>().ok()).ok_or(format!("{}/{f}: unreadable", dbg.display()));
    let platform = num("hardware_platform")?;
    let subtype = num("hardware_platform_subtype")?;
    let files = [
        ("hw_platform", PLATFORMS.iter().find(|(n, _)| *n == platform).map(|(_, s)| s.to_string()).unwrap_or_else(|| "Unknown".into())),
        ("platform_subtype", if subtype == 0 { "Unknown".into() } else { subtype.to_string() }),
        ("platform_subtype_id", subtype.to_string()),
        ("platform_version", num("platform_version")?.to_string()),
        ("soc_id", rd("/sys/devices/soc0/soc_id").ok_or("soc0/soc_id: unreadable")?),
        ("revision", rd("/sys/devices/soc0/revision").ok_or("soc0/revision: unreadable")?),
    ];
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for (f, v) in files {
        std::fs::write(dir.join(f), format!("{v}\n")).map_err(|e| format!("{f}: {e}"))?;
    }
    Ok(())
}

fn symlink(target: &Path, link: &Path) -> Result<(), String> {
    if std::fs::read_link(link).ok().as_deref() == Some(target) {
        return Ok(());
    }
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link).map_err(|e| format!("{}: {e}", link.display()))
}

/// The served tree (`hexagonrpcd -R`): dsp/, sensors/{registry, sns_reg_version, sns_reg.conf, config/}, socinfo/.
fn prepare(conf: &Conf) -> Result<PathBuf, String> {
    let state = Path::new(STATE);
    let root = state.join("root");
    std::fs::create_dir_all(root.join("sensors/config")).map_err(|e| e.to_string())?;
    copy_from_partition(&conf.dsp_partition, ".", &root.join("dsp"))?;
    copy_from_partition(&conf.persist_partition, "sensors/registry/registry", &state.join("registry"))?;
    symlink(&state.join("registry"), &root.join("sensors/registry"))?;
    symlink(&state.join("sns_reg_version"), &root.join("sensors/sns_reg_version"))?;
    std::fs::write(root.join("sensors/sns_reg.conf"), SNS_REG_CONF).map_err(|e| e.to_string())?;
    socinfo(&root.join("socinfo"))?;
    for (group, content) in &conf.overrides {
        if group.contains('/') || group.starts_with('.') {
            return Err(format!("registry override {group:?}: not a group name"));
        }
        std::fs::write(state.join("registry").join(group), content).map_err(|e| format!("registry {group}: {e}"))?;
    }
    Ok(root)
}

/// The DSP's remoteproc directory, by its name.
fn remoteproc(name: &str) -> Option<PathBuf> {
    std::fs::read_dir("/sys/class/remoteproc").ok()?.flatten().map(|e| e.path()).find(|p| rd(p.join("name")).as_deref() == Some(name))
}

/// Where the sensor service is registered (QRTR name service): (node, port).
fn sensor_service_at() -> Option<(u32, u32)> {
    let q = Qrtr::open().ok()?;
    q.lookup(SNS).ok()?;
    let end = Instant::now() + Duration::from_millis(500);
    while Instant::now() < end {
        if poll(&[q.fd()], Some(Duration::from_millis(100))).is_empty() {
            continue;
        }
        while let Ok(Some((_, port, data))) = q.recv() {
            match ctrl_packet(&data) {
                Some((ctrl::NEW_SERVER, s, _, node, p)) if port == PORT_CTRL && s == SNS => return Some((node, p)),
                Some((ctrl::NEW_SERVER, 0, ..)) if port == PORT_CTRL => return None, // end of the list
                _ => {}
            }
        }
    }
    None
}

fn sensor_service() -> bool {
    sensor_service_at().is_some()
}

// ---------------------------------------------------------------- serving

/// A stream of the DSP's that the slot's sensors are made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Feed {
    Accel,
    Gyro,
    Mag,
    Proximity,
    Light,
    GyroCal,
    MagCal,
}

impl Feed {
    /// the SEE data type
    fn data_type(self) -> &'static str {
        match self {
            Feed::Accel => "accel",
            Feed::Gyro => "gyro",
            Feed::Mag => "mag",
            Feed::Proximity => "proximity",
            Feed::Light => "ambient_light",
            Feed::GyroCal => "gyro_cal",
            Feed::MagCal => "mag_cal",
        }
    }

    fn streaming(self) -> bool {
        matches!(self, Feed::Accel | Feed::Gyro | Feed::Mag)
    }
}

/// What a slot sensor is made of: calibrated motion = the stream minus the DSP's calibration bias.
fn feeds(s: goldfish::Sensor) -> &'static [Feed] {
    use goldfish::Sensor::*;
    match s {
        Accel | AccelUncal => &[Feed::Accel],
        Gyro => &[Feed::Gyro, Feed::GyroCal],
        GyroUncal => &[Feed::Gyro],
        Mag => &[Feed::Mag, Feed::MagCal],
        MagUncal => &[Feed::Mag],
        Proximity => &[Feed::Proximity],
        Light => &[Feed::Light],
    }
}

/// One VM's slot console.
struct Peer {
    conn: UnixStream,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    /// the split layout's data connection (Android 17: /dev/hvc19): the events go there, the control connection
    /// carries the requests and their replies
    data: Option<UnixStream>,
    data_out: Vec<u8>,
    enabled: BTreeSet<goldfish::Sensor>,
    delay_ms: u32,
    last: BTreeMap<goldfish::Sensor, Instant>,
    dropped: u64,
}

impl Peer {
    fn send(&mut self, payload: &str) {
        let f = goldfish::frame(payload);
        if self.out.len() + f.len() > OUT_MAX {
            self.dropped += 1;
            return;
        }
        self.out.extend_from_slice(&f);
        self.flush();
    }

    /// An event: on the data connection in the split layout, else with everything else.
    fn send_event(&mut self, payload: &str) {
        if self.data.is_none() {
            return self.send(payload);
        }
        let f = goldfish::frame(payload);
        if self.data_out.len() + f.len() > OUT_MAX {
            self.dropped += 1;
            return;
        }
        self.data_out.extend_from_slice(&f);
        self.flush();
    }

    /// false: gone
    fn flush(&mut self) -> bool {
        fn drain(conn: &mut UnixStream, out: &mut Vec<u8>) -> bool {
            while !out.is_empty() {
                match conn.write(out) {
                    Ok(0) => return false,
                    Ok(n) => {
                        out.drain(..n);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => return false,
                }
            }
            true
        }
        let ctl = drain(&mut self.conn, &mut self.out);
        let data = match self.data.as_mut() {
            Some(d) => drain(d, &mut self.data_out),
            None => true,
        };
        ctl && data
    }

    fn backlog(&self) -> bool {
        !self.out.is_empty() || !self.data_out.is_empty()
    }

    /// A streaming sample to this guest, at its own rate.
    fn sample(&mut self, s: goldfish::Sensor, payload: impl FnOnce() -> String) {
        if !self.enabled.contains(&s) {
            return;
        }
        let interval = Duration::from_millis(self.delay_ms as u64 * 9 / 10);
        if !s.on_change() && self.last.get(&s).is_some_and(|t| t.elapsed() < interval) {
            return;
        }
        self.last.insert(s, Instant::now());
        self.send_event(&payload());
    }
}

struct Slot {
    path: PathBuf,
    listener: UnixListener,
    peer: Option<Peer>,
    /// the split layout: the data console's listener (`<vm>.data.sock`) and a data connection that came before the
    /// control one
    data: Option<(PathBuf, UnixListener)>,
    pending_data: Option<UnixStream>,
}

/// A VM's enabled sensors and rate, kept over a restart of this service (the slot's console stays connected meanwhile,
/// the guest's HAL doesn't ask again). Removed when the guest disconnects: a guest that connects to a running service
/// is a new boot, and its HAL expects nothing before its `list-sensors` reply.
fn state_file(vm: &str) -> PathBuf {
    Path::new(SOCKS).join(format!("{vm}.state"))
}

/// VM directories whose manifest has a mship.sensors slot.
fn sensor_vms() -> BTreeSet<String> {
    std::fs::read_dir(VMS).into_iter().flatten().flatten()
        .filter(|e| {
            let m: Value = std::fs::read_to_string(e.path().join("vm.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            m["slots"].as_object().is_some_and(|s| s.values().any(|x| x["contract"] == CONTRACT))
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

struct Serving {
    slots: BTreeMap<String, Slot>,
    /// the sensor service, while it is known
    sns_at: Option<(u32, u32)>,
    suids: BTreeMap<Feed, (u64, u64)>,
    streams: BTreeMap<Feed, (see::Stream, Option<f32>)>,
    gyro_bias: [f32; 3],
    mag_bias: [f32; 3],
    /// the on-change sensors' current values: a guest that enables one gets it at once
    proximity: Option<f32>,
    light: Option<f32>,
    events: u64,
    stream_err: String,
    /// the phone is suspending (`sleep` .. `resume`): no stream runs
    asleep: bool,
}

fn sub3(v: &[f32], b: [f32; 3]) -> [f32; 3] {
    [v[0] - b[0], v[1] - b[1], v[2] - b[2]]
}

impl Serving {
    fn scan(&mut self) {
        let names = sensor_vms();
        let new: Vec<String> = names.iter().filter(|n| !self.slots.contains_key(*n)).cloned().collect();
        let listen = |path: &Path| -> std::io::Result<UnixListener> {
            fresh(path);
            let l = UnixListener::bind(path)?;
            l.set_nonblocking(true)?;
            for_session(path, 0o660);
            Ok(l)
        };
        for n in new {
            // one console for everything (Android 16: hvc13, `<vm>.sock`), and the split layout (Android 17: control
            // hvc18 `<vm>.ctl.sock`, data hvc19 `<vm>.data.sock`): the guest's HAL uses one of them
            let path = Path::new(SOCKS).join(format!("{n}.sock"));
            match listen(&path) {
                Ok(listener) => {
                    self.slots.insert(n.clone(), Slot { path, listener, peer: None, data: None, pending_data: None });
                }
                Err(e) => log(format!("{n}: {e}")),
            }
            let (ctl, data) = (Path::new(SOCKS).join(format!("{n}.ctl.sock")), Path::new(SOCKS).join(format!("{n}.data.sock")));
            match (listen(&ctl), listen(&data)) {
                (Ok(listener), Ok(dl)) => {
                    self.slots.insert(format!("{n}{SPLIT}"), Slot { path: ctl, listener, peer: None, data: Some((data, dl)), pending_data: None });
                }
                (a, b) => log(format!("{n}: split layout: {:?} {:?}", a.err(), b.err())),
            }
        }
        let gone: Vec<String> = self.slots.keys().filter(|n| !names.contains(n.trim_end_matches(SPLIT))).cloned().collect();
        for n in gone {
            if let Some(s) = self.slots.remove(&n) {
                let _ = std::fs::remove_file(s.path);
                if let Some((p, _)) = s.data {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        self.update();
    }

    /// The DSP restarted (or its service went away): every stream starts over.
    fn reset(&mut self) {
        self.streams.clear();
        self.suids.clear();
        self.sns_at = None;
        self.proximity = None;
        self.light = None;
    }

    /// Streams follow what the guests want: each at the highest rate any of them asks for, off when none wants it.
    fn update(&mut self) {
        let mut want: BTreeMap<Feed, Option<f32>> = BTreeMap::new();
        for p in self.slots.values().filter_map(|s| s.peer.as_ref()) {
            let hz = 1000.0 / p.delay_ms as f32;
            for f in p.enabled.iter().flat_map(|s| feeds(*s)) {
                let e = want.entry(*f).or_insert(None);
                if f.streaming() {
                    *e = Some(e.unwrap_or(0.0).max(hz));
                }
            }
        }
        // while the phone suspends the DSP must not need the host: its sensor process calls the file server
        // (hexagonrpcd, frozen in suspend) and crashed on the timeout (frpc_dsp) right after a resume
        if self.asleep {
            want.clear();
        }
        // off
        self.streams.retain(|f, _| want.contains_key(f));
        if want.is_empty() {
            return;
        }
        if self.sns_at.is_none() {
            self.sns_at = sensor_service_at();
        }
        let Some((node, port)) = self.sns_at else { return };
        for (f, rate) in want {
            match self.streams.get_mut(&f) {
                Some((s, cur)) if *cur != rate => {
                    if let Err(e) = s.configure(rate) {
                        self.stream_err = format!("{}: {e}", f.data_type());
                        log(&self.stream_err);
                    } else {
                        *cur = rate;
                    }
                }
                Some(_) => {}
                None => {
                    let suid = match self.suids.get(&f) {
                        Some(s) => Ok(*s),
                        None => see::lookup(node, port, f.data_type()),
                    };
                    match suid.and_then(|suid| see::Stream::open(node, port, suid, rate).map(|s| (suid, s))) {
                        Ok((suid, s)) => {
                            self.suids.insert(f, suid);
                            log(format!("{}: on{}", f.data_type(), rate.map(|r| format!(" ({r:.0} Hz)")).unwrap_or_default()));
                            self.streams.insert(f, (s, rate));
                        }
                        Err(e) => {
                            if e != self.stream_err {
                                log(format!("{}: {e}", f.data_type()));
                                self.stream_err = e;
                            }
                        }
                    }
                }
            }
        }
    }

    fn on_stream(&mut self, f: Feed) {
        let Some((s, _)) = self.streams.get_mut(&f) else { return };
        let evs = match s.events() {
            Ok(e) => e,
            Err(e) => {
                log(format!("{}: {e}; the sensor service is gone", f.data_type()));
                return self.reset();
            }
        };
        use goldfish::Sensor as S;
        for e in evs {
            self.events += 1;
            let peers = self.slots.values_mut().filter_map(|s| s.peer.as_mut());
            match (f, e.msg_id) {
                (Feed::GyroCal, see::msg::CAL_EVENT) if e.data.len() >= 3 => self.gyro_bias = [e.data[0], e.data[1], e.data[2]],
                (Feed::MagCal, see::msg::CAL_EVENT) if e.data.len() >= 3 => self.mag_bias = [e.data[0], e.data[1], e.data[2]],
                (Feed::Accel, see::msg::STD_EVENT) if e.data.len() >= 3 => {
                    for p in peers {
                        p.sample(S::Accel, || goldfish::vec3(S::Accel, [e.data[0], e.data[1], e.data[2]]));
                        p.sample(S::AccelUncal, || goldfish::vec3(S::AccelUncal, [e.data[0], e.data[1], e.data[2]]));
                    }
                }
                (Feed::Gyro, see::msg::STD_EVENT) if e.data.len() >= 3 => {
                    let cal = sub3(&e.data, self.gyro_bias);
                    for p in peers {
                        p.sample(S::Gyro, || goldfish::vec3(S::Gyro, cal));
                        p.sample(S::GyroUncal, || goldfish::vec3(S::GyroUncal, [e.data[0], e.data[1], e.data[2]]));
                    }
                }
                (Feed::Mag, see::msg::STD_EVENT) if e.data.len() >= 3 => {
                    let cal = sub3(&e.data, self.mag_bias);
                    for p in peers {
                        p.sample(S::Mag, || goldfish::vec3(S::Mag, cal));
                        p.sample(S::MagUncal, || goldfish::vec3(S::MagUncal, [e.data[0], e.data[1], e.data[2]]));
                    }
                }
                // near: 0 cm; far: the slot's range (1)
                (Feed::Proximity, _) if e.near.is_some() => {
                    let cm = if e.near == Some(true) { 0.0 } else { 1.0 };
                    self.proximity = Some(cm);
                    for p in peers {
                        p.sample(S::Proximity, || goldfish::scalar(S::Proximity, cm));
                    }
                }
                (Feed::Light, see::msg::STD_EVENT) if !e.data.is_empty() => {
                    self.light = Some(e.data[0]);
                    for p in peers {
                        p.sample(S::Light, || goldfish::scalar(S::Light, e.data[0]));
                    }
                }
                _ => {}
            }
        }
    }

    fn accept(&mut self, name: &str) {
        let Some(Ok((conn, _))) = self.slots.get(name).map(|s| s.listener.accept()) else { return };
        let _ = conn.set_nonblocking(true);
        let data = self.slots.get_mut(name).and_then(|s| s.pending_data.take());
        let mut p = Peer { conn, inbuf: vec![], out: vec![], data, data_out: vec![], enabled: BTreeSet::new(), delay_ms: DEFAULT_DELAY_MS,
                           last: BTreeMap::new(), dropped: 0 };
        // what the guest asked for before this service (re)started: its HAL only says it again when it starts over
        if let Some(v) = std::fs::read_to_string(state_file(name)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            p.delay_ms = v["delay_ms"].as_u64().map(|d| d as u32).unwrap_or(DEFAULT_DELAY_MS);
            p.enabled = v["enabled"].as_array().into_iter().flatten().filter_map(|h| h.as_u64())
                .filter_map(|h| goldfish::Sensor::ALL.iter().find(|s| s.handle() as u64 == h).copied()).collect();
        }
        if self.slots.get_mut(name).unwrap().peer.replace(p).is_some() {
            log(format!("{name}: new connection replaces the old one"));
        }
        log(format!("{name}: guest connected"));
        self.update();
    }

    /// The split layout's data console connected: to the guest's peer, or kept until its control console connects.
    fn accept_data(&mut self, name: &str) {
        let Some(sl) = self.slots.get_mut(name) else { return };
        let Some(Ok((conn, _))) = sl.data.as_ref().map(|(_, l)| l.accept()) else { return };
        let _ = conn.set_nonblocking(true);
        match sl.peer.as_mut() {
            Some(p) => {
                p.data = Some(conn);
                p.data_out.clear();
            }
            None => sl.pending_data = Some(conn),
        }
        log(format!("{name}: data console connected"));
    }

    fn on_guest(&mut self, name: &str) {
        let Some(p) = self.slots.get_mut(name).and_then(|s| s.peer.as_mut()) else { return };
        let mut alive = p.flush();
        let mut b = [0u8; 4096];
        while alive {
            match p.conn.read(&mut b) {
                Ok(0) => alive = false,
                Ok(n) => p.inbuf.extend_from_slice(&b[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => alive = false,
            }
        }
        // the data console carries nothing from the guest: read to notice it closing
        while alive && let Some(d) = p.data.as_mut() {
            match d.read(&mut b) {
                Ok(0) => alive = false,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => alive = false,
            }
        }
        let frames = goldfish::frames(&mut p.inbuf);
        let mut changed = false;
        match frames {
            Ok(fs) => {
                for f in fs {
                    match goldfish::command(&f) {
                        goldfish::Command::ListSensors => {
                            // the guest's HAL starts over: nothing is enabled until it says so
                            changed |= !p.enabled.is_empty();
                            p.enabled.clear();
                            p.send(&goldfish::mask().to_string());
                        }
                        goldfish::Command::Set(s, on) => {
                            let new = if on { p.enabled.insert(s) } else { p.enabled.remove(&s) };
                            // an on-change sensor starts with its current value (when one is known)
                            let current = match s {
                                goldfish::Sensor::Proximity => self.proximity,
                                goldfish::Sensor::Light => self.light,
                                _ => None,
                            };
                            if let (true, true, Some(v)) = (on, new, current) {
                                p.send_event(&goldfish::scalar(s, v));
                            }
                            changed |= new;
                        }
                        goldfish::Command::Delay(ms) => {
                            let ms = ms.clamp(DELAY_MS.0, DELAY_MS.1);
                            changed |= p.delay_ms != ms;
                            p.delay_ms = ms;
                        }
                        goldfish::Command::Time => {}
                        goldfish::Command::Other(s) => log(format!("{name}: unknown request {s:?}")),
                    }
                }
            }
            Err(()) => {
                log(format!("{name}: not the protocol, stream reset"));
                p.inbuf.clear();
            }
        }
        if changed && alive {
            let st = json!({"delay_ms": p.delay_ms, "enabled": p.enabled.iter().map(|s| s.handle()).collect::<Vec<_>>()});
            let _ = std::fs::write(state_file(name), st.to_string());
        }
        if !alive {
            self.slots.get_mut(name).unwrap().peer = None;
            let _ = std::fs::remove_file(state_file(name));
            log(format!("{name}: guest disconnected"));
            changed = true;
        }
        if changed {
            self.update();
        }
    }

    fn status(&self) -> Value {
        json!({
            "streams": self.streams.iter().map(|(f, (_, r))| (f.data_type().to_string(), json!(r))).collect::<serde_json::Map<_, _>>(),
            "events": self.events,
            "vms": self.slots.iter().map(|(n, s)| (n.clone(), json!({
                "connected": s.peer.is_some(),
                "enabled": s.peer.as_ref().map(|p| p.enabled.iter().map(|x| format!("{x:?}")).collect::<Vec<_>>()),
                "delay_ms": s.peer.as_ref().map(|p| p.delay_ms),
                "dropped": s.peer.as_ref().map(|p| p.dropped),
            }))).collect::<serde_json::Map<_, _>>(),
            "gyro_bias": self.gyro_bias,
            "mag_bias": self.mag_bias,
            "last_stream_problem": (!self.stream_err.is_empty()).then_some(&self.stream_err),
        })
    }
}

struct Sensors {
    conf: Conf,
    rp: PathBuf,
    root: PathBuf,
    server: Option<Child>,
    attaches: u64,
    restarts: Vec<Instant>,
    last_exit: String,
    paused_until: Option<Instant>,
    /// the DSP was restarted since the serving side last looked: its streams start over
    restarted: bool,
    /// stopped for the phone's suspend (`sleep`): not brought up until `resume`
    asleep: bool,
}

impl Sensors {
    fn state(&self) -> String {
        rd(self.rp.join("state")).unwrap_or_default()
    }

    fn set_state(&self, s: &str) -> Result<(), String> {
        std::fs::write(self.rp.join("state"), s).map_err(|e| format!("{} {s}: {e}", self.conf.remoteproc))
    }

    fn wait_state(&self, want: &str, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.state() == want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// A fresh DSP: stopped if it runs (or crashed), started.
    fn restart_dsp(&mut self) -> Result<(), String> {
        if self.state() != "offline" {
            let _ = self.set_state("stop");
            self.wait_state("offline", Duration::from_secs(10));
        }
        self.set_state("start")?;
        if !self.wait_state("running", Duration::from_secs(20)) {
            return Err(format!("{}: not running after start ({})", self.conf.remoteproc, self.state()));
        }
        self.restarts.push(Instant::now());
        self.restarted = true;
        Ok(())
    }

    /// The DSP up and its file server attached (restarting the DSP when its sensor process had a server before).
    fn bring_up(&mut self) -> Result<(), String> {
        match self.state().as_str() {
            "running" if sensor_service() => {
                log(format!("{}: its sensor process was attached before: restarting it", self.conf.remoteproc));
                self.restart_dsp()?;
            }
            "running" => {}
            _ => self.restart_dsp()?,
        }
        // the FastRPC node appears with the DSP's glink channel
        let end = Instant::now() + Duration::from_secs(10);
        while !Path::new(&self.conf.fastrpc).exists() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(100));
        }
        let child = Command::new(SERVER)
            .args(["-f", &self.conf.fastrpc, "-d", &self.conf.dsp, "-s", "-R", &self.root.to_string_lossy(), "-w", STATE])
            .spawn()
            .map_err(|e| format!("{SERVER}: {e}"))?;
        self.attaches += 1;
        log(format!("{}: file server attached (pid {})", self.conf.remoteproc, child.id()));
        self.server = Some(child);
        Ok(())
    }

    /// The phone suspends: the DSP off with its file server. A running sensor process calls the server over FastRPC
    /// and, the server frozen with all of userspace, crashed at every resume (frpc_dsp, even with no stream); off, it
    /// also draws nothing while the phone sleeps.
    fn sleep(&mut self) {
        self.asleep = true;
        if let Some(mut c) = self.server.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if self.state() != "offline" {
            let _ = self.set_state("stop");
            self.wait_state("offline", Duration::from_secs(5));
        }
        log(format!("sleep: {} {}", self.conf.remoteproc, self.state()));
    }

    /// The phone resumed: the next check brings the DSP up and attaches (a planned restart, not counted).
    fn resume(&mut self) {
        self.asleep = false;
        self.restarted = true;
        self.restarts.clear(); // planned: not a crash burst
        self.paused_until = None;
    }

    /// The server gone: the DSP is restarted and attached again, unless that keeps happening.
    fn check(&mut self) {
        if self.asleep {
            return;
        }
        let Some(c) = self.server.as_mut() else {
            if self.paused_until.is_none_or(|t| Instant::now() >= t) {
                self.paused_until = None;
                if let Err(e) = self.bring_up() {
                    self.fail(e);
                }
            }
            return;
        };
        match c.try_wait() {
            Ok(None) => {}
            Ok(Some(st)) => {
                self.last_exit = format!("file server exited ({st}); {} {}", self.conf.remoteproc, self.state());
                log(&self.last_exit);
                self.server = None;
                self.restarts.retain(|t| t.elapsed() < BURST_WINDOW);
                if self.restarts.len() >= BURST {
                    self.paused_until = Some(Instant::now() + BURST_WINDOW);
                    log(format!("{} restarts in {} min: waiting before the next", BURST, BURST_WINDOW.as_secs() / 60));
                } else if let Err(e) = self.restart_dsp().and_then(|_| self.bring_up()) {
                    self.fail(e);
                }
            }
            Err(e) => log(format!("file server: {e}")),
        }
    }

    fn fail(&mut self, e: String) {
        log(format!("bring-up: {e} (retrying in a minute)"));
        self.last_exit = e;
        self.paused_until = Some(Instant::now() + Duration::from_secs(60));
    }

    fn status(&self, serving: &Serving) -> String {
        json!({
            "serving": serving.status(),
            "dsp": self.conf.remoteproc,
            "state": self.state(),
            "file_server": self.server.as_ref().map(|c| c.id()),
            "attaches": self.attaches,
            "restarts_recent": self.restarts.iter().filter(|t| t.elapsed() < BURST_WINDOW).count(),
            "sensor_service": sensor_service(),
            "last_problem": (!self.last_exit.is_empty()).then_some(&self.last_exit),
        })
        .to_string()
    }
}

fn serve() -> Result<(), String> {
    let conf = Conf::load()?;
    let root = prepare(&conf)?;
    let end = Instant::now() + Duration::from_secs(60);
    let rp = loop {
        if let Some(p) = remoteproc(&conf.remoteproc) {
            break p;
        }
        if Instant::now() > end {
            return Err(format!("no remoteproc named {}", conf.remoteproc));
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let mut s = Sensors { conf, rp, root, server: None, attaches: 0, restarts: vec![], last_exit: String::new(), paused_until: None, restarted: false, asleep: false };
    std::fs::create_dir_all(SOCKS).map_err(|e| format!("{SOCKS}: {e}"))?;
    for_session(Path::new(SOCKS), 0o750);
    let mut sv = Serving { slots: BTreeMap::new(), sns_at: None, suids: BTreeMap::new(), streams: BTreeMap::new(), gyro_bias: [0.0; 3],
                           mag_bias: [0.0; 3], proximity: None, light: None, events: 0, stream_err: String::new(), asleep: false };
    sv.scan();
    fresh(Path::new(CTL));
    let ctl = UnixDatagram::bind(CTL).map_err(|e| format!("{CTL}: {e}"))?;
    for_session(Path::new(CTL), 0o660);
    publish_handles("sensors", &[Handle::new("sensors:0", CONTRACT, "Sensors", "find-location-symbolic", Direction::In, "")
        .every_vm(None)
        .caps(json!({"sensors": ["accelerometer", "gyroscope", "magnetometer", "proximity", "light"], "max_rate_hz": 1000 / DELAY_MS.0}))]);
    log(format!("serving {} VM slots in {SOCKS}", sv.slots.len()));
    let (mut last_check, mut last_scan) = (Instant::now() - Duration::from_secs(10), Instant::now());
    loop {
        if last_check.elapsed() >= Duration::from_secs(1) {
            s.check();
            if std::mem::take(&mut s.restarted) {
                sv.reset();
            }
            // streams that could not start yet (the service registers a little after the attach)
            sv.update();
            last_check = Instant::now();
        }
        if last_scan.elapsed() >= Duration::from_secs(10) {
            sv.scan();
            last_scan = Instant::now();
        }
        // (fd, what, name): 0 control, 1 VM listener, 2 VM console, 3 stream, 4 VM data listener
        let mut fds: Vec<(RawFd, u8, String, Option<Feed>)> = vec![(ctl.as_raw_fd(), 0, String::new(), None)];
        for (n, sl) in &sv.slots {
            fds.push((sl.listener.as_raw_fd(), 1, n.clone(), None));
            fds.extend(sl.data.as_ref().map(|(_, l)| (l.as_raw_fd(), 4, n.clone(), None)));
            fds.extend(sl.peer.as_ref().map(|p| (p.conn.as_raw_fd(), 2, n.clone(), None)));
            fds.extend(sl.peer.as_ref().and_then(|p| p.data.as_ref()).map(|d| (d.as_raw_fd(), 2, n.clone(), None)));
        }
        fds.extend(sv.streams.iter().map(|(f, (st, _))| (st.fd(), 3, String::new(), Some(*f))));
        let backlog = sv.slots.values().filter_map(|s| s.peer.as_ref()).any(|p| p.backlog());
        let raw: Vec<RawFd> = fds.iter().map(|f| f.0).collect();
        let ready = poll(&raw, Some(if backlog { Duration::from_millis(20) } else { Duration::from_millis(500) }));
        for (fd, what, name, feed) in &fds {
            if !ready.contains(fd) && !(*what == 2 && backlog) {
                continue;
            }
            match what {
                0 => {
                    let mut b = [0u8; 256];
                    if let Ok((n, peer)) = ctl.recv_from(&mut b) {
                        let reply = match String::from_utf8_lossy(&b[..n]).trim() {
                            "status" => s.status(&sv),
                            // systemd sleep hook (dist/rootfs/usr/lib/systemd/system-sleep/mothership)
                            "sleep" => {
                                sv.asleep = true;
                                sv.update();
                                s.sleep();
                                sv.reset();
                                "ok".into()
                            }
                            "resume" => {
                                sv.asleep = false;
                                s.resume();
                                sv.update();
                                log("resume: streams follow the guests again");
                                "ok".into()
                            }
                            _ => "error: status | sleep | resume".into(),
                        };
                        if let Some(p) = peer.as_pathname() {
                            let _ = ctl.send_to(reply.as_bytes(), p);
                        }
                    }
                }
                1 => sv.accept(name),
                2 => sv.on_guest(name),
                3 => sv.on_stream(feed.unwrap()),
                4 => sv.accept_data(name),
                _ => {}
            }
        }
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: mship-sensors (control: {CTL})");
        return std::process::ExitCode::from(2);
    }
    if let Err(e) = serve() {
        eprintln!("mship-sensors: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
