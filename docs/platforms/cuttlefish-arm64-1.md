# Platform cuttlefish-arm64 1.x

Runs an arm64 Cuttlefish-style Android guest (AOSP `vsoc_arm64` derived) with direct kernel boot. The host
implements it with crosvm; the VMM is not part of the contract.

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
| consoles | 18 virtio-console ports in Cuttlefish order: 0 kernel console (read by the host), 1 Android shell console, 2 logcat, 5 = `bt` slot (`mship.bt`, H4), the others present and drained |
| serial | one 8250 UART used as earlycon |
| vsock | one vsock device; the host allocates the CID. Port 5555 (adb) is the guest's; host-side services the guest expects (lights) are answered or drained; host port 9600 = `modem` slot (`mship.modem`, AT) |
| network | four virtio-net devices in order: eth0 = data path of the `modem` slot (mobile data; never connected without it), eth1 (ethernet, never connected), eth2 = `internet` slot, eth3 = data path of the `wifi` slot. Without a `mship.wifi` slot, eth2 is wrapped by virt_wifi as `wlan0` (Cuttlefish default). With one, `mship_wifi` on eth3 is `wlan0` and eth2 stays ethernet |
| battery | `mship.battery`: one goldfish battery (device tree `google,goldfish-battery`, today at MMIO 0x20e9000) |
| USB | `mship.usb`: crosvm's xHCI controller on PCI (root hubs `usb1`/`usb2`); devices hotplugged by the host |
| GPU / display | `mship.gpu`, `mship.display` |
| input | `mship.touch`, `mship.keys` |

**History:** 1.0 (2026.1) initial.

**Gaps in the current implementation (fix before 2026.1):**
- The bootconfig keys are written into the package's initrd once (`cargo xtask android-initrd`) instead of being set
  by the host at every start, so a host change wouldn't reach existing packages.
- The vsock CID is fixed (3) instead of allocated.
- eth1 has DHCP and forwarding chains; V1 defines it as never connected.
- eth3 and the `mship_wifi` wlan0 assignment don't exist yet.
