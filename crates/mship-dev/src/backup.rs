//! `backup`: raw partitions of the phone onto the NAS, verified end to end. The phone streams with busybox
//! (`dd | nc` to the PC over the dev link); here the stream is hashed and zstd-compressed as it arrives and goes to
//! the NAS over ssh. Progress goes to the UART monitor's log.
//!
//! - `partition PART NAS_DIR`: one stream, nothing staged locally; checked against a re-read on the phone and the
//!   file the NAS stored
//! - `chunked PART NAS_DIR [--chunk-mib 512] [--streams 3] [--stage DIR] [--stage-gib 50]`: chunks in parallel,
//!   staged (bounded) on the PC, resumable (NAS_DIR/PART.manifest: "idx raw_sha zst_sha len"), every chunk re-read
//!   on the phone at the end. Restore: `zstd -dc PART.*.zst | dd of=...`
//! - `firmware --serial S [--out DIR] [--include-huge]`: every firmware/boot partition and the GPTs of a phone in
//!   Android (Magisk su over adb), read only

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::Digest;

use crate::{live, shell};

const GIB: f64 = (1u64 << 30) as f64;

fn hex(d: impl AsRef<[u8]>) -> String {
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn gui(m: &str) {
    live::line("nsh", m);
}

// ---------------------------------------------------------------- the phone

/// A phone shell command's output (it must finish).
fn phone(cmd: &str, secs: u64) -> Result<String> {
    let (out, rc) = shell::run(cmd, Duration::from_secs(secs))?;
    ensure!(rc.is_some(), "phone: {cmd}: timed out");
    Ok(out)
}

/// (block device, bytes) of a partition by name
fn partition(part: &str) -> Result<(String, u64)> {
    let out = phone(&format!("for d in /sys/class/block/*; do grep -q '^PARTNAME={part}$' $d/uevent && basename $d; done"), 20)?;
    let dev = out.lines().last().context("no such partition")?.trim().to_string();
    let size: u64 = phone(&format!("echo $(( $(cat /sys/class/block/{dev}/size) * 512 ))"), 10)?.lines().last().context("size")?.trim().parse()?;
    Ok((dev, size))
}

/// Run `dd ... | nc` on the phone into a listener here; the stream to `sink` (its byte count).
fn pull(phone_cmd: String, port: u16, timeout: u64, mut sink: impl FnMut(&[u8]) -> Result<()>) -> Result<u64> {
    let l = TcpListener::bind(("10.42.0.1", port)).context("the dev link (10.42.0.1) is not up")?;
    let remote = std::thread::spawn(move || shell::run(&phone_cmd, Duration::from_secs(timeout)));
    let (mut c, _) = l.accept()?;
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let got = c.read(&mut buf)?;
        if got == 0 {
            break;
        }
        sink(&buf[..got])?;
        n += got as u64;
    }
    let _ = remote.join();
    Ok(n)
}

// ---------------------------------------------------------------- the NAS

pub struct Nas(pub String);

impl Nas {
    fn ssh(&self) -> Command {
        let mut c = Command::new("ssh");
        c.args(["-o", "BatchMode=yes", "-c", "aes128-gcm@openssh.com", &self.0]);
        c
    }

    pub fn run(&self, cmd: &str) -> Result<String> {
        let o = self.ssh().arg(cmd).output()?;
        ensure!(o.status.success(), "NAS: {cmd}: {}", String::from_utf8_lossy(&o.stderr).trim());
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    }

    /// A writer into a new file on the NAS (`cat > path`); finish() waits for it.
    fn writer(&self, path: &str) -> Result<std::process::Child> {
        Ok(self.ssh().arg(format!("cat > {}", q(path))).stdin(Stdio::piped()).spawn()?)
    }

    /// The verified chunks of a part: idx -> [idx, raw_sha, zst_sha, len]
    fn manifest(&self, dir: &str, part: &str) -> Result<BTreeMap<u64, Vec<String>>> {
        let m = format!("{dir}/{part}.manifest");
        let out = self.run(&format!("mkdir -p {} && touch {} && cat {}", q(dir), q(&m), q(&m)))?;
        Ok(out.lines().filter_map(|l| {
            let w: Vec<String> = l.split_whitespace().map(String::from).collect();
            (w.len() == 4).then(|| w[0].parse().ok().map(|i| (i, w)))?
        }).collect())
    }

    fn sha256(&self, path: &str) -> Result<String> {
        Ok(self.run(&format!("sha256sum {} | cut -d' ' -f1", q(path)))?.trim().to_string())
    }
}

/// zstd-compress a stream into a NAS file, hashing what goes in and what comes out: (raw sha, zst sha).
struct Upload {
    raw: sha2::Sha256,
    zst: Arc<Mutex<sha2::Sha256>>,
    enc: zstd::stream::write::Encoder<'static, HashingWriter>,
    child: std::process::Child,
}

struct HashingWriter {
    out: std::process::ChildStdin,
    hash: Arc<Mutex<sha2::Sha256>>,
}

impl Write for HashingWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.out.write(b)?;
        self.hash.lock().unwrap().update(&b[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

impl Upload {
    fn new(nas: &Nas, path: &str, level: i32) -> Result<Self> {
        let mut child = nas.writer(path)?;
        let zst = Arc::new(Mutex::new(sha2::Sha256::new()));
        let w = HashingWriter { out: child.stdin.take().unwrap(), hash: zst.clone() };
        let mut enc = zstd::stream::write::Encoder::new(w, level)?;
        let _ = enc.multithread(std::thread::available_parallelism().map_or(2, |n| n.get() as u32));
        Ok(Upload { raw: sha2::Sha256::new(), zst, enc, child })
    }

    fn write(&mut self, d: &[u8]) -> Result<()> {
        self.raw.update(d);
        self.enc.write_all(d)?;
        Ok(())
    }

    fn finish(self) -> Result<(String, String)> {
        let w = self.enc.finish()?;
        drop(w);
        let mut child = self.child;
        ensure!(child.wait()?.success(), "NAS upload failed");
        let zst = hex(self.zst.lock().unwrap().clone().finalize());
        Ok((hex(self.raw.finalize()), zst))
    }
}

/// Progress lines every `every` (bytes so far of `total`)
struct Meter {
    label: String,
    total: u64,
    t0: Instant,
    last: Instant,
    every: Duration,
}

impl Meter {
    fn new(label: &str, total: u64, every: Duration) -> Self {
        Meter { label: label.into(), total, t0: Instant::now(), last: Instant::now(), every }
    }

    fn tick(&mut self, n: u64) {
        if self.last.elapsed() < self.every {
            return;
        }
        self.last = Instant::now();
        let rate = n as f64 / self.t0.elapsed().as_secs_f64().max(0.001);
        let eta = (self.total.saturating_sub(n)) as f64 / rate.max(1.0) / 60.0;
        gui(&format!("{}: {:7.2}/{:.2} GiB {:5.1}%  {:5.1} MB/s  ETA {eta:5.1} min", self.label, n as f64 / GIB, self.total as f64 / GIB,
                     100.0 * n as f64 / self.total.max(1) as f64, rate / 1e6));
    }
}

// ---------------------------------------------------------------- partition: one stream

pub fn partition_backup(part: &str, nas_dir: &str, nas: &Nas, level: i32) -> Result<i32> {
    let (dev, size) = partition(part)?;
    let name = format!("{part}.img");
    let dst = format!("{nas_dir}/{name}.zst");
    gui(&format!("BACKUP {part} = /dev/{dev}, {:.2} GiB -> {}:{dst}", size as f64 / GIB, nas.0));
    nas.run(&format!("mkdir -p {}", q(nas_dir)))?;
    let port = 9100 + (std::process::id() % 800) as u16;
    let t0 = Instant::now();
    let mut up = Upload::new(nas, &format!("{dst}.part"), level)?;
    let mut meter = Meter::new(&format!("BACKUP {part}"), size, Duration::from_secs(5));
    let mut got = 0u64;
    pull(format!("dd if=/dev/{dev} bs=4M 2>/dev/null | nc 10.42.0.1 {port}; echo SENT"), port, 6 * 3600, |d| {
        got += d.len() as u64;
        meter.tick(got);
        up.write(d)
    })?;
    let (raw, zst) = up.finish()?;
    if got != size {
        gui(&format!("BACKUP {part} FAILED: got {got} of {size} bytes"));
        bail!("short stream");
    }
    gui(&format!("BACKUP {part}: streamed in {:.1} min; verifying...", t0.elapsed().as_secs_f64() / 60.0));
    let on_phone = phone(&format!("sha256sum /dev/{dev} | cut -d' ' -f1"), 3 * 3600)?.lines().last().unwrap_or("").trim().to_string();
    let stored = nas.run(&format!(
        "cd {d} && mv {zp} {z} && echo '{raw}  {name}' > {s} && echo '{zst}  {name}.zst' > {zs} && sha256sum {z} | cut -d' ' -f1",
        d = q(nas_dir), zp = q(&format!("{name}.zst.part")), z = q(&format!("{name}.zst")), s = q(&format!("{name}.sha256")), zs = q(&format!("{name}.zst.sha256"))
    ))?;
    let ok = on_phone == raw && stored.trim() == zst;
    gui(&format!("BACKUP {part}: phone re-read {}, NAS file {} => {}", if on_phone == raw { "MATCH" } else { "MISMATCH" },
                 if stored.trim() == zst { "MATCH" } else { "MISMATCH" }, if ok { "VERIFIED" } else { "NOT VERIFIED" }));
    println!("{part}: raw_sha256={raw} zst_sha256={zst} size={size} verified={ok}");
    Ok(if ok { 0 } else { 2 })
}

// ---------------------------------------------------------------- chunked

pub struct Chunked {
    pub part: String,
    pub nas_dir: String,
    pub chunk_mib: u64,
    pub streams: usize,
    pub stage: PathBuf,
    pub stage_gib: f64,
    pub level: i32,
}

fn staged_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

fn upload_file(nas: &Nas, src: &Path, dst: &str) -> Result<String> {
    let mut child = nas.writer(dst)?;
    let mut f = std::fs::File::open(src)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    {
        let stdin = child.stdin.as_mut().unwrap();
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            stdin.write_all(&buf[..n])?;
        }
    }
    drop(child.stdin.take());
    ensure!(child.wait()?.success(), "upload {}", src.display());
    Ok(hex(h.finalize()))
}

pub fn chunked(a: Chunked, nas: Nas) -> Result<i32> {
    std::fs::create_dir_all(&a.stage)?;
    let (dev, size) = partition(&a.part)?;
    let cb = a.chunk_mib << 20;
    let n = size.div_ceil(cb);
    let blocks = a.chunk_mib / 4; // dd bs=4M
    let man = format!("{}/{}.manifest", a.nas_dir, a.part);
    let done = Arc::new(Mutex::new(nas.manifest(&a.nas_dir, &a.part)?));
    let todo: Vec<u64> = (0..n).filter(|i| !done.lock().unwrap().contains_key(i)).collect();
    gui(&format!("CHUNKED BACKUP {}=/dev/{dev} {:.2} GiB, {n} x {} MiB, {} already on NAS, {} to go, {} streams",
                 a.part, size as f64 / GIB, a.chunk_mib, n - todo.len() as u64, todo.len(), a.streams));
    let nas = Arc::new(nas);
    let work = Arc::new(Mutex::new(todo.clone()));
    let (tx, rx) = std::sync::mpsc::channel::<(u64, PathBuf, String, u64)>();
    let stats = Arc::new(Mutex::new((0u64, 0u64, 0u64))); // raw bytes, ok, failed
    let t0 = Instant::now();
    // the uploader: staged chunk -> NAS, checked, manifest
    let up = {
        let (nas, done, stats, man, nas_dir, part) = (nas.clone(), done.clone(), stats.clone(), man.clone(), a.nas_dir.clone(), a.part.clone());
        std::thread::spawn(move || {
            for (i, file, raw, len) in rx {
                let name = format!("{part}.{i:04}.zst");
                let dst = format!("{nas_dir}/{name}");
                let mut ok = false;
                for _ in 0..3 {
                    let Ok(local) = upload_file(&nas, &file, &dst) else { continue };
                    if nas.sha256(&dst).is_ok_and(|r| r == local) && nas.run(&format!("echo '{i} {raw} {local} {len}' >> {}", q(&man))).is_ok() {
                        done.lock().unwrap().insert(i, vec![i.to_string(), raw.clone(), local, len.to_string()]);
                        ok = true;
                        break;
                    }
                }
                let _ = std::fs::remove_file(&file);
                let mut s = stats.lock().unwrap();
                if ok {
                    s.1 += 1;
                } else {
                    s.2 += 1;
                    gui(&format!("CHUNKED BACKUP {part}: chunk {i} upload/verify FAILED (re-run to retry)"));
                }
            }
        })
    };
    // streams: phone chunk -> zstd file in the stage
    let mut streams = vec![];
    for _ in 0..a.streams {
        let (work, tx, stats, stage, part, dev) = (work.clone(), tx.clone(), stats.clone(), a.stage.clone(), a.part.clone(), dev.clone());
        let (stage_gib, level) = (a.stage_gib, a.level);
        streams.push(std::thread::spawn(move || {
            loop {
                let Some(i) = work.lock().unwrap().pop() else { return };
                let mut res = None;
                for attempt in 1..=3 {
                    // the staging disk is the buffer: wait for the uploader when it is full
                    while staged_bytes(&stage) + cb > (stage_gib * GIB) as u64 {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                    let file = stage.join(format!("{part}.{i:04}.zst"));
                    let expect = cb.min(size - i * cb);
                    let r = (|| -> Result<(String, u64)> {
                        let mut enc = zstd::stream::write::Encoder::new(std::fs::File::create(&file)?, level)?;
                        let mut h = sha2::Sha256::new();
                        let got = pull(format!("dd if=/dev/{dev} bs=4M skip={} count={blocks} 2>/dev/null | nc 10.42.0.1 {}", i * blocks, 9500 + i % 400),
                                       (9500 + i % 400) as u16, 1800, |d| {
                            h.update(d);
                            enc.write_all(d)?;
                            Ok(())
                        })?;
                        enc.finish()?;
                        ensure!(got == expect, "{got} of {expect} B");
                        Ok((hex(h.finalize()), got))
                    })();
                    match r {
                        Ok((raw, len)) => {
                            res = Some((file, raw, len));
                            break;
                        }
                        Err(e) => {
                            let _ = std::fs::remove_file(&file);
                            gui(&format!("CHUNKED BACKUP {part}: chunk {i} receive failed ({e}), attempt {attempt}"));
                            std::thread::sleep(Duration::from_secs(3));
                        }
                    }
                }
                match res {
                    Some((file, raw, len)) => {
                        stats.lock().unwrap().0 += len;
                        let _ = tx.send((i, file, raw, len));
                    }
                    None => stats.lock().unwrap().2 += 1,
                }
            }
        }));
    }
    drop(tx);
    while streams.iter().any(|s| !s.is_finished()) {
        std::thread::sleep(Duration::from_secs(10));
        let s = *stats.lock().unwrap();
        let rate = s.0 as f64 / t0.elapsed().as_secs_f64().max(1.0);
        gui(&format!("CHUNKED BACKUP {}: pulled {:6.1} GiB ({:5.1} MB/s) | NAS verified {}/{n} chunks, staged {:5.1} GiB, failures {}",
                     a.part, s.0 as f64 / GIB, rate / 1e6, done.lock().unwrap().len(), staged_bytes(&a.stage) as f64 / GIB, s.2));
    }
    for s in streams {
        let _ = s.join();
    }
    let _ = up.join();
    let done = done.lock().unwrap().clone();
    let missing: Vec<u64> = (0..n).filter(|i| !done.contains_key(i)).collect();
    if !missing.is_empty() {
        gui(&format!("CHUNKED BACKUP {}: INCOMPLETE, {} chunks missing: {:?} -- re-run to resume", a.part, missing.len(), &missing[..missing.len().min(20)]));
        return Ok(1);
    }
    gui(&format!("CHUNKED BACKUP {}: all {n} chunks on NAS in {:.1} min; re-reading on phone to verify", a.part, t0.elapsed().as_secs_f64() / 60.0));
    let mut bad = vec![];
    for start in (0..n).step_by(4) {
        let idx: Vec<u64> = (start..n.min(start + 4)).collect();
        let cmd = idx.iter().map(|i| format!("(dd if=/dev/{dev} bs=4M skip={} count={blocks} 2>/dev/null | sha256sum | sed 's/^/C{i} /') &", i * blocks)).collect::<Vec<_>>().join(" ; ") + " wait";
        for l in phone(&cmd, 900)?.lines().filter(|l| l.starts_with('C')) {
            let w: Vec<&str> = l.split_whitespace().collect();
            if let (Some(k), Some(h)) = (w.first().and_then(|k| k[1..].parse::<u64>().ok()), w.get(1))
                && done[&k][1] != *h
            {
                bad.push(k);
            }
        }
        gui(&format!("CHUNKED BACKUP {}: phone re-read {}/{n}, mismatches {}", a.part, n.min(start + 4), bad.len()));
    }
    gui(&format!("CHUNKED BACKUP {}: {}", a.part, if bad.is_empty() { "VERIFIED -- every chunk matches the phone".into() } else { format!("MISMATCH in chunks {bad:?}") }));
    println!("{}: chunks={n} verified={} bad={bad:?}", a.part, bad.is_empty());
    Ok(if bad.is_empty() { 0 } else { 2 })
}

// ---------------------------------------------------------------- firmware (Android, Magisk su)

const SKIP_HUGE: &[&str] = &["super", "userdata", "ramdump"];

fn su(serial: &str, cmd: &str) -> Command {
    let mut c = Command::new("adb");
    c.args(["-s", serial, "exec-out", "su", "-c", cmd]);
    c
}

fn sha256_file(p: &Path) -> Result<String> {
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut std::fs::File::open(p)?, &mut h)?;
    Ok(hex(h.finalize()))
}

/// /dev/block/by-name also aliases whole LUNs (sda..sdf): never back those up
fn bare_disk(name: &str) -> bool {
    (name.len() == 3 && name.starts_with("sd")) || name.starts_with("mmcblk") && !name.contains('p') || name.starts_with("nvme")
}

pub fn firmware(serial: &str, out: Option<PathBuf>, include_huge: bool) -> Result<i32> {
    let id = su(serial, "id").output()?;
    ensure!(id.stdout.starts_with(b"uid=0"), "root (su) not available; is the phone booted into Android with Magisk ready?");
    let stamp = {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::gmtime_r(&t, &mut tm) };
        format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
    };
    let out = out.unwrap_or_else(|| crate::repo().join("motorola_nio/backups").join(format!("nio-firmware-{stamp}")));
    std::fs::create_dir_all(&out)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o700))?;
    let listing = su(serial, "for f in /dev/block/by-name/*; do echo \"$(basename $f)|$(readlink -f $f)|$(blockdev --getsize64 $(readlink -f $f))\"; done").output()?;
    let mut parts: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for l in String::from_utf8_lossy(&listing.stdout).lines() {
        let w: Vec<&str> = l.trim().split('|').collect();
        if let [name, dev, size] = w.as_slice() {
            parts.insert(name.to_string(), (dev.to_string(), size.parse().unwrap_or(0)));
        }
    }
    let skip: &[&str] = if include_huge { &[] } else { SKIP_HUGE };
    let mut man_parts = serde_json::Map::new();
    let mut failures = vec![];
    for (name, (dev, size)) in &parts {
        if skip.contains(&name.as_str()) || bare_disk(name) {
            man_parts.insert(name.clone(), serde_json::json!({"skipped": true, "bytes": size, "device": dev}));
            continue;
        }
        let image = out.join(format!("{name}.img"));
        if std::fs::metadata(&image).is_ok_and(|m| m.len() == *size) {
            man_parts.insert(name.clone(), serde_json::json!({"bytes": size, "device": dev, "sha256": sha256_file(&image)?}));
            println!("have {name:24} {size:>12}");
            continue;
        }
        let st = su(serial, &format!("cat {dev}")).stdout(std::fs::File::create(&image)?).stderr(Stdio::piped()).output()?;
        let got = std::fs::metadata(&image)?.len();
        if !st.status.success() || got != *size {
            std::fs::rename(&image, image.with_extension("img.failed"))?;
            man_parts.insert(name.clone(), serde_json::json!({"status": "FAILED", "expected": size, "got": got,
                "error": String::from_utf8_lossy(&st.stderr).chars().take(200).collect::<String>()}));
            println!("FAILED {name}: expected {size}, got {got}");
            failures.push(name.clone());
            continue;
        }
        man_parts.insert(name.clone(), serde_json::json!({"bytes": size, "device": dev, "sha256": sha256_file(&image)?}));
        println!("ok  {name:24} {size:>12}");
    }
    // the GPT of each LUN (head and tail: primary and backup), so the layout itself is recoverable
    let luns: std::collections::BTreeSet<String> = parts.values().map(|(d, _)| d.rsplit('/').next().unwrap_or("").chars().filter(|c| c.is_ascii_alphabetic()).collect()).collect();
    let mut gpt = serde_json::Map::new();
    for lun in luns {
        let node = format!("/dev/block/{lun}");
        let s = su(serial, &format!("blockdev --getsize64 {node} 2>/dev/null || true")).output()?;
        let Ok(total) = String::from_utf8_lossy(&s.stdout).trim().parse::<u64>() else { continue };
        let (head, tail) = (out.join(format!("gpt-{lun}-head.bin")), out.join(format!("gpt-{lun}-tail.bin")));
        su(serial, &format!("dd if={node} bs=1048576 count=1 2>/dev/null")).stdout(std::fs::File::create(&head)?).status()?;
        su(serial, &format!("dd if={node} bs=1048576 skip={} count=1 2>/dev/null", total / (1 << 20) - 1)).stdout(std::fs::File::create(&tail)?).status()?;
        gpt.insert(lun.clone(), serde_json::json!({"device": node, "total_bytes": total, "head_sha256": sha256_file(&head)?, "tail_sha256": sha256_file(&tail)?}));
        println!("gpt {lun}: {total} bytes");
    }
    let saved: u64 = man_parts.values().filter(|v| v.get("sha256").is_some()).filter_map(|v| v["bytes"].as_u64()).sum();
    let count = man_parts.values().filter(|v| v.get("sha256").is_some()).count();
    let manifest = serde_json::json!({"device": "nio", "serial_captured": true, "created": stamp,
        "note": "Read-only firmware/boot backup. No phone partition was written.", "skipped": skip, "partitions": man_parts, "gpt": gpt});
    std::fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    println!("\n{count} partitions, {:.2} GB -> {}", saved as f64 / 1e9, out.display());
    ensure!(failures.is_empty(), "FAILED partitions: {}", failures.join(", "));
    Ok(0)
}
