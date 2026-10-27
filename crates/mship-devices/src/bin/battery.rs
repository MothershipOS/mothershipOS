//! mship-battery: the mothershipOS battery state (root), the mship.battery 1.x contract
//! (docs/contracts/mship.battery-1.md).
//!
//! battery:0 is a shared, read-only handle: every running VM sees the phone's battery and charger, and nothing a VM
//! does reaches them (charge limits and charging stay host settings). The state comes from the kernel's
//! power_supply class and is published as one line per update on /run/mothership/battery.sock, to any number of
//! readers; each VM's crosvm reads it into the guest's goldfish battery.
//!
//! `mship-battery` runs the service, `mship-battery --once` prints the current line.
//!
//! Line: `ac=1 present=1 status=charging percent=9 voltage=3932362 current=1025390 charge_counter=450000
//! charge_full=5000000 temp=375 health=good` (µV, µA with + = into the battery, µAh, 0.1 °C). Readers ignore keys
//! they don't know. Changes in ac/present/status/percent/health and temperature steps >= 1 °C go out at once (on
//! the power_supply uevent). Voltage, current and charge go out with them, or at most every REFRESH s, so an idle
//! guest isn't woken more often than Android's own battery polling.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use mship_devices::{for_session, fresh, log, poll, read_uevent, uevents};

const PSY: &str = "/sys/class/power_supply";
const SOCK: &str = "/run/mothership/battery.sock";
/// voltage/current/charge-only changes
const REFRESH: Duration = Duration::from_secs(60);
/// re-read even without uevents (not every gauge driver sends them)
const POLL: Duration = Duration::from_secs(10);
const KEYS: &[&str] = &["ac", "present", "status", "percent", "voltage", "current", "charge_counter", "charge_full", "temp", "health"];
/// a change here is sent at once
const PROMPT: &[&str] = &["ac", "present", "status", "percent", "health"];

type Props = BTreeMap<String, String>;
type State = BTreeMap<&'static str, String>;

fn props(name: &str) -> Props {
    std::fs::read_to_string(Path::new(PSY).join(name).join("uevent"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim_start_matches("POWER_SUPPLY_").to_lowercase(), v.to_string())))
        .collect()
}

/// sysfs enum text -> wire token: "Not charging" -> not-charging
fn word(v: Option<&String>) -> String {
    v.filter(|v| !v.is_empty()).map_or("unknown".into(), |v| v.trim().to_lowercase().replace(' ', "-"))
}

/// The state from every supply's properties (pure).
fn state_from(supplies: &BTreeMap<String, Props>) -> State {
    let bat = supplies.values().find(|p| p.get("type").is_some_and(|t| t == "Battery") && p.get("scope").is_none_or(|s| s != "Device"));
    let ac = supplies
        .values()
        .any(|p| p.get("type").is_some_and(|t| t != "Battery") && p.get("online").is_some_and(|o| o == "1"));
    let mut st = State::from([("ac", (ac as u8).to_string()), ("present", "0".into())]);
    if let Some(b) = bat.filter(|b| b.get("present").is_none_or(|p| p == "1")) {
        let num = |k: &str| b.get(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        let or = |a: i64, b: i64| if a != 0 { a } else { b };
        st.insert("present", "1".into());
        st.insert("status", word(b.get("status")));
        st.insert("percent", num("capacity").to_string());
        st.insert("voltage", num("voltage_now").to_string());
        st.insert("current", num("current_now").to_string());
        st.insert("charge_counter", or(num("charge_now"), num("charge_counter")).to_string());
        st.insert("charge_full", or(num("charge_full"), num("charge_full_design")).to_string());
        st.insert("temp", num("temp").to_string());
        st.insert("health", word(Some(&b.get("health").cloned().unwrap_or_else(|| "Good".into()))));
    }
    st
}

fn read_state() -> State {
    let mut names: Vec<String> = std::fs::read_dir(PSY).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    state_from(&names.into_iter().map(|n| { let p = props(&n); (n, p) }).collect())
}

fn line(st: &State) -> String {
    let mut out = KEYS.iter().filter_map(|k| st.get(k).map(|v| format!("{k}={v}"))).collect::<Vec<_>>().join(" ");
    out.push('\n');
    out
}

/// Should this state go out now (a prompt change, or REFRESH since the last line)?
fn due(old: Option<&State>, st: &State, since: Duration) -> bool {
    let temp = |s: &State| s.get("temp").and_then(|t| t.parse::<i64>().ok()).unwrap_or(0);
    match old {
        None => true,
        Some(o) => PROMPT.iter().any(|k| o.get(k) != st.get(k)) || (temp(o) - temp(st)).abs() >= 10 || since >= REFRESH,
    }
}

struct Service {
    clients: Vec<UnixStream>,
    sent: Option<(String, State, Instant)>,
}

impl Service {
    fn send_all(&mut self, text: &str) {
        // one short line; a reader that is that far behind is dropped
        self.clients.retain_mut(|c| c.write_all(text.as_bytes()).is_ok());
    }

    fn update(&mut self) {
        let st = read_state();
        if !due(self.sent.as_ref().map(|s| &s.1), &st, self.sent.as_ref().map_or(REFRESH, |s| s.2.elapsed())) {
            return;
        }
        let text = line(&st);
        if self.sent.as_ref().is_some_and(|s| s.0 == text) {
            return;
        }
        self.send_all(&text);
        self.sent = Some((text, st, Instant::now()));
    }
}

fn serve() -> std::io::Result<()> {
    fresh(Path::new(SOCK));
    let l = UnixListener::bind(SOCK)?;
    for_session(Path::new(SOCK), 0o660);
    l.set_nonblocking(true)?;
    let ue = uevents().map_err(|e| log(format!("no uevents ({e}), polling only"))).ok();
    let mut s = Service { clients: vec![], sent: None };
    s.update();
    log(format!("serving {SOCK}: {}", s.sent.as_ref().map_or("", |x| x.0.trim())));
    loop {
        let mut fds = vec![l.as_raw_fd()];
        fds.extend(ue.as_ref().map(|u| u.as_raw_fd()));
        fds.extend(s.clients.iter().map(|c| c.as_raw_fd()));
        let ready = poll(&fds, Some(POLL));
        if ready.contains(&l.as_raw_fd()) {
            while let Ok((c, _)) = l.accept() {
                let _ = c.set_nonblocking(true);
                let mut c = c;
                if let Some((text, ..)) = &s.sent
                    && c.write_all(text.as_bytes()).is_err()
                {
                    continue;
                }
                s.clients.push(c);
            }
        }
        // readers don't talk: readable means closed (or chatter, ignored)
        s.clients.retain(|c| {
            if !ready.contains(&c.as_raw_fd()) {
                return true;
            }
            let mut b = [0u8; 4096];
            match unsafe { libc::recv(c.as_raw_fd(), b.as_mut_ptr().cast(), b.len(), libc::MSG_DONTWAIT) } {
                0 => false,
                n if n > 0 => true,
                _ => std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock,
            }
        });
        if let Some(u) = ue.as_ref().filter(|u| ready.contains(&u.as_raw_fd())) {
            while read_uevent(u.as_raw_fd()).is_some() {} // drained: the state is re-read below anyway
        }
        s.update();
    }
}

fn main() -> std::process::ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--once") => {
            print!("{}", line(&read_state()));
            std::process::ExitCode::SUCCESS
        }
        Some(_) => {
            eprintln!("usage: mship-battery [--once]");
            std::process::ExitCode::from(2)
        }
        None => match serve() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("mship-battery: {e}");
                std::process::ExitCode::FAILURE
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(kv: &[(&str, &str)]) -> Props {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn phone() {
        let s = BTreeMap::from([
            ("battery".to_string(), p(&[("type", "Battery"), ("status", "Not charging"), ("capacity", "80"), ("voltage_now", "4000000"),
                                       ("current_now", "-5000"), ("charge_counter", "4000000"), ("charge_full_design", "5000000"),
                                       ("temp", "312")])),
            ("usb".to_string(), p(&[("type", "USB"), ("online", "1")])),
        ]);
        assert_eq!(line(&state_from(&s)), "ac=1 present=1 status=not-charging percent=80 voltage=4000000 current=-5000 \
                                           charge_counter=4000000 charge_full=5000000 temp=312 health=good\n");
        assert_eq!(line(&state_from(&BTreeMap::new())), "ac=0 present=0\n");
    }

    #[test]
    fn pacing() {
        let a = State::from([("percent", "80".to_string()), ("temp", "300".into()), ("voltage", "1".into())]);
        let mut b = a.clone();
        b.insert("voltage", "2".into());
        assert!(!due(Some(&a), &b, Duration::from_secs(5))); // voltage alone waits
        assert!(due(Some(&a), &b, REFRESH));
        b.insert("temp", "310".into());
        assert!(due(Some(&a), &b, Duration::ZERO)); // 1 °C
        b.insert("temp", "300".into());
        b.insert("percent", "79".into());
        assert!(due(Some(&a), &b, Duration::ZERO));
    }
}
