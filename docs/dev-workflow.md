# Development workflow

Cargo and `cargo xtask` run in `mothershipOS/`; the port (`../motorola_nio`) and the VMs (`../pixelOS_16_VM`,
`../ubuntu_26.04_desktop_VM`) are found next to it. arm64 builds are cross-compiled on the PC or a build machine,
never emulated.

## Tools
| Tool | Where | What |
|---|---|---|
| `cargo xtask` | `xtask/` (PC) | `check` (clippy + tests), `host-kernel`, `initramfs`, `fit`, `gpu`, `sway`, `vhost-sound`, `dist`, `abl-dtb`; `cargo xtask help` |
| `mship-dev` | `crates/mship-dev` (PC) | `sh`, `proxy`, `console`, `deploy-fit`, `edl`, `uart`, `backup`; `cargo run --release -p mship-dev -- --help` |
| `mship-system` | `crates/mship-system` (phone, root) | `install <tar>`, `setup`, `boot-image <fit>`, `vm-install <repository \| package>`, and the system jobs its units run |
| `mship` | `crates/mship` (phone) | mothershipd's command line: VMs, hardware handover, screen, power |

## Link to the phone
- USB-NCM: phone `10.42.0.2`, PC `10.42.0.1` (NetworkManager profile `mothership-usb`). `ssh root@10.42.0.2`
  works (key auth). `telnetd` on :2323 serves `mship-dev sh`.
- `mship-dev sh 'cmd' [timeout]` runs a shell command on the phone and mirrors it into the UART monitor log
  (`motorola_nio/tools/uart/live.log`, override with `NSH_LIVE_LOG`). It mangles long multi-line commands and
  heredocs: for anything longer, serve a script over HTTP from 10.42.0.1 and run it with `wget` on the phone.
- The phone has no internet until Wi-Fi is up. `mship-dev proxy` is an HTTP/HTTPS proxy on 10.42.0.1:3128 for apt.
  Set the clock after boot (`date -u -s …`) before apt, unless `mship-timesync` has run.
- `mship-dev uart` is the debug UART monitor, `mship-dev console` the diagnostic initramfs shell over USB ACM.

## Kernel, DT, initramfs (nio)
- `cargo xtask host-kernel` builds the kernel and modules (the pinned LTS from
  `motorola_nio/kernel/sources.lock.json` + `motorola_nio/kernel/patches`, config fragment `nio-host.config`),
  `cargo xtask initramfs` the initramfs (mship-init, busybox, early firmware), `cargo xtask fit` the FIT
  (`motorola_nio/boot/nio-el2.its`: kernel + DTB + initramfs) in `target/boot/nio-el2.itb`.
- `mship-dev deploy-fit [itb]` writes it to boot_b from the running phone, verifies it and reboots
  (`--no-reboot` to skip that). On the phone itself: `mship-system boot-image <fit> [--reboot]`.
- u-boot (hyp_b) is written over EDL: `mship-dev edl --program …` (the port's
  [boot notes](https://github.com/MothershipOS/motorola_nio/blob/main/docs/boot.md)).
- Modules ship with the release (`cargo xtask dist`); modules from the same tree and config load without
  reflashing.

## Host userspace
`cargo xtask dist` builds the release tarball (programs, `dist/rootfs` and the port's `rootfs/`, crosvm, sway,
vhost-device-sound); `mship-system install <tar>` on the phone installs it and runs `mship-system setup` (packages,
the mship user, audio, swap). Restarting `mship-sway` restarts the whole session.

## VMs
mothershipOS builds no VM. Each VM repository has its own build; the phone installs it with
`mship-system vm-install` ([vm-packages.md](vm-packages.md)). PixelOS is built on a build machine with the PixelOS
tree ([android-vm.md](https://github.com/MothershipOS/pixelOS_16_VM/blob/main/docs/android-vm.md)).

## Rules
- Slot A is untouched stock Android (fallback). Slot B is the development slot: writing **boot_b** (FIT) and
  **hyp_b** (u-boot) is a routine development step and needs no approval each time. Secure boot is not enforced
  on this unit. A FIT or u-boot that doesn't boot means EDL recovery: `mship-dev edl` with the recovery set in
  `motorola_nio/unbrick/` (loader + stock hyp; known-good FIT `motorola_nio/boot/nio-el2-known-good.itb`).
- Writing or wiping any other partition (slot A, modem/EFS, persist, userdata, GPT) needs explicit approval from
  the user. RAM boots and reboots are fine.
- Back up before risky work: `mship-dev backup partition|chunked|firmware` (to a NAS: `--nas` or `MSHIP_NAS`);
  backups stay out of the repositories.
- Never put FIT load addresses inside DT `no-map` reserved ranges (bus stall → watchdog).
- Never read Q6/LPASS MMIO directly from userspace: it resets the SoC.
- The speaker amp only leaves -36 dB on the protected route (Awinic SKT in the ADSP with V/I feedback); never
  give it full gain on a path that can run without that protection (`motorola_nio/docs/audio.md`).
- An oops in a system-wq work item can wedge that CPU's pool (UFS I/O hangs). `echo 0 >
  /sys/devices/system/cpu/cpuN/online` with N from the oops unsticks it.

## Commits
Sources and docs only; build outputs, firmware blobs, boot images and device dumps stay local (`.gitignore`).
Large or proprietary files never go into history. Before pushing, check what would be published for personal data.
