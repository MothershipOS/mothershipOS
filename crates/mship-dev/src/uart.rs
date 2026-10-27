//! `uart [device]`: the nio debug UART (uart12 @ 0xa90000, 115200 8N1) and the phone's USB mode, in the terminal.
//!
//! - A feeder owns the serial port only while nothing else does: it opens it without TIOCEXCL and steps aside the
//!   moment another process opens the device (a `cat`, a capture), then takes it again when it is free. It writes
//!   each line, with a host timestamp, to the shared log (motorola_nio/tools/uart/live.log): the single reader of the
//!   port, and anything (mship-dev sh, captures) can append to the log too.
//! - The display tails that log, highlighting boot and fault markers, with a status line: the phone's USB mode
//!   (EDL / fastboot / adb / off, every 500 ms) and the feeder's state.
//! - Keys: m writes a MARK line, r reopens the port, c clears the screen, q quits.
//!
//! The device defaults to the FT232 adapter (0403:6001), else /dev/ttyUSB0. It survives the phone and the adapter
//! going away (reset, EDL, reboot).

use std::io::{BufRead, Read, Seek, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use crate::live;
use crate::serial::Serial;

const FT232: (&str, &str) = ("0403", "6001");
const PHONE_VIDS: &[&str] = &["05c6", "18d1", "22b8", "2a70"];
const USB_IDS: &[(&str, &str, &str)] = &[
    ("05c6", "9008", "EDL (QDL 9008): ready to flash"),
    ("05c6", "900e", "Diag 900e"),
    ("18d1", "4ee0", "FASTBOOT"),
    ("18d1", "d001", "FASTBOOT (fastbootd)"),
    ("22b8", "2e80", "FASTBOOT (Motorola)"),
    ("18d1", "4ee7", "ADB (Android)"),
    ("18d1", "4e11", "ADB / Android (composite)"),
    ("22b8", "2e81", "ADB (Motorola)"),
    ("22b8", "2e76", "Motorola (adb/mtp)"),
    ("1d6b", "0104", "mothershipOS dev link (NCM + ACM)"),
];
/// (marker, ANSI colour, bold)
const HIGHLIGHTS: &[(&str, &str, bool)] = &[
    ("EL2 FAULT", "31", true),
    ("HANDOFF", "35", true),
    ("HYP3", "35", true),
    ("HYP2", "35", true),
    ("Secure Boot", "36", false),
    ("QHEE", "34", false),
    ("UEFI", "35", false),
    ("Sahara", "33", false),
    ("ESR_EL2", "31", false),
    ("error", "31", false),
    ("fault", "31", false),
    ("panic", "31", false),
];

fn rd(p: impl AsRef<std::path::Path>) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_lowercase()).unwrap_or_default()
}

fn ft232_tty() -> Option<String> {
    let mut ttys: Vec<String> = std::fs::read_dir("/sys/class/tty").ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("ttyUSB")).collect();
    ttys.sort();
    ttys.into_iter().find(|n| {
        std::fs::canonicalize(format!("/sys/class/tty/{n}/device")).is_ok_and(|d| {
            d.ancestors().take(5).find(|p| p.join("idVendor").exists()).is_some_and(|p| (rd(p.join("idVendor")), rd(p.join("idProduct"))) == (FT232.0.into(), FT232.1.into()))
        })
    }).map(|n| format!("/dev/{n}"))
}

/// (phone mode, FT232 present)
fn usb_state() -> (Option<String>, bool) {
    let mut phone = None;
    let mut ft = false;
    for e in std::fs::read_dir("/sys/bus/usb/devices").into_iter().flatten().flatten() {
        let (v, p) = (rd(e.path().join("idVendor")), rd(e.path().join("idProduct")));
        if (v.as_str(), p.as_str()) == FT232 {
            ft = true;
        }
        if phone.is_none() && (PHONE_VIDS.contains(&v.as_str()) || (v == "1d6b" && p == "0104")) {
            let known = USB_IDS.iter().find(|(a, b, _)| *a == v && *b == p).map(|(_, _, l)| l.to_string());
            phone = Some(known.unwrap_or_else(|| format!("{} ({v}:{p})", std::fs::read_to_string(e.path().join("product")).unwrap_or("phone".into()).trim())));
        }
    }
    (phone, ft)
}

/// Other processes with `dev` open.
fn other_openers(dev: &str) -> Vec<String> {
    let me = std::process::id().to_string();
    let real = std::fs::canonicalize(dev).unwrap_or_else(|_| dev.into());
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()) && e.file_name().to_string_lossy() != me)
        .filter(|e| std::fs::read_dir(e.path().join("fd")).into_iter().flatten().flatten().any(|fd| std::fs::read_link(fd.path()).is_ok_and(|t| t == real)))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

struct Shared {
    device: Mutex<String>,
    state: Mutex<String>,
    reopen: AtomicBool,
    stop: AtomicBool,
}

fn feeder(s: Arc<Shared>) {
    let mut port: Option<Serial> = None;
    let mut buf = vec![];
    while !s.stop.load(Ordering::Relaxed) {
        let dev = s.device.lock().unwrap().clone();
        if s.reopen.swap(false, Ordering::Relaxed) {
            port = None;
        }
        let set = |m: String| *s.state.lock().unwrap() = m;
        if !std::path::Path::new(&dev).exists() {
            port = None;
            set(format!("waiting for {dev}"));
            std::thread::sleep(Duration::from_millis(400));
            continue;
        }
        let holders = other_openers(&dev);
        if !holders.is_empty() {
            port = None;
            set(format!("YIELDED: port used by PID {}", holders.iter().take(3).cloned().collect::<Vec<_>>().join(",")));
            std::thread::sleep(Duration::from_millis(400));
            continue;
        }
        if port.is_none() {
            match Serial::open(&dev, libc::B115200, Duration::from_millis(200)) {
                Ok(p) => {
                    port = Some(p);
                    set(format!("reading {dev}"));
                }
                Err(e) => {
                    set(format!("open failed: {e}"));
                    std::thread::sleep(Duration::from_millis(400));
                    continue;
                }
            }
        }
        match port.as_mut().unwrap().read_some() {
            Ok(d) => {
                buf.extend(d);
                while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=i).collect();
                    let text: String = line.iter().map(|b| *b as char).collect::<String>().trim_end_matches(['\n', '\r']).to_string();
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(live::path()) {
                        let _ = writeln!(f, "{} | {text}", live::stamp());
                    }
                }
            }
            Err(_) => {
                port = None;
                set("read error, retrying".into());
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
}

fn show(line: &str) {
    let (ts, body) = line.split_once(" | ").map_or(("", line), |(t, b)| (t, b));
    let lower = body.to_lowercase();
    let hl = HIGHLIGHTS.iter().find(|(m, _, _)| lower.contains(&m.to_lowercase()));
    let mut out = std::io::stdout().lock();
    if !ts.is_empty() {
        let _ = write!(out, "\x1b[90m{ts} | \x1b[0m");
    }
    let _ = match hl {
        Some((_, c, bold)) => writeln!(out, "\x1b[{}{c}m{body}\x1b[0m", if *bold { "1;" } else { "" }),
        None => writeln!(out, "{body}"),
    };
}

/// Raw keys from stdin (no echo, no line buffering) while it lives.
struct RawStdin(Option<libc::termios>);

impl RawStdin {
    fn new() -> Self {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
            return RawStdin(None);
        }
        let old = t;
        t.c_lflag &= !(libc::ICANON | libc::ECHO);
        t.c_cc[libc::VMIN] = 0;
        t.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        RawStdin(Some(old))
    }
}

impl Drop for RawStdin {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        }
    }
}

pub fn main(device: Option<String>) -> Result<i32> {
    let dev = device.or_else(ft232_tty).unwrap_or_else(|| "/dev/ttyUSB0".into());
    let log = live::path();
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let s = Arc::new(Shared { device: Mutex::new(dev.clone()), state: Mutex::new("starting".into()), reopen: AtomicBool::new(false), stop: AtomicBool::new(false) });
    let f = s.clone();
    std::thread::spawn(move || feeder(f));
    let _raw = RawStdin::new();
    println!("\x1b[33m=== UART {dev} @ 115200 -> {} (m mark, r reopen, c clear, q quit) ===\x1b[0m", log.display());
    let mut tail = std::io::BufReader::new(std::fs::File::open(&log)?);
    tail.seek(std::io::SeekFrom::End(0))?;
    let mut last_status = String::new();
    let mut tick = 0u32;
    let mut partial = String::new();
    loop {
        let mut chunk = String::new();
        while tail.read_line(&mut chunk)? > 0 {}
        partial.push_str(&chunk);
        while let Some(i) = partial.find('\n') {
            let line: String = partial.drain(..=i).collect();
            show(line.trim_end_matches('\n'));
        }
        if tick.is_multiple_of(8) {
            let (phone, ft) = usb_state();
            let status = format!("USB: {} | UART {} | probe: {}", phone.as_deref().unwrap_or("phone not present (off / enumerating)"), if ft { "✓" } else { "✗" }, s.state.lock().unwrap());
            if status != last_status {
                // the status in the terminal title, and as a line when it changes
                print!("\x1b]0;nio probe: {status}\x07");
                println!("\x1b[33m[monitor] {status}\x1b[0m");
                last_status = status;
            }
        }
        tick += 1;
        let mut key = [0u8; 1];
        if std::io::stdin().read(&mut key).unwrap_or(0) == 1 {
            match key[0] {
                b'q' => break,
                b'm' => live::line("mark", &format!("========== MARK {} ==========", live::stamp())),
                b'r' => {
                    s.reopen.store(true, Ordering::Relaxed);
                    println!("\x1b[33m[monitor] reopen {}\x1b[0m", s.device.lock().unwrap());
                }
                b'c' => print!("\x1b[2J\x1b[H"),
                _ => {}
            }
        }
        std::io::stdout().flush()?;
        std::thread::sleep(Duration::from_millis(60));
    }
    s.stop.store(true, Ordering::Relaxed);
    Ok(0)
}
