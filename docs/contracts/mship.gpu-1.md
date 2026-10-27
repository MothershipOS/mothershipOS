# mship.gpu 1.x

**Guest view:** the virtio-gpu device of the platform with 3D enabled. The context types listed in the slot's
`params.context_types` are offered as capsets; the guest picks among them.

| context type | guest userspace |
|---|---|
| `drm` | DRM native context: the guest's own Mesa for the host GPU (on nio: freedreno + turnip, `freedreno-kmds=virtio`) drives the host kernel driver (virglrenderer msm renderer). Near-native speed |
| `cross-domain` | buffer allocation by the host (gralloc/minigbm `cross_domain` backend): exact layout + modifier of host GPU buffers, importable by `drm` contexts and scanned out by `mship.display` |
| `virgl` | Mesa virgl (OpenGL ES / OpenGL) |
| `gfxstream-gles` | gfxstream guest GLES (Android `ro.hardware.egl=emulation`) |
| `gfxstream-composer` | gfxstream host composition |
| `gfxstream-vulkan` | reserved in 1.x: not offered (host Vulkan through gfxstream is not reliable on Adreno/turnip) |

Android guests: `drm` + `cross-domain` with Mesa as `ro.hardware.egl=mesa` / `vulkan=freedreno`; the guest's
minigbm must not take getparam 8 as CREATE_GUEST_HANDLE (GKI: EXPLICIT_DEBUG_NAME). The GPU-specific Mesa build is
the one hardware-specific part of the guest image (it ships drivers for every GPU family mothershipOS hosts).

**Shared handle:** no attach/detach. GPU time is shared by the host scheduler.
**Required:** a requested context type the host can't offer makes the slot unsupported (required → the VM
won't start).

**History:** 1.0 (2026.1) initial (`drm` and `cross-domain` added before the release).
**Gaps:** host cached-coherent BOs are not offered (SM8250 lacks stage-2 FWB); `virgl` gralloc on GKI guests
cannot learn host layouts (use `cross-domain`).
