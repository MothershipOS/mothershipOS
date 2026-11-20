//! The VM's console slots (mship.bt, mship.gnss, mship.sensors, ...): hot-pluggable for the VM's whole life.
//!
//! crosvm connects a slot's virtio-console to `<vm dir>/slot-<name>.sock` once, at start, and never reconnects. The
//! launcher holds that socket and relays it to the device service's socket for the VM, connecting whenever the
//! service is there: a service that starts after the VM, or restarts while it runs, reaches the guest without the
//! VM restarting. What the guest sends while no service is there is kept (up to [`HELD`]) and delivered when one
//! connects; what the service sends goes straight to the guest.

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{Res, Vm, err};

/// guest bytes kept while no service is connected
const HELD: usize = 64 * 1024;
/// how often a missing service is tried
const RETRY: Duration = Duration::from_secs(1);

fn poll(fds: &[RawFd], timeout: Duration) -> Vec<RawFd> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    let n = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout.as_millis() as i32) };
    if n <= 0 {
        return vec![];
    }
    p.iter().filter(|x| x.revents != 0).map(|x| x.fd).collect()
}

/// Write everything (the peers are blocking sockets with a short timeout). false: the peer is gone.
fn send(s: &mut UnixStream, b: &[u8]) -> bool {
    s.write_all(b).is_ok()
}

/// The slot's socket for crosvm (`type=unix-stream,path=<returned>`), relayed to `service` (the device service's
/// socket for this VM) from now until the VM's console closes.
pub fn slot(vm: &Vm, name: &str, service: &str) -> Res<String> {
    let path: PathBuf = vm.dir.join(format!("slot-{name}.sock"));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (name, service) = (name.to_string(), service.to_string());
    std::thread::Builder::new()
        .name(format!("slot-{name}"))
        .spawn(move || {
            let Ok((guest, _)) = listener.accept() else { return };
            drop(listener);
            relay(&name, guest, Path::new(&service));
        })
        .map_err(err)?;
    Ok(path.to_string_lossy().into_owned())
}

fn connect(service: &Path) -> Option<UnixStream> {
    let s = UnixStream::connect(service).ok()?;
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
    Some(s)
}

fn relay(name: &str, mut guest: UnixStream, service: &Path) {
    let _ = guest.set_write_timeout(Some(Duration::from_secs(2)));
    let mut svc: Option<UnixStream> = None;
    let mut held: Vec<u8> = vec![];
    let mut last_try = Instant::now() - RETRY;
    let mut buf = vec![0u8; 65536];
    loop {
        if svc.is_none() && last_try.elapsed() >= RETRY {
            last_try = Instant::now();
            if let Some(mut s) = connect(service) {
                println!("slot {name}: connected to {}", service.display());
                if !held.is_empty() && !send(&mut s, &held) {
                    continue;
                }
                held.clear();
                svc = Some(s);
            }
        }
        let mut fds = vec![guest.as_raw_fd()];
        fds.extend(svc.as_ref().map(|s| s.as_raw_fd()));
        let ready = poll(&fds, if svc.is_some() { Duration::from_secs(10) } else { RETRY });
        if ready.contains(&guest.as_raw_fd()) {
            match guest.read(&mut buf) {
                Ok(0) => return, // the VM is gone
                Ok(n) => {
                    let sent = svc.as_mut().is_some_and(|s| send(s, &buf[..n]));
                    if !sent {
                        if svc.take().is_some() {
                            println!("slot {name}: service gone");
                        }
                        held.extend_from_slice(&buf[..n]);
                        if held.len() > HELD {
                            let cut = held.len() - HELD;
                            held.drain(..cut);
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted || e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }
        if let Some(s) = svc.as_mut()
            && ready.contains(&s.as_raw_fd())
        {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => {
                    println!("slot {name}: service gone");
                    svc = None;
                }
                Ok(n) => {
                    if !send(&mut guest, &buf[..n]) {
                        return;
                    }
                }
            }
        }
    }
}
