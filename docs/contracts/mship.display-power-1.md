# mship.display-power 1.x

How the guest tells the host that it turned its screen off or on. 1.x is the format Cuttlefish's ranchu
hwcomposer already writes, so stock Cuttlefish guests speak it without changes.

- **Works:** `cuttlefish-arm64` (PixelOS): screen on/off; brightness (Android's brightness
  moves the panel's backlight while PixelOS has the display).

**Guest view:** the guest writes lines to `/dev/kmsg` that reach the kernel console:
`VIRTUAL_DEVICE_DISPLAY_POWER_MODE_CHANGED display=<n> mode=<MODE>`, with `MODE` ∈ `ON`, `OFF` (`DOZE`,
`DOZE_SUSPEND` and `ON_SUSPEND` are reserved; the host treats them as `ON`). The platform guarantees that the
kernel console is a channel the host reads, at a log level that passes kmsg writes (level 4).

**Host behaviour:** while the display handle is attached, `OFF` turns the panel off (CRTC and backlight).
`ON` turns it back on and shows the newest frame. While empty, the lines are ignored. Attaching the display
handle while the guest is `OFF` keeps the panel off.

**Brightness:** the guest's own brightness drives the panel's backlight while the display handle is attached:
the guest serves Cuttlefish's lights protocol on vsock port 6900 (a 4-byte little-endian length and JSON
`{"event": "VIRTUAL_DEVICE_LIGHTS_UPDATE", "lights": [{"id", "color", "light_type"}]}`, light_type 0 = backlight,
brightness = `colour & 0xffff` of 65535 when the colour's top byte is 0x01 (16 bits), else the colour's low byte of
255; written at the panel's full resolution; 0 is ignored, the panel's power is the lines above). The host connects
while the VM runs. When the display goes back to the host, the host's brightness returns. A guest without a
backlight leaves the panel at the host's brightness.

**Optional slot:** a guest that never writes the lines keeps the panel on whenever the display is attached.

**History:** 1.0 (2026.1) initial.
