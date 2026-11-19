//! fpc-test: bring-up tool for the nio fingerprint sensor (motorola_nio/docs/fingerprint.md).
//!
//! Drives `/dev/fpc_tee` directly so the load path can be tested before any
//! command protocol exists. The decisive step-1 check is `fpc-test load`:
//! a non-negative app id means the (vendor-signed) TA loaded under the EL2 host.
//!
//!   fpc-test power <0|1>
//!   fpc-test reset <0|1>          # 1 = asserted (in reset)
//!   fpc-test load                 # loads cmnlib + TA, prints the app id
//!   fpc-test unload
//!   fpc-test wait <ms>            # wait for a finger-down interrupt
//!   fpc-test probe               # power on, pulse reset, load, print app id

use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use mship_devices::fpc::Fpc;

fn usage() -> ExitCode {
    eprintln!("usage: fpc-test power <0|1> | reset <0|1> | load | unload | wait <ms> | probe");
    ExitCode::FAILURE
}

fn run() -> std::io::Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let arg = |i: usize| args.get(i).and_then(|s| s.parse::<i32>().ok());

    let fpc = Fpc::open()?;
    match cmd {
        "power" => fpc.power(arg(1).ok_or_else(bad)? != 0)?,
        "reset" => fpc.reset(arg(1).ok_or_else(bad)? != 0)?,
        "unload" => fpc.unload()?,
        "load" => println!("app id {}", fpc.load()?),
        "wait" => {
            let ms = arg(1).unwrap_or(10_000);
            println!("{}", if fpc.wait_irq(ms)? { "interrupt" } else { "timeout" });
        }
        "probe" => {
            fpc.power(true)?;
            fpc.reset(true)?;
            sleep(Duration::from_millis(5));
            fpc.reset(false)?;
            sleep(Duration::from_millis(5));
            println!("app id {}", fpc.load()?);
        }
        _ => return Ok(usage()),
    }
    Ok(ExitCode::SUCCESS)
}

fn bad() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing or bad argument")
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fpc-test: {e}");
            ExitCode::FAILURE
        }
    }
}
