# mship.battery 1.x

The phone's battery and charger state, read-only, for every running VM at once. Handle `battery:0`,
sharing `shared`, no group: it isn't lent, and there is no attach or release. The host keeps the fuel gauge,
the charger and every charging decision (charge limit, charging on/off in the host's Settings). Nothing a guest
does reaches them.

## Guest view
- **Device:** a goldfish battery (`compatible = "google,goldfish-battery"`, MMIO + one interrupt; the platform
  says where). The guest's `goldfish_battery` driver turns it into two Linux power_supply devices: `battery`
  (type Battery) and `ac` (type Mains). A change raises the device's interrupt, and the guest gets a
  power_supply uevent.
- **Values** (Linux power_supply units and enums):

| Property | Meaning |
|---|---|
| `ac/online` | 1 = the phone has external power (any host supply online: USB, charger) |
| `present` | 0 = the phone has no battery (the other values are then meaningless) |
| `status` | `Charging`, `Discharging`, `Not charging` (e.g. host charge limit reached), `Full`, `Unknown` |
| `capacity` | 0-100 % |
| `health` | power_supply health (`Good`, `Overheat`, `Cold`, …), `Unknown` if the host doesn't know it |
| `voltage_now` | µV |
| `current_now` | µA, positive = into the battery (Android convention). Negative while charging means the phone uses more than the charger delivers |
| `charge_counter` | µAh remaining |
| `charge_full` | µAh at full charge (the host's learned or design capacity) |
| `temp` | battery temperature in 0.1 °C |

  `cycle_count`, `current_avg`, `voltage_max` and `current_max` read 0 (not provided in 1.0).
- **Updates:** a change in `ac/online`, `present`, `status`, `capacity` or `health`, or a temperature step of
  ≥ 1 °C, reaches the guest within about a second. A change in voltage, current or charge alone is delivered
  at most once per minute, so an idle guest isn't woken more often than Android's own battery polling.
- **Guest side:** any power_supply reader. Android: AOSP's health HAL (`android.hardware.health-service.example`)
  reads it as shipped. Cuttlefish's own health HAL reports fixed values and must not be used.

## Caps
`present`, `technology` (`Li-ion`), `charge_full_design_uah`, `model` in the handle descriptor. The guest
needs none of them to work.

## Attached / empty
Always attached while the VM runs. Every VM sees the same values at the same time. Before the host's first
value arrives (at most a moment after boot), the device reads capacity 50, status `Unknown`. If the host
service restarts, the guest keeps the last values until it is back (reconnect every 2 s).

**History:** 1.0 (2026.1) initial.

**Implementation:** host `crates/mship-devices` (`mship-battery`, `mship-battery.service`: kernel power_supply, uevents + 10 s poll,
one line per update on `/run/mothership/battery.sock`, any number of readers); crosvm `--battery
type=goldfish` with `host/gpu/patches/crosvm/0003-mship-battery.patch` (power monitor `mship` reads the socket; adds
temperature, health and `Full`); guest product `LOCAL_HEALTH_PRODUCT_PACKAGE` = the AOSP health HAL, sysfs label
in `sepolicy/genfs_contexts`. Tested 2026-10-18 on the nio with PixelOS: Android's battery service (`dumpsys battery`) shows the
phone's live level, charging status, AC power, voltage and temperature, the same as the host's power_supply,
and follows changes (27 % → 29 % while charging). A second reader on the socket gets the same lines alongside
the VM. Two VMs running at once is not tested yet.

**Gaps (fix before 2026.1):**
- The sysfs label (`genfs_contexts`) names the device's MMIO address (`20e9000`), which comes from crosvm's
  allocation. A different device set or crosvm version can move it. The guest runs Permissive today, so a
  move would go unnoticed until it runs Enforcing.
- The gfxstream fallback (Cuttlefish's crosvm) has no `mship` monitor, so that path has no battery slot.
- Caps aren't published (no broker `list-handles` yet).
- No battery for `efi-virt-arm64` guests (QEMU) yet.
