//! The phone's VMM (host/gpu): crosvm with our patches, linked against virglrenderer with the msm DRM native context
//! renderer (the guests' Mesa freedreno/turnip drive the Adreno through virtio-gpu) and a static minigbm with its msm
//! backend (rutabaga's gralloc: buffers the guests allocate are real msm GEM buffers with the Adreno layout).
//! Every source is pinned (host/gpu/source.json) and patched (host/gpu/patches/<project>); cross-built for the
//! phone, installed to /opt/mship-gpu (bin/crosvm, lib/libvirglrenderer.so.1; crosvm finds it by its RUNPATH).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::TARGET;
use crate::util::*;

pub const PREFIX: &str = "opt/mship-gpu";

pub struct Gpu {
    pub crosvm: PathBuf,
    pub virglrenderer: PathBuf,
}

fn source(src: &serde_json::Value, name: &str) -> Result<(String, String)> {
    let s = &src[name];
    Ok((s["git"].as_str().context("git")?.to_string(), s["rev"].as_str().context("rev")?.to_string()))
}

/// virglrenderer (msm native context only), installed under `prefix`
fn virglrenderer(src: &serde_json::Value, work: &Path, prefix: &Path) -> Result<()> {
    let (url, rev) = source(src, "virglrenderer")?;
    let tree = work.join("virglrenderer");
    git_checkout(&url, &rev, &tree)?;
    apply(&tree, &patches(&root().join("host/gpu/patches/virglrenderer"))?)?;
    let build = work.join("virglrenderer-build");
    if !build.join("build.ninja").exists() {
        let cross = meson_cross(work)?;
        run_in(&tree, &["meson", "setup", &build.to_string_lossy(), "--cross-file", &cross.to_string_lossy(), "-Ddrm-renderers=msm",
                        "-Dvenus=false", "-Dbuildtype=release", &format!("--prefix={}", prefix.display()), "--libdir=lib"])?;
    }
    run_in(&tree, &["ninja", "-C", &build.to_string_lossy(), "install"])
}

/// minigbm with its msm backend, static, under the name rutabaga links ("gbm"), with a pkg-config file for it
fn minigbm(src: &serde_json::Value, work: &Path) -> Result<PathBuf> {
    let (url, rev) = source(src, "minigbm")?;
    let tree = work.join("minigbm");
    git_checkout(&url, &rev, &tree)?;
    let out = work.join("minigbm-out");
    // the msm backend needs the define itself (the Makefile's DRV_MSM only adds -ldl)
    let st = Command::new("make")
        .current_dir(&tree)
        .args(["-s", "DRV_MSM=1", &format!("OUT={}", out.display()), "CC_STATIC_LIBRARY(libminigbm.pie.a)"])
        .env("CC", "aarch64-linux-gnu-gcc")
        .env("AR", "aarch64-linux-gnu-ar")
        .env("CFLAGS", "-O2 -DDRV_MSM")
        .status()?;
    ensure!(st.success(), "minigbm: make failed");
    std::fs::copy(out.join("libminigbm.pie.a"), out.join("libgbm.a"))?;
    let pc = out.join("pkgconfig");
    std::fs::create_dir_all(&pc)?;
    std::fs::write(
        pc.join("gbm.pc"),
        format!("Name: gbm\nDescription: minigbm (msm), static\nVersion: 99.0.0\nLibs: -L{} -lgbm -ldrm\nCflags:\n", out.display()),
    )?;
    Ok(pc)
}

/// The crates.io crates crosvm uses that we patch: (name, patched source tree)
fn crates(src: &serde_json::Value, work: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = vec![];
    for (name, c) in src["crates"].as_object().context("crates")? {
        let version = c["version"].as_str().context("version")?;
        let archive = fetch(
            &format!("https://crates.io/api/v1/crates/{name}/{version}/download"),
            c["sha256"].as_str().context("sha256")?,
            &work.join(format!("crates/{name}-{version}.crate")),
        )?;
        let tree = work.join(format!("crates/{name}-{version}"));
        let _ = std::fs::remove_dir_all(&tree);
        tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(&archive)?)).unpack(work.join("crates"))?;
        apply(&tree, &patches(&root().join("host/gpu/patches").join(name))?)?;
        out.push((name.clone(), tree));
    }
    Ok(out)
}

pub fn gpu() -> Result<Gpu> {
    let src: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root().join("host/gpu/source.json"))?)?;
    let work = target_dir().join("gpu");
    std::fs::create_dir_all(&work)?;
    let prefix = work.join("prefix");
    virglrenderer(&src, &work, &prefix)?;
    let gbm_pc = minigbm(&src, &work)?;
    let crates = crates(&src, &work)?;

    let (url, rev) = source(&src, "crosvm")?;
    let tree = work.join("crosvm");
    git_checkout(&url, &rev, &tree)?;
    run_in(&tree, &["git", "submodule", "update", "-q", "--init", "--depth", "1", "third_party/minijail"])?;
    apply(&tree, &patches(&root().join("host/gpu/patches/crosvm"))?)?;
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&tree)
        .args(["build", "--release", "--target", TARGET, "--features", "gpu,virgl_renderer,wl-dmabuf,media"])
        // the image's toolchain, not crosvm's pinned one
        .env("RUSTUP_TOOLCHAIN", "stable")
        .env("CARGO_TARGET_DIR", work.join("crosvm-target"))
        .env("CROSVM_USE_SYSTEM_VIRGLRENDERER", "1")
        .env(
            "PKG_CONFIG_PATH",
            format!("{}:{}/lib/pkgconfig:/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig", gbm_pc.display(), prefix.display()),
        )
        // minigbm stays out of crosvm's dynamic symbols: virglrenderer in the same process uses Mesa's libgbm
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
            format!("-C link-arg=-Wl,--exclude-libs,libgbm.a -C link-arg=-Wl,-rpath,/{PREFIX}/lib"),
        );
    for (name, path) in &crates {
        cmd.args(["--config", &format!("patch.crates-io.{name}.path=\"{}\"", path.display())]);
    }
    ensure!(cmd.status()?.success(), "crosvm: cargo build failed");
    let virglrenderer = std::fs::canonicalize(prefix.join("lib/libvirglrenderer.so.1"))?;
    Ok(Gpu { crosvm: work.join("crosvm-target").join(TARGET).join("release/crosvm"), virglrenderer })
}
