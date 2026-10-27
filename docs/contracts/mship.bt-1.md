# mship.bt 1.x

Native Bluetooth: the guest's own Bluetooth stack (pairing, profiles, settings UI) drives the phone's real
controller at the HCI level. The host keeps the transport driver, firmware download, BD address and power.
Handle: `bt:hci0` (group `bluetooth`), exclusive: while a VM holds it, the host has no Bluetooth.

## Guest view
- **Transport:** one virtio-console port (the platform says which; `cuttlefish-arm64`: port 5, `/dev/hvc5`)
  carries HCI packets in H4 framing: a packet indicator byte (1 command, 2 ACL, 3 SCO, 4 event, 5 ISO)
  followed by the packet as defined by the Bluetooth Core spec. Packets go to and come from the controller
  unchanged. Nothing is filtered or rewritten, and nothing is added except the Hardware Error events below.
- **Guest driver:** none. The guest's HCI HAL opens the console in raw mode (the Cuttlefish
  `android.hardware.bluetooth` HAL does this as shipped, `vendor.ser.bt-uart=/dev/hvc5`).
- **Controller:** whatever the phone has. The guest learns it like on real hardware (Read Local Version,
  Read Local Supported Features/Commands, Read BD_ADDR). The public address is the phone's.
- **No negotiation message:** H4 has no hello. Major and minor versions are pinned by the manifest slot.
  1.x additions can only be ones a plain H4 host ignores.

## Caps
`transport` (`h4`), `features` (`br-edr`, `le`), `address` (public BD_ADDR) in the handle descriptor. The
guest reads the same values from the controller.

## Attached / empty
- **Empty:** packets from the guest are dropped. No controller answers, so the guest's stack can't start
  (Android: Bluetooth stays off or turning on, and the stack gives up after its retries).
- **Attach:** the controller has been reset when the guest first uses it (fresh on attach; no links, no
  scan, no host state). If the guest's last command while empty was `HCI_Reset` (a HAL waiting to
  initialize), the controller answers it, and the stack comes up then. Otherwise the guest receives a
  Hardware Error event (code 0x00), and its stack restarts and initializes the controller itself.
- **Release (any reason):** the host sends `HCI_Reset` to the controller (every ACL/SCO/ISO link and
  scan/advertising set ends; no half-done operation completes), takes the controller back, then sends the
  guest a Hardware Error event (code 0x00). In-flight packets in both directions are lost. Afterwards the slot
  is empty.
- **Host default afterwards:** the controller returns to the host's stack (BlueZ), which restores its own
  power state and pairings.

**History:** 1.0 (2026.1) initial.

**Implementation:** host `host/bt-bridge/` (`mship-bt-bridge.service`): an `HCI_CHANNEL_USER` socket on
`hci0` while attached, the guest's stream on `/run/mothership/bt/<vm>.sock` (crosvm `unix-stream` serial),
control socket `/run/mothership/bt-bridge.sock` (`attach <vm>`, `detach <vm> [reason]`, `status`) used by
`mship-handover` (hardware item `bluetooth`, lent while the VM runs). Tested 2026-10-18 on the nio with PixelOS
(QCA6390 over UART): Android turns Bluetooth on with the phone's address and discovers nearby devices.
Release gives BlueZ the controller back, powered; re-attach brings Android's Bluetooth back on without user
action.

**Gaps (fix before 2026.1):**
- Pairing and profiles (A2DP, HFP, LE Audio, HID) are not tested. SCO over HCI depends on the controller's
  routing. Voice paths routed to PCM/I2S stay with the host's audio (needs `mship.audio`).
- The caps aren't published (no broker `list-handles` yet).
- Pairing keys are per side: devices paired with the host don't work in the VM and vice versa (both use the
  same address, so a remote may try the other side's keys and fail until re-paired).
- If the bridge restarts while a VM runs, crosvm doesn't reconnect the console. The VM's Bluetooth stays empty
  until the VM restarts.
- The QCA firmware is downloaded again on every attach and release (~1.1 s), because the kernel re-runs the
  driver's setup when the user channel opens.
