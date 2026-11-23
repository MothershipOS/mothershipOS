# mship.gnss 1.x

The phone's satellite positioning (GPS, GLONASS, Galileo, BeiDou, QZSS) as a standard NMEA 0183 receiver. The host
keeps the engine, its unlocking, time and predicted orbits (XTRA); the guest gets positions, accuracies and the
satellites in view, and decides when the receiver runs. Handle `gnss:0` (group `location`), exclusive: one holder at
a time, the host's own clients get the receiver back when no VM holds it.

- **Works:** in PixelOS up to satellites and NMEA: XTRA is injected in parts (Inject XTRA Data); after a start the
  engine lists GPS, GLONASS and Galileo satellites; Android's location requests start and stop the receiver
  (`$PMSHG,START`/`STOP`), and the framework's GNSS status and NMEA listeners get every epoch at 1 Hz. The group can
  be lent and taken back while the VM runs; `mship-gnss` keeps each VM's protocol state over its own restart, and a
  service that starts after the VM or restarts while it runs reaches the guest without a VM restart.
- **Needs work:** no position fix verified (only checked indoors): positions, accuracy and used-in-fix flags are unit-tested
  on synthetic reports, not on the engine's.
- **Not implemented:**
  - An `efi-virt-arm64` slot (planned: a virtio-console port named `mship.gnss`, gpsd with a device hook in the
    Ubuntu image).
  - Raw measurements, navigation messages, geofencing, batching (the HAL reports none of these capabilities).
  - Passing the guest's network location and time to the engine (the host injects its own time; no position
    assistance, no SUPL).

## Guest view
- **Transport:** one virtio-console port carrying text lines (the platform says which; `cuttlefish-arm64`: port 6,
  `/dev/hvc6`, the `/dev/gnss0` Cuttlefish's GNSS HAL path). The guest opens it in raw mode. Lines end in `\r\n`;
  every sentence has an NMEA checksum (`*hh`).
- **Host → guest, while the receiver runs:** one **epoch** at least every interval the guest asked for, as NMEA 0183
  4.11 sentences in this order:
  1. `GSV` for each talker with satellites in view (`GP` GPS + SBAS, `GL` GLONASS, `GA` Galileo, `GB` BeiDou, `GQ`
     QZSS): elevation and azimuth (degrees), C/N0 (dB-Hz, empty while not tracked), the signal id (1, Galileo 7).
     Satellite numbers within the talker: GPS 1-32, SBAS 33-64 (PRN − 87), GLONASS 65-96 (slot + 64), Galileo 1-36,
     BeiDou 1-63, QZSS 1-10 (PRN − 192).
  2. `GNGSA` per system (4.11 system id: 1 GPS/SBAS, 2 GLONASS, 3 Galileo, 4 BeiDou, 5 QZSS) with the satellites
     used in the fix, mode 1 (no fix) / 2 / 3, PDOP, HDOP, VDOP.
  3. `GNGGA`: position, quality 1 with a fix (0 without), satellites used, HDOP, altitude above mean sea level and
     the geoid separation (ellipsoid height = altitude + separation).
  4. `GNGST` (with a fix that has an accuracy): latitude and longitude error σ in metres such that
     √(σlat² + σlon²) is the horizontal accuracy (68 % radius), altitude error σ (68 %). The first four fields are
     empty.
  5. `GNRMC`: UTC time and date, status `A`/`V`, speed (knots), course over ground (degrees true), mode `A`/`N`.
     **`RMC` ends the epoch.**
  Without a fix, epochs still come (satellites, `GGA` quality 0, `RMC` status `V`) with the host's UTC time.
- **Control sentences** (`$PMSHG`, proprietary NMEA, both ways):

  | Sentence | Direction | Meaning |
  |---|---|---|
  | `$PMSHG,HELLO,<v>[,<v>…]` | guest → host | the contract versions the guest speaks (optional: without it, 1.0) |
  | `$PMSHG,HELLO,<v>` / `$PMSHG,HELLO,UNSUPPORTED` | host → guest | the version both speak (highest major, then minor ≤ both) |
  | `$PMSHG,START,<ms>` | guest → host | run the receiver, an epoch at least every `<ms>` (clamped to 1000-60000); a new START changes the interval |
  | `$PMSHG,STOP` | guest → host | the guest needs no positions: the receiver may stop |
  | `$PMSHG,STATE,ATTACHED` | host → guest | the slot holds the receiver (sent on connect and on attach) |
  | `$PMSHG,STATE,EMPTY[,<reason>]` | host → guest | the slot is empty (`USER`, `SCREEN`, `STOP`, `REVOKED`, `GONE`) |

  The guest keeps what it asked for across empty and attached: a START sent while empty takes effect on attach.
  Sentences the receiver doesn't know are ignored by both sides (1.x additions are new optional sentences or
  fields).
- **Guest driver:** none. A GNSS HAL (Android) or gpsd (Linux) reads the port. Power follows the guest: the receiver
  runs only while the holder has asked for it (a gpsd device hook can send START/STOP on activate/deactivate).

## Caps
`constellations` (`gps`, `glonass`, `galileo`, `beidou`, `qzss`, `sbas`), `min_interval_ms` (1000), `assistance`
(`xtra`, `time`) in the handle descriptor.

## Attached / empty
- **Empty:** the guest's sentences are answered (HELLO) and remembered (START/STOP); no epochs come.
- **Attach:** `STATE,ATTACHED`; if the guest asked for positions, the receiver starts (or keeps running) at its
  interval. Fresh on attach: no fix or satellite list from before the attach is sent; the first epoch comes from the
  engine's reports after it.
- **Release (any reason):** epochs stop at once, then `STATE,EMPTY,<reason>`. A fix in progress is not completed.
- **Host default afterwards:** the receiver serves the host's own clients (it runs only while one of them asked for
  it).

**History:** 1.0 (2026.1) initial.

**Implementation:** host `mship-gnss` (`crates/mship-devices/src/bin/gnss/`, `mship-gnss.service`): one QMI LOC client
on the modem's engine (unlocked at every start, the host clock injected when it asks, XTRA downloaded from the servers
it names into /var/lib/mothership/gnss/xtra.bin and injected); NMEA built from the engine's position and satellite
reports (`nmea.rs`, not the engine's own sentences). The VM's stream is `/run/mothership/gnss/<vm>.sock` for VMs
whose manifest has a `mship.gnss` slot, relayed by `mship-launch` to crosvm's console socket
`<vm dir>/slot-gnss.sock` for the VM's whole life (`crates/mship-launch/src/slots.rs`); host clients connect to
`/run/mothership/gnss/host.sock` (same protocol). Control socket `/run/mothership/gnss.sock` (`attach <vm>`,
`detach <vm> [reason]`, `status`) used by mothershipd (group `location`, lent while the VM runs). PixelOS: the HAL
`android.hardware.gnss-service.mship` (pixelOS_17_VM `device/mothership/mothership_vm/gnss`). Test client for the
framework's GNSS status and NMEA listeners: `motorola_nio/archive/prototypes/modem/GpsTest.java`.
