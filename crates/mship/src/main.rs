//! mship: the mothershipOS command line. Everything goes through mothershipd (root, or the session user mship);
//! results are printed as JSON.
//!
//! Bound in the session: the assist key runs `mship home`, the power key `mship screen toggle`, and sway starts
//! `mship idle` (swayidle with the screen timeout, Settings › Display).

use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mship_core::api::{Op, TOPICS, Topic};
use mship_core::client::{Client, ClientError};
use serde_json::Value;

#[derive(Parser)]
#[command(name = "mship", version = mship_core::RELEASE, about = "mothershipOS: VMs, hardware, screen, power and settings")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// mothershipd's release and topics
    Hello,
    /// host status (Wi-Fi, CPU, battery, SSH)
    Status,
    /// VMs
    Vm {
        #[command(subcommand)]
        cmd: VmCmd,
    },
    /// hardware handover: who holds the display, buttons, Wi-Fi bands, internet, Bluetooth, USB
    Handover {
        #[command(subcommand)]
        cmd: HandoverCmd,
    },
    /// the assist key: from a terminal back to the shell; else take the screen back, or lend it to the running VM
    Home,
    /// the phone's screen
    Screen {
        #[command(subcommand)]
        cmd: ScreenCmd,
    },
    /// run swayidle with the screen timeout (in the session; 0 = never)
    Idle,
    /// mship.power: participants, votes, floors, whether the phone may sleep
    Power,
    /// CPU temperature guard
    Guard,
    /// per-VM CPU, memory and GPU use
    Resources,
    /// print events as they happen (all topics if none given)
    Watch { topics: Vec<String> },
    /// any API request: `mship call wifi.connect '{"ssid": "x", "password": "y"}'`
    Call { op: String, args: Option<String> },
    /// stop every VM, then power off
    Poweroff,
    /// stop every VM, then reboot
    Reboot,
}

#[derive(Subcommand)]
enum VmCmd {
    List,
    Start { vm: String },
    Stop { vm: String },
    /// bring it to the front
    Show { vm: String },
    /// freeze it (it wakes on a key, a click or `mship vm wake`)
    Sleep { vm: String },
    Wake { vm: String },
}

#[derive(Subcommand)]
enum HandoverCmd {
    /// who holds each item now
    Status,
    /// who each item is assigned to (Settings › Hardware)
    Assoc,
    /// lend items to a VM (all assigned to it if none given)
    Give { vm: String, items: Vec<String> },
    /// take items back (all if none given)
    Take { items: Vec<String> },
    /// a VM stopped: take back what it holds
    Release { vm: String },
    Toggle,
    /// assign an item to "host" or a VM
    Assign { item: String, owner: String },
}

#[derive(Subcommand)]
enum ScreenCmd {
    On,
    Off,
    Toggle,
    Status,
}

fn topic(s: &str) -> Result<Topic, String> {
    serde_json::from_value(Value::String(s.into())).map_err(|_| format!("unknown topic {s}"))
}

/// The session user's swayidle: the timeout from Settings, the screen off when it fires.
fn idle() -> Result<(), String> {
    let me = unsafe { libc::getuid() };
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|p| p.parse::<i32>().ok()) else { continue };
        use std::os::unix::fs::MetadataExt;
        let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
        if comm.trim() == "swayidle" && e.metadata().is_ok_and(|m| m.uid() == me) {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
    let t = mship_host::power::power_conf().screen_timeout;
    if t == 0 {
        return Ok(());
    }
    let err = std::process::Command::new("swayidle")
        .args(["-w", "timeout", &t.to_string(), "/usr/local/bin/mship screen off"])
        .exec();
    Err(format!("swayidle: {err}"))
}

fn run(cmd: Cmd) -> Result<Value, String> {
    let op = match cmd {
        Cmd::Idle => return idle().map(|_| Value::Null),
        Cmd::Watch { topics } => {
            let topics = if topics.is_empty() { Ok(TOPICS.to_vec()) } else { topics.iter().map(|t| topic(t)).collect() }?;
            let err = |e: ClientError| e.to_string();
            let mut c = Client::connect().map_err(err)?;
            c.subscribe(&topics).map_err(err)?;
            loop {
                let e = c.event().map_err(err)?;
                println!("{}", serde_json::to_string(&e).unwrap());
            }
        }
        Cmd::Call { op, args } => {
            let mut req = match args {
                Some(a) => serde_json::from_str::<Value>(&a).map_err(|e| format!("args: {e}"))?,
                None => Value::Object(Default::default()),
            };
            let obj = req.as_object_mut().ok_or("args must be a JSON object")?;
            obj.insert("op".into(), Value::String(op));
            serde_json::from_value::<Op>(req).map_err(|e| format!("bad request: {e}"))?
        }
        Cmd::Hello => Op::Hello,
        Cmd::Status => return host_status(),
        Cmd::Vm { cmd } => match cmd {
            VmCmd::List => Op::VmList,
            VmCmd::Start { vm } => Op::VmStart { vm },
            VmCmd::Stop { vm } => Op::VmStop { vm },
            VmCmd::Show { vm } => Op::VmShow { vm },
            VmCmd::Sleep { vm } => Op::VmSleep { vm },
            VmCmd::Wake { vm } => Op::VmWake { vm },
        },
        Cmd::Handover { cmd } => match cmd {
            HandoverCmd::Status => Op::HandoverStatus,
            HandoverCmd::Assoc => Op::HandoverAssoc,
            HandoverCmd::Give { vm, items } => Op::HandoverGive { vm, items },
            HandoverCmd::Take { items } => Op::HandoverTake { items },
            HandoverCmd::Release { vm } => Op::HandoverRelease { vm },
            HandoverCmd::Toggle => Op::HandoverToggle,
            HandoverCmd::Assign { item, owner } => Op::HandoverAssign { item, owner },
        },
        Cmd::Home => {
            // on the terminals' workspace while the host has the screen: home is the shell
            let host_shows = mship_core::client::call(Op::HandoverStatus).is_ok_and(|h| h["display"] == "host");
            if host_shows
                && let Ok(mut s) = mship_core::sway::Sway::session()
                && s.focused_workspace().ok().flatten().as_deref() == Some("terminal")
            {
                return s.command("workspace number 1").map(|_| Value::Null).map_err(|e| e.to_string());
            }
            Op::HandoverToggle
        }
        Cmd::Screen { cmd } => match cmd {
            ScreenCmd::On => Op::ScreenSet { on: true },
            ScreenCmd::Off => Op::ScreenSet { on: false },
            ScreenCmd::Toggle => Op::ScreenToggle,
            ScreenCmd::Status => Op::ScreenStatus,
        },
        Cmd::Power => Op::PowerGet,
        Cmd::Guard => Op::CpuGuard,
        Cmd::Resources => Op::Resources,
        Cmd::Poweroff => Op::PowerOff,
        Cmd::Reboot => Op::Reboot,
    };
    mship_core::client::call(op).map_err(|e| e.to_string())
}

/// The host topic's current data (the daemon pushes it on subscribe).
fn host_status() -> Result<Value, String> {
    let err = |e: ClientError| e.to_string();
    let mut c = Client::connect().map_err(err)?;
    c.subscribe(&[Topic::Host]).map_err(err)?;
    Ok(c.event().map_err(err)?.data)
}

fn main() -> ExitCode {
    match run(Cli::parse().cmd) {
        Ok(Value::Null) => ExitCode::SUCCESS,
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("mship: {e}");
            ExitCode::FAILURE
        }
    }
}
