# mship.display 1.x

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

**Gaps in the current implementation (fix before 2026.1):**
- While empty, `mship-vm-display` completes frame callbacks immediately (unthrottled), instead of at the
  nominal rate.
- `efi-virt-arm64`: the display is a QEMU window; its size follows the compositor window, not the caps.
