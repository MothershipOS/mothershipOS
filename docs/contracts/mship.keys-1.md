# mship.keys 1.x

**Guest view:** one virtio-input keyboard device named `mship-keys`, reporting only `KEY_VOLUMEUP` (115),
`KEY_VOLUMEDOWN` (114), `KEY_POWER` (116), `KEY_WAKEUP` (143), plus `EV_SYN`.

**Which key when:**
- Volume keys reach the guest while the `buttons` group is attached.
- `KEY_POWER` reaches the guest while the `display` group is attached (the guest owns its screen power, see
  `mship.display-power`).
- When the `display` group is attached, the host sends one `KEY_WAKEUP` press and release.
- Holding both volume keys for 1 s is reserved by the host (it takes the screen back). Those presses are
  released in the guest when the host acts.

**Attach:** keys already held at attach time are not delivered until released.
**Release (any reason):** every key the guest saw pressed gets a release event before anything else, so
autorepeat stops. For the volume take-back combo, both volume releases reach the guest before the take-back
acts. (Linux key events have no cancel; a release is the defined end.)
**Empty:** no events.

**History:** 1.0 (2026.1) initial.

**Gaps in the current implementation (fix before 2026.1):**
- The device is named by crosvm's default keyboard name, not `mship-keys`.
- Release covers held volume keys only when the buttons group goes, not the power key.
- Keys held at attach are forwarded (their release reaches the guest without a press).
