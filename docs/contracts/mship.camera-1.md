# mship.camera 1.x

The phone's cameras as ordinary V4L2 cameras in the guest: the guest's own camera stack (Android's camera service,
PipeWire, browsers, any V4L2 application) uses them like webcams. The host keeps the sensors, CAMSS and the image
processing (debayering, white balance, exposure, focus). Handle `camera:all` (group `cameras`), exclusive: one VM holds every
camera at a time.

- **Works:** Linux guests stream the cameras (V4L2, PipeWire). Every camera offers several modes (size and rate) and runs in
  the one the guest picks: on the nio up to the full sensor size (4608x3464 at 16 fps on the back camera), 3840x2880
  at 24, 4K, 1080p and 720p at 30, 60 fps where the sensor has a binned mode. Frames are rendered on the host's GPU
  straight into the guest's buffers (NV12 or XR24, the size the guest asked for); no CPU copies a frame. In PixelOS
  4K video records at the camera's rate (24 fps with Aperture's 4:3 photo stream, 3840x2880). In PixelOS the camera app (Aperture) takes
  full-resolution photos, previews and records video (Venus hardware H.264 encoder, mship.video) on all five
  cameras, with facing and orientation from the device's controls. Cameras with a focus lens (on the nio: back,
  back 2) take the guest's focus: continuous, a triggered one-shot search (tap to focus), hold, manual position;
  the host reports the autofocus state. Cameras with a light (on the nio: back = the dual flash LED, back 2 = the
  ring light) take the guest's flash / torch; Android's flashlight uses it. A raw-only mono sensor (the nio's
  depth camera) is a grey camera, exposed by the host. The group can be lent and taken back while the VM runs.
- **Needs work:**
  - One camera at a time host-wide: libcamera's simple pipeline locks CAMSS's media device.
  - The software ISP costs about one CPU core per running camera (libcamera's GPU debayer with CPU statistics).
  - Image quality: libcamera has no sensor helpers or sensor properties for the nio's sensors (gain is taken as
    linear, sensor delays are defaults); the tuning files set only the black level, there is no calibrated colour
    matrix. The rear ultrawide at full resolution shows its PDAF pixels. The front camera's modes above 1080p come
    out grey with a dot pattern (libcamera's own output; its full-size sensor mode is not debayered right, the
    binned 1080p mode is).
  - Autofocus is contrast-based on the processed frames (a full search takes ~2-4 s) and always on the frame's
    centre (the guest's AF region is not used). Manual focus is the lens range spread linearly over 0-10 diopters
    (uncalibrated). Flash: auto flash fires like always flash (the host measures no light for the decision).
  - A mode change restarts the host pipeline (about a second). Full-size modes are cut to whole 64-pixel groups per
    line (libcamera pads frame lines to 256 bytes): up to 56 columns of the sensor are left out.
- **Not implemented:** zoom, exposure / white balance controls, AF regions; a guest-visible rate below the
  mode's (the host runs every mode at its offered rate).

## Guest view
- **Devices:** one V4L2 video capture device per camera (the platform says how; `cuttlefish-arm64` and
  `efi-virt-arm64`: a virtio-media device each, after the video decoder's, in camera order). The device's card name
  names the camera by facing: `back camera`, `back 2 camera`, `front camera`, `front 2 camera` (a phone's
  configuration may give better names, e.g. `back ultrawide camera`). The set of cameras is fixed while the VM runs.
- **Format:** `NV12` (BT.601 limited range; the chroma plane after the luma rows padded to the `nv12 scanlines`
  control, `V4L2_CID_USER_BASE + 0xf006`, 1..64, default 1; a larger `bytesperline` than the default is taken) or
  `XR24` (V4L2_PIX_FMT_XBGR32: B, G, R, unused byte in memory), sRGB. Processed images: debayered, white
  balanced, exposed by the host. Memory: MMAP buffers, or DMABUF: the guest's own virtio-gpu buffers (virtio
  objects), which the host renders into.
- **Modes:** `VIDIOC_ENUM_FRAMESIZES` lists the camera's sizes (discrete) and `VIDIOC_ENUM_FRAMEINTERVALS` the rates
  at each; `VIDIOC_S_FMT` (capture) takes one of the sizes (another size gets the nearest offered one) and
  `VIDIOC_S_PARM` the rate. Set them before `VIDIOC_REQBUFS`; the host's camera follows, and frames of the new mode
  arrive within about a second. A device starts in 1280x720 at 30 fps (on the nio, unless configured).
- **Controls:** the standard camera controls `V4L2_CID_CAMERA_ORIENTATION` (front, back, external) and
  `V4L2_CID_CAMERA_SENSOR_ROTATION` (the sensor's mounting in degrees, as the host's device tree and libcamera state
  it; Android's sensor orientation) describe the camera. Focus and light, active (not `V4L2_CTRL_FLAG_INACTIVE`)
  only on cameras that have them: `V4L2_CID_FOCUS_AUTO` (1 continuous AF, 0 the lens stays),
  `V4L2_CID_FOCUS_ABSOLUTE` (0..1023, 0 = far; with FOCUS_AUTO 0: manual), `V4L2_CID_AUTO_FOCUS_START` (one search),
  `V4L2_CID_AUTO_FOCUS_STATUS` (busy / reached / failed, reported by the host), `V4L2_CID_FLASH_LED_MODE` (torch
  = the light on). Exposure and white balance are automatic.
- **No negotiation message:** devices only; the manifest slot pins the version.

## Caps
`cameras`: per camera its name, facing (`back`, `front`, `external`) and size.

## Attached / empty
- **Empty:** the devices exist and can be opened, configured and streamed, but no camera runs: a stream delivers
  nothing.
- **Attach:** a camera runs while a reader in the guest streams from its device (and only then: an unused camera is
  off); frames arrive within about a second of streaming.
- **Release (any reason):** frames stop at once; a streaming guest gets one black frame (its next buffer), so its
  last image is not the camera's; streams stay open (and empty). The camera turns off.
- **Host default afterwards:** the cameras are the host's again (off until a host application opens one).

**History:** 1.0 (2026.1) initial.

**Implementation:** `mship-camera` (`crates/mship-devices/src/bin/camera/`, `mship-camera.service`): cameras found with
libcamera (`cam -l`); a raw-only mono sensor (the nio's depth camera) is read raw (`cam`) and unpacked on the CPU.
One mship-vcam camera per VM and camera (kernel module `mship-vcam`, motorola_nio kernel patch 0035, created through
`/dev/mship-vcam`): a capture node linked as `/run/mothership/camera/<vm>/<n>-<name>` (the launchers proxy it,
`--v4l2-proxy`) and a source node. mship-camera is each camera's writer: the modes are offered through the `modes`
control (`WxH@FPS,...`) and a guest's choice arrives as its mode event (the pipeline restarts in that mode); while
the holder's guest streams (the streaming event), `gst-launch-1.0 libcamerasrc ! v4l2sink io-mode=dmabuf-import`
(software ISP on the GPU, `LIBCAMERA_SOFTISP_MODE=gpu`) queues the ISP's buffers on the source node, and each job
(the reader's next buffer and the newest frame, both as dma-bufs) is rendered on the GPU (`gpu.rs`: EGL/GLES on
Mesa, surfaceless; crop, scale, BT.601). Autofocus (`af.rs`): a camera whose sensor has `lens-focus` in the device tree gets its V4L2 lens subdev held
open while it runs (powers the actuator) and V4L2_CID_FOCUS_ABSOLUTE set from the frames' centre contrast (sampled on
the GPU: a 160x90 grid): coarse
sweep (step 96 of 0..1023), fine sweep (step 16) around the best, locked until sharpness drops below 60 % or
brightness moves by 30 % for 10 frames; `"autofocus": false` per camera in `/etc/mothership/camera.json` turns it
off; `status` reports each running camera's lens position and state. Control socket `/run/mothership/camera.sock` (`attach <vm>`, `detach <vm> [reason]`, `status`), mothershipd group
`cameras` (lent while the VM runs). Kernel: mship-vcam (patch 0035), sensor drivers `ov64b` (patch 0008) and `motcam` (patch 0028: OV16A10,
OV16A1Q, OV02B1B, S5K4H7), lens driver `motvcm` (patch 0033: GT9767, GT9772), CAMSS (patch 0009), dma-buf heaps
for libcamera; libcamera tuning files per sensor (`motorola_nio/rootfs/usr/share/libcamera/ipa/simple/`). Bring-up: `motorola_nio/docs/camera.md`.
