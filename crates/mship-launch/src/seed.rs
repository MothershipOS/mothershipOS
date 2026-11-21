//! cloud-init's NoCloud seed for a new VM's first boot: its seed directory over plain HTTP (GET only), on the VM's
//! own tap, while the VM runs (cloud-init also fetches the guest packages from it). Once the guest has its user-data
//! the seed is used up (seed/used): later starts offer none. `@SEED_URL@` in the seed's text files is the server's own
//! address (the VM's network is its manifest's: the host's address on it is known only when the VM starts).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::Res;

fn handle(dir: &std::path::Path, url: &str, s: std::net::TcpStream) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut r = BufReader::new(s.try_clone()?);
    let mut req = String::new();
    r.read_line(&mut req)?;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
    }
    let mut w = s;
    let path = req.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap_or("/").trim_start_matches('/');
    let file = (!path.split('/').any(|c| c == ".." || c.starts_with('.')) && req.starts_with("GET "))
        .then(|| dir.join(path))
        .filter(|f| f.is_file())
        .and_then(|f| std::fs::read(f).ok())
        .map(|b| match String::from_utf8(b) {
            Ok(t) => t.replace("@SEED_URL@", url).into_bytes(),
            Err(e) => e.into_bytes(),
        });
    println!("seed: {} {}", req.trim(), if file.is_some() { 200 } else { 404 });
    match file {
        Some(b) => {
            write!(w, "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n", b.len())?;
            w.write_all(&b)?;
            if path == "user-data" {
                let _ = std::fs::write(dir.join("used"), "");
            }
            Ok(())
        }
        None => w.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n"),
    }
}

pub fn serve(dir: PathBuf, addr: &str, stop: Arc<AtomicBool>) -> Res<()> {
    let url = format!("http://{addr}");
    let addr = addr.to_string();
    std::thread::spawn(move || {
        // the address comes with the shared internet's attach (after the VM has started)
        let l = loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            match TcpListener::bind(&addr) {
                Ok(l) => break l,
                Err(_) => std::thread::sleep(Duration::from_secs(1)),
            }
        };
        if l.set_nonblocking(true).is_err() {
            return;
        }
        while !stop.load(Ordering::Relaxed) {
            match l.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nonblocking(false);
                    let _ = handle(&dir, &url, s);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    });
    Ok(())
}
