//! mship-dev: the mothershipOS developer tools for the nio, run on the PC.
//!
//! The dev link (USB NCM: the PC is 10.42.0.1, the phone 10.42.0.2): `sh`, `proxy`, `deploy-fit`.
//! The diagnostic initramfs (USB ACM): `console`. Recovery: `edl`. The debug UART: `uart`. Backups to the NAS:
//! `backup`. (One-off prototypes stay in archive/prototypes.)

mod backup;
mod console;
mod deploy;
mod edl;
mod live;
mod proxy;
mod serial;
mod shell;
mod uart;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// The folder holding mothershipOS and the port (motorola_nio) side by side: the ancestor of the working directory
/// or of this binary that has both
pub fn repo() -> PathBuf {
    let starts = [std::env::current_dir().ok(), std::env::current_exe().ok()];
    starts.into_iter().flatten()
        .find_map(|s| s.ancestors().find(|a| a.join("motorola_nio").is_dir() && a.join("mothershipOS").is_dir()).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.."))
}


#[derive(Parser)]
#[command(name = "mship-dev", about = "mothershipOS developer tools for the nio (on the PC)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// LUN START NUM [FILE]: parsed by hand (clap's num_args gives strings)
fn lun_range(v: &[String]) -> Result<(u32, u64, u64)> {
    Ok((v[0].parse().context("LUN")?, v[1].parse().context("START")?, v[2].parse().context("NUM")?))
}

#[derive(Subcommand)]
enum Cmd {
    /// A shell command on the phone over the dev link's telnet shell
    Sh { command: String, #[arg(default_value_t = 60.0)] timeout: f64 },
    /// An HTTP/HTTPS forward proxy for the phone on the PC end of the dev link
    Proxy { #[arg(default_value = "10.42.0.1")] bind: String, #[arg(default_value_t = 3128)] port: u16 },
    /// The diagnostic initramfs shell over USB ACM: wait | run <cmd>... | collect <dir> | reboot [mode]
    Console { action: Vec<String>, #[arg(long, default_value_t = 30.0)] timeout: f64 },
    /// Write a FIT to the running phone's boot_b, verify it and reboot
    DeployFit { fit: Option<PathBuf>, #[arg(long)] no_reboot: bool },
    /// Qualcomm EDL (Sahara + Firehose) over raw USB or a serial port
    Edl {
        #[arg(long)] loader: Option<String>,
        #[arg(long)] port: Option<String>,
        #[arg(long)] attach: bool,
        /// LUN START NUM IMG
        #[arg(long, num_args = 4, value_names = ["LUN", "START", "NUM", "IMG"])] program: Option<Vec<String>>,
        #[arg(long, num_args = 3, value_names = ["LUN", "START", "NUM"])] digest: Option<Vec<String>>,
        #[arg(long, num_args = 4, value_names = ["LUN", "START", "NUM", "OUT"])] read: Option<Vec<String>>,
        #[arg(long)] probe: bool,
        #[arg(long, value_name = "IMG")] restore_hyp_b: Option<String>,
        #[arg(long)] commit: bool,
        #[arg(long)] reset: bool,
    },
    /// The debug UART and the phone's USB mode in the terminal
    Uart { device: Option<String> },
    /// Raw partitions of the phone onto the NAS, verified end to end (--nas user@host, or MSHIP_NAS)
    Backup { #[command(subcommand)] cmd: BackupCmd, #[arg(long, global = true, env = "MSHIP_NAS")] nas: Option<String> },
}

#[derive(Subcommand)]
enum BackupCmd {
    /// One stream, nothing staged locally
    Partition { part: String, nas_dir: String, #[arg(long, default_value_t = 3)] level: i32 },
    /// Parallel chunks, staged on the PC, resumable
    Chunked {
        part: String,
        nas_dir: String,
        #[arg(long, default_value_t = 512)] chunk_mib: u64,
        #[arg(long, default_value_t = 3)] streams: usize,
        #[arg(long)] stage: Option<PathBuf>,
        #[arg(long, default_value_t = 50.0)] stage_gib: f64,
        #[arg(long, default_value_t = 3)] level: i32,
    },
    /// Every firmware/boot partition of a phone in Android (Magisk su over adb), read only
    Firmware { #[arg(long)] serial: String, #[arg(long)] out: Option<PathBuf>, #[arg(long)] include_huge: bool },
}

fn backup(cmd: BackupCmd, nas: Option<String>) -> Result<i32> {
    let n = backup::Nas(nas.ok_or_else(|| anyhow::anyhow!("where to: --nas user@host, or MSHIP_NAS"))?);
    match cmd {
        BackupCmd::Partition { part, nas_dir, level } => backup::partition_backup(&part, &nas_dir, &n, level),
        BackupCmd::Chunked { part, nas_dir, chunk_mib, streams, stage, stage_gib, level } => {
            let stage = stage.unwrap_or_else(|| std::env::temp_dir().join("nio-backup-stage"));
            backup::chunked(backup::Chunked { part, nas_dir, chunk_mib, streams, stage, stage_gib, level }, n)
        }
        BackupCmd::Firmware { serial, out, include_huge } => backup::firmware(&serial, out, include_huge),
    }
}

fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Sh { command, timeout } => shell::main(&command, timeout),
        Cmd::Proxy { bind, port } => proxy::main(&bind, port).map(|_| 0),
        Cmd::Console { action, timeout } => console::main(&action, timeout),
        Cmd::DeployFit { fit, no_reboot } => deploy::main(&fit.unwrap_or_else(|| repo().join("mothershipOS/target/boot/nio-el2.itb")), !no_reboot),
        Cmd::Edl { loader, port, attach, program, digest, read, probe, restore_hyp_b, commit, reset } => {
            let with_file = |v: Option<Vec<String>>| -> Result<Option<(u32, u64, u64, String)>> {
                v.map(|v| lun_range(&v).map(|(l, s, n)| (l, s, n, v[3].clone()))).transpose()
            };
            edl::main(edl::Args {
                loader, port, attach, program: with_file(program)?, digest: digest.map(|v| lun_range(&v)).transpose()?, read: with_file(read)?,
                probe, restore_hyp_b, commit, reset,
            })
        }
        Cmd::Uart { device } => uart::main(device),
        Cmd::Backup { cmd, nas } => backup(cmd, nas),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(c) => ExitCode::from(c.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("mship-dev: {e:#}");
            ExitCode::FAILURE
        }
    }
}
