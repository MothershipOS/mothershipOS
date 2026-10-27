# mothershipOS architecture (updated 2026-10-12)

mothershipOS is **u-boot (as the hypervisor-partition image, EL2) → Linux host at EL2 (KVM/VHE) → guest VMs**.
The host owns all physical hardware. Guests get generic virtual devices and **handles** to real hardware that
mothershipOS lends and takes back live (`hardware-handles.md`). The host userspace and the guest images are
the same on every phone. Only the port (`ports/<phone>/`: kernel, boot chain, firmware, small rootfs bits)
differs.

## Boot chain (nio, achieved)
`PBL → XBL → TZ → [hyp partition = u-boot, EL2] → Linux 6.18 at EL2 (KVM VHE)`.
QHEE (Qualcomm's hypervisor) and ABL/UEFI are no longer in the path. u-boot loads a FIT (kernel + DTB +
initramfs) from UFS; the initramfs switches to a persistent Ubuntu 26.04 root (`mship-root`), with data
on `mship-data` (/var/lib/mothership). Details: `motorola_nio/docs/boot.md`.
Linux owns the apps SMMU, so VFIO/IOMMU isolation is in our hands. Things QHEE used to do for remote
processors (LPASS bring-up, SMMU identity mappings, CMD-DB/SMEM sharing) are done by Linux now
(`motorola_nio/docs/audio.md`).

## Layers (current → target)

| Layer | Now | Target |
|---|---|---|
| Host kernel | Linux 6.18 LTS + nio patches at EL2 | same, patches upstreamed where possible |
| Host userspace | Ubuntu 26.04, sway (patched: `lease_output`), mothership-shell, `mship-*` services (`host/`) | + `mothershipd` broker |
| VMM | crosvm (Android: upstream build with minigbm gralloc, `host/gpu`; Cuttlefish's prebuilt for the gfxstream fallback); QEMU (Ubuntu Touch) | crosvm for all guests |
| GPU sharing | Android: virtio-gpu DRM native context (msm, guest Mesa) + cross-domain gralloc; virgl (Ubuntu Touch) | native context for all guests |
| Display | Android: DRM lease of the panel to a per-VM presenter (`host/vm-display`), guest buffers scanned out zero-copy. Ubuntu Touch: sway window | every VM through display handles |
| Input | evdev → virtio-input via `mship-vm-inputd` (touch, volume keys, power key), lent by `mship-handover` | input handles |
| Network | taps + dnsmasq + iptables NAT (`host/vm-net`); Wi-Fi item lends internet | native Wi-Fi via cfg80211 proxy + L2 redirect |
| Audio | host PipeWire only | virtio-snd ⇄ ALSA PCM handles |
| VM definitions | `/var/lib/mothership/vms/<vm>/vm.json` (name, launcher, handover checklist, display mode) | same, owned by `mothershipd` |
| CPU / RAM policy | the VM pool (2026-10-27): every VM gets all CPUs (vCPUs mirroring the host's types) and the pool's RAM; mothershipd weighs CPU (per-VM cgroups) and moves balloons by which VMs are awake (`crates/mothershipd/src/pool.rs`); optional caps in Settings › Virtual machines | the same, with the guests' working-set reports |

## Hardware ownership
- `mship-handover` (user mship) is today's broker for display, buttons and Wi-Fi. The assist key, holding both
  volume buttons, or Show/Take back in the shell move the screen. Wi-Fi stays lent until it is taken back
  explicitly or the VM stops.
- `mship-admin` (root via sudo) performs the privileged parts: Wi-Fi ownership/NAT, VM stop, CPU, power,
  Bluetooth, SSH, region.
- The general model (handles, slots, revocation, lowest generic Linux level per device class) is
  `hardware-handles.md`. `mothershipd` will replace `mship-handover`/`mship-admin owner-set`.

## Target scenario
- Android owns the internal panel and touchscreen. Ubuntu runs on a USB-C dock (DP alt-mode output +
  dock USB devices attached to its virtual xHCI). Hardware moves between VMs and the host live.
- CPU, GPU and RAM are shared and adjustable per VM from the UI.

## Milestones

| ID | Goal | State |
|---|---|---|
| A1 | Real guest under KVM | ✅ kvmtool guest (2026-10-08) |
| A2 | Persistent host root on UFS + dev transport | ✅ (2026-10-08) |
| A3 | crosvm on the host | ✅ for Android (2026-10-12) and Ubuntu; balloon + VM pool (2026-10-27) |
| A4 | Host display + GPU + touch + compositor | ✅ (2026-10-11) |
| A5 | GPU native context | ✅ (2026-10-13): Android on Mesa freedreno/turnip via virtio-gpu `drm` contexts |
| A6 | Android VM with GPU, input, display on the panel | ✅ (2026-10-12): DRM lease + zero copy (UBWC, native context since 2026-10-13) |
| A7 | Type-C dock routing (DP + USB to a VM) | ✅ basics (2026-10-18): TCPM dual role + DP alt mode; hardware item `display-ext` leases DP-1 to a VM's presenter (Ubuntu desktop), `usb` passes the monitor's/dock's devices; Android keeps the panel, mothershipOS never uses DP-1 |
| A8 | `mothershipd` + UI | partial: shell has a VM list + per-VM hardware checklist; no broker, no resource sliders |
| A9 | Hardware handles (`hardware-handles.md`) | spec written; broker, native Wi-Fi, audio next |

## Known issues and risks
- **Android display performance**: ~35-45 fps while scrolling. SurfaceFlinger GPU composition through
  gfxstream costs ~12 ms per frame. See `android-vm.md`.
- **Speaker protection**: the AW882xx amp runs at a fixed -36 dB. The SKT feedback path fails at
  DEVICE_START. Don't raise gain before it works.
- **Remote processors at EL2**: ADSP solved; CDSP, modem and video PIL are untested and probably need the
  same QHEE-replacement treatment.
- **Heterogeneous cores**: guests see one MIDR while vCPUs may float across A55/A77; launchers pin vCPUs to
  the big cores.
- **Build host disk space** (~1 GB free): Android/kernel builds run on a separate build machine (the buildfarm).
