# mship.display-power 1.x

How the guest tells the host that it turned its screen off or on. 1.x is the format Cuttlefish's ranchu
hwcomposer already writes, so stock Cuttlefish guests speak it without changes.

**Guest view:** the guest writes lines to `/dev/kmsg` that reach the kernel console:
`VIRTUAL_DEVICE_DISPLAY_POWER_MODE_CHANGED display=<n> mode=<MODE>`, with `MODE` ∈ `ON`, `OFF` (`DOZE`,
`DOZE_SUSPEND` and `ON_SUSPEND` are reserved; the host treats them as `ON`). The platform guarantees that the
kernel console is a channel the host reads, at a log level that passes kmsg writes (level 4).

**Host behaviour:** while the display handle is attached, `OFF` turns the panel off (CRTC and backlight).
`ON` turns it back on and shows the newest frame. While empty, the lines are ignored. Attaching the display
handle while the guest is `OFF` keeps the panel off.

**Optional slot:** a guest that never writes the lines keeps the panel on whenever the display is attached.

**History:** 1.0 (2026.1) initial.
**Gaps:** none known (`cuttlefish-arm64` only).
