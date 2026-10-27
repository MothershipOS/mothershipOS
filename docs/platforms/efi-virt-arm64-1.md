# Platform efi-virt-arm64 1.x

Runs an arm64 guest that boots by UEFI from a disk image (Ubuntu 26.04 desktop: `ubuntu_26.04_desktop_VM/`;
Ubuntu Touch). The host implements it with crosvm today (QEMU's `virt` machine before); the VMM is not part of the
contract.

**Boot:** the VM's own kernel `boot.kernel` (from its package, docs/vm-packages.md), booted directly with the disk
`boot.disk` (root on its first partition), as mothershipOS does today; or UEFI (AAVMF, no secure boot) with a
persistent variable store `boot.efi_vars` (created from the firmware's template if missing) and the disk
`boot.disk`.

**Devices the guest can rely on:**
| Device | Detail |
|---|---|
| CPUs / memory | one vCPU per host CPU, mirroring its type (capacity, cluster: the guest schedules big.LITTLE); the pool's RAM, lent through a virtio-balloon (page reporting on). Optional caps: `resources.cpus` (the fastest CPUs kept), `resources.memory_mb` |
| disk | one virtio-blk device = `boot.disk` (discard supported) |
| GPU / display | virtio-gpu PCI with 3D (`mship.gpu`; context types `drm` native context for the guest's Mesa, `virgl`/`virgl2` for 2D boot console and splash, `cross-domain`), `mship.display`: no display until a monitor is connected, then one at its preferred mode (virtio-gpu display hotplug; removed when unplugged) |
| battery | `mship.battery`: one goldfish battery (device tree `google,goldfish-battery`); the guest kernel's `goldfish_battery` driver gives the power_supply devices `battery` and `ac` |
| input | `mship.touch`; a virtio keyboard and mouse may be present |
| network | one virtio-net device = `wifi` slot (`mship.net-shared`) |
| serial | one PL011 UART (the host keeps a log) |

**History:** 1.0 (2026.1) initial.

**Gaps in the current implementation (fix before 2026.1):**
- Display and touch go through a QEMU window in the host compositor, not through handles (see the contract
  gaps).
- Networking is QEMU user mode (always on, host port forwards for ssh/adb), not a lendable slot.
- An HDA sound card is present, which V1 doesn't define (it must be removed or become `mship.audio`).

**Debugging:** `vm.json` `"env": {"NAME": "value"}` sets variables for the launcher, crosvm and the presenter
(e.g. `MSHIP_VM_DISPLAY_STATS=1`: per-second frame/flip/copy statistics in `display.log`).
