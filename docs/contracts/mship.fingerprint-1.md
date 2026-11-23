# mship.fingerprint 1.x

- **Works (1.0):** host service `mship-fingerprint` (crates/mship-devices/src/bin/fingerprint) on the nio: the
  handle, per-VM template sets that persist over reboots, enrolment authorized through the phone's Gatekeeper,
  enumerate/remove. Guest: the PixelOS 17 HAL (pixelOS_17_VM `device/mothership/mothership_vm/fingerprint`).
- **Works (PixelOS 17, 2026-11-23):** enrolling several fingers from Settings and unlocking with them.
- **Needs work:** lockout not verified; templates over a full reboot not verified; the nio `location` is an
  estimate (the side-sensor hint may be off).
- **Not implemented:** a signed authentication result (`hw_auth_token`, 1.x); the Linux guest side (libfprint/fprintd).

A fingerprint reader for one VM at a time. Handle `fingerprint:0`, group `fingerprint`, sharing `exclusive`, lent
while the VM runs (assigned in Settings › Hardware like the other devices): only the VM that holds the handle can
enrol or match, and the host runs the sensor only on its behalf.

The sensor and the templates live in the host's secure world (the vendor Trusted Application; see the port's notes,
e.g. motorola_nio/docs/fingerprint.md). The guest never sees image data or templates: it asks the host to enrol,
match or remove and gets results and progress back. No sensor driver or vendor blob is in the VM image.

## Guest view
- **Channel:** vsock, host (CID 2) port **9700**; one stream per guest. Each message is a big-endian `u32` length
  and that many bytes of UTF-8 JSON, both directions. Responses have `"type": "response"`, the request's `id` and
  `ok` (`false` with an `error` string); events have `"type": "event"` and `event`. A guest that doesn't hold the
  handle gets `error: "not_attached"` for everything but `hello`.
- **Requests** (guest → host; `id` is the guest's, any JSON value):

| `op` | Arguments | Response |
|---|---|---|
| `hello` | `versions` | `version` (the newest both speak, or null), `attached`, `caps` |
| `set_active_group` | `gid` (u32: the guest's user) | `authenticator_id`; loads that group's templates. Needed before the ops below |
| `enumerate` | | `fids` |
| `authenticator_id` | | `authenticator_id` (u64; changes when the group's templates change) |
| `challenge` | | `challenge` (u64, for the guest's credential check before `enroll`; valid 10 min) |
| `revoke_challenge` | `challenge` | |
| `enroll` | `hat` {`challenge`, `user_id`, `authenticator_id`, `authenticator_type`, `timestamp`, `mac` hex}, `timeout_s` (10-600, default 60) | starts; events follow |
| `authenticate` | | starts; matches touches until a match, `cancel`, or another op |
| `detect` | | starts; ends at the first touch |
| `cancel` | | ends the running op (event `error` `canceled`) |
| `remove` | `fids` (empty: all of the group) | `removed`, `authenticator_id` |

  A new op replaces a running one without an event. Errors: `not_attached`, `hw_unavailable`, `no_group`,
  `no_challenge`, `bad_token`, `no_space`, or a host diagnostic string.
- **Events** (host → guest):

| `event` | Fields | Meaning |
|---|---|---|
| `state` | `attached` | sent on connect and whenever this VM gets or loses the handle (or the reader is unavailable) |
| `acquired` | `info`: `good`, `partial`, `insufficient`, `dirty`, `duplicate` | a touch was sensed |
| `enroll_progress` | `fid` (0), `remaining` (>0) | one accepted enrol sample |
| `enrolled` | `fid`, `authenticator_id` | enrolment finished and stored |
| `authenticated` | `fid` | a match; `fid` 0 = no match (matching goes on) |
| `interaction` | | `detect` saw a touch |
| `error` | `code`: `canceled`, `timeout`, `unable_to_process` | the running op ended |

## Enrolment authorization
The host's matcher stores a template only after an enrolment authorized by a HardwareAuthToken for its own
challenge, signed by the phone's Keymaster. The guest's Gatekeeper is its own (software) one; its token can't be
checked by the host. So the guest verifies its user's screen lock against the host's `challenge` (as Android does
before any enrolment) and sends that token in `enroll`; the host checks that it names the outstanding challenge
and a user, then verifies the VM's own credential in the phone's real Gatekeeper against the same challenge and
passes that genuine token to the matcher. The VM's credential is a random secret only the host service knows
(per VM, `/var/lib/mothership/fingerprint/<vm>/gatekeeper`, root only). The trust decision: the VM holding the
handle vouches for its user. No token is forged.

## Authentication result (why 1.0 is non-crypto)
Android releases Keystore keys on a fingerprint match only when the matcher and the guest's Gatekeeper/KeyMint
share an auth-token key. The host's matcher shares one with the phone's Keymaster, not with the guest's KeyMint.
1.0 returns `authenticated {fid}` only, and `caps.hw_auth_token` is `false`: a guest offers the reader as a weak
biometric (screen unlock, app prompts that accept it), never for Keystore-bound keys. A signed result is 1.x.

## Caps
`sensor_type` (`rear`, `side`, `under_display`, `other`), `location` (`{x, y, radius}` in panel pixels, or null),
`max_templates` (per group), `vendor`, `hw_auth_token`. A guest sizes its sensor description from them: Android's
HAL reports a `side` reader as its power-button sensor, so the framework matches only while the screen is on unless
the user enables "Touch to unlock anytime" (no listening when unlocked either: the framework stops then).

## Attached / empty
On release (the VM stops, loses the handle, or it is taken in Settings) the running op is canceled (`error`
`canceled`), the group is unloaded and the sensor sleeps. Template sets stay on the host per VM and group
(`/var/lib/mothership/fingerprint/<vm>/<gid>.db`, encrypted by the matcher). Without a working reader the handle is
not present and `state.attached` stays false.

## Host side
Control datagrams on `/run/mothership/fingerprint.sock`: `attach <vm>`, `detach <vm> [reason]`, `status`. The port
enables the service with `/etc/mothership/fingerprint.json` (`device`, `sensor_type`, `location`).
