# venus-vaapi driver (guest) — browser/player HW video decode via Venus

A VA-API (libva) backend over the Qualcomm Venus **stateful V4L2 M2M** decoder,
so VA-API clients (mpv, Firefox, Chromium) in the Ubuntu guest VM can hardware-
decode video on the phone's Venus block. Upstream:
`https://github.com/snowf14k3/venus-vaapi-driver` (targets SM8150/Raphael).

`0001-mothership-venus-vaapi-fixes.patch` adapts it to our stack
(Motorola nio / SM8250 at EL2, Venus exposed into the guest over
crosvm **virtio-media**, GPU is **virtio-gpu** with the freedreno native
context). Apply on top of a fresh clone, then build with meson.

## Fixes in the patch

1. **Accept the virtio-media-proxied device.** The upstream probe matches the
   V4L2 `cap.driver` string `"qcom-venus"`. In the guest the device is proxied
   by crosvm so `cap.driver == "virtio-media"` (card is still
   "Qualcomm Venus video decoder"). `v4l2_probe.c`, `v4l2_decoder.c`,
   `v4l2_encoder.c` now also accept `"virtio-media"`.

2. **Low-delay / decode-order output for all codecs** (`v4l2_decoder.c`).
   Upstream set `V4L2_CID_MPEG_VIDEO_DEC_DISPLAY_DELAY(_ENABLE)` only for HEVC.
   For H.264/VP9 with reordering, Venus buffered frames for display-order output,
   but a VA-API client feeds ~4 access units then blocks on `vaSyncSurface` — the
   reorder window never fills → **deadlock** (`VA_STATUS_ERROR_HW_BUSY`). We now
   request DISPLAY_DELAY=0 + ENABLE=1 for every decode codec (non-fatal if the
   control is unsupported). The client reorders by PTS.

3. **GBM surface allocation** (`objects.c`, `meson.build`). Upstream allocates VA
   surfaces with raw `DRM_IOCTL_MSM_GEM_NEW` on `/dev/dri/renderD128`. In the
   guest that node is **virtio-gpu**, so the MSM ioctl returns `EINVAL` and every
   surface fell back to a plain `calloc` buffer that cannot be exported as a
   dmabuf — so `vaExportSurfaceHandle` failed, clients never got a real dmabuf,
   and the only working path was a slow CPU copy-back. We now allocate surfaces
   via **GBM** (`gbm_bo_create`, linear, CPU-mappable, exported as dmabuf), which
   works on both virtio-gpu and real MSM; the bare-metal MSM path remains as a
   fallback. This is what makes **zero-copy** decode→display work.

4. **Parallel frame copy, no full clear** (`objects.c`). Decoded frames land in the V4L2 CAPTURE mapping, which
   virtio-media maps write-combined (uncached; a cached guest alias corrupted frames), so reading a 4K NV12
   frame is latency-bound: 13 ms on one core. The copy into the VA surface is split over 4 threads (uncached
   read bandwidth scales with readers) and the surface is no longer memset first: 5.9 ms.

5. **No busy wait in `vaSyncSurface`** (`v4l2_decoder.c`). virtio-media reports POLLOUT whenever any OUTPUT
   buffer is unqueued, not only when one completed, so the pump's poll returned at once and spun on DQBUF
   ioctls with the backend mutex held. It now polls POLLOUT only when every OUTPUT buffer is queued.
   `VENUS_DBG` is read once instead of per pump.

6. **Declared frame rate** (`v4l2_decoder.c`): VIDIOC_S_PARM on OUTPUT with 60 fps, so Venus clocks up
   (`VENUS_VAAPI_DECODE_FPS=<n>` overrides, `0` keeps the kernel default); see "Clock" below.

Why the per-frame cost matters: Firefox syncs each surface right after submitting it, so it pays Venus decode
latency + copy serially per frame (mpv/ffmpeg pipeline several frames). At the 4K30 clock that sum must stay
under 33 ms.

Build dep added: `gbm` (pkg-config).

## Package

The Ubuntu VM's `cargo xtask guest-vaapi` (in the build image) fetches upstream at the pinned commit (`source.json`, checked
against its SHA-256), applies the patch, cross-builds it **release** (meson's default debug build is -O0) and
packs `mothership-venus-vaapi_<version>_arm64.deb`: the driver in `<libdir>/dri`, `environment.conf` as
`/etc/environment.d/80-mothership-vaapi.conf` (`LIBVA_DRIVER_NAME=venus`; log out and in once) and
`firefox-policies.json` as `/etc/firefox/policies/policies.json` (hardware decode on, AV1 off so YouTube serves VP9,
Firefox composites video itself). The Ubuntu VM's release carries it and its first start installs it; by hand:
`sudo apt install ./mothership-venus-vaapi_*.deb`. Firefox's RDD sandbox stays on: it allows the VA driver's
`/dev/video*` access.

Check: `LIBVA_DRIVER_NAME=venus vainfo --display drm --device /dev/dri/renderD128`.

## Playback

Any VA-API client: mpv `--hwdec=vaapi --vo=gpu --gpu-api=opengl --profile=fast` plays on the Venus HW decoder
zero-copy (VA-API → GBM dmabuf → GL); for YouTube URLs prefer VP9 ≤2160p30 (Venus can't do AV1; 4K60 exceeds the
~33–40 fps Venus VP9 decode rate, so 4K30 is the smooth target).

## Status

- **Works:** zero-copy VP9/H.264 HW decode in the guest; smooth **4K30** playback on the DisplayPort via mpv. H.264 is bit-exact (PSNR ∞) vs software. ffmpeg decodes the 4K30 VP9 YouTube stream at
  ~92 fps (pipelined).
- **Firefox:** YouTube plays through VA-API (VP9, sandboxed RDD, dmabuf → GL). 4K30 mostly keeps up with
  bursts of dropped frames: Firefox's serial sync puts Venus latency on every frame, and the host's Venus core
  clock stays at its lowest level (see Clock).
- Several decoder sessions at once (Firefox: ad + video) need the patched crosvm (`mothershipOS/host/gpu/patches/virtio-media/0001-per-session-mmap-offsets.patch`);
  the stock proxy panicked and stopped the VM.

## Clock

Venus sets its core clock from the sessions' declared load (~210 cycles per macroblock × MB/s). With the kernel's
default 30 fps a 4K VP9 session gets the lowest level (mvs0 240 MHz): enough throughput when frames are pipelined,
but ~30 ms per frame, too slow for Firefox's serial sync. The driver declares 60 fps: 4K then runs at the top level,
444 MHz (ffmpeg 70 → 108 fps); 1440p and below fit the lowest level and stay at 240 MHz, fast enough for them.
444 MHz (the video PLL's 1332 MHz / 3) is SM8250's top Venus level, the same as the stock kernel's turbo.

This needs `motorola_nio/kernel/patches/0020-*` (the SM8250 OPP table keyed to the vcodec0 clock); without it every
level above the lowest fails to set.

See the `nio-venus-el2-wall` memory for the full bring-up history.
