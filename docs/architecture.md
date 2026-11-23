# mothershipOS architecture

mothershipOS is **u-boot (as the hypervisor-partition image, EL2) → Linux host at EL2 (KVM/VHE) → guest VMs**.
The host owns all physical hardware. Guests get generic virtual devices and **handles** to real hardware that
mothershipOS lends and takes back live ([hardware-handles.md](hardware-handles.md)). The host userspace and the
guest images are the same on every phone. Only the port (a repository of its own, e.g.
[motorola_nio](https://github.com/MothershipOS/motorola_nio): kernel, boot chain, firmware, small rootfs bits)
differs.

- **Works:** the boot chain below; PixelOS and the Ubuntu desktop running at the same time with the layers in the
  table (exceptions below and in the contracts); the handle registry (`mship handles`) and a VM's page › Hardware built
  from it; lending and taking back every hardware group while a VM runs, including after a device service starts
  late or restarts.
- **Broken:** Wi-Fi crash recovery: when the QCA6390 firmware crashes (during a connection) Wi-Fi is gone for the host and the VMs until a reboot
  ([mship.wifi](contracts/mship.wifi-1.md)).
- **Needs work:** the items listed in each [contract](contracts/README.md) and [platform](compat.md#platforms) file;
  release 2026.1 waits for them ([compat.md](compat.md)). CDSP at EL2 is untested.
- **Not implemented:** broker-allocated vsock CIDs and sockets (they come from `vm.json` and the launcher); per-VM
  resource sliders beyond the CPU/RAM caps; call audio.

## Boot chain (nio)
`PBL → XBL → TZ → [hyp partition = u-boot, EL2] → Linux 6.18 at EL2 (KVM VHE)`.
QHEE (Qualcomm's hypervisor) and ABL/UEFI are not in the path. u-boot loads a FIT (kernel + DTB + initramfs) from
boot_b; the initramfs (`mship-init`) switches to a persistent Ubuntu 26.04 root (`mship-root`), with data on
`mship-data` (/var/lib/mothership). Details: the port's
[boot notes](https://github.com/MothershipOS/motorola_nio/blob/main/docs/boot.md).
Linux owns the apps SMMU, so VFIO/IOMMU isolation is the host's. Linux does what QHEE does for remote processors in
the stock chain (LPASS bring-up, SMMU identity mappings, CMD-DB/SMEM sharing)
([nio audio notes](https://github.com/MothershipOS/motorola_nio/blob/main/docs/audio.md)).

## Layers

| Layer | What |
|---|---|
| Host kernel | Linux 6.18 LTS + the port's patches, at EL2 |
| Host userspace | Ubuntu 26.04; `sway-mship` (sway with `lease_output`, `host/sway`); the touch UI `mship-shell`; `mothershipd` (root: VMs, hardware handover, power, settings). The shell has two places: Home (the VMs, each with a page for its hardware, performance, memory and update; the store) and Phone (the host) and the device services (`crates/`, installed by `mship-system install`) |
| VMM | crosvm for every VM (mothershipOS's build with virglrenderer and minigbm, `host/gpu`); Cuttlefish's prebuilt crosvm only for PixelOS's gfxstream fallback |
| GPU | virtio-gpu DRM native context (the guest's Mesa freedreno/turnip on the host's msm driver) + cross-domain buffer allocation ([mship.gpu](contracts/mship.gpu-1.md)) |
| Display | a DRM lease of a display to the VM's presenter (`mship-presenter`), guest buffers scanned out zero-copy: the panel for PixelOS, the USB-C external display for the Ubuntu desktop |
| Input | the touchscreen and buttons through the VM's `mship-inputd` (virtio-input), only while lent; the Ubuntu desktop uses the monitor's/dock's USB keyboard and mouse |
| Network | taps + dnsmasq + iptables NAT (`mship-system net`); shared internet (item `internet`); native Wi-Fi per band (`mship-wifi-bridge`) |
| Audio | PixelOS: one virtio-snd stream per phone device, routed by `mship-audio` ([mship.audio](contracts/mship.audio-1.md)); Ubuntu desktop: virtio-snd on the external display's audio |
| Other devices | Bluetooth, USB, battery, modem, GNSS, cameras, hardware video decode: one device service each (`crates/mship-devices`), see the [contracts](contracts/README.md) |
| Hardware handles | every lendable or shared device is a handle with one universal descriptor (direction, sharing, lend mode, group, contract and caps, control socket; `crates/mship-core/src/handle.rs`, schema 1): device services publish theirs in /run/mothership/handles/<service>.json, mothershipd's registry (`crates/mothershipd/src/handles.rs`) discovers them, lends groups from them and publishes the `handles` topic (`handles.list`, `mship handles`); handover and a VM's page › Hardware are built from it ([hardware-handles.md §2b](hardware-handles.md)). Every handle is hot-pluggable: a group can be lent or taken back while the VM runs |
| VM definitions | `/var/lib/mothership/vms/<vm>/vm.json` (manifest schema 1), installed by `mship-system vm-install` ([vm-packages.md](vm-packages.md)) |
| CPU / RAM | the VM pool: every VM gets all CPUs (vCPUs mirror the host's core types) and the pool's RAM; mothershipd weighs CPU (per-VM cgroups) and moves balloons to the VMs that are awake (`crates/mothershipd/src/pool.rs`); optional caps in Settings › Virtual machines |
| VM speed | against the host with the same binaries: CPU, memory, syscalls, GPU fill/draw rate, disk reads native; cross-vCPU wakeups 1.3x (big cores) to 1.8x (little), GPU submit+finish round trip 200 vs 80 us, fsync 1.5x. What it takes: guest RAM in huge pages (`--hugepages` + shmem THP `advise`, dist/rootfs tmpfiles), MSI-X through a GIC ITS (`--irqchip kernel[allow-vgic-its]`), guest `kpti=0`, device workers off the vCPUs' CPUs (crosvm patch 0012); all in `mship-launch` `pool_args` |
| Power | `mothershipd` collects power votes from the host and every VM, sets CPU/GPU floors, freezes sleeping VMs and suspends the phone ([mship.power](contracts/mship.power-1.md)) |

## Hardware ownership
- Every exclusive hardware group (display, buttons, Wi-Fi bands, internet, Bluetooth, USB, cellular, cameras,
  speaker, earpiece, headset, microphone, location, external display) has one assigned owner, mothershipOS or one
  VM, set in a VM's page › Hardware. `mothershipd` lends assigned groups automatically: display and buttons while the
  VM is shown, the rest while it runs. Shared handles (GPU, battery, sensors) go to every VM and have no owner.
- Holding the assist key for 3 s, or Show in the shell, moves the screen.
- `mothershipd` drives the device services over their control sockets; its API is
  `/run/mothership/mothershipd.sock` (CLI `mship`). The general model (handles, slots, revocation, lowest generic
  Linux level per device class) is [hardware-handles.md](hardware-handles.md).

## Known issues and risks
- **Remote processors at EL2**: ADSP and Venus run without QHEE/TrustZone help; CDSP is untested and may need the
  same treatment.
- Unverified: Android scrolling performance under native context is not measured; the gfxstream fallback path
  does ~35-45 fps ([android-vm.md](https://github.com/MothershipOS/pixelOS_16_VM/blob/main/docs/android-vm.md)).
