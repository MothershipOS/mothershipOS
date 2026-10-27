//! `edl`: Qualcomm EDL recovery for the nio (SM8250, 05c6:9008): Sahara loads a Firehose programmer, Firehose
//! then reads, hashes and writes UFS sectors (4 KiB). Over raw USB (usbdevfs: the qcserial tty layer races and
//! flushes the Sahara HELLO), or over a serial port (`--port /dev/ttyUSBn`).
//!
//! - `--program LUN START NUM IMG`: write an image (padded to NUM sectors), then its digest
//! - `--digest LUN START NUM`, `--read LUN START NUM OUT`
//! - `--probe`: read only: the digests of hyp_a and hyp_b against the stock hyp, the hyp_b entry of LUN5's GPT
//! - `--restore-hyp-b IMG [--commit]`: the stock hyp back into hyp_b (LUN5, 1184, 2048 sectors): only an image
//!   with the stock hyp's SHA-256; without --commit only says what it would write
//! - `--attach`: talk to an already running Firehose (no Sahara); `--reset`: power-reset at the end
//!
//! No bootloader partitions are touched except by an explicit --program.

use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::Digest;

use crate::serial::Serial;

const VID: &str = "05c6";
const PID: &str = "9008";
const SECTOR: usize = 4096;
/// the stock hyp image (hyp_a / hyp_b)
const STOCK_HYP: &str = "f69538bf3fded342bb1679c3d01f0b2ad4ba930b235c11fda210efcf0240d8b2";

const HELLO: u32 = 1;
const HELLO_RESP: u32 = 2;
const READ_DATA: u32 = 3;
const END_IMG: u32 = 4;
const DONE: u32 = 5;
const DONE_RESP: u32 = 6;
const RESET: u32 = 7;
const READ_DATA64: u32 = 0x12;

fn log(m: &str) {
    eprintln!("{m}");
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

// ---------------------------------------------------------------- transports

trait Link {
    /// up to `n` bytes within `timeout` (empty: nothing came)
    fn read(&mut self, n: usize, timeout: Duration) -> Result<Vec<u8>>;
    fn write(&mut self, data: &[u8]) -> Result<()>;
}

/// The EDL device's bulk endpoints through usbdevfs.
struct Usb {
    file: std::fs::File,
    ep_in: u32,
    ep_out: u32,
}

#[repr(C)]
struct BulkTransfer {
    ep: u32,
    len: u32,
    timeout: u32,
    data: *mut libc::c_void,
}

#[repr(C)]
struct DisconnectClaim {
    interface: u32,
    flags: u32,
    driver: [u8; 256],
}

const USBDEVFS_BULK: libc::c_ulong = 0xC018_5502;
const USBDEVFS_SETCONFIGURATION: libc::c_ulong = 0x8004_5505;
const USBDEVFS_DISCONNECT_CLAIM: libc::c_ulong = 0x8108_551b;

impl Usb {
    fn open() -> Result<Self> {
        let t = Instant::now();
        let dev = loop {
            let found = std::fs::read_dir("/sys/bus/usb/devices")?.flatten().map(|e| e.path()).find(|p| {
                std::fs::read_to_string(p.join("idVendor")).is_ok_and(|v| v.trim() == VID)
                    && std::fs::read_to_string(p.join("idProduct")).is_ok_and(|v| v.trim() == PID)
            });
            match found {
                Some(d) => break d,
                None if t.elapsed() > Duration::from_secs(30) => bail!("no {VID}:{PID} device"),
                None => std::thread::sleep(Duration::from_millis(500)),
            }
        };
        let num = |f: &str| -> Result<u32> { Ok(std::fs::read_to_string(dev.join(f))?.trim().parse()?) };
        let node = format!("/dev/bus/usb/{:03}/{:03}", num("busnum")?, num("devnum")?);
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&node).with_context(|| node.clone())?;
        // the interface's bulk endpoints: ep_XX directories with a direction
        let name = dev.file_name().unwrap().to_string_lossy().into_owned();
        let intf = dev.join(format!("{name}:1.0"));
        let (mut ep_in, mut ep_out) = (None, None);
        for e in std::fs::read_dir(&intf)?.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            let Some(addr) = n.strip_prefix("ep_") else { continue };
            if std::fs::read_to_string(e.path().join("type")).map(|t| t.trim() != "Bulk").unwrap_or(true) {
                continue;
            }
            let a = u32::from_str_radix(addr, 16)?;
            if a & 0x80 != 0 { ep_in = Some(a) } else { ep_out = Some(a) }
        }
        let u = Usb { file, ep_in: ep_in.context("no bulk IN endpoint")?, ep_out: ep_out.context("no bulk OUT endpoint")? };
        let mut cfg: u32 = 1;
        unsafe { libc::ioctl(u.file.as_raw_fd(), USBDEVFS_SETCONFIGURATION, &mut cfg) };
        // take interface 0 from qcserial (or whoever has it)
        let mut claim = DisconnectClaim { interface: 0, flags: 0, driver: [0; 256] };
        if unsafe { libc::ioctl(u.file.as_raw_fd(), USBDEVFS_DISCONNECT_CLAIM, &mut claim) } != 0 {
            bail!("claim interface 0: {}", std::io::Error::last_os_error());
        }
        Ok(u)
    }

    fn bulk(&mut self, ep: u32, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        let mut t = BulkTransfer { ep, len: buf.len() as u32, timeout: timeout.as_millis() as u32, data: buf.as_mut_ptr().cast() };
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), USBDEVFS_BULK, &mut t) };
        if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(r as usize) }
    }
}

impl Link for Usb {
    fn read(&mut self, n: usize, timeout: Duration) -> Result<Vec<u8>> {
        let mut b = vec![0u8; n];
        match self.bulk(self.ep_in, &mut b, timeout) {
            Ok(got) => {
                b.truncate(got);
                Ok(b)
            }
            Err(e) if e.raw_os_error() == Some(libc::ETIMEDOUT) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(1 << 20) {
            let mut c = chunk.to_vec();
            let n = self.bulk(self.ep_out, &mut c, Duration::from_secs(15))?;
            ensure!(n == chunk.len(), "short USB write ({n} of {})", chunk.len());
        }
        Ok(())
    }
}

impl Link for Serial {
    fn read(&mut self, n: usize, timeout: Duration) -> Result<Vec<u8>> {
        let t = Instant::now();
        loop {
            let mut b = self.read_some()?;
            if !b.is_empty() || t.elapsed() >= timeout {
                b.truncate(n);
                return Ok(b);
            }
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.write_all(data)
    }
}

// ---------------------------------------------------------------- Sahara

/// A Sahara packet (command, whole packet), or None if nothing came.
fn packet(l: &mut dyn Link, timeout: Duration) -> Result<Option<(u32, Vec<u8>)>> {
    let mut p = l.read(0x4000, timeout)?;
    if p.len() < 8 {
        return Ok(None);
    }
    // on a serial port a packet may come in pieces
    let len = le32(&p, 4) as usize;
    let t = Instant::now();
    while p.len() < len && t.elapsed() < timeout {
        p.extend(l.read(len - p.len(), timeout)?);
    }
    Ok(Some((le32(&p, 0), p)))
}

fn words(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn sahara_load(l: &mut dyn Link, loader: &[u8]) -> Result<()> {
    // the one-shot HELLO: read it patiently; RESET (the device sends it again) only as a last resort
    let mut hello = None;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(12) && hello.is_none() {
        if let Some((HELLO, p)) = packet(l, Duration::from_millis(800))?
            && p.len() >= 48
        {
            hello = Some(p);
        }
    }
    for _ in 0..3 {
        if hello.is_some() {
            break;
        }
        let _ = l.write(&words(&[RESET, 8]));
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(3) {
            if let Some((HELLO, p)) = packet(l, Duration::from_millis(800))?
                && p.len() >= 48
            {
                hello = Some(p);
                break;
            }
        }
    }
    let h = hello.context("no Sahara HELLO (replug the phone)")?;
    let (version, compat, maxlen, mode) = (le32(&h, 8), le32(&h, 12), le32(&h, 16), le32(&h, 20));
    log(&format!("HELLO v={version} maxlen={maxlen:#x} mode={mode}"));
    l.write(&words(&[HELLO_RESP, 48, version, compat, 0, mode, 0, 0, 0, 0, 0, 0]))?;
    let mut sent = 0usize;
    loop {
        let (cmd, p) = packet(l, Duration::from_secs(8))?.context("Sahara: the device went quiet")?;
        match cmd {
            READ_DATA | READ_DATA64 => {
                let (off, len) = if cmd == READ_DATA { (le32(&p, 12) as usize, le32(&p, 16) as usize) } else { (le64(&p, 16) as usize, le64(&p, 24) as usize) };
                ensure!(off + len <= loader.len(), "the device asks for {len} B at {off:#x} beyond the loader");
                l.write(&loader[off..off + len])?;
                sent += len;
            }
            END_IMG => {
                let status = le32(&p, 12);
                ensure!(status == 0, "the loader was rejected (status {status})");
                l.write(&words(&[DONE, 8]))?;
            }
            DONE_RESP => {
                log(&format!("firehose up (sent {sent:#x}); draining its hello burst"));
                std::thread::sleep(Duration::from_secs(1));
                // until ~2 s of silence: stopping at the first empty read races the burst and the first command
                // then times out
                let (mut burst, mut silence, t) = (0usize, 0, Instant::now());
                while t.elapsed() < Duration::from_secs(12) {
                    let c = l.read(0x4000, Duration::from_millis(500))?;
                    if c.is_empty() {
                        silence += 1;
                        if silence >= 4 {
                            break;
                        }
                    } else {
                        burst += c.len();
                        silence = 0;
                    }
                }
                log(&format!("  drained {burst} B"));
                return Ok(());
            }
            other => bail!("Sahara command {other:#x}"),
        }
    }
}

// ---------------------------------------------------------------- Firehose

struct Firehose<'a>(&'a mut dyn Link);

impl Firehose<'_> {
    fn send(&mut self, xml: &str) -> Result<()> {
        self.0.write(format!("<?xml version=\"1.0\" ?>\n<data>\n{xml}\n</data>\n").as_bytes())
    }

    /// The responses until `idle` without data.
    fn drain(&mut self, idle: Duration) -> Result<String> {
        let mut out = vec![];
        let mut t = Instant::now();
        while t.elapsed() < idle {
            let c = self.0.read(0x4000, Duration::from_millis(500))?;
            if !c.is_empty() {
                out.extend(c);
                t = Instant::now();
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn cmd(&mut self, xml: &str, idle: Duration) -> Result<String> {
        self.send(xml)?;
        self.drain(idle)
    }

    fn configure(&mut self) -> Result<String> {
        self.cmd("<configure MemoryName=\"ufs\" Verbose=\"0\" AlwaysValidate=\"0\" MaxDigestTableSizeInBytes=\"2048\" MaxPayloadSizeToTargetInBytes=\"1048576\" ZlpAwareHost=\"1\" SkipStorageInit=\"0\" />", Duration::from_secs(2))
    }

    fn range(lun: u32, start: u64, num: u64) -> String {
        format!("SECTOR_SIZE_IN_BYTES=\"{SECTOR}\" num_partition_sectors=\"{num}\" physical_partition_number=\"{lun}\" start_sector=\"{start}\"")
    }

    /// Wait for the raw mode ACK of a read or program.
    fn rawmode(&mut self) -> Result<()> {
        let (mut hdr, t) = (vec![], Instant::now());
        while t.elapsed() < Duration::from_secs(8) {
            hdr.extend(self.0.read(0x2000, Duration::from_millis(500))?);
            if hdr.windows(15).any(|w| w == b"rawmode=\"true\"") {
                return Ok(());
            }
        }
        bail!("no rawmode ACK: {}", String::from_utf8_lossy(&hdr).chars().take(300).collect::<String>())
    }

    /// The SHA-256 of sectors (lowercase hex), from either form Firehose prints it in.
    fn digest(&mut self, lun: u32, start: u64, num: u64) -> Result<String> {
        let out = self.cmd(&format!("<getsha256digest {} />", Self::range(lun, start, num)), Duration::from_secs(3))?;
        if let Some(i) = out.find("Digest ") {
            let h: String = out[i + 7..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if h.len() == 64 {
                return Ok(h.to_lowercase());
            }
        }
        let bytes: Vec<String> = out
            .split("0x")
            .skip(1)
            .filter_map(|s| s.get(..2).filter(|b| b.chars().all(|c| c.is_ascii_hexdigit())).map(str::to_lowercase))
            .collect();
        ensure!(bytes.len() >= 32, "no digest in: {}", out.lines().filter(|l| l.contains("igest") || l.contains("ERROR")).collect::<Vec<_>>().join(" | "));
        Ok(bytes[..32].concat())
    }

    fn read(&mut self, lun: u32, start: u64, num: u64) -> Result<Vec<u8>> {
        self.send(&format!("<read {} />", Self::range(lun, start, num)))?;
        self.rawmode()?;
        let need = num as usize * SECTOR;
        let (mut data, t) = (Vec::with_capacity(need), Instant::now());
        while data.len() < need && t.elapsed() < Duration::from_secs(30 + num / 256) {
            data.extend(self.0.read((need - data.len()).min(1 << 20), Duration::from_secs(5))?);
        }
        ensure!(data.len() >= need, "read {} of {need} B", data.len());
        data.truncate(need);
        self.drain(Duration::from_secs(1))?;
        Ok(data)
    }

    fn program(&mut self, lun: u32, start: u64, num: u64, image: &[u8]) -> Result<String> {
        let need = num as usize * SECTOR;
        ensure!(image.len() <= need, "the image ({} B) is larger than {num} sectors", image.len());
        let mut payload = image.to_vec();
        payload.resize(need, 0);
        self.send(&format!("<program {} />", Self::range(lun, start, num)))?;
        self.rawmode()?;
        self.0.write(&payload)?;
        self.drain(Duration::from_secs(3))
    }

    fn reset(&mut self) {
        let _ = self.cmd("<power value=\"reset\" DelayInSeconds=\"1\" />", Duration::from_secs(2));
    }
}

fn sha256_hex(d: &[u8]) -> String {
    sha2::Sha256::digest(d).iter().map(|b| format!("{b:02x}")).collect()
}

/// hyp_b's entry in LUN5's GPT (sector 2 on)
fn check_hyp_b(fh: &mut Firehose) -> Result<()> {
    let gpt = fh.read(5, 0, 6)?;
    for e in gpt[2 * SECTOR..].chunks(128).take(40) {
        if e.len() < 128 || e[..16].iter().all(|b| *b == 0) {
            continue;
        }
        let name: Vec<u16> = e[56..128].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|u| *u != 0).collect();
        if String::from_utf16_lossy(&name) == "hyp_b" {
            let (first, last) = (le64(e, 32), le64(e, 40));
            println!("LUN5 GPT: hyp_b LBA {first}-{last} ({} B) {}", (last - first + 1) as usize * SECTOR, if first == 1184 && last == 3231 { "MATCH" } else { "MISMATCH" });
            return Ok(());
        }
    }
    println!("hyp_b not found in LUN5's GPT");
    Ok(())
}

pub struct Args {
    pub loader: Option<String>,
    pub port: Option<String>,
    pub attach: bool,
    pub program: Option<(u32, u64, u64, String)>,
    pub digest: Option<(u32, u64, u64)>,
    pub read: Option<(u32, u64, u64, String)>,
    pub probe: bool,
    pub restore_hyp_b: Option<String>,
    pub commit: bool,
    pub reset: bool,
}

pub fn main(a: Args) -> Result<i32> {
    let mut link: Box<dyn Link> = match &a.port {
        Some(p) => Box::new(Serial::open(p, libc::B115200, Duration::from_millis(500))?),
        None => Box::new(Usb::open()?),
    };
    if !a.attach {
        let loader = std::fs::read(a.loader.as_deref().context("--loader (or --attach to a running Firehose)")?)?;
        sahara_load(link.as_mut(), &loader)?;
    }
    let mut fh = Firehose(link.as_mut());
    let nop = fh.cmd("<nop />", Duration::from_secs(1))?;
    println!("configure ACK: {} (nop: {})", fh.configure()?.contains("ACK"), nop.contains("ACK"));
    if let Some((lun, start, num, img)) = &a.program {
        let data = std::fs::read(img)?;
        let res = fh.program(*lun, *start, *num, &data)?;
        println!("{}", res.chars().rev().take(120).collect::<Vec<_>>().into_iter().rev().collect::<String>());
        println!("digest: {}", fh.digest(*lun, *start, *num)?);
    }
    if let Some((lun, start, num)) = a.digest {
        println!("digest: {}", fh.digest(lun, start, num)?);
    }
    if let Some((lun, start, num, out)) = &a.read {
        std::fs::write(out, fh.read(*lun, *start, *num)?)?;
        println!("{out}: {} sectors", num);
    }
    if a.probe {
        for (name, lun) in [("hyp_a", 3), ("hyp_b", 5)] {
            let d = fh.digest(lun, 1184, 2048)?;
            println!("{name} (LUN{lun} 1184+2048): {d} {}", if d == STOCK_HYP { "== stock" } else { "!= stock" });
        }
        check_hyp_b(&mut fh)?;
        println!("probe done (read only)");
    }
    if let Some(img) = &a.restore_hyp_b {
        let data = std::fs::read(Path::new(img))?;
        let sum = sha256_hex(&data);
        println!("{img}: {} B sha256 {sum} {}", data.len(), if sum == STOCK_HYP { "(stock)" } else { "(NOT stock)" });
        ensure!(data.len() == 2048 * SECTOR, "the image is not 8 MiB: refusing");
        ensure!(sum == STOCK_HYP, "the image is not the stock hyp: refusing");
        if !a.commit {
            println!("dry run: would program LUN5 start 1184, 2048 sectors with it; --commit writes");
        } else {
            println!(">>> programming hyp_b (LUN5 1184+2048)");
            println!("{}", fh.program(5, 1184, 2048, &data)?.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect::<String>());
            let d = fh.digest(5, 1184, 2048)?;
            println!("after the write: {d} {}", if d == STOCK_HYP { "== stock" } else { "!= stock" });
        }
    }
    if a.reset {
        fh.reset();
        println!("reset sent");
    }
    Ok(0)
}
