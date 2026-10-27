# mship.modem 1.x

The phone's modem for the guest's own radio stack: SIM, network registration, calls, SMS, mobile data. The guest
talks to the real modem in 3GPP AT commands. The host keeps what the modem needs to live (its firmware, its
remote file system and NV, the SIM's provisioning, the data path) and makes the data calls itself, so their
packets reach the guest's data NIC. Handle `modem:0` (group `cellular`), exclusive: while a VM holds it, the host
has no mobile data.

## Guest view
- **Control:** an AT command channel (the platform says which; `cuttlefish-arm64`: a vsock stream to the host,
  port 9600, the port Cuttlefish's RIL finds in `androidboot.modem_simulator_ports`). Commands are 3GPP TS
  27.007 / 27.005 and V.250 as the phone's modem implements them: they reach the modem unchanged, and its
  responses and unsolicited result codes come back unchanged, except:
  - **Data calls are the host's.** `ATD*99***<cid>#` and `AT+CGACT=1,<cid>` start a data call on the context
    (defined by the guest with `AT+CGDCONT`; a dial right after `AT+CGDCONT=<cid>,...` uses that context) and
    are answered `OK` or `ERROR`, never `CONNECT` (no PPP). `AT+CGACT=1,0` is answered `OK` and does
    nothing. `AT+CGACT=0[,<cid>...]` ends the guest's data calls only, never contexts the host or the network
    keeps (IMS, the default bearer). `AT+CGDATA` is answered `ERROR`.
  - **An active context shows its connection.** In answers to `AT+CGDCONT?` its PDP address is
    `"<address>/<prefix length>"`; in answers to `AT+CGCONTRDP` an empty gateway is filled in.
  - SMS PDUs (after `AT+CMGS=` / `AT+CMGW=`, up to Ctrl-Z or ESC) pass unchanged.
- **Data:** one network device (the platform says which; `cuttlefish-arm64`: eth0, the guest's
  `buried_eth0`). A data call's address is the guest's on that device; its packets go to the mobile network and
  back without NAT. The guest configures the address itself (from the context answers above); the host answers
  ARP for every address on that link, so a gateway, any or none, works. IPv4 in 1.0.
- **No negotiation message:** the AT channel has no hello. Major and minor versions are pinned by the manifest
  slot.

## Caps
`at` (`3gpp-27.007`), `data` (`ipv4`), `slots` (SIM slots), `voice` (`signalling`: see the gaps).

## Attached / empty
- **Empty:** the guest can connect, but its bytes are dropped and nothing answers (Android: radio unavailable).
- **Attach:** the guest's connection is closed once, so its stack reconnects and initializes the modem itself
  (fresh on attach: no call, no data call of the guest's; the modem's own state, registration and SIM stay as
  they are).
- **Release (any reason):** every call is hung up (`AT+CHUP`), the radio is switched on again (`AT+CFUN=1`: a guest
  may have turned it off), the guest's data calls end, the guest's connection is closed and the slot is empty. Packets in flight are lost; nothing the guest started completes.
- **Host default afterwards:** the host's own mobile data comes back (if enabled).
- The SIM's provisioning, the modem's power and its recovery are never the guest's.

**History:** 1.0 (2026.1) initial.

**Status (2026-10-26): in progress, paused.** Working on the phone: the modem boots, stays up, serves its files,
writes its NV back, registers on LTE with a SIM, host data proven by hand (before `mship-modem`). The slot was
tested only with a loopback vsock client (empty / attached / released behave as specified). Not yet done:
- SIM back in: automatic provisioning on insert and `cell0` data through `mship-modem` (only tested without a card).
- PixelOS with telephony (`TARGET_NO_TELEPHONY` removed, bootconfig `modem_simulator_ports`): build started on the
  buildfarm, not packaged or booted; then the new PixelOS package installed (`mship-system vm-install`) and the RIL against the real modem
  (registration, SMS, a data call on eth0, calls).
- Committed: nothing yet.

**Implementation:** `mship-modem` (`crates/mship-devices/src/bin/modem`, `mship-modem.service`): the modem's
QRTR TFTP service (`/lib/firmware/qcom/sdx55m`, `/var/lib/mothership/modem/readwrite`), NV write-back from the
kernel's Sahara dumps to `mdm1m9kefs1`/`2` (originals kept in `/var/lib/mothership/modem/efs-backup`), SIM
provisioning (QMI UIM), the host's data (`cell0`, QMI WDS on QMAP mux 1, metric 700). For a VM: the AT channel
to the modem's WWAN AT port, data calls as QMI sessions on mux 10+cid (`cellvm<cid>`), routed in the kernel to
tap `pxmtap0` (policy routing, proxy ARP, forwarding chain `MSHIP_CELL`). Control socket
`/run/mothership/modem.sock` (`attach <vm>`, `detach <vm> [reason]`, `status`), mothershipd item `cellular`
(lent while the VM runs). Kernel side: `motorola_nio/kernel/patches/0025`-`0027` (MHI for a flashless SDX55, the
Sahara image and EFS server). Bring-up notes: `motorola_nio/firmware/nio-sdx55m/README.md`.

**Gaps (fix before 2026.1):**
- Voice: calls are signalled (dial, ring, answer, hang up), but call audio runs between the modem and the
  phone's audio DSP, which is the host's; it needs `mship.audio` and a voice route.
- IPv6 and several simultaneous data calls (Android's IMS and MMS APNs) are untested; only IPv4 is routed.
- The guest's RIL never tells the modem when a data call ends (Cuttlefish's RIL only takes its interface down),
  so a call lives until the guest dials again (reused) or the handle is released.
- Cuttlefish's RIL also sends modem-simulator commands (`AT+CTEC`, CDMA queries) that the real modem answers
  `ERROR`.
- Only the `cuttlefish-arm64` platform has the slot; `efi-virt-arm64` guests (ModemManager) need a transport.
- The caps aren't published (no broker `list-handles` yet).
