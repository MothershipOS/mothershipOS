# mship.fingerprint 1.x

- **Not implemented:** design only. Not in the registry, no host service and no hardware group; the only host code
  is the nio bring-up tool `fpc-test` (`crates/mship-devices/src/bin/fpc_test.rs`). Template storage needs the
  auth-token design below first.

A fingerprint reader for one VM at a time. Handle `fingerprint:0`, group `fingerprint`, sharing `exclusive`:
enrolled fingers are sensitive, so only the VM that holds the handle can enrol or match, and the host runs the
sensor only on its behalf. Lent in Settings › Hardware like the other devices.

The sensor and the enrolled templates live entirely in the host's secure world (the vendor Trusted Application;
see the port's notes, e.g. motorola_nio/docs/fingerprint.md). The guest never sees image data or templates — it
asks the host to enrol, match or remove, and gets results and progress back. This keeps the VM image generic:
no sensor driver, no vendor blobs inside it.

## Guest view
- **Device:** a vsock channel (the platform says the CID/port), not a virtio device — there is no standard
  virtio fingerprint device, and Android/Linux fingerprint stacks sit in userspace already. One length-prefixed
  JSON request/response stream, plus unsolicited event messages. A guest with no fingerprint stack can ignore it.
- **Requests** (guest → host), each answered:

| Request | Meaning |
|---|---|
| `enumerate` | the ids of the fingers enrolled for this VM's current user group |
| `enroll {gid, timeout_s}` | start enrolling a new finger; progress arrives as events, ending in `enrolled` or `error` |
| `authenticate {gid, [challenge]}` | match the next touch against this group's fingers |
| `cancel` | stop the current enrol or authenticate |
| `remove {gid, fid}` | delete one finger, or all of a group when `fid` = 0 |
| `set_active_group {gid, path}` | select the template set (per guest user); `path` is advisory, the host stores the set |

- **Events** (host → guest, unsolicited):

| Event | Meaning |
|---|---|
| `acquired {info}` | a touch was sensed (good, partial, too fast, too slow, insufficient) — for user feedback |
| `enroll_progress {fid, remaining}` | one good enrol capture; `remaining` reaches 0 at `enrolled` |
| `enrolled {fid}` | enrol finished |
| `authenticated {fid, [token]}` | a match; `fid` 0 = no match. `token` carries the signed auth result when a challenge was given |
| `error {code}` | hardware unavailable, timeout, canceled, unable to process |

- **Guest side:** Android — a fingerprint HAL (AIDL `IFingerprint`) that forwards to this channel instead of a
  local TA. Linux — a libfprint driver or a small fprintd backend. Both are generic (no per-phone code).

## The auth-token question (why `token` is optional in 1.0)
Android releases Keystore keys on a fingerprint match only when the fingerprint TA and Gatekeeper/KeyMint share
an auth-token HMAC key, and the match returns a `HardwareAuthToken` signed with it. The host's TA has that key;
the guest's KeyMint does not. 1.0 therefore supports **non-crypto** unlock (lockscreen, app prompts that accept
device credential fallback) and returns `authenticated {fid}` without a usable `token`. Full `HardwareAuthToken`
binding across host-TA ↔ guest-KeyMint is a 1.x extension and is the main open design item; a VM that needs
crypto-grade fingerprint auth must know it is absent (see Caps).

## Caps
In the handle descriptor: `sensor_type` (`rear`, `side`, `under_display`, `other`), `max_templates` per group,
`vendor` (`fpc`), and `hw_auth_token` (`false` in 1.0). A guest needs none of them to enrol and match; it reads
`hw_auth_token` to decide whether to offer fingerprint for crypto-gated flows.

## Attached / empty
Attached only while this VM holds the handle (`exclusive`). On release — the VM stops, loses it to another VM,
or it is taken in Settings — any in-flight enrol or authenticate is canceled and the host powers the sensor down.
Enrolled templates persist in the host (per VM and group) across releases; removing the VM removes them. Before
the host's TA is up, or on a phone without the sensor, the slot stays empty and `enumerate` returns nothing.

## Platform
The platform doc gives the vsock CID and port for the channel. The message framing is one big-endian `u32`
length followed by that many bytes of UTF-8 JSON, in both directions; events and responses share the stream and
are told apart by a `type` field (`response` carries the request's `id`; `event` has none).
