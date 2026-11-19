# mship.modem 1.x

The phone's modem for the guest's own, unmodified radio stack: SIM, network registration, signal, calls, SMS,
mobile data. The guest sees the modem Android's emulators are built against (the Cuttlefish modem simulator's AT
dialect) with the phone's real network behind it. The host keeps what the modem needs to live (its firmware, its
remote file system and NV, the SIM's provisioning, the data path), talks to it over QMI, and makes the data calls
itself, so their packets reach the guest's data NIC. Handle `modem:0` (group `cellular`), exclusive: while a VM
holds it, the host has no mobile data.

## Guest view
- **Control:** an AT command channel (the platform says which; `cuttlefish-arm64`: a vsock stream to the host,
  port 9600, the port Cuttlefish's RIL finds in `androidboot.modem_simulator_ports`) speaking the dialect of
  Cuttlefish's modem simulator (`device/google/cuttlefish/host/commands/modem_simulator`), so the stock
  Cuttlefish RIL works unchanged:
  - **Network, from the modem's state:** `AT+CSQ` (the simulator's 22 values: GSM, LTE RSSI/RSRP/RSRQ/RSSNR,
    WCDMA, NR SS-RSRP/RSRQ/SINR; INT_MAX = not available), `AT+CTEC?` / `=?` / `=<current>,<preferred mask>`
    (technology index: 0 GSM, 1 WCDMA, 5 LTE, 6 NR; setting the mask sets the modem's mode preference),
    `AT+CREG` / `+CGREG` / `+CEREG` (`?` and `=<n>`; with n=2 `<stat>,"<lac/tac>","<cell>",<AcT>`, AcT 3 GSM,
    6 WCDMA, 7 LTE, 11 NR), `AT+COPS?` and the simulator's three-format query (long, short, numeric).
  - **Identity:** `AT+CICCID` (the card's ICCID), `AT+CGSN` (the modem's IMEI).
  - **Unsolicited, on change:** `+CREG:` / `+CGREG:` / `+CEREG:` (per their `<n>`), `+CTEC: <technology>`,
    `+CSQ:` (signal: the modem reports crossings of thresholds around Android's signal levels),
    `%CGFPCCFG: <status>,<bandwidth>,<technology>,<frequency range>,<cid>` per active data call (status 1 the
    serving cell; with an NR leg next to LTE, EN-DC / 5G NSA, a second line with status 2 and technology NR;
    bandwidth and frequency range 0 = unknown).
  - **SMS:** incoming SMS arrive as `+CMT: ,<length>` and the PDU (empty SMSC address `00`, then the TPDU);
    `AT+CNMA` (or `=1`) acknowledges, `AT+CNMA=2` refuses (the network retries later). `AT+CNMI` is answered
    `OK` (delivery is always on). Sending is standard (`AT+CMGS`, PDU up to Ctrl-Z or ESC).
  - **Calls:** `RING` while a call is incoming or waiting, `NO CARRIER` when one ends; the guest lists calls with
    `AT+CLCC` (voice calls only), answers with `ATA`, dials with `ATD<number>;`, hangs up with `ATH` / `AT+CHLD`.
  - **Data calls are the host's.** `AT+CGDCONT=<cid>,"<type>","<apn>",...` defines a context (the host keeps it;
    the modem's own profiles are not touched). `ATD*99***<n>#` starts the context defined last (the simulator's
    RIL dials 1 for it), `AT+CGACT=1,<cid>...` the named ones: answered `OK` or `ERROR`, never `CONNECT`.
    `AT+CGACT=1,0` is `OK` and does nothing; `AT+CGACT=0[,<cid>...]` ends the guest's calls; `AT+CGDATA` is
    `ERROR`. `AT+CGACT?` lists the active ones, `AT+CGDCONT?` gives an active context's address as
    `"<address>/<prefix length>"`, `AT+CGCONTRDP=<cid>` its gateway and DNS servers (space separated).
  - **Everything else** is standard 3GPP TS 27.007 / 27.005 / V.250 as the phone's modem implements it (SIM access
    and PIN, call control, DTMF, USSD, supplementary services, SMS sending, network selection, radio power).
- **Data:** one network device (the platform says which; `cuttlefish-arm64`: eth0, the guest's
  `buried_eth0`). A data call's address is the guest's on that device; its packets go to the mobile network and
  back without NAT. The guest configures the address itself (from the context answers above); the host answers
  ARP for every address on that link, so a gateway, any or none, works. IPv4 in 1.0.
- **No negotiation message:** the AT channel has no hello. Major and minor versions are pinned by the manifest
  slot.

## Caps
`at` (`cuttlefish-modem-simulator`), `data` (`ipv4`), `slots` (SIM slots), `voice` (`signalling`: see the gaps),
`sms` (`3gpp`), `nr` (`nsa`).

## Attached / empty
- **Empty:** the guest talks to a modem without a SIM card and without service: network queries answer "not
  registered", SIM commands `+CME ERROR: 10`, dialing `NO CARRIER`, settings `OK` (Android: radio on, no SIM;
  nothing retries).
- **Attach:** the guest's connection is closed once, so its stack reconnects and initializes the modem itself
  (fresh on attach: no call, no data call of the guest's; the modem's own state, registration and SIM stay as
  they are).
- **Release (any reason):** every call is hung up (`AT+CHUP`), the radio is switched on again (`AT+CFUN=1`: a guest
  may have turned it off), the guest's data calls end, the guest's connection is closed and the slot is empty.
  Packets in flight are lost; nothing the guest started completes.
- **Host default afterwards:** the host's own mobile data comes back (if enabled).
- The SIM's provisioning, the modem's power and its recovery, its SMS routing and its voice domain (circuit
  switched preferred: no IMS) are never the guest's.

**History:** 1.0 (2026.1) initial.

**Status (2026-11-18):** working with the stock Cuttlefish RIL in PixelOS on the nio (O2 SK, LTE): registration,
signal, operator, mobile data on the VM's NIC, SMS both ways, calls both ways (signalling). Committed: nothing yet.

**Implementation:** `mship-modem` (`crates/mship-devices/src/bin/modem`, `mship-modem.service`):
- `main.rs`: the modem's QRTR TFTP service (`/lib/firmware/qcom/sdx55m`, `/var/lib/mothership/modem/readwrite`),
  NV write-back from the kernel's Sahara dumps to `mdm1m9kefs1`/`2` (originals kept in
  `/var/lib/mothership/modem/efs-backup`), SIM provisioning and ICCID (QMI UIM), IMEI (DMS), incoming SMS (WMS:
  transfer routes, acks), call events (VOICE), voice domain CS preferred (NAS, until the modem's next power cycle),
  the host's data (`cell0`, QMI WDS on QMAP mux 1, metric 700), a VM's data calls as WDS sessions by APN on mux
  10+cid (`cellvm<cid>`), routed in the kernel to tap `pxmtap0` (policy routing, proxy ARP, forwarding chain
  `MSHIP_CELL` kept ahead of `mship-vm-net`'s).
- `nas.rs`: the network from QMI NAS (serving system, signal info with thresholds, PLMN names).
- `radio.rs`: the dialect (pure: commands to answers / modem pass-through, state changes to unsolicited results,
  the empty slot).
- `at.rs`: transport (vsock guests, the modem's WWAN AT port).

Control socket `/run/mothership/modem.sock` (`attach <vm>`, `detach <vm> [reason]`, `status` with the network
summary), mothershipd item `cellular` (lent while the VM runs). Kernel side: `motorola_nio/kernel/patches/0025`-
`0027` (MHI for a flashless SDX55, the Sahara image and EFS server), policy routing in the host config. Bring-up
notes: `motorola_nio/firmware/nio-sdx55m/README.md`.

**Gaps (fix before 2026.1):**
- Voice audio: calls are signalled, but the speech path needs the host kernel's voice stack: the vocoder runs on
  the SM8250's ADSP and the SDX55 exchanges speech frames with it over PCIe (a shared mailbox, `voice_mhi`),
  steered over an ADSP-to-SDX55 IPC link (MHI satellite channels). None of that exists in mainline yet; plan in
  `motorola_nio/docs/voice.md`.
- IPv6 and several simultaneous data calls (Android's IMS and MMS APNs) are untested; only IPv4 is routed.
- The guest's RIL never tells the modem when a data call ends (Cuttlefish's RIL only takes its interface down),
  so a call lives until the guest dials again (reused) or the handle is released.
- Cell bandwidth and frequency range in `%CGFPCCFG` are not reported (0); cell info lists are the RIL's own.
- Only the `cuttlefish-arm64` platform has the slot; `efi-virt-arm64` guests (ModemManager) need a transport.
- The caps aren't published (no broker `list-handles` yet).
