# virtio_gpu (PixelOS guest)

The guest kernel's own virtio-gpu driver (`drivers/gpu/drm/virtio` of the GKI tree at the shipped commit, named in
`upstream`), patched (`*.patch`), built as the drop-in `virtio-gpu.ko` for the stock kernel:
`cargo xtask gki-module <gki dir> <aosp dir> virtio_gpu` (pixelOS_16_VM, docs/gki-modules.md) copies the sources
into the build, applies the patches and links against `virtio_dma_buf.symvers` (the CRCs of the stock
virtio_dma_buf.ko exports, read from its `__kcrctab`). All 236 imported symbol CRCs match the stock module's.
`cargo xtask android-initrd` puts it into the first-stage ramdisk in place of Cuttlefish's.

- `0001-fence-every-primary-flush.patch`: page flips complete when the frame is on screen (with crosvm patch 0009 and
  mship-presenter): zero-copy scanout without tearing, real vblank pacing.
- `0002-no-default-context-on-native-hosts.patch`: on a native-context host (DRM capset, no virgl capset), a file
  that never called CONTEXT_INIT (the HWC) gets no context, so its buffer imports and closes no longer send a failing
  attach/detach each (crosvm `InvalidContextId`, thousands per minute).
