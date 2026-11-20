# mship.net-shared 1.x (group `internet`)

Internet through the host's uplink, shared (NAT). The guest doesn't control the radio or see real networks;
native Wi-Fi is `mship.wifi`.

- **Works:** in PixelOS as group `internet`: DHCP, NAT through the host's uplink, lent and taken back while the VM
  runs.
- **Needs work:**
  - `efi-virt-arm64`: its tap (`ubtap0`) is always NATed, not lendable.
  - The host tap's carrier follows the attachment, but crosvm doesn't forward tap carrier to virtio-net (the guest
    keeps its link up; Android still drops to Wi-Fi once the network loses DHCP/validation).
- **Not implemented:** an RST/ICMP path on NAT release.

**Guest view:** one virtio-net device, which the platform names (`cuttlefish-arm64`: the third NIC, which
Cuttlefish turns into virt_wifi `wlan0`).
- DHCPv4 on the link: one address in a private /24, a default gateway and DNS servers, while attached.
- IPv4 only in 1.x.

**Attached:** traffic is forwarded with NAT through whatever uplink the host uses.
**Release:** forwarding closes and the link loses carrier, so the network disappears in the guest (Android
then uses another network such as native Wi-Fi; without carrier it would keep preferring this "Ethernet").
**Empty:** no carrier.

**History:** 1.0 (2026.1) initial.
