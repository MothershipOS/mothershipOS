//! mship-audio: the phone's speaker, earpiece, headset and microphones for VMs (root), the mship.audio 1.x contract
//! (docs/contracts/mship.audio-1.md).
//!
//! The devices stay the host's PipeWire nodes: drivers, the ADSP speaker protection and the mixing with the host's own
//! sounds stay on the host. A VM's audio slot is a virtio-snd device (vhost-device-sound, started by the launcher)
//! with one stream per phone device; each stream plays into / records from the VM's own PipeWire node
//! `mship-vm-<vm>.<device>` (pw-loopbacks, also the launcher's). The guest routes between them itself, as on the phone
//! (Android: earpiece for calls, headset when plugged in). This service links each VM node to the phone's device while
//! the VM holds that handle, else to silence (outputs into `mship-silence`, a null sink; inputs from its monitor), so a
//! guest keeps running in real time and hears / records nothing:
//! - `speaker`: `.speaker.out` -> the speaker sink
//! - `earpiece`: `.earpiece.out` -> the earpiece sink
//! - `headset`: `.headset.out` -> the headphones sink, the headset mic -> `.headsetmic-in`, and the jack state into the
//!   VM's `audio/jacks` (the backend turns it into virtio-snd jack events)
//! - `mic`: the built-in mics (one stereo source: channel 1 the main mic, channel 2 the back mic) -> `.mic-in`,
//!   `.backmic-in`
//!
//! The links are made and removed here (pw-link), never by the session manager: the VM nodes' phone side does not
//! autoconnect. Release removes the device's link first (immediate silence). The host's own routing follows the
//! headset jack as on a phone: on a jack change the card profile switches between earpiece and headphones, and between
//! the built-in and the headset mic. The state is reconciled on every request, on jack changes and every few seconds
//! while a VM with an audio slot runs (a VM lent a device before its nodes exist, a restarted PipeWire).
//!
//! PipeWire is the session user's: pw-* commands run as that user. The nodes are /etc/mothership/audio.json (the
//! port's): `speaker`, `earpiece`, `headphones` (sinks), `mic`, `headset_mic` (sources); each a node name or a list of
//! names (the first that exists). Control: datagrams on /run/mothership/audio.sock, one reply each:
//! `attach <vm> <handle>`, `detach <vm> [user|screen|stop|revoked|gone] <handle>`, `status`.

use std::collections::BTreeMap;
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mship_devices::{GROUP, for_session, fresh, log};
use serde_json::{Value, json};

const CONF: &str = "/etc/mothership/audio.json";
const CTL: &str = "/run/mothership/audio.sock";
/// who holds what, kept across a restart of this service (mothershipd lends a handle once, at VM start or assignment)
const LENT: &str = "/run/mothership/audio-lent.json";
const VMS: &str = "/var/lib/mothership/vms";
const SILENCE: &str = "mship-silence";
const REASONS: &[&str] = &["user", "screen", "stop", "revoked", "gone"];
const HANDLES: &[&str] = &["speaker", "earpiece", "headset", "mic"];
/// the card's headset jack input device (its name ends with this)
const JACK_INPUT: &str = "Headset Jack";

/// The session user (owner of PipeWire): uid, gid.
fn session_user() -> (u32, u32) {
    let name = std::ffi::CString::new(GROUP).unwrap();
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() { (1000, 1000) } else { unsafe { ((*pw).pw_uid, (*pw).pw_gid) } }
}

/// A pw-* command as the session user, in its PipeWire: (success, stdout).
fn pw(args: &[&str]) -> (bool, String) {
    let (uid, gid) = session_user();
    let out = Command::new(args[0])
        .args(&args[1..])
        .uid(uid)
        .gid(gid)
        .env_clear()
        .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) => (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned()),
        Err(_) => (false, String::new()),
    }
}

/// VMs whose manifest has an audio slot (mship.audio), and whether each runs (its launcher's run.pid is alive).
fn vms() -> Vec<(String, bool)> {
    let mut v: Vec<(String, bool)> = std::fs::read_dir(VMS)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let m: Value = serde_json::from_str(&std::fs::read_to_string(e.path().join("vm.json")).ok()?).ok()?;
            m["slots"].as_object()?.values().any(|s| s["contract"] == "mship.audio").then_some(())?;
            let pid: i32 = std::fs::read_to_string(e.path().join("run.pid")).ok()?.trim().parse().unwrap_or(0);
            let running = pid > 0 && unsafe { libc::kill(pid, 0) } == 0;
            Some((e.file_name().into_string().ok()?, running))
        })
        .collect();
    v.sort();
    v
}

/// The headset jack: (headphones, mic) from the card's jack input device switches (EVIOCGSW).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Jack {
    headphones: bool,
    mic: bool,
}

fn jack_device() -> Option<PathBuf> {
    std::fs::read_dir("/sys/class/input").ok()?.flatten().find_map(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        let name = std::fs::read_to_string(e.path().join("device/name")).ok()?;
        (n.starts_with("event") && name.trim_end().ends_with(JACK_INPUT)).then(|| Path::new("/dev/input").join(n))
    })
}

fn read_jack(dev: &Path) -> Option<Jack> {
    use std::os::fd::AsRawFd;
    const SW_HEADPHONE_INSERT: u32 = 0x02;
    const SW_MICROPHONE_INSERT: u32 = 0x04;
    const SW_LINEOUT_INSERT: u32 = 0x06;
    let f = std::fs::File::open(dev).ok()?;
    let mut bits = [0u8; 8];
    // EVIOCGSW(8) = _IOC(_IOC_READ, 'E', 0x1b, 8)
    let req: libc::c_ulong = (2 << 30) | (8 << 16) | ((b'E' as libc::c_ulong) << 8) | 0x1b;
    if unsafe { libc::ioctl(f.as_raw_fd(), req, bits.as_mut_ptr()) } < 0 {
        return None;
    }
    let sw = u64::from_le_bytes(bits);
    let on = |b: u32| sw >> b & 1 == 1;
    Some(Jack { headphones: on(SW_HEADPHONE_INSERT) || on(SW_LINEOUT_INSERT), mic: on(SW_MICROPHONE_INSERT) })
}

/// The PipeWire graph: nodes, links as (output node, input node), ports (node, direction, port name, channel).
struct Graph {
    nodes: Vec<Value>,
    links: Vec<(String, String)>,
    ports: Vec<(String, String, String, String)>,
}

impl Graph {
    fn read() -> Option<Self> {
        let (ok, out) = pw(&["pw-dump", "--no-colors"]);
        let all: Vec<Value> = serde_json::from_str(&out).ok().filter(|_| ok)?;
        let name = |id: &Value| -> String {
            all.iter()
                .find(|o| &o["id"] == id)
                .and_then(|o| o["info"]["props"]["node.name"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        let links = all
            .iter()
            .filter(|o| o["type"].as_str().is_some_and(|t| t.ends_with(":Link")))
            .map(|o| (name(&o["info"]["output-node-id"]), name(&o["info"]["input-node-id"])))
            .collect();
        let mut ports: Vec<(u64, (String, String, String, String))> = all
            .iter()
            .filter(|o| o["type"].as_str().is_some_and(|t| t.ends_with(":Port")))
            .map(|o| {
                let p = &o["info"]["props"];
                let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
                (o["id"].as_u64().unwrap_or(0), (name(&p["node.id"]), s("port.direction"), s("port.name"), s("audio.channel")))
            })
            .collect();
        ports.sort_by_key(|(id, _)| *id);
        let ports = ports.into_iter().map(|(_, p)| p).collect();
        let nodes = all.into_iter().filter(|o| o["type"].as_str().is_some_and(|t| t.ends_with(":Node") || t.ends_with(":Device"))).collect();
        Some(Self { nodes, links, ports })
    }

    fn has(&self, node: &str) -> bool {
        self.nodes.iter().any(|n| n["info"]["props"]["node.name"] == node)
    }

    fn linked(&self, from: &str, to: &str) -> bool {
        self.links.iter().any(|(a, b)| a == from && b == to)
    }

    /// A configured node (a name, or a list: the first that exists), if it exists.
    fn device(&self, conf: &Value, key: &str) -> Option<String> {
        let names: Vec<&str> = match &conf[key] {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        names.into_iter().find(|n| self.has(n)).map(String::from)
    }

    /// The ALSA card device holding `node` (its PipeWire id) and the card's profiles (index, name).
    fn card_profiles(&self, conf: &Value) -> Option<(u64, Vec<(u64, String)>)> {
        let speaker = self.device(conf, "speaker")?;
        let card = self.nodes.iter().find(|n| n["info"]["props"]["node.name"] == speaker.as_str())?["info"]["props"]["device.id"].as_u64()?;
        let dev = self.nodes.iter().find(|n| n["id"].as_u64() == Some(card))?;
        let profiles = dev["info"]["params"]["EnumProfile"]
            .as_array()?
            .iter()
            .filter(|p| p["available"] != "no")
            .filter_map(|p| Some((p["index"].as_u64()?, p["name"].as_str()?.to_string())))
            .collect();
        Some((card, profiles))
    }
}

/// Link node `from`'s output ports to node `to`'s input ports: by audio channel, else by position. With `channel`, only
/// `from`'s port of that channel, to every input port (a channel of a stereo source into a mono node). Unlinking
/// removes every link between the two nodes, whichever ports they join.
fn link(g: &Graph, from: &str, to: &str, channel: Option<&str>, on: bool) {
    if !on {
        for o in g.ports.iter().filter(|p| p.0 == from && p.1 == "out") {
            for i in g.ports.iter().filter(|p| p.0 == to && p.1 == "in") {
                pw(&["pw-link", "-d", &format!("{from}:{}", o.2), &format!("{to}:{}", i.2)]);
            }
        }
        return;
    }
    let outs: Vec<_> = g.ports.iter().filter(|p| p.0 == from && p.1 == "out" && channel.is_none_or(|c| p.3 == c)).collect();
    let ins: Vec<_> = g.ports.iter().filter(|p| p.0 == to && p.1 == "in").collect();
    for (n, i) in ins.iter().enumerate() {
        let o = outs.iter().find(|o| !o.3.is_empty() && o.3 == i.3).or_else(|| outs.get(n)).or_else(|| outs.first());
        if let Some(o) = o {
            let (o, i) = (format!("{from}:{}", o.2), format!("{to}:{}", i.2));
            pw(&["pw-link", &o, &i]);
        }
    }
}

/// One VM node's wanted source/target: `Some(node, channel)` = the phone's device, `None` = silence.
type Want = Option<(String, Option<&'static str>)>;

/// Make an output node (`.out`, plays into a sink) go to `want`, or the silence sink. Unlinks first.
fn route_out(g: &Graph, vm_out: &str, want: &Want, sinks: &[String]) -> String {
    let target = want.as_ref().map(|(n, _)| n.as_str()).unwrap_or(SILENCE);
    for s in sinks.iter().map(String::as_str).chain([SILENCE]) {
        if s != target && g.linked(vm_out, s) {
            link(g, vm_out, s, None, false);
        }
    }
    if !g.linked(vm_out, target) {
        link(g, vm_out, target, None, true);
    }
    target.to_string()
}

/// Make an input node (`-in`, records from a source) take `want` (a source and channel), or the silence monitor.
fn route_in(g: &Graph, vm_in: &str, want: &Want, sources: &[String]) -> String {
    let (target, channel) = match want {
        Some((n, c)) => (n.as_str(), *c),
        None => (SILENCE, None),
    };
    for s in sources.iter().map(String::as_str).chain([SILENCE]) {
        if s != target && g.linked(s, vm_in) {
            link(g, s, vm_in, None, false);
        }
    }
    if !g.linked(target, vm_in) {
        link(g, target, vm_in, channel, true);
    }
    format!("{target}{}", channel.map(|c| format!(":{c}")).unwrap_or_default())
}

struct Service {
    conf: Value,
    /// handle -> the VM that holds it
    lent: BTreeMap<String, String>,
    jack_dev: Option<PathBuf>,
    jack: Jack,
    /// for status: what the last reconcile saw
    seen: Value,
}

impl Service {
    fn save(&self) {
        let _ = std::fs::write(LENT, json!(self.lent).to_string());
    }

    fn holds(&self, vm: &str, handle: &str) -> bool {
        self.lent.get(handle).is_some_and(|h| h == vm)
    }

    /// The host's routing follows the headset jack, as on a phone: headphones instead of the earpiece, the headset mic
    /// instead of the built-in mics. Switches the card's profile when the jack changes.
    fn follow_jack(&self, g: &Graph) {
        let Some((card, profiles)) = g.card_profiles(&self.conf) else {
            return;
        };
        let out = if self.jack.headphones { "Headphones" } else { "Earpiece" };
        let inp = if self.jack.mic { "Headset Mic" } else { "Mic" };
        let want = profiles.iter().find(|(_, n)| {
            let devices: Vec<&str> = n.trim_end_matches(')').split(['(', ',']).skip(1).map(str::trim).collect();
            devices.contains(&out) && devices.contains(&inp)
        });
        if let Some((index, name)) = want {
            pw(&["wpctl", "set-profile", &card.to_string(), &index.to_string()]);
            log(format!("headset jack {:?}: host profile {name}", self.jack));
        }
    }

    /// The jack state a VM's guest sees: the real one while it holds the headset, unplugged otherwise.
    fn write_jacks(&self, vm: &str) {
        let j = if self.holds(vm, "headset") { self.jack } else { Jack::default() };
        let path = Path::new(VMS).join(vm).join("audio/jacks");
        let want = format!("hp={} mic={}\n", j.headphones as u8, j.mic as u8);
        if path.parent().is_some_and(Path::is_dir) && std::fs::read_to_string(&path).ok().as_deref() != Some(want.as_str()) {
            let _ = std::fs::write(&path, want);
        }
    }

    /// Make the links match `lent`.
    fn reconcile(&mut self) {
        let vms = vms();
        if !vms.iter().any(|(_, running)| *running) && self.lent.is_empty() {
            self.seen = json!({});
            return;
        }
        let Some(mut g) = Graph::read() else {
            self.seen = json!({"error": "PipeWire not reachable"});
            return;
        };
        if !g.has(SILENCE) {
            pw(&["pw-cli", "create-node", "adapter", &format!(
                "{{ factory.name=support.null-audio-sink node.name={SILENCE} node.description=\"mothershipOS silence\" \
                 media.class=Audio/Sink object.linger=true audio.position=[FL FR] priority.session=0 priority.driver=0 }}")]);
            match Graph::read() {
                Some(n) => g = n,
                None => return,
            }
        }
        let dev = |k: &str| g.device(&self.conf, k);
        let (speaker, earpiece, headphones) = (dev("speaker"), dev("earpiece"), dev("headphones"));
        let (mic, headset_mic) = (dev("mic"), dev("headset_mic"));
        let sinks: Vec<String> = [&speaker, &earpiece, &headphones].iter().filter_map(|s| (*s).clone()).collect();
        let sources: Vec<String> = [&mic, &headset_mic].iter().filter_map(|s| (*s).clone()).collect();
        let mut seen = serde_json::Map::new();
        for (vm, _) in &vms {
            self.write_jacks(vm);
            let base = format!("mship-vm-{vm}");
            if !g.has(&format!("{base}.speaker.out")) {
                seen.insert(vm.clone(), json!("no nodes (VM not running)"));
                continue;
            }
            let held = |h: &str| self.holds(vm, h);
            let want = |h: &str, d: &Option<String>, c: Option<&'static str>| -> Want {
                if held(h) { d.clone().map(|n| (n, c)) } else { None }
            };
            let mut m = serde_json::Map::new();
            m.insert("speaker".into(), json!(route_out(&g, &format!("{base}.speaker.out"), &want("speaker", &speaker, None), &sinks)));
            m.insert("earpiece".into(), json!(route_out(&g, &format!("{base}.earpiece.out"), &want("earpiece", &earpiece, None), &sinks)));
            m.insert("headset".into(), json!(route_out(&g, &format!("{base}.headset.out"), &want("headset", &headphones, None), &sinks)));
            m.insert("mic".into(), json!(route_in(&g, &format!("{base}.mic-in"), &want("mic", &mic, Some("FL")), &sources)));
            m.insert("backmic".into(), json!(route_in(&g, &format!("{base}.backmic-in"), &want("mic", &mic, Some("FR")), &sources)));
            m.insert("headsetmic".into(), json!(route_in(&g, &format!("{base}.headsetmic-in"), &want("headset", &headset_mic, Some("FL")), &sources)));
            seen.insert(vm.clone(), Value::Object(m));
        }
        self.seen = Value::Object(seen);
    }

    /// Poll the jack; on a change the host's routing follows it and the holder's guest gets it.
    fn poll_jack(&mut self) {
        if self.jack_dev.as_ref().is_none_or(|d| !d.exists()) {
            self.jack_dev = jack_device();
        }
        let Some(j) = self.jack_dev.as_deref().and_then(read_jack) else {
            return;
        };
        if j != self.jack {
            self.jack = j;
            if let Some(g) = Graph::read() {
                self.follow_jack(&g);
            }
            // the card's nodes change with the profile: let them appear, then link
            std::thread::sleep(Duration::from_millis(500));
            self.reconcile();
        }
    }

    fn release(&mut self, vm: &str, handle: &str, reason: &str) {
        if !self.holds(vm, handle) {
            return;
        }
        self.lent.remove(handle);
        self.save();
        self.reconcile();
        log(format!("{vm}: {handle} released ({reason})"));
    }

    fn control(&mut self, msg: &str) -> String {
        let w: Vec<&str> = msg.split_whitespace().collect();
        let handle = w.iter().skip(2).find(|x| HANDLES.contains(x)).copied();
        match (w.as_slice(), handle) {
            (["attach", vm, ..], Some(h)) if vms().iter().any(|(n, _)| n == vm) => {
                if let Some(old) = self.lent.get(h).cloned().filter(|o| o != vm) {
                    self.release(&old, h, "revoked");
                }
                self.lent.insert(h.to_string(), vm.to_string());
                self.save();
                self.reconcile();
                log(format!("{vm}: {h} attached"));
                "ok".into()
            }
            (["attach", vm, ..], Some(_)) => format!("error: {vm} has no audio slot"),
            (["detach", vm, rest @ ..], Some(h)) => {
                let reason = rest.iter().find(|r| REASONS.contains(r)).copied().unwrap_or("user");
                self.release(vm, h, reason);
                "ok".into()
            }
            (["status", ..], _) => {
                self.reconcile();
                json!({"lent": self.lent, "jack": {"headphones": self.jack.headphones, "mic": self.jack.mic}, "vms": self.seen})
                    .to_string()
            }
            _ => format!("error: attach <vm> <{0}> | detach <vm> [reason] <{0}> | status", HANDLES.join("|")),
        }
    }
}

fn serve() -> std::io::Result<()> {
    let conf: Value = std::fs::read_to_string(CONF).ok().and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
    // the handles lent before a restart (a VM that stopped meanwhile is detached by mothershipd as usual)
    let lent: BTreeMap<String, String> =
        std::fs::read_to_string(LENT).ok().and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
    let mut s = Service { conf, lent, jack_dev: None, jack: Jack::default(), seen: json!({}) };
    fresh(Path::new(CTL));
    let ctl = UnixDatagram::bind(CTL)?;
    for_session(Path::new(CTL), 0o660);
    ctl.set_read_timeout(Some(Duration::from_millis(300)))?;
    s.poll_jack();
    if let Some(g) = Graph::read() {
        s.follow_jack(&g);
    }
    s.reconcile();
    let mut last = Instant::now();
    loop {
        let mut m = [0u8; 512];
        if let Ok((n, peer)) = ctl.recv_from(&mut m) {
            let reply = s.control(&String::from_utf8_lossy(&m[..n]));
            if let Some(p) = peer.as_pathname() {
                let _ = ctl.send_to(reply.as_bytes(), p);
            }
        }
        s.poll_jack();
        if last.elapsed() >= Duration::from_secs(3) {
            s.reconcile();
            last = Instant::now();
        }
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: mship-audio (control: {CTL})");
        return std::process::ExitCode::from(2);
    }
    if let Err(e) = serve() {
        eprintln!("mship-audio: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
