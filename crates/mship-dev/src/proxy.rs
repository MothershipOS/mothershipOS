//! `proxy [bind] [port]`: an HTTP/HTTPS (CONNECT) forward proxy on the PC end of the dev link (default
//! 10.42.0.1:3128), so the phone reaches the internet over USB without root or NAT on the PC.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result};

fn pipe(a: TcpStream, b: TcpStream) {
    let (mut a2, mut b2) = (a.try_clone().unwrap(), b.try_clone().unwrap());
    let (mut a, mut b) = (a, b);
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut a2, &mut b2);
        let _ = b2.shutdown(std::net::Shutdown::Both);
    });
    let _ = std::io::copy(&mut b, &mut a);
    let _ = a.shutdown(std::net::Shutdown::Both);
    let _ = t.join();
}

fn handle(mut c: TcpStream) -> Result<()> {
    c.set_read_timeout(Some(Duration::from_secs(300)))?;
    let mut buf = vec![];
    let mut chunk = [0u8; 65536];
    let end = loop {
        let n = c.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let rest = buf[end + 4..].to_vec();
    let lines: Vec<&str> = head.split("\r\n").collect();
    let mut first = lines[0].splitn(3, ' ');
    let (method, target, ver) = (first.next().context("method")?, first.next().context("target")?, first.next().unwrap_or("HTTP/1.1"));
    let mut u;
    if method == "CONNECT" {
        u = TcpStream::connect(target)?;
        c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
        u.write_all(&rest)?;
    } else {
        // http://host[:port]/path
        let t = target.strip_prefix("http://").context("only http:// targets")?;
        let (hostport, path) = t.split_once('/').map_or((t, "/".to_string()), |(h, p)| (h, format!("/{p}")));
        let addr = if hostport.contains(':') { hostport.to_string() } else { format!("{hostport}:80") };
        u = TcpStream::connect(addr)?;
        let headers: Vec<&str> = lines[1..].iter().copied().filter(|l| !l.to_ascii_lowercase().starts_with("proxy-connection:")).collect();
        u.write_all(format!("{method} {path} {ver}\r\n{}\r\n\r\n", headers.join("\r\n")).as_bytes())?;
        u.write_all(&rest)?;
    }
    pipe(c, u);
    Ok(())
}

pub fn main(bind: &str, port: u16) -> Result<()> {
    let l = TcpListener::bind((bind, port)).with_context(|| format!("{bind}:{port}"))?;
    println!("proxy on {bind}:{port}");
    for c in l.incoming().flatten() {
        std::thread::spawn(move || {
            let _ = handle(c);
        });
    }
    Ok(())
}
