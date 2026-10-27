//! cloud-init's NoCloud seed for a new VM's first boot: its seed directory over plain HTTP (GET only), on the VM's
//! own tap, while the VM runs (cloud-init also fetches the guest packages from it). Once the guest has its user-data
//! the seed is used up (seed/used): later starts offer none.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::{Res, err};

fn handle(dir: &std::path::Path, s: std::net::TcpStream) -> std::io::Result<()> {
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
        .and_then(|f| std::fs::read(f).ok());
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
    let l = TcpListener::bind(addr).map_err(|e| format!("seed {addr}: {e}"))?;
    l.set_nonblocking(true).map_err(err)?;
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match l.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nonblocking(false);
                    let _ = handle(&dir, s);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    });
    Ok(())
}
