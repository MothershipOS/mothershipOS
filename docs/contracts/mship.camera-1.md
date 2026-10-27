# mship.camera 1.x

The phone's cameras as ordinary V4L2 cameras in the guest: the guest's own camera stack (Android's camera service,
PipeWire, browsers, any V4L2 application) uses them like webcams. The host keeps the sensors, CAMSS and the image
processing (debayering, white balance, exposure). Handle `camera:all` (group `cameras`), exclusive: one VM holds every
camera at a time.

## Guest view
- **Devices:** one V4L2 video capture device per camera (the platform says how; `cuttlefish-arm64` and
  `efi-virt-arm64`: a virtio-media device each, after the video decoder's, in camera order). The device's card name
  names the camera by facing: `back camera`, `back 2 camera`, `front camera`, `front 2 camera` (a phone's
  configuration may give better names, e.g. `back ultrawide camera`). The set of cameras is fixed while the VM runs.
- **Format:** `XR24` (V4L2_PIX_FMT_XBGR32: B, G, R, unused byte in memory), one size per camera (1280x720 on the
  nio unless configured), 30 frames per second, sRGB. Processed images: debayered, white balanced, exposed by the
  host. Memory: MMAP buffers.
- **Controls:** none in 1.0 (exposure and white balance are automatic).
- **No negotiation message:** devices only; the manifest slot pins the version.

## Caps
`cameras`: per camera its name, facing (`back`, `front`, `external`) and size.

## Attached / empty
- **Empty:** the devices exist and can be opened, configured and streamed, but no camera runs: a stream delivers one
  black frame and then nothing.
- **Attach:** a camera runs while a reader in the guest streams from its device (and only then: an unused camera is
  off); frames arrive within about a second of streaming.
- **Release (any reason):** frames stop at once and the device gets a black frame, so nothing the camera saw stays in
  it; streams stay open (and empty). The camera turns off.
- **Host default afterwards:** the cameras are the host's again (off until a host application opens one).

**History:** 1.0 (2026.1) initial.

**Implementation:** `mship-camera` (`crates/mship-devices/src/bin/camera.rs`, `mship-camera.service`): cameras found with
libcamera (`cam -l`); those its software ISP cannot process (the nio's mono depth sensor) are left out. One v4l2loopback
device per VM and camera (`motorola_nio/kernel/drivers/v4l2loopback.c`, created through `/dev/v4l2loopback`), linked as
`/run/mothership/camera/<vm>/<n>-<name>`; the launchers proxy them (`--v4l2-proxy`). mship-camera is each device's
writer: the format is set from the start, frames are written while the holder's guest streams (v4l2loopback's
client-usage event), from `gst-launch-1.0 libcamerasrc` (software ISP on the GPU, `LIBCAMERA_SOFTISP_MODE=gpu`) over a
pipe. Control socket `/run/mothership/camera.sock` (`attach <vm>`, `detach <vm> [reason]`, `status`), mothershipd item
`cameras` (lent while the VM runs). Kernel: sensor drivers `ov64b` (patch 0008) and `motcam` (patch 0028: OV16A10,
OV16A1Q, OV02B1B, S5K4H7), CAMSS (patch 0009), dma-buf heaps for libcamera. Bring-up: `motorola_nio/docs/camera.md`.

**Gaps (fix before 2026.1):**
- Android: the external camera HAL (patched for XR24) lists all four cameras; a capture through an app is not yet
  verified (PixelOS was credential-locked, so no camera app could start). Every camera reports the same facing and
  one orientation (the HAL's config has one value for all).
- One camera at a time host-wide: libcamera's simple pipeline locks CAMSS's media device.
- One size per camera; no stills at full resolution, no zoom, no controls, no flash, no autofocus (the AF actuators
  have no driver yet), no depth camera.
- The software ISP costs about one CPU core per running camera, and frames are copied twice on the host (pipe, then
  the loopback device).
- The sensors' gain codes follow each family's convention (no stock formula); no libcamera tuning or sensor helper
  for the nio's sensors yet, so exposure and colours are uncalibrated. The rear ultrawide at full resolution shows its
  PDAF pixels.
- The caps aren't published (no broker `list-handles` yet).
