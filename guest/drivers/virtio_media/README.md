# Guest virtio-media driver (host Venus → guest V4L2)

The guest-side driver that binds crosvm's `--v4l2-proxy` device and re-exposes the host's Venus V4L2
decoder as a V4L2 decoder *inside* a guest: built into the Linux guests' kernel (`CONFIG_MSHIP_VIRTIO_MEDIA`, in
`drivers/mothership`, see `../Kconfig`; the Ubuntu VM's `cargo xtask guest-kernel`), a module (`virtio-media.ko`)
for PixelOS's GKI kernel (the PixelOS VM's `cargo xtask gki-module`). The VMs build it from this source. It is the generic
device-brokering bridge for hardware video decode (see `[[vm-device-brokering]]`,
`motorola_nio/docs/video.md`).

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
Host side: our media-enabled crosvm (`cargo xtask gpu`, `--features …,media`) +
`--v4l2-proxy=/dev/<venus-decoder>` from the launcher.

## Our changes
- `virtio_media_device_poll()`: EPOLLERR only when neither queue can make progress (as `v4l2_m2m_poll_for_data()`).
- A session the host reports an error for (`VIRTIO_MEDIA_EVT_ERROR`) is marked broken instead of being freed in the
  event handler: its file still refers to it, and freeing it there crashed a waiting `VIDIOC_DQBUF` (NULL pointer
  dereference) and then hung the process's exit in `virtio_media_vma_close`. A broken session wakes its waiters,
  fails every ioctl with `-EIO`, polls as EPOLLERR, and is closed and freed when its file is released.
- A DQBUF event for a buffer the guest hasn't queued (still in flight when STREAMOFF cleared the queue) is dropped:
  it used to be added to `pending_dqbufs`, and once the app queued that buffer again its real event added it a second
  time (list corruption, guest panic; Android's camera app restarting its stream hit it).
- `override_timestamp` defaults to on: frames carry the guest's CLOCK_MONOTONIC at arrival, not the host's.
