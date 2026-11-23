# mship.sensors 1.x

The phone's motion and environment sensors for every running VM at once: accelerometer, gyroscope, magnetometer,
proximity, ambient light. Handle `sensors:0`, shared (like `mship.battery`): there is no group to lend, no attach or
release; every VM whose manifest has the slot gets every sensor, at the rate it asks for. The sensors, their DSP,
calibration and fusion inputs stay on the host.

- **Works:** in PixelOS: accelerometer, gyroscope, magnetometer, proximity and light, calibrated and uncalibrated;
  the host streams only the sensors Android has active. `mship-sensors` keeps each VM's protocol state over its own
  restart, and a service that starts after the VM or restarts while it runs reaches the guest without a VM restart.
- **Needs work:** the HAL's sensor descriptions are its own (names "Goldfish …", fixed ranges and resolutions, 100 Hz
  at most), not the phone's parts.
- **Not implemented:**
  - Step counter / detector, significant motion, tilt and the phone's gestures (the DSP has them; the HAL has no
    slots for them); sensor temperature, SAR, hall.
  - An `efi-virt-arm64` slot (planned: IIO devices or iio-sensor-proxy in the Ubuntu image).

## Guest view (`cuttlefish-arm64`)
- **Transport:** virtio-console ports the stock Cuttlefish sensors HAL (the Android emulator's goldfish multi-HAL)
  opens: one console for everything, port 13 (`/dev/hvc13`, Android 16), or a control console, port 18
  (`/dev/hvc18`: `list-sensors` and its reply, the commands), and a data console, port 19 (`/dev/hvc19`: the
  events), from Android 17. The host offers both layouts; the guest's HAL uses its own. The guest needs no code of its own; PixelOS patches the HAL so that it
  enables only the sensors Android has active (stock goldfish enables all of them once and filters inside the guest,
  which would keep the phone's sensor DSP streaming).
- **Framing:** every message is `u32 command` (bits 0-30, 0) `| response` (bit 31), `u32 payload size`, payload
  (little endian; Cuttlefish's transport frame).
- **Guest → host** (the emulator's sensor commands, text):
  - `list-sensors` → answered with the mask of offered sensors, decimal, by the HAL's handles: 0 accelerometer,
    1 gyroscope, 2 magnetic field, 5 proximity, 6 light, 9 magnetic field uncalibrated, 10 gyroscope uncalibrated,
    17 accelerometer uncalibrated (mask 132711 in 1.0).
  - `set:<name>:<0|1>` disables/enables one (`acceleration`, `gyroscope`, `magnetic-field`, `proximity`, `light`,
    `magnetic-field-uncalibrated`, `gyroscope-uncalibrated`, `acceleration-uncalibrated`).
  - `set-delay:<ms>`: one interval for the guest's streaming sensors (clamped to 10-1000 ms). A guest should enable
    only what it uses: every enabled sensor is streamed from the hardware.
  - `time:<ns>`: accepted, unused.
- **Host → guest**, while enabled (streaming at most once per interval, on-change sensors when they change):
  `acceleration:x:y:z` (m/s²), `gyroscope:x:y:z` (rad/s, bias removed), `magnetic:x:y:z` (µT, hard-iron bias
  removed), the `-uncalibrated` forms with the raw values, `proximity:<cm>` (0 near, 1 far), `light:<lux>`. Axes
  are Android's (the device frame: x right, y up, z out of the screen). Events are stamped when the guest receives
  them (no `guest-sync`).
- Android builds rotation vector, game rotation vector, gravity, linear acceleration and orientation from these
  itself (SensorService fusion).

## Caps
`sensors` (the list above with each one's part: accel/gyro STMicro LSM6DS3C, magnetometer Memsic MMC56x3x,
proximity/light Vishay VCNL36866 on the nio), `max_rate_hz` (100).

## Shared, always attached
- Every VM with the slot gets the same data; one VM's rate or enable never changes another's (the host streams each
  sensor at the highest rate any VM asks for and sends every VM its own rate).
- The sensors are read only; nothing a guest sends reaches the hardware except its rate and on/off.
- If the sensor DSP restarts, events pause and resume by themselves; the guest sees a gap, nothing else.

**History:** 1.0 (2026.1) initial.

**Implementation:** host `mship-sensors` (`crates/mship-devices/src/bin/sensors/`, `mship-sensors.service`): brings up the
sensor DSP's sensor process (files over FastRPC, host/hexagonrpc), reads the sensors from its SEE service (QMI 400,
`see.rs`), calibrated motion = the stream minus the DSP's gyro_cal / mag_cal bias, and speaks the guest protocol
(`goldfish.rs`) on `/run/mothership/sensors/<vm>.sock` for VMs whose manifest has a `mship.sensors` slot, relayed by
`mship-launch` to crosvm's console socket `<vm dir>/slot-sensors.sock` for the VM's whole life
(`crates/mship-launch/src/slots.rs`). `status` on `/run/mothership/sensors.sock`. The phone side: the port's notes
([nio sensors](https://github.com/MothershipOS/motorola_nio/blob/main/docs/sensors.md)).
