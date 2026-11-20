# Guest virtio-media driver (host Venus → guest V4L2)

The guest-side driver that binds crosvm's `--v4l2-proxy` device and re-exposes the host's Venus V4L2
decoder as a V4L2 decoder *inside* a guest: built into the Linux guests' kernel (`CONFIG_MSHIP_VIRTIO_MEDIA`, in
`drivers/mothership`, see `../Kconfig`; the Ubuntu VM's `cargo xtask guest-kernel`), a module (`virtio-media.ko`)
for PixelOS's GKI kernel (the PixelOS VM's `cargo xtask gki-module`). The VMs build it from this source. It is the generic
device-brokering bridge for hardware video decode and the cameras (`motorola_nio/docs/video.md`,
`docs/contracts/mship.camera-1.md`).

## Source
Upstream: https://github.com/chromeos/virtio-media `driver/` (main). Same project as the
`virtio-media` crate crosvm vendors (0.0.8) for the device side — one stable VIRTIO_ID_MEDIA
protocol, no version negotiation. Self-contained out-of-tree module (`obj-m := virtio-media.o` from
`scatterlist_builder.o virtio_media_ioctls.o virtio_media_driver.o`); no external module deps. `Kbuild`
builds it in when `CONFIG_MSHIP_VIRTIO_MEDIA=y` and as a module out of tree (`M=`).

## Build as a module (against PixelOS's GKI kernel)
Built by the PixelOS VM's repository with the same GKI tree + toolchain as `../mship_wifi`:
`cargo xtask gki-module <gki tree> <aosp tree> virtio_media` there (its `docs/gki-modules.md`).

vermagic `6.12.38-4k-g501f58f107ab … modversions aarch64` (matches the shipped guest kernel). The
guest GKI config has `CONFIG_MEDIA_SUPPORT=y`, `VIDEO_DEV=y`, `VIDEOBUF2_CORE=y`, `VIRTIO=y`, so
modpost resolves every symbol from the built-in kernel.

## Install (PixelOS)
Copy `virtio-media.ko` into the VM's `pkg/` dir (same place as `mship_wifi.ko`);
`cargo xtask android-initrd` injects it into the guest first-stage ramdisk when present.
Host side: mothershipOS's media-enabled crosvm (`cargo xtask gpu`, `--features …,media`) +
`--v4l2-proxy=/dev/<venus-decoder>` from the launcher.

## Changes from upstream
- `virtio_media_device_poll()`: EPOLLERR only when neither queue can make progress (as `v4l2_m2m_poll_for_data()`).
- A session the host reports an error for (`VIRTIO_MEDIA_EVT_ERROR`) is marked broken, not freed in the event
  handler: its file still refers to it, and freeing it there would crash a waiting `VIDIOC_DQBUF` (NULL pointer
  dereference) and hang the process's exit in `virtio_media_vma_close`. A broken session wakes its waiters, fails
  every ioctl with `-EIO`, polls as EPOLLERR, and is closed and freed when its file is released.
- A DQBUF event for a buffer the guest hasn't queued (still in flight when STREAMOFF cleared the queue) is dropped.
  It must not go into `pending_dqbufs`: once the app queues that buffer again, its real event would add it a second
  time (list corruption, guest panic; an app restarting its stream, e.g. Android's camera app, triggers this).
- A dequeued MMAP buffer carries the device's offset, not the `m` the caller passed at QBUF (kept only for
  USERPTR and DMABUF). Android's external camera HAL re-queues frames with `m` 0; returning that at DQBUF made the
  next mmap() of the buffer fail (EINVAL, the camera dies after its first frames are recycled).
- MMAP buffers are mapped write-combine only on memory-to-memory devices (the codecs: Venus DMA); capture devices
  (the host's cameras, written by the host CPU) are mapped cacheable, as the host maps them, so frame reads are fast.
- A DMABUF buffer must be a virtio-gpu buffer (checked by its exporter name; `is_virtio_dma_buf()` is true for every
  dma-buf on CFI kernels such as GKI, and calling `virtio_dma_buf_get_uuid()` on another one panics the guest);
  other dma-bufs are refused with EINVAL.
- `override_timestamp` defaults to on: frames carry the guest's CLOCK_MONOTONIC at arrival, not the host's.

## DMABUF buffers (mothershipOS)
Upstream defines DMABUF buffers as virtio objects in its README but implements them nowhere. Here: QBUF and
PREPARE_BUF of a `V4L2_MEMORY_DMABUF` buffer resolve each plane's fd to a virtio object (`is_virtio_dma_buf()`,
`virtio_dma_buf_get_uuid()`: virtio-gpu exports blobs created with `VIRTGPU_BLOB_FLAG_USE_CROSS_DEVICE`, which
minigbm sets for hardware-use buffers) and send the 16-byte UUIDs after the planes, in the command and the response
(upstream's layout). Other DMABUFs are refused (-EINVAL). The driver holds each plane's DMABUF until the buffer is
queued again or its queue is freed (REQBUFS, close). The device side is virtio-media crate patch 0003 + crosvm
patch 0010 (host/gpu/patches): the UUID resolves through the GPU's resource bridge to the resource's host DMABUF,
which is queued to the host device. Out of tree the GKI config lacks `CONFIG_VIRTIO_DMA_SHARED_BUFFER` (Cuttlefish
loads `virtio_dma_buf.ko`), so `Kbuild` declares it and links against `virtio_dma_buf.symvers` (the stock module's
export CRCs). PixelOS decodes into its gralloc buffers this way (v4l2_codec2, the `c2.v4l2.*` codecs).
