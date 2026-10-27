# guest/: the guest side of the contracts

The drivers that implement mothershipOS's contracts inside a guest, as source. mothershipOS builds no VM: each VM
is a repository of its own and builds these into its image from here (the checkout next to it):
[pixelOS_16_VM](https://github.com/MothershipOS/pixelOS_16_VM) as modules for PixelOS's GKI kernel,
[ubuntu_26.04_desktop_VM](https://github.com/MothershipOS/ubuntu_26.04_desktop_VM) into its kernel and packages.

| Dir | What |
|---|---|
| `drivers/mship_wifi/` | native Wi-Fi (`mship.wifi`): the guest's wlan device, per band |
| `drivers/virtio_media/` | V4L2 devices through virtio-media: hardware video decode, the cameras (`mship.camera`) |
| `drivers/venus-vaapi/` | the VA-API driver for Linux guests on the host's Venus decoder (a package) |
| `drivers/Kconfig`, `drivers/Makefile` | building them into a Linux kernel (`drivers/mothership`) |

How a VM gets onto the phone: [docs/vm-packages.md](../docs/vm-packages.md).
