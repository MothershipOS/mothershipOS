//! `sh '<command>' [timeout]`: run a shell command on the phone over the dev link's telnet shell (busybox telnetd
//! on 10.42.0.2:2323: the RAM host's, or mship-telnet.service). The output and exit status are the command's.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::live;

pub const HOST: &str = "10.42.0.2:2323";

/// Telnet bytes without option negotiation, refusing every option.
fn strip_telnet(s: &mut TcpStream, d: &[u8], out: &mut Vec<u8>) -> std::io::Result<()> {
    let mut i = 0;
    while i < d.len() {
        if d[i] == 255 && i + 1 < d.len() {
            let c = d[i + 1];
            if (251..=254).contains(&c) && i + 2 < d.len() {
                s.write_all(&[255, if c == 251 || c == 252 { 254 } else { 252 }, d[i + 2]])?;
                i += 3;
                continue;
            }
            i += 2;
            continue;
        }
        out.push(d[i]);
        i += 1;
    }
    Ok(())
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// (output, exit status; None: timed out)
pub fn run(cmd: &str, timeout: Duration) -> Result<(String, Option<i32>)> {
    let mut s = TcpStream::connect_timeout(&HOST.parse()?, Duration::from_secs(5))?;
    s.set_read_timeout(Some(Duration::from_millis(200)))?;
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() % 100_000_000;
    let mark = format!("ZQ{nonce}QZ");
    let mut buf = vec![];
    let mut raw = [0u8; 65536];
    let mut pump = |s: &mut TcpStream, buf: &mut Vec<u8>| -> std::io::Result<bool> {
        match s.read(&mut raw) {
            Ok(0) => Ok(false),
            Ok(n) => {
                let chunk = raw[..n].to_vec();
                strip_telnet(s, &chunk, buf)?;
                Ok(true)
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(true),
            Err(e) => Err(e),
        }
    };
    // the banner and prompt
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(600) {
        pump(&mut s, &mut buf)?;
    }
    buf.clear();
    // the marker is split in the command so its echo never matches
    s.write_all(format!("stty -echo 2>/dev/null; {cmd}\necho {}''{} $?\n", &mark[..2], &mark[2..]).as_bytes())?;
    live::line("nsh", &format!("$ {}", cmd.replace('\n', " ; ").chars().take(300).collect::<String>()));
    let t = Instant::now();
    let mut rc = None;
    let mut shown = 0;
    while t.elapsed() < timeout {
        if !pump(&mut s, &mut buf)? {
            break;
        }
        if let Some(nl) = buf.iter().rposition(|b| *b == b'\n')
            && nl >= shown
        {
            for l in String::from_utf8_lossy(&buf[shown..nl]).replace('\r', "").split('\n') {
                let l = strip_ansi(l);
                if !l.trim().is_empty() && !l.contains("ZQ") && !l.contains("stty -echo") {
                    live::line("nsh", &l.chars().take(400).collect::<String>());
                }
            }
            shown = nl + 1;
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some(i) = text.find(&format!("{mark} ")) {
            let rest = &text[i + mark.len() + 1..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() && rest.len() > digits.len() {
                rc = digits.parse().ok();
                buf.truncate(i);
                break;
            }
        }
    }
    live::line("nsh", &rc.map_or(format!("TIMEOUT after {}s", timeout.as_secs()), |r| format!("exit={r}")));
    let text = strip_ansi(&String::from_utf8_lossy(&buf).replace('\r', ""));
    // the echoed first line (the command with stty) goes
    let text = text.split_once("stty -echo").map_or(text.clone(), |(_, r)| r.split_once('\n').map_or(String::new(), |(_, r)| r.to_string()));
    let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with("echo ZQ") && !matches!(l.trim(), "~ #" | "/ #")).collect();
    Ok((lines.join("\n").trim().to_string(), rc))
}

pub fn main(cmd: &str, timeout: f64) -> Result<i32> {
    let (out, rc) = run(cmd, Duration::from_secs_f64(timeout))?;
    println!("{out}");
    Ok(rc.unwrap_or(124))
}
