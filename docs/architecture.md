# mothershipOS architecture

mothershipOS is **u-boot (as the hypervisor-partition image, EL2) → Linux host at EL2 (KVM/VHE) → guest VMs**.
The host owns all physical hardware. Guests get generic virtual devices and **handles** to real hardware that
mothershipOS lends and takes back live ([hardware-handles.md](hardware-handles.md)). The host userspace and the
guest images are the same on every phone. Only the port (a repository of its own, e.g.
[motorola_nio](https://github.com/MothershipOS/motorola_nio): kernel, boot chain, firmware, small rootfs bits)
differs.

## Boot chain (nio)
`PBL → XBL → TZ → [hyp partition = u-boot, EL2] → Linux 6.18 at EL2 (KVM VHE)`.
QHEE (Qualcomm's hypervisor) and ABL/UEFI are not in the path. u-boot loads a FIT (kernel + DTB + initramfs) from
boot_b; the initramfs (`mship-init`) switches to a persistent Ubuntu 26.04 root (`mship-root`), with data on
`mship-data` (/var/lib/mothership). Details: the port's
[boot notes](https://github.com/MothershipOS/motorola_nio/blob/main/docs/boot.md).
Linux owns the apps SMMU, so VFIO/IOMMU isolation is in our hands. Things QHEE used to do for remote processors
(LPASS bring-up, SMMU identity mappings, CMD-DB/SMEM sharing) are done by Linux
([nio audio notes](https://github.com/MothershipOS/motorola_nio/blob/main/docs/audio.md)).

## Layers

| Layer | Now |
|---|---|
| Host kernel | Linux 6.18 LTS + the port's patches, at EL2 |
| Host userspace | Ubuntu 26.04; `sway-mship` (sway with `lease_output`, `host/sway`); the touch UI `mship-shell`; `mothershipd` (root: VMs, hardware handover, power, settings) and the device services (`crates/`, installed by `mship-system install`) |
| VMM | crosvm for every VM (our build with virglrenderer and minigbm, `host/gpu`); Cuttlefish's prebuilt crosvm only for PixelOS's gfxstream fallback |
| GPU | virtio-gpu DRM native context (the guest's Mesa freedreno/turnip on the host's msm driver) + cross-domain buffer allocation ([mship.gpu](contracts/mship.gpu-1.md)) |
| Display | a DRM lease of a display to the VM's presenter (`mship-presenter`), guest buffers scanned out zero-copy: the panel for PixelOS, the USB-C external display for the Ubuntu desktop |
| Input | the touchscreen and buttons through the VM's `mship-inputd` (virtio-input), only while lent; the Ubuntu desktop uses the monitor's/dock's USB keyboard and mouse |
| Network | taps + dnsmasq + iptables NAT (`mship-system net`); shared internet (item `internet`); native Wi-Fi per band (`mship-wifi-bridge`) |
| Audio | PixelOS: one virtio-snd stream per phone device, routed by `mship-audio` ([mship.audio](contracts/mship.audio-1.md)); Ubuntu desktop: virtio-snd on the external display's audio |
| Other devices | Bluetooth, USB, battery, modem, cameras, hardware video decode: one device service each (`crates/mship-devices`), see the [contracts](contracts/README.md) |
| VM definitions | `/var/lib/mothership/vms/<vm>/vm.json` (manifest schema 1), installed by `mship-system vm-install` ([vm-packages.md](vm-packages.md)) |
| CPU / RAM | the VM pool: every VM gets all CPUs (vCPUs mirror the host's core types) and the pool's RAM; mothershipd weighs CPU (per-VM cgroups) and moves balloons to the VMs that are awake (`crates/mothershipd/src/pool.rs`); optional caps in Settings › Virtual machines |
| VM speed | measured against the host with the same binaries (2026-11-19): CPU, memory, syscalls, GPU fill/draw rate, disk reads native; cross-vCPU wakeups 1.3x (big cores) to 1.8x (little), GPU submit+finish round trip 200 vs 80 us, fsync 1.5x. What it takes: guest RAM in huge pages (`--hugepages` + shmem THP `advise`, dist/rootfs tmpfiles), MSI-X through a GIC ITS (`--irqchip kernel[allow-vgic-its]`), guest `kpti=0`, device workers off the vCPUs' CPUs (crosvm patch 0012); all in `mship-launch` `pool_args` |
| Power | `mothershipd` collects power votes from the host and every VM, sets CPU/GPU floors, freezes sleeping VMs and suspends the phone ([mship.power](contracts/mship.power-1.md)) |

## Hardware ownership
- Every hardware item (display, buttons, Wi-Fi bands, internet, Bluetooth, USB, cellular, cameras, speaker,
  earpiece, headset, microphone, external display) has one assigned owner, mothershipOS or one VM, set in
  Settings › Hardware. `mothershipd` lends assigned items automatically: display and buttons while the VM is shown,
  the rest while it runs.
- The assist key, holding both volume buttons, or Show/Take back in the shell move the screen.
- `mothershipd` drives the device services over their control sockets; its API is
  `/run/mothership/mothershipd.sock` (CLI `mship`). The general model (handles, slots, revocation, lowest generic
  Linux level per device class) is [hardware-handles.md](hardware-handles.md).

## Not done yet
- The broker's handle registry (descriptors, caps, `list-handles`) and broker-allocated vsock CIDs.
- Per-VM resource sliders beyond the CPU/RAM caps.
- The contract gaps listed in each [contract](contracts/README.md) and [platform](compat.md#platforms) file; release
  2026.1 waits for them ([compat.md](compat.md)).

## Known issues and risks
- **Remote processors at EL2**: ADSP and Venus run without QHEE/TrustZone help; CDSP is untested and may need the
  same treatment.
- Open / unverified: Android scrolling performance under native context has not been re-measured; the old
  gfxstream path did ~35-45 fps ([android-vm.md](https://github.com/MothershipOS/pixelOS_16_VM/blob/main/docs/android-vm.md)).
