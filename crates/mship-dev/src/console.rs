//! `console wait | run <cmd>... | collect <dir> | reboot [mode]`: the diagnostic initramfs shell over the USB CDC-ACM
//! gadget (1d6b:0104). The first command marks /tmp/host-attached, which cancels the diagnostic image's automatic
//! return to fastboot. Nothing here writes the phone's storage.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::serial::Serial;

const GADGET: (&str, &str) = ("1d6b", "0104");

fn find_port() -> Option<String> {
    for e in std::fs::read_dir("/sys/class/tty").ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("ttyACM") {
            continue;
        }
        let dev = std::fs::canonicalize(e.path()).ok()?;
        for p in dev.ancestors() {
            let (v, d) = (std::fs::read_to_string(p.join("idVendor")), std::fs::read_to_string(p.join("idProduct")));
            if let (Ok(v), Ok(d)) = (v, d) {
                if (v.trim(), d.trim()) == GADGET {
                    return Some(format!("/dev/{name}"));
                }
                break;
            }
        }
    }
    None
}

pub fn wait_port(timeout: Duration) -> Result<String> {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if let Some(p) = find_port() {
            return Ok(p);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!("nio diagnostic gadget {}:{} not found within {}s", GADGET.0, GADGET.1, timeout.as_secs())
}

pub struct Shell(Serial);

impl Shell {
    pub fn open(port: &str) -> Result<Self> {
        let mut s = Shell(Serial::open(port, libc::B115200, Duration::from_millis(200))?);
        s.run("stty -echo 2>/dev/null; touch /tmp/host-attached", Duration::from_secs(10))?;
        Ok(s)
    }

    /// (exit status, output)
    pub fn run(&mut self, command: &str, timeout: Duration) -> Result<(i32, String)> {
        let nonce = format!("{:08x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.subsec_nanos());
        // printf assembles the marker, so the echoed command never matches it
        let line = format!("{command}\nprintf '\\n__%s_{nonce}__ %s\\n' END $?\n");
        self.0.flush_input();
        self.0.write_all(line.as_bytes())?;
        let marker = format!("__END_{nonce}__ ");
        let mut buf = vec![];
        let t = Instant::now();
        while t.elapsed() < timeout {
            buf.extend(self.0.read_some()?);
            let text = String::from_utf8_lossy(&buf).into_owned();
            if let Some(i) = text.find(&marker)
                && let Some(nl) = text[i..].find('\n')
            {
                let status = text[i + marker.len()..i + nl].trim().parse().unwrap_or(0);
                let body = text[..i].replace('\r', "");
                // busybox ash echoes the input: drop the echoed command and marker lines
                let sent: Vec<&str> = command.lines().collect();
                let kept: Vec<&str> = body
                    .split('\n')
                    .filter(|l| !sent.contains(&l.strip_prefix("~ # ").unwrap_or(l)) && !l.contains("printf '\\n__%s_"))
                    .collect();
                let body = kept.join("\n").trim_matches('\n').to_string();
                return Ok((status, if body.trim().is_empty() { String::new() } else { body + "\n" }));
            }
        }
        bail!("no response to {command:?}; partial output:\n{}", String::from_utf8_lossy(&buf))
    }
}

const PROBES: &[(&str, &str)] = &[
    ("uname.txt", "uname -a"),
    ("cmdline.txt", "cat /proc/cmdline"),
    ("dmesg.txt", "dmesg"),
    ("kvm.txt", "/init kvm; ls -l /dev/kvm"),
    ("pstore.txt", "ls -la /sys/fs/pstore; for f in /sys/fs/pstore/*; do [ -f \"$f\" ] && { echo \"=== $f\"; cat \"$f\"; }; done"),
    ("cpuinfo.txt", "cat /proc/cpuinfo"),
    ("meminfo.txt", "cat /proc/meminfo"),
    ("iomem.txt", "cat /proc/iomem"),
    ("interrupts.txt", "cat /proc/interrupts"),
    ("deferred.txt", "cat /sys/kernel/debug/devices_deferred 2>/dev/null || { mount -t debugfs debugfs /sys/kernel/debug && cat /sys/kernel/debug/devices_deferred; }"),
    ("devices.txt", "ls /sys/bus/platform/devices; echo; for d in /sys/bus/platform/drivers/*/; do echo \"$d: $(ls $d | grep -v -E \"^(bind|unbind|uevent|module|new_id|remove_id)$\" | tr \"\\n\" \" \")\"; done"),
    ("regulators.txt", "for r in /sys/class/regulator/*; do echo \"$(cat $r/name 2>/dev/null) $(cat $r/microvolts 2>/dev/null) $(cat $r/state 2>/dev/null)\"; done"),
    ("clk_summary.txt", "cat /sys/kernel/debug/clk/clk_summary"),
];

pub fn main(action: &[String], timeout: f64) -> Result<i32> {
    let port = wait_port(Duration::from_secs_f64(timeout))?;
    match action.first().map(String::as_str) {
        Some("wait") => {
            println!("{port}");
            Ok(0)
        }
        Some("run") => {
            let mut sh = Shell::open(&port)?;
            let mut worst = 0;
            for c in &action[1..] {
                let (status, out) = sh.run(c, Duration::from_secs(60))?;
                print!("{out}");
                if worst == 0 {
                    worst = status;
                }
            }
            Ok(worst)
        }
        Some("collect") => {
            let dir = Path::new(action.get(1).map(String::as_str).unwrap_or("."));
            std::fs::create_dir_all(dir)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            let mut sh = Shell::open(&port)?;
            for (name, cmd) in PROBES {
                let (status, out) = sh.run(cmd, Duration::from_secs(120))?;
                std::fs::write(dir.join(name), &out)?;
                println!("{name}: {} bytes, exit {status}", out.len());
            }
            Ok(0)
        }
        Some("reboot") => {
            let mut sh = Shell::open(&port)?;
            let cmd = format!("/init reboot{}", action.get(1).map(|m| format!(" {m}")).unwrap_or_default());
            sh.0.write_all(format!("{cmd}\n").as_bytes())?;
            println!("reboot requested: {cmd}");
            Ok(0)
        }
        _ => bail!("console wait | run <cmd>... | collect <dir> | reboot [mode]"),
    }
}
