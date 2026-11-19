# mship.power 1.0 — system power votes

Status: **implemented on the host (`mothershipd`); not yet in the registry** (`docs/compat.md`,
`crates/mship-core/src/contracts.rs`) and no manifest slot: every running VM takes part. Open items at the end.

Every participant states what it wants; the host decides. Participants are `mothershipOS` itself and every running
VM. The phone suspends only when all of them agree, and runs its clocks at least at the highest floor anyone asks
for, but never above the caps the host sets (CPU mode, temperature guard, devfreq max).

## Host

`mothershipd` (root, `crates/mothershipd`, power domain in `power.rs`) keeps the table in memory, applies floors,
switches VMs' display outputs, and suspends the phone (suspend-to-idle, after the screen has been off for the sleep
delay) when the table allows it. The `mothershipOS` entry is built there: the phone's screen, the USB dev link, an
external display, charging towards the charge limit, and Settings › sleep. Nothing is written to files: the shell and
tools get the state from mothershipd's API (`/run/mothership/mothershipd.sock`; `power.get`, or `subscribe` to
`power` for a push on every change; `power.set_vote` for host-side agents, root only). Settings › Power votes shows
it raw.

## Entry

| Field | Values | Meaning |
|---|---|---|
| `state` | `active` \| `idle` \| `screen_off` \| `sleeping` \| `suspended` | What the participant is doing |
| `may_sleep` | bool | It agrees to a physical host suspend. Independent of `state` (screen off, but downloading: false) |
| `demand` | `{cpu \| gpu \| codec: low \| balanced \| high}` | Floors while the host is awake. Named levels; the host maps them to the phone's frequencies |
| `reason` | text ≤ 200 | For the UI only |
| `seq` | increasing int | Older requests are rejected |

A participant counts as asleep when `state` is `sleeping`/`suspended`, or it is not `active` and `may_sleep`.
A running VM without a fresh entry (no adapter, disconnected, or no update for 150 s) is `unknown` and **blocks**
sleep.

Floors: the highest level asked for per resource. `cpu` → every policy's `scaling_min_freq`, `gpu` → the GPU
devfreq `min_freq` (`low` = lowest step, `balanced` = a third up the table, `high` = the current cap). `codec` is
recorded but has no control yet (Venus clocks follow its decode sessions' declared load).

## Guest channel

vsock stream to the host (CID 2, port **7720**). Messages are a 4-byte big-endian length followed by a UTF-8 JSON
object `{"op": …, "id": n, …}` (the `mship.wifi` framing). The host identifies the VM by the peer CID (`vm.json`
`"cid"`); a VM's vote is dropped when its last connection closes.

| op (guest → host) | fields | result |
|---|---|---|
| `hello` | `contract: "mship.power"`, `versions: ["1.0"]` | `version`, `states`, `levels`, `resources`, `heartbeat_s` (60), `stale_s` (150) |
| `set` | an entry (above) | `status` 0, or −22 invalid, −116 old `seq` |
| `get_state` | — | `sleep_allowed`, `floors` |
| `sleep` | — | the host freezes the VM; the result and then `{"op": "resumed", "reason"}` arrive after the wake |

Every request gets `{"op": "result", "id": n, "status": 0 | -errno}`. The guest sends `set` on every change and
at least every `heartbeat_s`.

## VM sleep

Under crosvm no guest device can wake a suspended guest kernel (virtio devices and the virtual xHCI have no wake
support), so a guest never suspends itself: its suspend sends `sleep` and blocks. The host (or Power & sleep ›
Sleep now, API `vm.sleep`) freezes the VM's vCPUs (`crosvm suspend`), takes its USB devices back
(`mship-usb detach`) and grabs their input devices exclusively. The first key, button or motion, Wake (`vm.wake`),
or stopping the VM (`vm.stop`) hands the devices back and resumes it (`crosvm resume`); the guest's `sleep` then returns
and its normal resume path runs. A frozen VM counts as `suspended` without heartbeats. A guest can hold several
connections (the agent and the sleep helper); its vote is dropped when the last one closes.

## Display power

A VM's own display output (its `mship-presenter` DRM lease, e.g. the USB-C monitor) follows its vote:
off while its `state` is `screen_off`, `sleeping` or `suspended`, on otherwise; VMs without a vote are left alone.
A guest's blanking alone only draws black frames (virtio-gpu has no display power), so `screen_off` must mean
the guest really blanked its screen.

## Adapters

A VM without a guest adapter gets a vote built by the host from what it sees: its display (the presenter reports
`vm.display`: on, or blanked) and its video decoder sessions. A running VM without either is `unknown`.

- **Ubuntu desktop** (no agent in the guest; the host follows it from outside): GNOME blanking closes crosvm's
  window and the presenter reports `vm.display` (→ `screen_off`/`active`); an open session on the Venus decoder
  (virtio-media) counts as `codec: high`; the guest's own suspend (GNOME after 15 min) is PSCI SYSTEM_SUSPEND, which
  our crosvm turns into `vm.sleep` (`host/gpu/patches/crosvm/0004-psci-system-suspend.patch`). The power key is a
  virtio-input KEY_POWER that `crosvm powerbtn` presses (logind powers off, `PowerKeyIgnoreInhibited`).
- **PixelOS:** no guest adapter; only the host-built vote above.

## Open (not in 1.0)

- Registry and manifest: register `mship.power` in `docs/compat.md` and `contracts.rs`, and give VMs a slot for it
  (proposed: `"system_power": {"contract": "mship.power", "version": "1.0"}`, separate from the `power` slot, which
  is `mship.display-power`). Packages without the slot keep the host-built vote.
- Sleep preparation: `prepare_sleep` / `ready` / `not_ready` per preparation round, cancelled by any change, so
  guests can quiesce before a host suspend; final re-check against pending wake events (kernel wakeup count).
- Timed wakes: a guest with scheduled work needs a wake deadline the host honours before it can allow sleep.
- `codec` floor has no control yet.
- Open / unverified: whether PixelOS's host-built vote reaches `screen_off` (its display power goes through the
  launcher's presenter signals, `mship.display-power`); a PixelOS adapter (wakelocks, idle) is not written.

Standby measurements and the phone side of suspend: the port's notes
([nio standby](https://github.com/MothershipOS/motorola_nio/blob/main/docs/standby.md)).
