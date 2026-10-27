//! Helpers of the tasks: paths, downloads, archives, Docker, cross builds.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use sha2::Digest;

/// mothershipOS (this repository)
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// The phone port, next to mothershipOS (its own repository): kernel, device tree, boot chain, firmware, rootfs
pub fn port() -> PathBuf {
    root().parent().unwrap().join("motorola_nio")
}


pub fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root().join("target"))
}

pub fn cargo(args: &[&str]) -> Result<()> {
    let st = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())).args(args).current_dir(root()).status()?;
    if !st.success() {
        bail!("cargo {} failed", args.join(" "));
    }
    Ok(())
}

pub fn add_file(tar: &mut tar::Builder<std::fs::File>, src: &Path, dst: &str, mode: u32, owner: (u64, &str)) -> Result<()> {
    let data = std::fs::read(src).with_context(|| src.display().to_string())?;
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(mode);
    h.set_uid(owner.0);
    h.set_gid(owner.0);
    h.set_username(owner.1)?;
    h.set_groupname(owner.1)?;
    h.set_mtime(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs());
    h.set_cksum();
    tar.append_data(&mut h, dst, data.as_slice())?;
    Ok(())
}

pub fn sha256(path: &Path) -> Result<String> {
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut std::fs::File::open(path)?, &mut h)?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// `url` saved as `dest` (once), checked against its pinned SHA-256.
pub fn fetch(url: &str, sum: &str, dest: &Path) -> Result<PathBuf> {
    fetch_checked(url, sum, dest, sha256)
}

/// `url` saved as `dest` (once), checked with `digest` against `sum`.
pub fn fetch_checked(url: &str, sum: &str, dest: &Path, digest: fn(&Path) -> Result<String>) -> Result<PathBuf> {
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d)?;
    }
    if !dest.exists() {
        println!("downloading {url}");
        let partial = dest.with_extension("partial");
        let mut body = ureq::get(url).call()?.into_body();
        std::io::copy(&mut body.as_reader(), &mut std::fs::File::create(&partial)?)?;
        std::fs::rename(&partial, dest)?;
    }
    let got = digest(dest)?;
    if got != sum {
        std::fs::remove_file(dest)?;
        bail!("{}: checksum {got}, expected {sum} (removed)", dest.display());
    }
    Ok(dest.to_path_buf())
}

/// Every file under `dir`.
pub fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            walk(&p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}

/// `docker` with these arguments (the host's Docker, from the build image: its socket is mounted).
pub fn docker(args: &[&str]) -> Result<()> {
    ensure!(Command::new("docker").args(args).status().context("docker (mount /var/run/docker.sock)")?.success(), "docker {} failed", args.join(" "));
    Ok(())
}

/// Where the host sees a path under the target directory (for volumes of containers the host's Docker runs).
pub fn host_path(p: &Path) -> Result<String> {
    let host = std::env::var("MSHIP_HOST_TARGET").context("MSHIP_HOST_TARGET: the host path of the target directory")?;
    Ok(Path::new(&host).join(p.strip_prefix(target_dir())?).to_string_lossy().into_owned())
}

/// A meson cross file for the phone (aarch64, the build image's arm64 libraries).
pub fn meson_cross(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let cross = dir.join("aarch64.ini");
    std::fs::write(
        &cross,
        "[binaries]\nc = 'aarch64-linux-gnu-gcc'\ncpp = 'aarch64-linux-gnu-g++'\nar = 'aarch64-linux-gnu-ar'\nstrip = 'aarch64-linux-gnu-strip'\n\
         pkg-config = 'pkg-config'\n[properties]\npkg_config_libdir = ['/usr/lib/aarch64-linux-gnu/pkgconfig', '/usr/share/pkgconfig']\n\
         [host_machine]\nsystem = 'linux'\ncpu_family = 'aarch64'\ncpu = 'aarch64'\nendian = 'little'\n",
    )?;
    Ok(cross)
}

/// Run a program in `dir`; an error names it.
pub fn run_in(dir: &Path, args: &[&str]) -> Result<()> {
    let st = Command::new(args[0]).args(&args[1..]).current_dir(dir).status().with_context(|| args[0].to_string())?;
    ensure!(st.success(), "{} failed (in {})", args.join(" "), dir.display());
    Ok(())
}

/// The patches of a directory (`*.patch`, in name order).
pub fn patches(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect::<std::io::Result<_>>()?;
    v.retain(|p| p.extension().is_some_and(|x| x == "patch"));
    v.sort();
    Ok(v)
}

/// Apply patches to a tree (`git apply`, which also works outside a repository).
pub fn apply(tree: &Path, patches: &[PathBuf]) -> Result<()> {
    for p in patches {
        let st = Command::new("git").arg("apply").arg(p).current_dir(tree).status()?;
        ensure!(st.success(), "{}: does not apply", p.display());
    }
    Ok(())
}

/// A git checkout of `url` at `rev` in `dir`, with local changes dropped (patches go on top afresh).
pub fn git_checkout(url: &str, rev: &str, dir: &Path) -> Result<()> {
    if !dir.join(".git").exists() {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir)?;
        run_in(dir, &["git", "init", "-q"])?;
        run_in(dir, &["git", "remote", "add", "origin", url])?;
    }
    let have = Command::new("git").args(["cat-file", "-e", &format!("{rev}^{{commit}}")]).current_dir(dir).status()?.success();
    if !have {
        run_in(dir, &["git", "fetch", "-q", "--depth", "1", "origin", rev])?;
    }
    run_in(dir, &["git", "checkout", "-q", "-f", rev])?;
    run_in(dir, &["git", "clean", "-q", "-f", "-d"])
}

/// The build's parallelism.
pub fn jobs() -> String {
    std::thread::available_parallelism().map_or(4, |n| n.get()).to_string()
}

/// A Linux source tree: the pinned tarball (`src`: version, url, sha256) downloaded once, unpacked, with
/// `patches_dir` applied in name order; a changed patch set unpacks it again.
pub fn kernel_tree(work: &Path, src: &serde_json::Value, patches_dir: &Path) -> Result<PathBuf> {
    let (version, url, sum) = (src["version"].as_str().context("version")?, src["url"].as_str().context("url")?, src["sha256"].as_str().context("sha256")?);
    let patches = patches(patches_dir)?;
    let mut h = sha2::Sha256::new();
    for p in &patches {
        h.update(std::fs::read(p)?);
    }
    let stamp: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let tree = work.join(format!("linux-{version}"));
    let applied = tree.join(".mothership-patches");
    if tree.join("Makefile").exists() && std::fs::read_to_string(&applied).is_ok_and(|s| s == stamp) {
        return Ok(tree);
    }
    let _ = std::fs::remove_dir_all(&tree);
    let archive = fetch(url, sum, &work.join(format!("linux-{version}.tar.xz")))?;
    println!("unpacking {}", archive.display());
    let staging = work.join("unpack");
    let _ = std::fs::remove_dir_all(&staging);
    tar::Archive::new(xz2::read::XzDecoder::new(std::fs::File::open(&archive)?)).unpack(&staging)?;
    std::fs::rename(staging.join(format!("linux-{version}")), &tree)?;
    std::fs::remove_dir_all(&staging)?;
    apply(&tree, &patches)?;
    std::fs::write(&applied, stamp)?;
    Ok(tree)
}

/// The files of a Debian package's data archive: (path, contents, mode) of every regular file and symlink target.
pub fn deb_files(deb: &Path) -> Result<Vec<(String, Vec<u8>, u32)>> {
    let data = std::fs::read(deb)?;
    ensure!(data.starts_with(b"!<arch>\n"), "{}: not a Debian package", deb.display());
    let mut off = 8;
    while off + 60 <= data.len() {
        let name = String::from_utf8_lossy(&data[off..off + 16]).trim().trim_end_matches('/').to_string();
        let size: usize = String::from_utf8_lossy(&data[off + 48..off + 58]).trim().parse()?;
        let body = &data[off + 60..off + 60 + size];
        if let Some(kind) = name.strip_prefix("data.tar") {
            let reader: Box<dyn std::io::Read> = match kind {
                ".xz" => Box::new(xz2::read::XzDecoder::new(body)),
                ".gz" => Box::new(flate2::read::GzDecoder::new(body)),
                ".zst" => Box::new(ruzstd::decoding::StreamingDecoder::new(body).map_err(|e| anyhow::anyhow!("{e}"))?),
                "" => Box::new(body),
                other => bail!("{}: data.tar{other}", deb.display()),
            };
            let mut out = vec![];
            for e in tar::Archive::new(reader).entries()? {
                let mut e = e?;
                if !e.header().entry_type().is_file() {
                    continue;
                }
                let path = e.path()?.to_string_lossy().trim_start_matches("./").to_string();
                let mode = e.header().mode()?;
                let mut buf = vec![];
                std::io::Read::read_to_end(&mut e, &mut buf)?;
                out.push((path, buf, mode));
            }
            return Ok(out);
        }
        off += 60 + size + (size & 1);
    }
    bail!("{}: no data archive", deb.display())
}
