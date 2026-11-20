# Hardware handles

**Goal:** VM images are generic and get distributed. mothershipOS is built per phone and updated over time. A VM
gets real hardware by attaching to **handles** that mothershipOS publishes. A VM built for an older mothershipOS
keeps working on a newer one.

**V1 is the baseline.** Compatibility starts with V1: manifest schema 1, every contract and platform at 1.0, first
shipped in mothershipOS 2026.1. VM directories without a schema-1 manifest, and per-VM launcher scripts, are not
covered by any compatibility promise.

Handles bridge at the **lowest generic Linux level**: the first kernel interface above the phone-specific
driver that is the same on every phone. Drivers, firmware, power and calibration stay on the host; everything
above that level runs natively in the guest. Pass the kernel UAPI object (fd, netdev, KMS object, frame
stream) where possible, never a userspace daemon's API (PipeWire, NetworkManager, BlueZ), never phone-specific
registers or firmware.

## 1. Terms

| Term | Meaning |
|---|---|
| **Handle** | One piece of real hardware at a stable generic level: `display:panel0`, `touch:panel0`, `keys:volume`, `keys:power`, `wifi:2g`, `wifi:5g`, `bt:hci0`, `audio:speaker`. It has a class, `caps` (per-phone facts: resolution, refresh rate, formats…) and an owner (`host` or a VM) |
| **Class** | The kind of handle. Each class has one **slot contract** per major version |
| **Slot** | The virtual device a VM boots with for a class (virtio-gpu scanout, virtio-input, a vsock channel…). It always exists. Without a handle attached it is **empty**: no events, no carrier, panel off, radio off |
| **Contract** | A versioned interface between mothershipOS and VMs (§3). Everything a VM depends on is a contract; everything else is a host implementation detail |
| **VM package** | What gets distributed: disk images + kernel/initrd + a **manifest**. **No host code**: launchers, presenters and bridges belong to mothershipOS |
| **Platform** | A host-implemented recipe that turns a manifest into a running VMM (`cuttlefish-arm64`, `efi-virt-arm64`). Versioned like a contract |
| **Broker** | `mothershipd` (root). Owns all handles, enforces policy, attaches and detaches handles, launches VMs from manifests (§6) |

## 2. Attach model
- **Attach / detach** connects a handle's backend to a VM's slot, or disconnects it. Live and idempotent.
  Hot-plug happens at the backend level, never on the virtual PCI bus: crosvm has no PCI hotplug on arm64,
  and empty slots are a state guests already handle.
- **Every handle is hot-pluggable:** any group can be lent or taken back while the VM runs (works for every group on
  the nio; for Wi-Fi bands see the firmware crash in [mship.wifi](contracts/mship.wifi-1.md)). Slots stay valid when
  their device service is missing: `mship-launch` relays the console slots (bt `hvc5`, gnss `hvc6`, sensors
  `hvc13`) for the VM's whole life (`crates/mship-launch/src/slots.rs`; crosvm connects once to
  `<vm dir>/slot-<name>.sock`, the launcher connects on to the service's socket for the VM whenever the service is
  there, retrying every second, and holds up to 64 KiB the guest sends meanwhile). A service that starts after the
  VM, or restarts while it runs, reaches the guest without a VM restart. mothershipd re-attaches the groups of a
  restarted service and hot-plugs groups whose service appears; `mship-gnss` and `mship-sensors` keep each VM's
  protocol state over their own restart.
- **Exclusive** handles (display, touch, keys, native Wi-Fi, HCI) have one owner. **Shared** handles (GPU,
  battery state, sensors) have many users: they go to every running VM with the slot and have no owner. A shared
  group with a `max` > 1 can be described in a descriptor but cannot be assigned: only exclusive groups have an
  owner.
- **Revocation is mandatory:** the broker takes a handle back instantly, without the VM's cooperation (DRM lease
  revoke, `EVIOCREVOKE`/grab, tc redirect removal, broker-held fds for interfaces without revoke).
- **Assignment:** every exclusive group has one assigned owner, mothershipOS or one VM (Settings › Hardware,
  `/var/lib/mothership/config/hardware.json`). Assigned groups are lent automatically: `display` and `buttons`
  while the VM is shown (they move with the screen: assist key, volume combo, Show), every other group while it
  runs, until taken back explicitly or the VM stops.
- **Groups** are what the user lends: `display` (display + display-power + touch + power key = "the screen"),
  `buttons` (volume keys), `wifi-2g`, `wifi-5g` (one Wi-Fi band each, native control), `internet` (shared
  internet through the host's uplink), `bluetooth` (the controller at the HCI level), `usb` (the devices plugged into the phone), `cellular` (the modem: SIM, calls, SMS, mobile data), `cameras` (every camera), `speaker` (the built-in speaker), `earpiece` (the earpiece), `headset` (the 3.5 mm jack), `microphone` (the built-in mics), `location` (the GNSS receiver), `display-ext` (the USB-C
  external display: leased to the VM, never used by mothershipOS, which stays on the panel). A VM may hold any set of groups; two VMs never hold the same group.
- **Wi-Fi bands are separate handles** when the radio can run two station interfaces on two channels at once
  (nl80211 interface combination with ≥2 managed and `#channels` ≥ 2; the nio's QCA6390 can). mothershipOS
  creates one station interface per band on the wiphy and restricts it to that band, so one VM can use
  2.4 GHz and another 5 GHz. A radio without that capability publishes a single `wifi:any` handle (group
  `wifi-any`) instead. The host's own Wi-Fi is just whichever band handles the host owns.

## 2a. Lifecycle and release
Every handle attachment goes through: `empty → attached → releasing → empty`.
- **Release reasons:** `user` (taken back in the UI or with `mship handover take`), `screen` (assist key, volume combo,
  Show of another VM: only `display` and `buttons`), `stop` (VM stopped or crashed), `revoked` (policy, e.g. the
  host needs its last uplink), `gone` (hardware disappeared).
- **Release is immediate and unconditional.** The VM's cooperation is never awaited. Channel contracts get a
  best-effort `detached {reason}` message, which is a notice, not a request.
- **Abort, never complete:** release leaves the guest in a consistent state in which no half-done user
  action completes (no tap from a finger that was down, no key that was mid-repeat, no connection that looks
  alive but isn't).
- **Fresh on attach:** the guest gets no state from before (no contacts, no pressed keys, no association).
  Physical state that already exists at attach time (a finger already on the panel, a key already held) is
  not delivered until it ends.
- **Host defaults:** the moment a handle is back with the host, the host applies its own default for it
  (table below), so the device never stays in a VM's configuration.

| Class | On attach | On release (guest sees) | Host default after release |
|---|---|---|---|
| display | newest guest frame within one refresh; panel off if the guest's last power state is OFF | nothing (frames still accepted and dropped, pacing continues) | panel back to the host compositor, on |
| display-power | guest's state applied | its lines are ignored | host screen-power policy |
| touch | only contacts that start after attach | every active contact **cancelled**: `ABS_MT_TOOL_TYPE=MT_TOOL_PALM` + `SYN`, then tracking id -1, `BTN_TOUCH` 0, `SYN` (Android: `ACTION_CANCEL`; libinput: ignored palm) | touch to the host compositor |
| keys | power: one `KEY_WAKEUP` press/release; keys already held are not delivered | every key the guest saw pressed gets a release (autorepeat stops); the volume combo's presses are released before the take-back acts | keys handled by the host |
| internet | carrier on, DHCP, forwarding opens | forwarding closes and the link loses carrier: the network disappears (Android switches to Wi-Fi instead of preferring a dead "Ethernet") | — |
| wifi band | band available in the guest (rfkill unblocked); the guest's supplicant connects on its own | host deauthenticates and deletes keys on that band's interface, flushes its scan results; the guest sees a disconnect (reason: local deauth) and the band as rfkill soft-blocked; pending scans/connects fail | interface returns to the host (NetworkManager may connect it) |
| bluetooth | controller reset; a guest HAL waiting for its `HCI_Reset` gets it answered, otherwise a Hardware Error event restarts the guest's stack | host sends `HCI_Reset` (every link ends) and takes the controller back; the guest gets a Hardware Error event | controller back to BlueZ, which restores its power state |
| usb | every present device (reset first), later ones on arrival | every device detached (an unplug; pending transfers fail) | devices reset, host drivers bind again |
| cameras | a camera runs while the guest streams from it | frames stop, a black frame; streams stay open and empty | cameras off |
| speaker, earpiece | the guest's sound plays on that output at once | silence at once; streams keep running in real time | host sounds only |
| microphone | the guest records the mics at once | the guest records silence; its streams keep running | the mics are the host's |
| headset | the guest's headset streams reach the jack's devices; jacks report the plug state | silence; jacks report unplugged | the headset is the host's |
| location | `STATE,ATTACHED`; the receiver runs if the guest asked (START), first epoch from reports after the attach | epochs stop, `STATE,EMPTY,<reason>` | the receiver serves the host's own clients |
| cellular | the guest's radio stack reconnects and initializes the modem; data calls on request | calls hung up, the guest's data calls ended, its AT channel closed (radio unavailable) | the host's own mobile data |
| gpu | shared: no attach/release; contexts end with the VM | — | — |
| sensors | shared, read-only: no attach/release; every running VM gets every sensor at its own rate | — | — |
| battery | shared, read-only: no attach/release; every running VM sees the live host values | — | — |

## 2b. Handle descriptor and discovery
Every handle has the same structure (`crates/mship-core/src/handle.rs`, schema 1); the classes differ only in their
contract and caps. A reader ignores fields it doesn't know (later schemas add, never change).
```jsonc
{
  "id": "wifi:5g",                    // <class>:<name>; names are generic (panel0, 2g, 5g), never phone part names
  "contract": "mship.wifi",
  "versions": ["1.0"],                // contract versions served
  "label": "Wi-Fi 5 GHz",             // what the user sees
  "icon": "network-wireless-symbolic",
  "direction": "both",                // seen from the VM: "in" (it receives: sensors, location, cameras, mics),
                                      // "out" (it sends: display, speaker), "both"
  "sharing": {"mode": "exclusive"},   // or {"mode": "shared", "max": 2}; no max = any number of VMs
  "lend": "while_running",            // "while_shown" (moves with the screen), "while_running", "every_vm" (shared:
                                      // every running VM with the slot gets it, no assignment)
  "group": "wifi-5g",                 // the unit the user assigns; handles of one group move together; none for every_vm
  "caps": {"bands": ["5g"]},          // per class facts
  "control": {"socket": "/run/mothership/wifi-bridge.sock", "arg": "5g"},
                                      // how it is lent: "attach <vm> [arg]", "detach <vm> <reason> [arg]" (datagram,
                                      // "ok" | "error: ..."); none: mothershipd lends it itself or nothing to lend
  "present": true                     // false while the hardware is missing (monitor unplugged, no cameras found)
}
```
**Discovery:** each device service publishes its handles in `/run/mothership/handles/<service>.json`
(`{"schema": 1, "service", "pid", "handles": [...]}`, `mship_core::handle::publish`) and rewrites the file when its
hardware changes. mothershipd's registry (`crates/mothershipd/src/handles.rs`) re-reads the directory every policy
tick (a file whose process is gone is ignored), adds its own handles (display, screen power, touch, keys, shared
internet, the external display, the GPU), groups them, and publishes the result on the `handles` topic
(`handles.list`; `mship handles`). Handover and Settings › Hardware work from it: there is no list of hardware in
mothershipOS's code. A new device service shows up in the UI, assignable or shared as its descriptors say, without
a change anywhere else. When a service appears, or its file shows a new pid (a restart), mothershipd lends its groups
(again) to the running VMs they are assigned to (`display`/`buttons` only to the shown VM); a group whose service is
gone is held by nobody until the service is back.

Handles on the nio:

| id | contract | group | caps |
|---|---|---|---|
| `display:panel0` | mship.display | display | width 1080, height 2520, refresh_hz 90, dpi 420 |
| `display-power:panel0` | mship.display-power | display | — |
| `display:dp0` | mship.display | display-ext | the USB-C monitor's modes (DP alt mode, 2 lanes: e.g. 2560x1440@60); present while one is plugged in |
| `touch:panel0` | mship.touch | display | width 1080, height 2520, max_contacts 10 |
| `keys:power` | mship.keys | display | keys [116 POWER, 143 WAKEUP] |
| `keys:volume` | mship.keys | buttons | keys [114 VOLUMEDOWN, 115 VOLUMEUP] |
| `wifi:2g` | mship.wifi | wifi-2g | bands [2g], max_width_mhz 40 |
| `wifi:5g` | mship.wifi | wifi-5g | bands [5g], max_width_mhz 80 (160 if the chip allows) |
| `internet:uplink` | mship.net-shared | internet | ipv4 |
| `bt:hci0` | mship.bt | bluetooth | transport h4, features [br-edr, le], address (QCA6390) |
| `usb:host` | mship.usb | usb | max_speed high (super is not offered), devices dynamic |
| `battery:0` | mship.battery | — (shared, read-only) | present, technology Li-ion, charge_full_design_uah 5000000 |
| `camera:all` | mship.camera | cameras | cameras: back (OV64B), back 2 (ultrawide OV16A10), front (OV16A1Q), front 2 (S5K4H7); 1280x720 each |
| `modem:0` | mship.modem | cellular | at 3gpp-27.007, data ipv4, slots 2, voice signalling (SDX55) |
| `gnss:0` | mship.gnss | location | constellations [gps, glonass, galileo, beidou, qzss, sbas], min_interval_ms 1000, assistance [xtra, time] (SDX55 engine) |
| `audio:speaker` | mship.audio | speaker | AW882xx with SKT speaker protection |
| `audio:earpiece` | mship.audio | earpiece | WCD9380 EAR output |
| `audio:headset` | mship.audio | headset | 3.5 mm jack: headphones, headset mic, plug state |
| `audio:mic` | mship.audio | microphone | the built-in mics (main and back) |
| `sensors:0` | mship.sensors | — (shared, read-only) | accelerometer, gyroscope (LSM6DS3C), magnetometer (MMC56x3x), proximity, light (VCNL36866); max_rate_hz 100 |
| `gpu:0` | mship.gpu | — (shared) | context_types [drm, cross-domain, virgl, gfxstream-gles, gfxstream-composer] |

A slot binds every attached handle with the same contract and a version that satisfies it. One slot can be fed
by several handles: the `keys` slot gets `keys:power` with *display* and `keys:volume` with *buttons*; a `wifi`
slot holding both band handles shows one interface with both bands.

## 3. Contracts and versioning

### What is a contract
| Contract | Between | Compatibility needed |
|---|---|---|
| **Manifest schema** (`vm.json`) | VM package ↔ mothershipOS | yes: every schema from 1 on is readable forever |
| **Slot contracts** (`mship.<class>`) | guest ↔ mothershipOS | yes: support window below |
| **Platforms** (`cuttlefish-arm64`, `efi-virt-arm64`) | guest image ↔ mothershipOS VMM setup | yes: same window as slots |
| Broker API (`/run/mothership/mothershipd.sock`, CLI `mship`) | host clients (shell, CLI, device services) ↔ broker | host-internal, ships together, no long window |
| Backend internals, install paths, VMM flags | — | **not** contracts, free to change |

### Version numbers
- Every contract has `MAJOR.MINOR`. **Minor** = backward-compatible additions only: new optional caps or
  messages, new optional fields. A peer that ignores them still works. **Major** = anything else.
- The **manifest** has an integer `schema`. New schema = new optional structure; old schemas are migrated in
  memory by pure functions and never rewritten without the user's action.
- **mothershipOS releases** are `YYYY.N` (file `VERSION`). They are independent of contract versions.
  `docs/compat.md` lists, per release, which contract versions it implements.

### Negotiation
- **virtio slots:** virtio feature bits do the per-device negotiation. The contract pins the device type, the
  required features and how caps map to device config (e.g. `mship.touch` 1.x = virtio-input multitouch with
  `ABS_MT_*` ranges = panel caps).
- **Channel slots** (vsock or virtio-console protocols, e.g. the Wi-Fi proxy): the first message is
  `hello {contract, versions: ["1.2", "2.0"]}`. The host answers with the highest common major and minor =
  min(both minors) of that major, or `unsupported`. Messages from a newer minor that the other side doesn't know
  are ignored (must be optional by the minor rule).
- **Implicit signals** (things the host reads from the guest that it doesn't know are a protocol, e.g. ranchu's
  `VIRTUAL_DEVICE_DISPLAY_POWER_MODE_CHANGED` kmsg lines): still named and versioned as contracts
  (`mship.display-power` 1.0 = Cuttlefish kmsg format), so a host change can't break them silently.
- **Manifest declarations:** each slot names its contract and version (`"contract": "mship.touch", "version":
  "1.0"`, optional `"required": false`). The broker checks them before boot:
  - **required + unsupported** → refuse to start: "needs mothershipOS ≥ 2027.2" (from `min_mothership`) or
    "uses mship.touch 1.x, removed in 2028.1".
  - **optional + unsupported** → the slot stays empty, and the UI shows why.

### Support window
- A contract major stays supported for **at least 4 mothershipOS releases and 12 months** after its successor
  ships, whichever is longer. Deprecation shows in the UI on affected VMs from the release that deprecates it.
- Superseded majors are implemented as **adapters** onto the current backend (inside the broker or in the
  slot's bridge), so old VMs don't keep old hot paths alive.
- Manifest schemas: readable forever. Platforms follow the slot window.
- Every contract version that ever shipped has fixtures in `tests/compat/`: manifests per schema, and protocol
  transcripts per channel contract. All are replayed against the current host in the test suite, and no
  release ships with a red compat test.

## 4. Manifest (`vm.json`, schema 1)

```json
{
  "schema": 1,
  "name": "PixelOS (Android 16)",
  "min_mothership": "2026.1",
  "platform": {"id": "cuttlefish-arm64", "version": "1.0"},
  "boot": {"kernel": "pkg/Image", "initrd": "pkg/initrd-crosvm.img", "cmdline": "pkg/cmdline.crosvm", "disk": "disk.img"},
  "slots": {
    "display": {"contract": "mship.display", "version": "1.0"},
    "touch":   {"contract": "mship.touch",   "version": "1.0"},
    "keys":    {"contract": "mship.keys",    "version": "1.0"},
    "power":   {"contract": "mship.display-power", "version": "1.0", "required": false},
    "internet": {"contract": "mship.net-shared", "version": "1.0", "required": false},
    "wifi":    {"contract": "mship.wifi", "version": "1.0", "required": false}
  },
  "handover": {"when_shown": ["display", "buttons"], "while_running": ["internet", "wifi-5g"]}
}
```
- Paths are relative to the VM directory. Unknown fields are kept and ignored (forward compatibility).
- A slot contract is the **guest's view** (e.g. `mship.display` 1.0 = one virtio-gpu scanout with the panel's
  mode). How the host presents it (a DRM lease of the panel for PixelOS, of the external display for the Ubuntu desktop) is host
  policy, never part of the manifest. Slots may carry `params` within what the contract defines (e.g.
  `mship.gpu` `context_types`).
- Host resources such as the vsock CID, taps and sockets are allocated by the broker, not declared.
- `handover` (the groups a VM gets, by `when_shown` / `while_running`) is validated but not read by the running
  system, which assigns hardware in Settings › Hardware (§2). Open decision: drop it from schema 2 or use it as the
  default assignment of a new VM.
- `schema` is required. A file with a newer schema than the host knows is refused with "needs mothershipOS
  ≥ `min_mothership`". Older schemas (from schema 2 on) are migrated in memory by pure functions and written
  back in their own schema, so the VM stays usable on the release it was made for.
- Handover groups the host doesn't know (from a newer release) are kept in the file and ignored.

## 5. Slot contracts at 1.0 and planned

| Contract | Slot (guest view) | Host side (bridged level) | Revocation | Status |
|---|---|---|---|---|
| `mship.display` 1.0 | virtio-gpu scanout, one display, caps = mode (1080×2520@90, DPI) | DRM lease of connector+CRTC+plane, guest buffers scanned out (zero copy) | lease revoke | works |
| `mship.display-power` 1.0 | guest writes ranchu kmsg power lines on hvc0 | presenter CRTC off/on | — | works |
| `mship.touch` 1.0 | virtio-input multitouch, caps = panel size | evdev events (grab) | grab release | works, [needs work](contracts/mship.touch-1.md) |
| `mship.keys` 1.0 | virtio-input keyboard: VOLUMEUP/DOWN, POWER, WAKEUP | evdev events | grab / stop forwarding | works, [needs work](contracts/mship.keys-1.md) |
| `mship.net-shared` 1.0 | virtio-net on a tap, DHCP | NAT out of the uplink | forwarding closed | works (as group `internet`) |
| `mship.gpu` | virtio-gpu context types | DRM native context (msm) + cross-domain gralloc; gfxstream fallback | context teardown | works (native context) |
| `mship.wifi` 1.0 | `mship_wifi` cfg80211 driver (nl80211 station subset over a channel) + virtio-net data | one band-restricted station interface per handle; frames via tc redirect | deauth, keys deleted, redirect removed | **broken**: firmware crash ([V1](contracts/mship.wifi-1.md)) |
| `mship.audio` 1.0 | virtio-snd, one PCM per phone device (speaker, earpiece, headset, mics) + headset jack: the guest routes natively | per-device PipeWire nodes of the VM, linked to the phone's devices while lent (speaker protection stays on the host) | device link removed, silence; jack unplugged | works in PixelOS, [needs work](contracts/mship.audio-1.md) |
| `mship.power` 1.0 | vsock channel (port 7720): each running VM votes its state, sleep permission and clock floors | mothershipd's power table: CPU/GPU floors, VM freeze, phone suspend | the VM's vote dropped; a running VM without a vote blocks sleep | works on the [host side](contracts/mship.power-1.md); not registered, no guest adapter (host-built votes) |
| `mship.bt` 1.0 | H4 HCI over a virtio-console port (Cuttlefish `hvc5`, the stock HAL) | `HCI_CHANNEL_USER` socket on the controller | `HCI_Reset`, socket closed, Hardware Error event to the guest | works, [needs work](contracts/mship.bt-1.md) |
| `mship.battery` 1.0 | goldfish battery → guest power_supply `battery` + `ac` (read-only, every VM) | kernel power_supply → `mship-battery` → crosvm power monitor | — (shared) | works, [needs work](contracts/mship.battery-1.md) |
| `mship.backlight`, `mship.vibrator` | channel / virtio-input FF | backlight / FF evdev | — | not implemented |
| `mship.usb` 1.0 | virtual xHCI, one root port per device | usbdevfs fds (opened by the host) → crosvm USB passthrough | detach + device reset | works, [needs work](contracts/mship.usb-1.md) |
| `mship.modem` 1.0 | AT channel (cuttlefish-arm64: vsock 9600, the stock RIL) + eth0 for data calls | WWAN AT port; data calls as QMI sessions routed to the VM's NIC | calls hung up, data calls ended, channel closed | works in PixelOS (registration, data, SMS, call signalling); no call audio ([V1](contracts/mship.modem-1.md)) |
| `mship.camera` 1.0 | V4L2 capture devices (virtio-media), one per camera, NV12 / XR24, MMAP or the guest's DMABUFs | libcamera (software ISP on the GPU) → mship-vcam, rendered on the GPU into the guest's buffers | frames stop, black frame | Linux guests stream (V4L2, PipeWire); PixelOS previews, takes photos, records (hardware encoder) ([V1](contracts/mship.camera-1.md)) |
| `mship.gnss` 1.0 | NMEA 0183 epochs + `$PMSHG` control sentences on a virtio-console port (cuttlefish-arm64: hvc6) | QMI LOC engine (modem): unlock, time, XTRA; NMEA built on the host | epochs stop, `STATE,EMPTY` | satellites and NMEA in PixelOS; no fix verified ([V1](contracts/mship.gnss-1.md)) |
| `mship.sensors` 1.0 | goldfish sensors protocol on a virtio-console port (cuttlefish-arm64: hvc13; the stock HAL) | the sensor DSP's SEE service (QMI), streams at the highest rate asked, calibration applied | — (shared) | works in PixelOS, [needs work](contracts/mship.sensors-1.md) |

Platforms 1.0 (both run under crosvm):
- [`cuttlefish-arm64`](platforms/cuttlefish-arm64-1.md): direct kernel boot, Cuttlefish virtual hardware (18
  virtio consoles with the Cuttlefish layout, vsock, virtio-gpu, 4 NICs, the bootconfig keys). PixelOS.
- [`efi-virt-arm64`](platforms/efi-virt-arm64-1.md): a desktop Linux from a disk image with its own kernel
  (virtio-blk/net/gpu, xHCI). The Ubuntu desktop.
Each platform document lists exactly what the guest can rely on; each contract has one in
[`contracts/`](contracts/README.md). The VMM that implements it is a host detail and may change.

## 6. Broker (`mothershipd`)
- Root service, socket `/run/mothership/mothershipd.sock` (root and group `mship`), one JSON object per line:
  requests `{"id", "op", …}`, replies `{"id", "ok", "result" | "error"}`, events `{"event", "data"}` to
  subscribers (topics `power`, `vms`, `handover`, `handles`, `host`, `cpu`). Types: `crates/mship-core/src/api.rs`. CLI:
  `mship` (`crates/mship`).
- Ops by area: `vm.*` (list, start, stop, show, sleep, wake), `handover.*` (status, assign, give, take, release,
  screen), `handles.list`, `power.*` (`mship.power`), plus the host's own settings (Wi-Fi, Bluetooth, SSH, CPU, region, screen,
  battery, VM store).
- Each device class is a device service with a control socket (`attach <vm> [handle]`, `detach <vm> [reason]`,
  `status`): `mship-wifi-bridge`, `mship-bt-bridge`, `mship-usb`, `mship-modem`, `mship-gnss`, `mship-sensors`, `mship-camera`, `mship-audio`.
  Display goes through sway's DRM lease and the VM's presenter, touch and keys through the VM's `mship-inputd`,
  `internet` through the host's NAT rules.
- The handle registry (§2b): `crates/mothershipd/src/handles.rs`, topic `handles`, op `handles.list`, CLI
  `mship handles`.
- Not implemented: broker-allocated vsock CIDs and sockets (they come from `vm.json` and the launcher).

## 7. Performance rules
1. Data planes are zero copy or in-kernel (dmabuf frames, vhost-net/tc packets, vhost-user queues). The broker
   is never in a per-frame or per-packet path.
2. Control planes (attach/detach, nl80211 ops, HCI commands) may use userspace.
3. Latency-sensitive slots get pinned host threads near the VM's vCPUs.
4. Every class gets a benchmark against native: frame rate, input-to-photon, audio round trip, iperf,
   scan/connect time.

## 8. Open decisions
- `mship.power` registration and its manifest slot, sleep preparation and timed wakes
  ([mship.power](contracts/mship.power-1.md), Not implemented). Keep `mship.display-power` independent.
- Support window length (proposed: ≥4 releases and ≥12 months).
- Release cadence and naming (`YYYY.N` proposed).
- Whether VM packages may be signed and the broker verifies them (not needed yet; the manifest leaves room
  with unknown-field tolerance).
