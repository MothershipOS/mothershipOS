# mship.touch 1.x

**Guest view:** one virtio-input device of type multi-touch, named `mship-touch`.
- `ABS_MT_SLOT`, `ABS_MT_TRACKING_ID`, `ABS_MT_POSITION_X` in `0 … width-1`, `ABS_MT_POSITION_Y` in
  `0 … height-1`, `BTN_TOUCH`. Protocol type B. Coordinates are display pixels of `mship.display` (same
  orientation, no scaling left to the guest).
- Events: `EV_SYN`, `EV_KEY`, `EV_ABS` only.

**Caps:** `width`, `height`, `max_contacts` (nio: 1080, 2520, 10).

**Attached:** every contact that starts after the attach reaches the guest. A finger already on the panel at
attach time is not delivered until it lifts.
**Release (any reason):** every active contact is **cancelled**, never lifted as if completed: for each slot
`ABS_MT_TOOL_TYPE = MT_TOOL_PALM` and `SYN_REPORT`, then `ABS_MT_TRACKING_ID -1`, `BTN_TOUCH 0`, `SYN_REPORT`.
Android turns this into `ACTION_CANCEL` and libinput ignores palms, so no tap, swipe or long-press
completes. The device therefore also reports `ABS_MT_TOOL_TYPE` (`MT_TOOL_FINGER`, `MT_TOOL_PALM`).
**Empty:** no events.

**History:** 1.0 (2026.1) initial.

**Gaps in the current implementation (fix before 2026.1):**
- The device is named `NT36672C` (the nio's controller): phone-specific. It must be `mship-touch`.
- Release does not cancel active contacts (no palm cancel, no lift).
- Contacts already down at attach are forwarded mid-gesture.
- Caps are hardcoded in the launcher (1080 × 2520) instead of coming from the display handle.
- `efi-virt-arm64`: touch comes from the QEMU window (device "QEMU Virtio MultiTouch"), not from the handle.
