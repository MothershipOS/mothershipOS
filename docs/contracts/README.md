# Slot contracts

One file per contract and major version (`<contract>-<major>.md`). A contract states what the **guest** can rely
on, in terms of devices and data the guest sees. Host internals (backends, VMMs, sockets) are not part of it.
Rules for changing a contract: [hardware-handles.md §3](../hardware-handles.md). Registry: [compat.md](../compat.md).

Every contract file has the same parts: a status block (**Works**, **Needs work**, **Broken**, **Not implemented**,
only those that apply), guest view, caps, attached/empty behaviour, minor-version history, and the host
implementation. Everything listed under Needs work, Broken or Not implemented must be resolved before the release
that introduces the version ships.

| Contract | What the guest gets |
|---|---|
| [mship.display](mship.display-1.md) | one virtio-gpu scanout at the display's mode |
| [mship.display-power](mship.display-power-1.md) | the guest turns its screen off/on (Cuttlefish kmsg lines) |
| [mship.touch](mship.touch-1.md) | the touchscreen as a virtio-input multi-touch device |
| [mship.keys](mship.keys-1.md) | volume and power keys as a virtio-input keyboard |
| [mship.gpu](mship.gpu-1.md) | virtio-gpu 3D: DRM native context, cross-domain, virgl, gfxstream |
| [mship.net-shared](mship.net-shared-1.md) | internet through the host's uplink (NAT), group `internet` |
| [mship.wifi](mship.wifi-1.md) | native Wi-Fi, one band per handle; broken: recovery from a firmware crash |
| [mship.bt](mship.bt-1.md) | the Bluetooth controller at the HCI level (H4) |
| [mship.usb](mship.usb-1.md) | the USB devices plugged into the phone, on a virtual xHCI |
| [mship.battery](mship.battery-1.md) | battery and charger state, read-only, every VM |
| [mship.modem](mship.modem-1.md) | the modem: SIM, calls, SMS, mobile data (AT channel + data NIC) |
| [mship.camera](mship.camera-1.md) | the cameras as V4L2 capture devices (virtio-media) |
| [mship.audio](mship.audio-1.md) | speaker, earpiece, headset and mics as the guest's own devices (virtio-snd, one stream each, jacks) |
| [mship.gnss](mship.gnss-1.md) | the GNSS receiver: NMEA epochs, the guest starts and stops it (virtio-console) |
| [mship.sensors](mship.sensors-1.md) | accelerometer, gyroscope, magnetometer, proximity, light for every VM at once (virtio-console) |
| [mship.power](mship.power-1.md) | system power votes: sleep permission and clock floors (vsock); implemented, not yet in the registry |
| [mship.fingerprint](mship.fingerprint-1.md) | a fingerprint reader for one VM (enrol/match in the host's secure world, vsock 9700); non-crypto unlock |
