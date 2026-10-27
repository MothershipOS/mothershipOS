# Development workflow

## Link to the phone
- USB-NCM: phone `10.42.0.2`, PC `10.42.0.1` (NetworkManager profile `mothership-usb`). `ssh root@10.42.0.2`
  works (key auth). `telnetd` on :2323 serves `mship-dev sh`. The PC tools are one binary:
  `cargo run --release -p mship-dev -- <command>` (`--help` lists them).
- `mship-dev sh 'cmd' [timeout]` runs a shell command on the phone and mirrors it into the UART monitor log
  (`motorola_nio/tools/uart/live.log`, override with `NSH_LIVE_LOG`). It mangles long multi-line commands and
  heredocs. For anything non-trivial, serve a script from the PC (`python3 -m http.server 8765 --bind 10.42.0.1`)
  and run `wget -O /tmp/x.sh http://10.42.0.1:8765/x.sh; sh /tmp/x.sh` on the phone.
- The phone has no internet until Wi-Fi is up. `mship-dev proxy` is an HTTP proxy on 10.42.0.1:3128 for apt.
  Set the clock after boot (`date -u -s …`) before apt, unless `mship-timesync` has run.

## Backups
Partition and firmware backups of the nio are on the NAS, not in the repo: `<nas>:<backups>/mothershipOS-backups/pc-backups-20261012/`
(stock firmware/boot images of both slots, GPTs, hyp, early boot dumps; `SHA256SUMS` there) and the userdata backup
in `…/mothershipOS-backups/nio-userdata-20261008/`. New ones: `mship-dev backup partition|chunked|firmware`. The EDL recovery set (loader + stock hyp) is kept locally in
`motorola_nio/unbrick/images/`.

Cargo and `cargo xtask` run in `mothershipOS/`; the port (`../motorola_nio`) and the VMs (`../pixelOS_16_VM`, `../ubuntu_26.04_desktop_VM`) are found next to it.

## Kernel, DT, initramfs (nio)
- `cargo xtask host-kernel` builds the kernel and modules (pristine 6.18.54 + `motorola_nio/kernel/patches`, config
  `nio-host.config`), `cargo xtask initramfs` the initramfs (mship-init, busybox, early firmware), `cargo xtask fit`
  the FIT (kernel + `nio-el2.dtb` + initramfs) in target/boot/nio-el2.itb.
- `mship-dev deploy-fit [itb]` writes it to boot_b from Linux, verifies it and reboots. No EDL needed, but a FIT
  that does not boot means EDL recovery (`motorola_nio/unbrick/`).
- Modules ship with the release (`cargo xtask dist`); modules from the same tree and config load without
  reflashing.

## Rules
- **Flashing or wiping any partition needs explicit approval from the user.** RAM boots and reboots are fine.
- Slot A is untouched stock Android (fallback). Slot B is the experiment slot. Secure boot is not enforced on
  this unit. Every brick so far was EDL-recoverable with `motorola_nio/unbrick/`. Known-good FIT:
  `motorola_nio/boot/nio-el2-known-good.itb`.
- Never put FIT load addresses inside DT `no-map` reserved ranges (bus stall → watchdog).
- Never read Q6/LPASS MMIO directly from userspace: it resets the SoC.
- Keep the speaker amp at -36 dB until speaker protection works.
- An oops in a system-wq work item can wedge that CPU's pool (UFS I/O hangs). `echo 0 >
  /sys/devices/system/cpu/cpuN/online` with N from the oops unsticks it.

## Host userspace
`cargo xtask dist` builds the release tarball (programs, `dist/rootfs` and the port's `rootfs/`, guest kernel and packages);
`mship-system install <tar>` on the phone installs it and runs `mship-system setup` (packages, the mship user,
audio, swap). Restarting `mship-sway` restarts the whole session.

## Android guest
Built on buildfarm (`~/aosp/PixelOS`, product `custom_nio_vm`). Package and disk tools are in
`pixelOS_16_VM/image/`. See `android-vm.md`.

## Commits
The repo is git. Sources and docs only; build outputs, firmware blobs, boot images and device dumps stay
local (`.gitignore`). Large or proprietary files never go into history.
