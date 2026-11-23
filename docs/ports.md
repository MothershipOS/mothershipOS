# Phone ports

mothershipOS has no phone in it. Everything specific to one phone lives in its **port**: a repository next to
mothershipOS with the kernel patches and configuration, the boot image, firmware, the phone's files for the host
root, and the phone's docs. At run time mothershipOS finds the phone's parts on the hardware (`mship_core::device`);
the little it can't find comes from the port's `/etc/mothership/device.json`.

| Port | Phone | Boot | State |
|---|---|---|---|
| [motorola_nio](https://github.com/MothershipOS/motorola_nio) | Motorola nio (SM8250) | u-boot in hyp_b, FIT on boot_b, Linux at EL2 | Works, with VMs |
| xiaomi_polaris | Xiaomi Mi MIX 2S (SDM845) | Xiaomi ABL, Android boot image (`fastboot boot`), Linux at EL1 | RAM host with the panel; no persistent root; no EL2, so no VMs |

## What a phone needs

- **An unlockable bootloader** that boots your own kernel (an Android boot image, or a chain like the nio's).
- **Linux at EL2 for VMs.** mothershipOS's VMs are KVM VMs. Qualcomm phones normally run their hypervisor at EL2 and
  start Linux at EL1. The nio runs u-boot in place of that hypervisor, which only works because that unit doesn't
  enforce secure boot. Without EL2 the host runs (display, shell, Wi-Fi, Bluetooth) but no VM starts, and
  Settings › System says so.
- **Upstream (or near upstream) kernel support** for the SoC and board: mothershipOS builds a pinned LTS kernel with
  the port's patches; there's no vendor kernel.
- **A way back** when an image doesn't boot: an A/B slot left stock, `fastboot boot` from RAM, or an EDL kit
  (`mship-dev edl` with the phone's Firehose programmer). Xiaomi's EDL needs an authorised account: on such phones
  only `boot` (and `userdata`) may ever be written.
- **Whether the hyp can be replaced:** `mship-dev edl --info` (EDL mode, read only, no programmer) prints the OEM PK
  hash the boot ROM has fused. All zeros: secure boot not enforced, the nio's route is open. Anything else: the hyp
  must be signed by the OEM, and Linux stays at EL1 unless there's an exploit.

## Building a port

`cargo xtask` and `mship-dev` act on the port named by `MSHIP_PORT` (its directory name), default `motorola_nio`:

```
cargo xtask boot-image                                # the nio: target/boot/nio-el2.itb
MSHIP_PORT=xiaomi_polaris cargo xtask boot-image      # target/xiaomi_polaris/boot/polaris-boot.img
MSHIP_PORT=xiaomi_polaris cargo xtask port-firmware   # the root's firmware, fetched and checked
MSHIP_PORT=xiaomi_polaris cargo xtask dist            # the release (with the port's rootfs and firmware)
```

The default port builds in `target/` (the paths `mship-dev deploy` and the nio's docs use), every other port in
`target/<port>/`.

## The port repository

| Path | What |
|---|---|
| `port.json` | what the build needs (below) |
| `kernel/sources.lock.json` | the pinned kernel source: `linux.version`, `url`, `sha256` |
| `kernel/patches/*.patch` | applied in order to the pristine source; the port's own numbering |
| `kernel/configs/<x>.config` | config requests on top of the arm64 defconfig trimmed to Qualcomm; every request is checked after `olddefconfig` (an unmet one fails the build) |
| `kernel/configs/<x>.defconfig` | the resolved savedefconfig, for review (a difference is reported, not an error) |
| `boot/initramfs.json` | busybox (pinned) and the firmware needed before the root: `{path, url or local, sha256}` |
| `boot/splash.png` | the boot logo (at most 224 colors; mothershipOS's logo unless the port has a reason) |
| `firmware/firmware.json` | optional: `{files: [{path: "lib/firmware/...", url, sha256}]}`, the root's firmware, shipped by `dist`. Proprietary blobs: fetched at build time, never committed, and such a release is not for publishing |
| `rootfs/` | files for the host root, installed after mothershipOS's `dist/rootfs`: `etc/mothership/device.json`, the device services' configs (below), modules-load, modprobe, UCM, WirePlumber rules, workarounds as scripts with their units |
| `docs/`, `README.md` | the phone's state per subsystem (Works / Needs work / Broken / Not implemented) |
| `tools/`, `unbrick/` | the phone's own procedures (first contact, EDL layouts), on top of `mship-dev` |

### port.json

```json
{
  "name": "Xiaomi Mi MIX 2S", "soc": "SDM845",
  "kernel": { "fragment": "kernel/configs/polaris-host.config", "defconfig": "kernel/configs/polaris-host.defconfig", "build_host": "polaris-build" },
  "boot": { "format": "android", "image": "polaris-boot.img", "max_size": 67108864, "...": "..." },
  "firmware": "firmware/firmware.json"
}
```

- `boot.format: "fit"` (u-boot): `its` (includes `Image`, `initramfs.cpio.gz` and the `dtb` name), `dts` (the board
  DTS, compiled with the port's `boot/` as include path; its Bluetooth address placeholder is replaced from the
  untracked `boot/bd-address`), `dtb`.
- `boot.format: "android"` (the phone's own bootloader): `header_version` (0), `dtb` (an in-tree DTB such as
  `qcom/sdm845-xiaomi-polaris.dtb`, appended to the gzip kernel), `base`, `kernel_offset`, `ramdisk_offset`,
  `tags_offset`, `page_size` (numbers or "0x..." strings), `cmdline`. Take them from the stock boot image.
- `max_size`: the boot partition's size; a larger image fails the build.
- `abl_dtb` (the nio): `cargo xtask abl-dtb`'s board DTS and its captured board facts.

### device.json (`/etc/mothership/device.json`, from the port's rootfs)

| Key | Default | What |
|---|---|---|
| `panel.refresh_hz` | 60 | the panel's refresh rate (VMs render at it) |
| `panel.dpi` | 400 | the panel's density (the display handle's caps, the VMs' display) |
| `panel.scale` | 2 | the host session's scale |
| `panel.top_inset` | 0 | px kept free over a camera cutout (the terminals' top gap) |
| `home_key` | 217 (KEY_SEARCH) | the key code that, held 3 s, switches the screen between mothershipOS and the VM; a phone without a spare key: 0 (none) |
| `backlight` | the first `/sys/class/backlight` entry | the panel's backlight |
| `boot.partition` | none | where `mship-system boot-image` / `mship-dev deploy` write the boot image (the nio: `boot_b`); absent: the phone boots its image another way |
| `dp_audio` | none | DisplayPort audio for a VM on the external display: `card` (ALSA id), `device`, `mixer` (`amixer cset` routes) |
| `modem.firmware` | `/lib/firmware/mothership/modem` | the files an external modem reads over TFTP (mship-modem) |
| `video.decoder`, `video.encoder` | the V4L2 device named `*-decoder` / `*-encoder` | the hardware codec for VMs |

## What mothershipOS finds on the hardware

`mship_core::device` (crates/mship-core/src/device.rs). A port's kernel must make these visible the usual way:

| Part | Found as |
|---|---|
| panel | the connected DSI / eDP / DPI / LVDS DRM connector; its size from its mode; its card is the KMS device for sway |
| external display | the other connected connectors (lent to VMs, disabled for sway) |
| touchscreen | the input device with multi-touch positions and `INPUT_PROP_DIRECT` |
| volume keys | input devices with KEY_VOLUMEUP/DOWN and no axes or switches (a headset jack also reports volume keys) |
| power key, wake gesture | input devices with KEY_POWER or KEY_WAKEUP and no volume keys |
| CPU clusters | cpufreq policies; core names from each cluster's MIDR; the CPU modes' caps from each cluster's frequency table |
| GPU | the devfreq node named `*.gpu`; its name from the DT compatible (`qcom,adreno-650.2` → Adreno 650) |
| battery, charger | power supplies of type `Battery` and `USB` (a USB-C port's `tcpm-source` excluded) |
| Wi-Fi | the first wireless interface (`phy80211`) that isn't a band interface (`mshw*`); its radio from the interface |
| model | the DT `model` (Settings › System) |
| KVM | `/dev/kvm` |

The session's phone-specific sway lines are written at start (`mship-system session-setup`:
`/run/mothership/sway/device.conf` and `WLR_DRM_DEVICES`), so the sway config names no connector.

## The device services' configs (in the port's rootfs)

Each device service reads its file from `/etc/mothership/`; without one it serves nothing or uses its defaults.

| File | Service | What |
|---|---|---|
| `audio.json` | mship-audio | the PipeWire nodes of speaker, earpiece, headphones, mics |
| `camera.json` | mship-camera | per camera (DT path): its lights and ranger |
| `sensors.json` | mship-sensors | the sensor DSP (Qualcomm SEE): remoteproc, fastrpc device, its partitions, registry overrides |
| `gnss.json` | mship-gnss | the GNSS engine |
| `wifi-bridge.json` | mship-wifi-bridge | band interfaces (default `mshw2g`/`mshw5g` on the host station's radio) |
| `bt-bridge.json`, `usb.json`, `modem.json` | mship-bt-bridge, mship-usb, mship-modem | their controllers |

## Porting checklist

What it takes, with each port's state. Commands from mothershipOS unless noted.

| Step | How | nio | polaris |
|---|---|---|---|
| 1. Kernel source and board DT | pin the LTS (`sources.lock.json`); upstream board DTS if there is one, else write one (`boot/` for a FIT, `kernel/patches` for an in-tree DTS) | own DTS on 6.18 | upstream `sdm845-xiaomi-polaris.dts` |
| 2. Missing drivers | patches, the port's numbering; carry mothershipOS's generic ones (GPU render node separate from KMS, mship-vcam, boot logo) | 30 patches | panel NT35596S (from sdm845-mainline), panel power/mode pin, WLED and DPU handover from ABL + the 3 generic ones |
| 3. Kernel config | the fragment: mothershipOS's requirements (KVM, cgroups, vhost/vsock, netfilter legacy, f2fs, udmabuf, NCM gadget, zstd modules, logo) + the SoC/board drivers built in for the root disk, the dev link and the panel | done | done (checked; `polaris-host.defconfig`) |
| 4. Boot image | `port.json boot`, `cargo xtask boot-image`; check the size against the partition | FIT | Android boot image v0, 29.9 MB |
| 5. Early firmware | `boot/initramfs.json` (GPU, anything the built-in drivers load before the root) | local blobs | GPU (linux-firmware + polaris zap), pinned |
| 6. Root firmware | `firmware/firmware.json` + `cargo xtask port-firmware`, or placed by hand | by hand | 22 files, pinned |
| 7. First boot | RAM boot or a spare slot; the dev link comes up (USB NCM 10.42.0.2, telnet 2323); `mship-init` stays in the RAM host without `mship-root` | done | done: RAM host, panel with logo and console |
| 8. EL2 | a way to start Linux at EL2 (replace the hypervisor where secure boot isn't enforced, Secure Launch on Windows devices); `mship-dev edl --info` shows which | u-boot in hyp_b | closed: secure boot enforced (Xiaomi key fused) |
| 9. Persistent root | a partition named `mship-root` (ext4), data on `mship-data`; `mship-system install <release>` | done | open: which partition |
| 10. device.json | the keys above | done | done (no home key: no spare button) |
| 11. Device services | their configs; UCM and WirePlumber for audio; the DSPs' firmware | done | audio, sensors, cameras not in the board DT |
| 12. Workarounds | as scripts + units in the port's rootfs, never in mothershipOS | power key re-arm | none yet |
| 13. Docs | README state table, a doc per subsystem | done | done |

## A bootloader that hands over a running display

Qualcomm's ABL shows its splash and leaves the display running into Linux (the nio's u-boot chain doesn't). On
the Mi MIX 2S every part of that hand-over needed handling, in the port; check each on a new ABL phone:

- **The timing engine keeps streaming.** Mainline DPU doesn't stop an INTF the bootloader started: the DSI host then
  sends the panel's init commands one per frame into the video stream, and the panel stays dark until the second
  modeset. Sign: `dsi_cmds2buf_tx` ~16 ms apart with `drm.debug=0x6`, the INTF's frame counter running before the
  first modeset, SMMU faults on the splash address. (polaris patch 0007)
- **The panel keeps its state.** A reset alone may not restart it; pins read at power-up (a mode select) only count
  after a power cycle. (polaris patch 0002)
- **Pins the vendor driver sets that the mainline DT lacks:** compare the stock display node
  (`/proc/device-tree/soc/qcom,dsi-display@*` under Android: `qcom,panel-mode-gpio`, enable GPIOs). (polaris 0005)
- **The backlight keeps the bootloader's setup.** `qcom-wled` skips its setup when the bootloader enabled the same
  strings, CABC included. Sign: `wled_ovp_irq` counting, fault register 0x08 = 0x02. (polaris 0005, 0006)
- **Panel commands before the DSI host is up** fail with -22: the panel driver needs `prepare_prev_first`.
- Panel facts from the running stock kernel beat any table: the init sequence (`qcom,mdss-dsi-on-command`), supply
  voltages (`qpnp-labibb`), WLED settings — the unit's own `/proc/device-tree`.

## VM images

The VM images are the same on every phone: no phone's drivers, names or panel in a guest. The guest takes the
display's size from virtio-gpu and its density from the host: PixelOS from `androidboot.lcd_density` (mship-launch,
the panel's `dpi`), Linux guests from the display's EDID (crosvm derives its millimetres from that density; Sailfish
computes Silica's pixel ratio from it at boot). The refresh rate is the display's one mode. adb into PixelOS is over
vsock only, without a host key.
