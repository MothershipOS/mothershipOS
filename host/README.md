# host/: upstream software the host builds

The host's own programs are the Rust crates in `crates/`; their units, udev rules and configuration are the
`dist/rootfs/` overlay. `host/` holds the pinned upstream sources (`source.json`: git revision or crate version,
sha256) and our patches for the C/C++/Rust projects the phone runs. `cargo xtask` builds each, and `cargo xtask
dist` packs them with the programs into the release tarball (`mship-system install <tar>` on the phone).

| Dir | Built by | Installs to | What |
|---|---|---|---|
| `gpu/` | `cargo xtask gpu` | `/opt/mship-gpu/bin/crosvm`, `/opt/mship-gpu/lib/libvirglrenderer.so.1` | the VMM: crosvm with virglrenderer (DRM native context for msm) and a static minigbm. Patches: PSCI system suspend (`vm.sleep` to mothershipd), xHCI events while halted, an aarch64 power button (`crosvm powerbtn`), battery and display power plumbing; rutabaga_gfx and virtio-media fixes |
| `sway/` | `cargo xtask sway` | `/usr/local/bin/sway-mship`, `/usr/local/lib/mothership-wlroots/libwlroots-0.19.so` | the session compositor: sway 1.11 + `lease_output <output> on/off` (DRM lease offer/revoke), on its own wlroots 0.19 |
| `vm-sound/` | `cargo xtask vhost-sound` | `/usr/local/bin/vhost-device-sound` | the VMs' sound backend (PipeWire) |

## The programs (crates/)

| Crate | On the phone | What |
|---|---|---|
| `mothershipd` | `/usr/local/sbin/mothershipd` + `mothershipd.service` | the host daemon, owner of the live state: VMs, hardware handover, mship.power votes, VM sleep/wake and display power, CPU guard, host status and settings; API and events on `/run/mothership/mothershipd.sock` |
| `mship` | `/usr/local/bin/mship` | command line client of mothershipd (VMs, handover, screen, power) |
| `mship-shell` | `/usr/local/bin/mship-shell` | the touch UI (GTK 4 + libadwaita) |
| `mship-launch` | `/usr/local/bin/mship-launch` | runs a VM (crosvm) and its helpers from its `vm.json`; stop: the guest's power button, then `crosvm stop` |
| `mship-presenter` | `/usr/local/bin/mship-presenter` | a VM on a display through a DRM lease, zero-copy page flips, follows the guest's display power |
| `mship-inputd` | `/usr/local/bin/mship-inputd` | lends the touchscreen and buttons to a VM (virtio-input) |
| `mship-devices` | `mship-battery`, `mship-bt-bridge`, `mship-usb`, `mship-wifi-bridge` (+ services) | the phone's battery, Bluetooth controller, USB devices and Wi-Fi bands for VMs |
| `mship-system` | `/usr/local/sbin/mship-system` | system jobs: VM networking, RTC, time sync, power key re-arm, GPU eviction off, USB gadget/Type-C, boot counter, FIT install, `setup`, `vm-install` (a VM from its repository or package), `install` |
| `mship-init` | `/init` of the initramfs (`cargo xtask initramfs`) | USB dev link, then the switch to mship-root (or the RAM host) |
| `mship-egl-shim`, `mship-gles-shim` | `/opt/crosvm/bin/libEGL.so`, `libGLESv2.so` | Cuttlefish crosvm/gfxstream (fallback GPU path): blob-cache hide, zero-copy scanout |
| `mship-dev` | on the PC | developer tools: shell over the dev link, proxy, console, FIT deploy, EDL, UART monitor, NAS backups |

VM definitions live on the phone in `/var/lib/mothership/vms/<vm>/vm.json` (from `vms/*/vm.json`). Design and
roadmap: `docs/architecture.md`, `docs/hardware-handles.md`.
