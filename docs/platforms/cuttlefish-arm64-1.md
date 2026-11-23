# Platform cuttlefish-arm64 1.x

Runs an arm64 Cuttlefish-style Android guest (AOSP `vsoc_arm64` derived) with direct kernel boot. The host
implements it with crosvm; the VMM is not part of the contract.

- **Works:** PixelOS boots and runs with every device below (the state of each slot is in its contract).
- **Needs work:**
  - The bootconfig keys are written into the package's initrd by the VM's build (`pixelOS_16_VM`: `cargo xtask
    android-initrd`) instead of being set by the host at every start, so a host change doesn't reach existing
    packages.
  - The vsock CID comes from `vm.json` `"cid"` (default 3) instead of being allocated.
  - eth1 has DHCP and forwarding chains; V1 defines it as never connected.

**Boot:** manifest `boot.kernel` (arm64 Image), `boot.initrd` (with an appended bootconfig), `boot.cmdline`,
`boot.disk`.
- The host adds or overrides these kernel cmdline parameters: `console=hvc0 loglevel=5`.
- The host sets these bootconfig keys (the guest must not rely on their values staying identical across
  minors, only on their presence and meaning): `androidboot.boot_devices`, `androidboot.console=hvc1`,
  `androidboot.hypervisor.version`, `androidboot.hardware.egl=emulation`, `androidboot.hardware.vulkan`,
  `androidboot.cpuvulkan.version`, `androidboot.hardware.gltransport=virtio-gpu-asg`,
  `androidboot.opengles.version`, `androidboot.hardware.hwcomposer.mode=client`,
  `androidboot.vsock_lights_port`/`_cid`, `androidboot.modem_simulator_ports` (the `modem` slot's vsock port).

**Devices the guest can rely on:**
| Device | Detail |
|---|---|
| CPUs / memory | one vCPU per host CPU, mirroring its type (capacity, cluster: the guest schedules big.LITTLE); the pool's RAM, lent through a virtio-balloon (page reporting on). Optional caps: `resources.cpus` (the fastest CPUs kept), `resources.memory_mb` |
| disk | one virtio-blk device = `boot.disk` (GPT with the Cuttlefish partitions the guest expects) |
| consoles | 20 virtio-console ports in Cuttlefish order: 0 kernel console (read by the host), 1 Android shell console, 2 logcat, 5 = `bt` slot (`mship.bt`, H4), 6 = `gnss` slot (`mship.gnss`, NMEA; `/dev/gnss0`), `mship.sensors` (goldfish sensors protocol): 13 = one console (Android 16), 18 control + 19 data (Android 17), the others present and drained |
| serial | one 8250 UART used as earlycon |
| vsock | one vsock device; the host allocates the CID. Port 5555 (adb) is the guest's; host-side services the guest expects (lights) are answered or drained; host port 9600 = `modem` slot (`mship.modem`, AT); host port 9700 = `fingerprint` slot (`mship.fingerprint`, length-prefixed JSON) |
| network | four virtio-net cards (`crates/mship-core/src/vmnet.rs`), blank until their handle is attached, at fixed PCI addresses 00:1c.0 to 00:1f.0 (the guest kernel names them by PCI address), in order: eth0 = data path of the `modem` slot (mobile data; never connected without it), eth1 (ethernet, never connected), eth2 = `internet` slot, eth3 = data path of the `wifi` slot. Without a `mship.wifi` slot, eth2 is wrapped by virt_wifi as `wlan0` (Cuttlefish default). With one, `mship_wifi` on eth3 is `wlan0` and eth2 stays ethernet |
| battery | `mship.battery`: one goldfish battery (device tree `google,goldfish-battery`, at MMIO 0x20e9000 with the current crosvm and device set) |
| USB | `mship.usb`: crosvm's xHCI controller on PCI (root hubs `usb1`/`usb2`); devices hotplugged by the host |
| GPU / display | `mship.gpu`, `mship.display` |
| input | `mship.touch`, `mship.keys` |

**History:** 1.0 (2026.1) initial.
