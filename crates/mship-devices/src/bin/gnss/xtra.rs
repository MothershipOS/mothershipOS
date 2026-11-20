//! Predicted orbits (Qualcomm XTRA): the file the engine asks for, downloaded in the background and kept, so a
//! cold start takes seconds instead of minutes. Downloaded from the servers the engine names (or Qualcomm's
//! defaults), never more often than [`RETRY`].

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant, SystemTime};

/// a file this old is replaced (the data covers 7 days)
pub const REFRESH: Duration = Duration::from_secs(24 * 3600);
/// older than this, the file is useless
pub const EXPIRED: Duration = Duration::from_secs(7 * 24 * 3600);
/// between download attempts
const RETRY: Duration = Duration::from_secs(15 * 60);
const DEFAULT_SERVERS: &[&str] =
    &["https://xtrapath1.izatcloud.net/xtra3grc.bin", "https://xtrapath2.izatcloud.net/xtra3grc.bin", "https://xtrapath3.izatcloud.net/xtra3grc.bin"];

pub struct Xtra {
    file: PathBuf,
    job: Option<Receiver<Result<Vec<u8>, String>>>,
    last_try: Option<Instant>,
}

impl Xtra {
    pub fn new(file: PathBuf) -> Self {
        Self { file, job: None, last_try: None }
    }

    /// The kept file and its age.
    pub fn cached(&self) -> Option<(Vec<u8>, Duration)> {
        let age = std::fs::metadata(&self.file).and_then(|m| m.modified()).ok()?.elapsed().unwrap_or_default();
        Some((std::fs::read(&self.file).ok().filter(|d| !d.is_empty())?, age))
    }

    pub fn age(&self) -> Option<Duration> {
        self.cached().map(|c| c.1)
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Start a download unless one runs or the last one was recent. `servers` empty: the defaults.
    pub fn fetch(&mut self, servers: Vec<String>, max_size: u32) {
        if self.job.is_some() || self.last_try.is_some_and(|t| t.elapsed() < RETRY) {
            return;
        }
        self.last_try = Some(Instant::now());
        let urls: Vec<String> = if servers.is_empty() { DEFAULT_SERVERS.iter().map(|s| s.to_string()).collect() } else { servers };
        let file = self.file.clone();
        let (tx, rx) = channel();
        self.job = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(download(&urls, &file, max_size));
        });
    }

    /// A finished download, if one finished since the last call.
    pub fn done(&mut self) -> Option<Result<Vec<u8>, String>> {
        match self.job.as_ref()?.try_recv() {
            Ok(r) => {
                self.job = None;
                Some(r)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.job = None;
                Some(Err("download thread ended".into()))
            }
        }
    }
}

fn download(urls: &[String], file: &std::path::Path, max_size: u32) -> Result<Vec<u8>, String> {
    let tmp = file.with_extension("part");
    if let Some(d) = file.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let mut errors = vec![];
    for url in urls.iter().filter(|u| u.starts_with("http")) {
        let o = Command::new("curl").args(["-fsSL", "--max-time", "60", "-o"]).arg(&tmp).arg(url).output();
        match o {
            Ok(o) if o.status.success() => {
                let data = std::fs::read(&tmp).map_err(|e| e.to_string())?;
                if data.is_empty() || (max_size > 0 && data.len() > max_size as usize) {
                    errors.push(format!("{url}: {} bytes", data.len()));
                    continue;
                }
                std::fs::rename(&tmp, file).map_err(|e| e.to_string())?;
                let _ = filetime_now(file);
                return Ok(data);
            }
            Ok(o) => errors.push(format!("{url}: {}", String::from_utf8_lossy(&o.stderr).trim())),
            Err(e) => errors.push(format!("curl: {e}")),
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(errors.join("; "))
}

/// the file's age counts from the download, not from the server's timestamp (curl keeps none without -R anyway)
fn filetime_now(file: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::options().append(true).open(file)?.set_modified(SystemTime::now())
}
