# Platform efi-virt-arm64 1.x

Runs an arm64 desktop Linux guest from a disk image (Ubuntu 26.04 desktop:
[ubuntu_26.04_desktop_VM](https://github.com/MothershipOS/ubuntu_26.04_desktop_VM)). The host implements it with
crosvm; the VMM is not part of the contract.

- **Works:** the Ubuntu desktop boots by direct kernel boot and runs with every device below (the state of each
  slot is in its contract).
- **Needs work:**
  - Networking is always on (NATed), not a lendable `mship.net-shared` slot.
  - Sound is a virtio-snd device played on the external display's audio (DisplayPort), not `mship.audio` handles.
  - The vsock CID comes from `vm.json` instead of being allocated.
- **Not implemented:** UEFI boot (AAVMF with a persistent variable store); `mship.touch` and `mship.keys`;
  `mship.gnss`, `mship.sensors` and `mship.modem` slots.

**Boot:** the VM's own kernel `boot.kernel` (from its package, [vm-packages.md](../vm-packages.md)), booted directly
with the disk `boot.disk`, root on its first partition (`/dev/vda1`, ext4). No initrd: the guest kernel has its
drivers built in. `boot.params` adds kernel parameters (debugging).

**Devices the guest can rely on:**
| Device | Detail |
|---|---|
| CPUs / memory | one vCPU per host CPU, mirroring its type (capacity, cluster: the guest schedules big.LITTLE); the pool's RAM, lent through a virtio-balloon (page reporting on). Optional caps: `resources.cpus` (the fastest CPUs kept), `resources.memory_mb` |
| disk | one virtio-blk device = `boot.disk` (discard supported) |
| GPU / display | virtio-gpu with 3D (`mship.gpu`; context types `drm` native context for the guest's Mesa, `virgl`/`virgl2` for the 2D boot splash and console, `cross-domain`), `mship.display`: no display until a monitor is connected, then one at its preferred mode (virtio-gpu display hotplug; removed when unplugged) |
| battery | `mship.battery`: one goldfish battery (device tree `google,goldfish-battery`); the guest kernel's `goldfish_battery` driver gives the power_supply devices `battery` and `ac` |
| USB | `mship.usb`: crosvm's xHCI; keyboard and mouse are the monitor's or dock's USB devices |
| video decode | the host's hardware decoder as a V4L2 device through virtio-media |
| cameras | `mship.camera`: virtio-media devices after the decoder's |
| network | one virtio-net device (tap, NATed) |
| vsock | one vsock device; CID from `vm.json` `"cid"` (default 4); `mship.power` on host port 7720 |
| console | virtio-console `hvc0` (the host keeps it as `serial.log`) |

**History:** 1.0 (2026.1) initial.

**Debugging:** `vm.json` `"env": {"NAME": "value"}` sets variables for the launcher, crosvm and the presenter
(e.g. `MSHIP_VM_DISPLAY_STATS=1`: per-second frame/flip/copy statistics in `display.log`; `MSHIP_VM_SOUND=0`,
`MSHIP_VM_VIDEO=0` leave out sound or the decoder).
