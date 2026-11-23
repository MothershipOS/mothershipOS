# mship.display 1.x

- **Works:** a DRM lease of the panel (PixelOS) or the USB-C external display (Ubuntu desktop) with the guest's
  buffers scanned out zero-copy.
- **Needs work:**
  - Frames still drop in PixelOS 17: with the blurred notification shade about 12% of frames take two vblanks at
    90 Hz (perfetto frame timeline, 2026-11-23: SurfaceFlinger GPU deadline missed, Display HAL), after the
    nonblocking present (pixelOS_17_VM goldfish patch 0003) took it from ~48%.
  - While empty, the presenter (`mship-presenter`) completes frame callbacks immediately (unthrottled), instead of
    at the nominal rate.
  - `efi-virt-arm64` gets the external display (`display:dp0`, group `display-ext`) hot-plugged at the monitor's
    preferred mode: the display appears and disappears with the monitor, which 1.0 (a fixed mode for the VM's
    lifetime) doesn't describe.

**Guest view:** one virtio-gpu device with one scanout (display 0).
- The scanout mode is the panel's: width × height px, refresh rate, DPI (`caps`). It never changes while the VM
  runs.
- The guest may present any buffer the virtio-gpu device lets it create. The host may scan buffers out directly
  (zero copy) or copy them; the guest can't tell the difference.
- Frame pacing: a flush (and the Wayland frame callback behind it in crosvm's display path) completes no faster
  than the panel refresh rate, whether or not the slot is attached.

**Caps:** `width`, `height`, `refresh_hz`, `dpi`. nio: 1080 × 2520, 90 Hz, 420 dpi.

**Attached:** the guest's newest frame is on the panel within one refresh (panel stays off if the guest's last
`mship.display-power` state is OFF).
**Release:** immediate; the panel goes back to the host. The guest isn't told.
**Empty:** frames are accepted and dropped, pacing continues at the nominal rate.

**History:** 1.0 (2026.1) initial.
