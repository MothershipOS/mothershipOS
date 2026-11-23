# Platform efi-virt-arm64 1.x

Runs an arm64 Linux guest from a disk image: a desktop on the external display (Ubuntu 26.04 desktop:
[ubuntu_26.04_desktop_VM](https://github.com/MothershipOS/ubuntu_26.04_desktop_VM)) or a phone OS on the panel
(`"output": "panel"`). The host implements it with
crosvm; the VMM is not part of the contract.

- **Works:** the Ubuntu desktop boots by direct kernel boot and runs with every device below (the state of each
  slot is in its contract).
- **Needs work:**
  - The panel layout (`"output": "panel"`) has no sound and no display power from the guest yet; no guest has been
    booted on it yet.
  - Sound is a virtio-snd device played on the external display's audio (DisplayPort), not `mship.audio` handles.
  - The vsock CID comes from `vm.json` instead of being allocated.
- **Not implemented:** UEFI boot (AAVMF with a persistent variable store); `mship.gnss`, `mship.sensors` and
  `mship.modem` slots.

**Boot:** the VM's own kernel `boot.kernel` (from its package, [vm-packages.md](../vm-packages.md)), booted directly
with the disk `boot.disk`. Kernel parameters: `root=/dev/vda1 rootfstype=ext4 rootwait console=hvc0 <boot.params>` (default
`ro quiet splash`; `boot.params` with its own `root=` replaces the first). No initrd: the guest kernel has its drivers built in.

**Screen (`output`):** `"external"` (default; or a connector name such as `"DP-1"`): the external display, below.
`"panel"` (or the panel's connector name): the phone's panel: one display at the panel's size and density (the
`display` handle's caps), shown while the VM holds the `display` item; multi-touch (`mship.touch`, "mship-touch")
and a keyboard device with the volume and power keys (`mship.keys`), both only while lent.

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
| network | one virtio-net card (`internet`, `crates/mship-core/src/vmnet.rs`): blank (no carrier) until the VM holds the shared internet (`mship.net-shared`: DHCP, NAT out of the phone's uplink) |
| vsock | one vsock device; CID from `vm.json` `"cid"` (default 4); `mship.power` on host port 7720 |
| console | virtio-console `hvc0` (the host keeps it as `serial.log`) |

**History:** 1.0 (2026.1) initial.

**Debugging:** `vm.json` `"env": {"NAME": "value"}` sets variables for the launcher, crosvm and the presenter
(e.g. `MSHIP_VM_DISPLAY_STATS=1`: per-second frame/flip/copy statistics in `display.log`; `MSHIP_VM_SOUND=0`,
`MSHIP_VM_VIDEO=0` leave out sound or the decoder).
