# mship.usb 1.x

USB devices plugged into the phone, driven by the guest's own USB drivers. Handle `usb:host` (group `usb`),
exclusive: while a VM holds it, every USB device on the phone's host ports is the VM's, including devices
plugged in later. The host keeps the USB controllers, the port's power and role, and the hubs.

## Guest view
- **Controller:** one virtual xHCI controller (the platform says which; `cuttlefish-arm64`: crosvm's xHCI on
  PCI, the guest's `usb1`/`usb2` root hubs). It is always there. With the handle empty it has no devices.
- **Devices:** each USB device is attached as a device on its own root port, with its descriptors and
  speed (up to the controller's) unchanged. The guest sees normal hotplug: a device appears when it is
  attached and disappears when it is unplugged or released. Hubs are not passed through; the devices behind a
  hub are attached one by one. The guest binds its own drivers (HID, storage, audio, network, …), so devices
  the guest has no driver for do nothing.
- **Fresh on attach:** each device is reset before the guest gets it. No configuration, open file or mount
  from the host carries over.
- **Excluded:** devices listed in the host's configuration (a phone's internal USB devices) are never
  attached.

## Caps
`max_speed` (`high`, `super`), `ports` (number of root ports the guest controller has). The device list is
dynamic, not part of the caps.

## Attach and release
- **Attach:** every present device is attached. Devices plugged in later are attached when they have
  enumerated on the host (about 0.2 s).
- **Release (any reason):** every device is detached (the guest sees an unplug; its pending transfers fail),
  then reset on the host. A device unplugged physically while lent is simply gone for the guest.
- **Host default afterwards:** the host's drivers bind to the devices again (the reset makes them start
  fresh), e.g. a USB disk becomes the host's block device.
- **VM stop:** the devices come back to the host the same way.

**History:** 1.0 (2026.1) initial.

**Implementation:** host `host/usb/` (`mship-usb.service`): device list from sysfs, uevents plus a 5 s
rescan, `crosvm usb attach/detach` on the VM's control socket (`<vm dir>/crosvm.sock`; the service opens
the usbdevfs node as root, and crosvm gets the fd), `USBDEVFS_RESET` and `drivers_probe` to give devices
back, control socket `/run/mothership/usb.sock` (`attach <vm>`, `detach <vm> [reason]`, `status`), the owner
kept in `/run/mothership/usb-owner` and attachments recovered from `crosvm usb list` after a service
restart. `mship-handover` item `usb` (lent while the VM runs). Android: the guest fstab lets vold mount
USB disks (`pixelOS_16_VM/patches/device/google/cuttlefish/0001-fstab-vold-usb-disks.patch`).
Tested 2026-10-18 on the nio with PixelOS, using `motorola_nio/archive/prototypes/usb/usb-test-device.sh` (dummy_hcd + a mass-storage
gadget, so the dev link stays up): the guest's `usb-storage` binds and Android mounts the FAT disk as a
public volume, unplug and re-plug while lent re-attach automatically, release and VM stop give the disk back to
the host with its data, the next VM start gets it again, and a service restart neither duplicates nor loses
attachments.

**Gaps (fix before 2026.1):**
- The nio's USB-C port is dual role since 2026-10-18 (TCPM, dwc3 role switch, VBUS out; `motorola_nio/docs/usb.md`),
  but host mode hasn't been tested with a real device or dock yet.
- Per-device assignment (one device to one VM, another to another) is not in 1.0: the whole group moves.
- Isochronous devices (webcams, USB audio) are untested through crosvm's passthrough.
- The VM's crosvm control socket is created by the launcher, not allocated by a broker.
