# mship.net-shared 1.x (group `internet`)

Internet through the host's uplink, shared (NAT). The guest doesn't control the radio or see real networks;
native Wi-Fi will be `mship.wifi`.

**Guest view:** one virtio-net device, which the platform names (`cuttlefish-arm64`: the third NIC, which
Cuttlefish turns into virt_wifi `wlan0`).
- DHCPv4 on the link: one address in a private /24, a default gateway and DNS servers, while attached.
- IPv4 only in 1.x.

**Attached:** traffic is forwarded with NAT through whatever uplink the host uses (Wi-Fi today).
**Release:** forwarding closes and the link loses carrier, so the network disappears in the guest (Android
then uses another network such as native Wi-Fi; without carrier it would keep preferring this "Ethernet").
**Empty:** no carrier.

**History:** 1.0 (2026.1) initial.

**Gaps in the current implementation (fix before 2026.1):**
- `efi-virt-arm64` uses QEMU user networking: always connected, not lendable.
- The host tap's carrier follows the attachment, but crosvm doesn't forward tap carrier to virtio-net (the guest
  keeps its link up; Android still drops to Wi-Fi once the network loses DHCP/validation).
- NAT release has no RST/ICMP path yet.
