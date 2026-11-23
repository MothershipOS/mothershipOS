# mship.wifi 1.x

Native Wi-Fi: the guest's own Wi-Fi stack (supplicant, settings UI, network selection) drives a real
station interface of the phone, restricted to the bands the VM holds. The host keeps the driver, firmware,
regulatory domain and radio power. Handles: one per band (`wifi:2g`, `wifi:5g`) on radios that can run two
stations on two channels, otherwise one `wifi:any`.

- **Works** (while the firmware runs): in PixelOS: scan shows only the attached band; WPA2-PSK connect with the guest's supplicant; DHCP from
  the real LAN; MAC randomization followed by the host; release (deauth, guest disconnected, interface down) and
  re-attach (the guest reconnects in ~3 s). A band is hot-plugged: lent to and taken from a running VM (the bridge
  attaches the band's interface to the VM's tap, and releases it). Both bands to one VM: they ride on one station
  interface (the first band's), the guest sees the channels of both; taking one band away leaves the other.
  The host's station is held disconnected while VMs use Wi-Fi: the firmware crashes adding a second station's peer
  (`Failed to add peer`, then RDDM), on the VM's AP and on another AP of the network alike. From a VM's first connect
  NetworkManager's device is disconnected (`nmcli device disconnect`, no autoconnect), again at each phone resume and
  whenever the host associates anyway (checked every 2 s); it reconnects once no VM has been
  connected or connecting for 60 s. Across the phone's suspend: the bands are released before and attached after.
  Scheduled scans (the guest driver runs them as host scans at the requested interval, and at once when bands come
  back): PixelOS rejoins while asleep after a phone resume (PNO; `MothershipWifiOverlay` sets
  `config_wifi_background_scan_support`). A BSS that doesn't answer
  (connect timeout) is skipped for 5 minutes and the same connect goes to the strongest other BSS of the SSID in the
  VM's bands; the guest sees only the final result (Android disables a network after a few failures).
- **Broken:** recovery from a firmware crash. The QCA6390 firmware can crash during a connection
  (`firmware crashed: MHI_CB_EE_RDDM` after `Failed to add peer` in the kernel log) and ath11k does not recover it;
  reloading `ath11k_pci` does not bring the firmware back (it stops after `qca6390 hw2.0`), only a reboot does. Until
  then neither the host nor a VM has Wi-Fi, and lending a band fails (`ip link set dev mshw5g up: ... transport
  endpoint shutdown`).
- **Needs work:**
  - The host has no Wi-Fi while a VM is connected (firmware limit, above).
  - `hello` carries no host capabilities: the guest advertises PMF-optional and a fixed cipher list; the bridge maps
    PMF-optional to no PMF when the host wiphy lacks it (PMF-required networks fail).
  - No rfkill state: an unattached band is only disabled channels. The host's regulatory domain isn't forwarded
    (`regdom`); the guest uses its own permissive "99" domain.
- **Not implemented:** WPA3-SAE (1.1).

## Guest view
- **Data:** one virtio-net device (the platform says which) carries the band interface's Ethernet II frames
  unchanged, including EAPOL (0x888e). The device's MAC starts with `02:6d:73` (the guest driver finds its
  data device by that prefix) and is the station's initial MAC. The guest may change the station MAC
  (`set_mac`, e.g. Android per-network randomization) while disconnected; the host applies it to the band's
  interface.
- **Control:** a vsock stream to the host (CID 2, port 7710). Messages are a 4-byte big-endian length followed
  by a UTF-8 JSON object `{"op": …, "id": n, …}`. The first message is `hello` (§ Negotiation).
- **Guest driver:** `mship_wifi` (generic, shipped in the image) registers a cfg80211 wiphy on top of the
  virtio-net device, the same way `virt_wifi` wraps a lower device. It declares every band the host listed in
  `hello`; channels of bands not currently attached are flagged disabled, and with no band attached the wiphy
  is rfkill soft-blocked. It creates the station netdev `wlan0` itself on top of the data device. To the
  guest's supplicant it is a FullMAC-style station (`connect`/`disconnect`; cfg80211's SME on the host does
  auth/assoc), while the 4-way handshake and group rekeying run in the guest supplicant over the data path
  and keys are installed through `add_key`. 1.0 covers open, WPA2-PSK (incl. PMF) and OWE. WPA3-SAE is planned
  as 1.1 (external auth: optional new messages).

## Requests (guest → host); each gets `{"op": "result", "id": n, "status": 0 | -errno}`
| op | fields | host action on the band's interface |
|---|---|---|
| `scan` | `ssids[]`, `freqs[]` (must be in attached bands; empty = all attached), `ies` (hex) | NL80211_CMD_TRIGGER_SCAN |
| `abort_scan` | — | NL80211_CMD_ABORT_SCAN |
| `connect` | `ssid`, `bssid` or `bssid_hint`, `freq` or `freq_hint`, `ies` (hex), `auth_type`, `crypto` {wpa_versions, pairwise[], group, akms[], control_port_ethertype}, `mfp` | NL80211_CMD_CONNECT (control port 0x888e); without a BSSID the host picks the best BSS of the SSID in the attached bands |
| `disconnect` | `reason` | NL80211_CMD_DISCONNECT |
| `set_mac` | `mac` (locally administered unicast) | interface down, address, up (refused while connected) |
| `add_key` / `del_key` / `set_default_key` | `index`, `pairwise`, `mac`, `cipher`, `key`, `seq`, `default_mgmt` | NL80211_CMD_NEW_KEY / DEL_KEY / SET_KEY |
| `set_station` | `mac`, `authorized` | NL80211_CMD_SET_STATION (port authorized) |
| `get_station` | `mac` | NL80211_CMD_GET_STATION → `signal_dbm`, `tx_bitrate`, `rx_bitrate`, counters |
| `set_power_save` | `enabled` | NL80211_CMD_SET_POWER_SAVE |
| `set_cqm_rssi` | `threshold_dbm`, `hysteresis_db` | NL80211_CMD_SET_CQM |

## Events (host → guest)
`scan_result` {bss: bssid, freq, signal_mbm, capability, beacon_interval, tsf, ies}, `scan_done` {aborted},
`connect_result` {status, bssid, freq, req_ies, resp_ies, timed_out, bss: {capability, beacon_interval, tsf,
signal_mbm, ies}}, `disconnected` {reason, locally_generated}, `cqm` {rssi_event}, `regdom` {country, rules},
`attached` {bands}, `detached` {reason}.

## Host rules
- Frequencies outside the attached bands are rejected (`-EINVAL`); results are filtered to them.
- No AP, monitor, mesh or P2P. The guest can't change the regulatory domain (it receives `regdom`), the MAC
  address or transmit power.
- Scans may be coalesced or rate-limited. A scan result older than the last attach is never delivered.

## Negotiation
`hello` from the guest: `{"op": "hello", "contract": "mship.wifi", "versions": ["1.0"]}`. Host answer:
`{"op": "hello", "version": "1.0", "bands": ["2g", "5g"], "caps": {…}}` or `{"op": "unsupported"}`.
The bands listed are those the host could ever attach to this slot (fixed for the boot).

**Caps:** `bands`, `max_width_mhz` per band, `security` (`open`, `wpa2-psk`, `wpa3-sae`, `owe`),
`max_scan_ssids`, `channels` {band: [freq MHz…]}.

## Attach and release
- Attach a band: `attached {bands}`, the band's channels become enabled, rfkill unblocked if it was blocked.
  The guest supplicant reconnects by itself.
- Release a band (any reason): the host deauthenticates on that interface, deletes its keys, flushes its scan
  results, closes the frame path, then sends `disconnected {reason: 3 (deauth leaving), locally_generated:
  true}` and `detached {reason}`. Its channels are disabled; with no band left the wiphy is rfkill-blocked.
  Pending requests on that band fail with `-ENETDOWN`.
- Host default afterwards: the interface returns to the host's NetworkManager.

**History:** 1.0 (2026.1) initial.

**Implementation:** guest `guest/drivers/mship_wifi/` (built for the Android GKI 6.12.38-android16-5 and loaded
from the first-stage ramdisk, `pixelOS_16_VM`'s `cargo xtask android-initrd`), host `crates/mship-devices`
(`mship-wifi-bridge`, `mship-wifi-bridge.service`); bands are lent by mothershipd (groups `wifi-2g`, `wifi-5g`).
