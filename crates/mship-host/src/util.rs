//! Files, JSON settings and external commands.

use std::path::Path;
use std::process::{Command, Stdio};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// A failure with a message for the user (the UI shows it as is).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

pub fn fail<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error(msg.into()))
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

/// A file's content, trimmed; `default` if it cannot be read.
pub fn rd(path: impl AsRef<Path>, default: &str) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_else(|_| default.to_string())
}

pub fn rd_u64(path: impl AsRef<Path>) -> u64 {
    rd(path, "0").parse().unwrap_or(0)
}

pub fn wr(path: impl AsRef<Path>, value: impl ToString) -> std::io::Result<()> {
    std::fs::write(path, value.to_string())
}

/// A settings file (`/etc/mothership/*.json`): its values over the defaults (a missing or broken file = defaults).
pub fn load_json<T: DeserializeOwned + Default>(path: impl AsRef<Path>) -> T {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn load_value(path: impl AsRef<Path>) -> Option<serde_json::Value> {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok())
}

/// Write a settings file atomically, readable by everyone (the shell reads some of them).
pub fn save_json(path: impl AsRef<Path>, value: &impl Serialize) -> Result<()> {
    let path = path.as_ref();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value).map_err(|e| Error(e.to_string()))?)?;
    std::fs::rename(&tmp, path)?;
    set_mode(path, 0o644)
}

pub fn set_mode(path: impl AsRef<Path>, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// The outcome of an external command.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// stderr (or stdout), for an error message
    pub fn message(&self) -> String {
        let s = if self.stderr.trim().is_empty() { &self.stdout } else { &self.stderr };
        s.trim().to_string()
    }
}

/// Run a command to completion (a missing program is exit code 127).
pub fn run(cmd: &str, args: &[&str]) -> Output {
    run_input(cmd, args, None)
}

pub fn run_input(cmd: &str, args: &[&str], input: Option<&str>) -> Output {
    let mut c = Command::new(cmd);
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = match c.spawn() {
        Ok(child) => child,
        Err(e) => return Output { code: 127, stdout: String::new(), stderr: format!("{cmd}: {e}") },
    };
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        use std::io::Write;
        let _ = stdin.write_all(text.as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) => Output {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => Output { code: -1, stdout: String::new(), stderr: e.to_string() },
    }
}

/// Run a command, failing with its message if it does not succeed.
pub fn run_check(cmd: &str, args: &[&str]) -> Result<Output> {
    let o = run(cmd, args);
    if o.ok() { Ok(o) } else { fail(if o.message().is_empty() { format!("{cmd} failed") } else { o.message() }) }
}

/// "on"/"yes"/"true"/"1" (how settings are switched on the command line)
pub fn truthy(v: &str) -> bool {
    matches!(v, "1" | "on" | "yes" | "true")
}

/// Is a process alive?
pub fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

pub fn sleep_s(s: f64) {
    std::thread::sleep(std::time::Duration::from_secs_f64(s));
}
