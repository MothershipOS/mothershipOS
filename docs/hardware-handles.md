# Hardware handles: design (V1 draft, 2026-10-12)

**Goal:** VM images are generic and get distributed. mothershipOS is built per phone and updated over time. A VM
gets real hardware by attaching to **handles** that mothershipOS publishes. A VM built for an older mothershipOS
keeps working on a newer one.

**V1 is the baseline.** Everything before it (the old `vm.json`, manifest-less VM directories, per-VM launcher
scripts) is pre-release and gets converted once; no compatibility promise covers it. Compatibility starts with
V1: manifest schema 1, every contract and platform at 1.0, first shipped in mothershipOS 2026.1.

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
| **Platform** | A host-implemented recipe that turns a manifest into a running VMM (`cuttlefish-arm64`, `linux-virt-arm64`). Versioned like a contract |
| **Broker** | `mothershipd` (root). Owns all handles, enforces policy, attaches and detaches handles, launches VMs from manifests |

## 2. Attach model
- **Attach / detach** connects a handle's backend to a VM's slot, or disconnects it. Live and idempotent.
  Hot-plug happens at the backend level, never on the virtual PCI bus: crosvm has no PCI hotplug on arm64,
  and empty slots are a state guests already handle.
- **Exclusive** handles (display, touch, keys, native Wi-Fi, HCI) have one owner. **Shared** handles (GPU,
  network uplink, battery state) have many users.
- **Revocation is mandatory:** the broker takes a handle back instantly, without the VM's cooperation (DRM lease
  revoke, `EVIOCREVOKE`/grab, tc redirect removal, broker-held fds for interfaces without revoke).
- **Policy per VM** (manifest + user settings): `when_shown` handles move with the screen (assist key, volume
  combo, Show); `while_running` handles stay until taken back explicitly or the VM stops.
- **Groups** are what the user lends: `display` (display + display-power + touch + power key = "the screen"),
  `buttons` (volume keys), `wifi-2g`, `wifi-5g` (one Wi-Fi band each, native control), `internet` (shared
  internet through the host's uplink), `bluetooth` (the controller at the HCI level), `usb` (the devices plugged into the phone), `cellular` (the modem: SIM, calls, SMS, mobile data), `cameras` (every camera), `display-ext` (the USB-C
  external display: leased to the VM, never used by mothershipOS, which stays on the panel). A VM may hold any set of groups; two VMs never hold the same group.
- **Wi-Fi bands are separate handles** when the radio can run two station interfaces on two channels at once
  (nl80211 interface combination with ≥2 managed and `#channels` ≥ 2; the nio's QCA6390 can). mothershipOS
  creates one station interface per band on the wiphy and restricts it to that band, so one VM can use
  2.4 GHz and another 5 GHz. A radio without that capability publishes a single `wifi:any` handle (group
  `wifi-any`) instead. The host's own Wi-Fi is just whichever band handles the host owns.

## 2a. Lifecycle and release
Every handle attachment goes through: `empty → attached → releasing → empty`.
- **Release reasons:** `user` (taken back in the UI or with `mship-hw`), `screen` (assist key, volume combo,
  Show of another VM: only `when_shown` groups), `stop` (VM stopped or crashed), `revoked` (policy, e.g. the
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
| cellular | the guest's radio stack reconnects and initializes the modem; data calls on request | calls hung up, the guest's data calls ended, its AT channel closed (radio unavailable) | the host's own mobile data |
| gpu | shared: no attach/release; contexts end with the VM | — | — |
| battery | shared, read-only: no attach/release; every running VM sees the live host values | — | — |

## 2b. Handle descriptor
What the broker publishes (`list-handles`, broker API 1):
```jsonc
{
  "id": "wifi:5g",                   // <class>:<name>; names are generic (panel0, 2g, 5g), never phone part names
  "contract": "mship.wifi",
  "versions": ["1.0"],
  "group": "wifi-5g",                 // null = not lendable (shared)
  "sharing": "exclusive",             // or "shared"
  "caps": {"bands": ["5g"], "max_width_mhz": 80, "security": ["open", "wpa2-psk", "wpa3-sae", "owe"]},
  "owner": "host",                    // or "vm:<vm id>"
  "state": "available"                // available | attached | releasing | gone
}
```
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
| `usb:host` | mship.usb | usb | max_speed high (super once host mode works), devices dynamic |
| `battery:0` | mship.battery | — (shared, read-only) | present, technology Li-ion, charge_full_design_uah 5000000 |
| `camera:all` | mship.camera | cameras | cameras: back (OV64B), back 2 (ultrawide OV16A10), front (OV16A1Q), front 2 (S5K4H7); 1280x720 each |
| `modem:0` | mship.modem | cellular | at 3gpp-27.007, data ipv4, slots 2, voice signalling (SDX55) |
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
| Broker API (`mship-hw`, socket) | host clients (shell, CLI) ↔ broker | host-internal, ships together. Versioned (`api`), no long window |
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
- **Implicit signals** (things we read from the guest that it doesn't know are a protocol, e.g. ranchu's
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
  mode). How the host presents it (a DRM lease for PixelOS, a compositor window for a QEMU guest) is host
  policy, never part of the manifest. Slots may carry `params` within what the contract defines (e.g.
  `mship.gpu` `context_types`).
- Host resources such as the vsock CID, taps and sockets are allocated by the broker, not declared.
- `handover` is the user's checklist. The UI edits it, and the broker owns the file (atomic writes,
  `schema` preserved).
- `schema` is required. A file with a newer schema than the host knows is refused with "needs mothershipOS
  ≥ `min_mothership`". Older schemas (from schema 2 on) are migrated in memory by pure functions and written
  back in their own schema, so the VM stays usable on the release it was made for.
- Handover groups the host doesn't know (from a newer release) are kept in the file and ignored.

## 5. Slot contracts at 1.0 (what exists today) and planned

| Contract | Slot (guest view) | Host side (bridged level) | Revocation | Status |
|---|---|---|---|---|
| `mship.display` 1.0 | virtio-gpu scanout, one display, caps = mode (1080×2520@90, DPI) | DRM lease of connector+CRTC+plane, guest buffers scanned out (zero copy) | lease revoke | ✅ |
| `mship.display-power` 1.0 | guest writes ranchu kmsg power lines on hvc0 | presenter CRTC off/on | — | ✅ |
| `mship.touch` 1.0 | virtio-input multitouch, caps = panel size | evdev events (grab) | grab release | ✅ |
| `mship.keys` 1.0 | virtio-input keyboard: VOLUMEUP/DOWN, POWER, WAKEUP | evdev events | grab / stop forwarding | ✅ |
| `mship.net-shared` 1.0 | virtio-net on a tap, DHCP | NAT out of the uplink | forwarding closed | ✅ (as group `internet`) |
| `mship.gpu` | virtio-gpu context types | DRM native context (msm) + cross-domain gralloc; gfxstream fallback | context teardown | ✅ native context (2026-10-13) |
| `mship.wifi` 1.0 | `mship_wifi` cfg80211 driver (nl80211 station subset over a channel) + virtio-net data | one band-restricted station interface per handle; frames via tc redirect | deauth, keys deleted, redirect removed | V1, not implemented |
| `mship.audio` | virtio-snd | ALSA PCM via vhost-user-snd | broker-held PCM | planned |
| `mship.power` (draft) | shared `system_power` channel; each running VM requests a profile and sleep permission | broker aggregation + host power backend | invalidate this attachment; a still-running VM remains an awake participant | [planning only](contracts/mship.power-1.draft.md), not registered |
| `mship.bt` 1.0 | H4 HCI over a virtio-console port (Cuttlefish `hvc5`, the stock HAL) | `HCI_CHANNEL_USER` socket on the controller | `HCI_Reset`, socket closed, Hardware Error event to the guest | ✅ [V1](contracts/mship.bt-1.md) (2026-10-18), gaps listed |
| `mship.battery` 1.0 | goldfish battery → guest power_supply `battery` + `ac` (read-only, every VM) | kernel power_supply → `mship-battery` → crosvm power monitor | — (shared) | ✅ [V1](contracts/mship.battery-1.md) (2026-10-18), gaps listed |
| `mship.backlight`, `mship.vibrator` | channel / virtio-input FF | backlight / FF evdev | — | planned |
| `mship.usb` 1.0 | virtual xHCI, one root port per device | usbdevfs fds (opened by the host) → crosvm USB passthrough | detach + device reset | ✅ [V1](contracts/mship.usb-1.md) (2026-10-18), tested with dummy_hcd; nio host mode not brought up |
| `mship.modem` 1.0 | AT channel (cuttlefish-arm64: vsock 9600, the stock RIL) + eth0 for data calls | WWAN AT port; data calls as QMI sessions routed to the VM's NIC | calls hung up, data calls ended, channel closed | 🚧 in progress: host side runs, [V1 draft](contracts/mship.modem-1.md); guest untested (see its status) |
| `mship.camera` 1.0 | V4L2 capture devices (virtio-media), one per camera, XR24 | libcamera (software ISP on the GPU) → v4l2loopback | frames stop, black frame | 🚧 [V1](contracts/mship.camera-1.md) (2026-10-27): Linux guests stream (V4L2, PipeWire); Android lists the cameras, capture unverified |
| `mship.sensors`, `mship.gnss` | channels | IIO / QMI LOC (modem) | held fds | later; GNSS engine runs on the host ([nio](https://github.com/MothershipOS/motorola_nio/blob/main/docs/gnss.md)) |

Platforms 1.0:
- `cuttlefish-arm64`: direct kernel boot, Cuttlefish virtual hardware (18 virtio consoles with the Cuttlefish
  layout, vsock, gfxstream GPU, 3 NICs, the bootconfig keys the host sets). Runs under crosvm.
- `efi-virt-arm64`: UEFI boot of a disk image on an arm64 `virt` machine with virtio-blk/net/gpu-gl/input
  (Ubuntu Touch). Runs under QEMU today.
Each platform has a document (`docs/platforms/<id>-<major>.md`) listing exactly what the guest can rely on; each
contract has one in `docs/contracts/`. The VMM
that implements it is a host detail and may change.

## 6. Broker (`mothershipd`)
- Root service, socket `/run/mothership/broker.sock` (group `mship`), JSON lines. The first message is
  `hello {client, api}`. CLI: `mship-hw`.
- Requests: `list-handles`, `list-vms`, `start <vm>`, `stop <vm>`, `attach <handle|group> <vm>`,
  `detach <handle|group>`, `show <vm>` (when_shown set), `take-screen` (screen switch), `watch` (event stream:
  attached, detached, vm-started, vm-stopped, caps-changed, handle-gone).
- State in `/run/mothership/broker/` (owners, running VMs). Ownership is rebuilt at start; VMs that are not
  running own nothing.
- Backends per class implement `attach(vm, slot)`, `detach()`, `revoke()`, `caps()`. Today's helpers become
  backends: the sway lease and presenter (display), `mship-vm-inputd` (touch/keys), `mship-admin` NAT (net-shared).
- `mship-handover` stays as a compatibility CLI on top of the broker until the shell and inputd talk to it directly.

## 7. Performance rules
1. Data planes are zero copy or in-kernel (dmabuf frames, vhost-net/tc packets, vhost-user queues). The broker
   is never in a per-frame or per-packet path.
2. Control planes (attach/detach, nl80211 ops, HCI commands) may use userspace.
3. Latency-sensitive slots get pinned host threads near the VM's vCPUs.
4. Every class gets a benchmark against native: frame rate, input-to-photon, audio round trip, iperf,
   scan/connect time.

## 8. Order
1. Versioning core: contract registry, manifest schema 1, compat fixtures and tests, `VERSION`,
   `docs/compat.md`. V1 manifests for PixelOS and Ubuntu Touch (deployed together with the broker).
2. Broker skeleton: handle registry, ownership, socket API, `mship-hw`. Port display, touch, keys and
   net-shared from `mship-handover` as backends; `mship-handover` becomes a client.
3. Generic launch: platforms `cuttlefish-arm64` and `efi-virt-arm64` replace the per-VM launcher scripts.
4. `mship.wifi` (native Wi-Fi), then audio, backlight/vibrator. (GPU native context, Bluetooth, battery: done.)

## 9. Open decisions
- System-power participation: [draft `mship.power` contract](contracts/mship.power-1.draft.md). Follow the
  existing slot/version/attachment model; decide shared-control bookkeeping, wire format, guest readiness,
  and wake behavior before implementation. Keep `mship.display-power` independent.
- Support window length (proposed: ≥4 releases and ≥12 months).
- Release cadence and naming (`YYYY.N` proposed).
- Whether VM packages may be signed and the broker verifies them (not needed yet; the manifest leaves room
  with unknown-field tolerance).
