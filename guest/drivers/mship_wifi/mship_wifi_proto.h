/* SPDX-License-Identifier: GPL-2.0 OR MIT */
/*
 * mship.wifi 1.x wire protocol (mothershipOS/docs/contracts/mship.wifi-1.md), shared by the guest driver and, by value,
 * by the host bridge (host/wifi-bridge/mship_wifi_proto.py must use the same numbers).
 *
 * Transport: vsock stream, guest -> host CID 2, port MW_PORT.
 * Message:   struct mw_hdr (little endian) followed by netlink-style attributes
 *            (struct nlattr {u16 nla_len; u16 nla_type;} + payload, each padded to 4 bytes).
 * Numbers are append-only: never reuse or renumber an op or attribute.
 */
#ifndef MSHIP_WIFI_PROTO_H
#define MSHIP_WIFI_PROTO_H

#define MW_PORT		7710
#define MW_HOST_CID	2
#define MW_VERSION	"1.0"
#define MW_MAX_MSG	(64 * 1024)

struct mw_hdr {
	__le32 len;	/* whole message, header included */
	__le16 op;
	__le16 flags;	/* 0 */
	__le32 id;	/* request id; replies carry the request's id, events 0 */
} __packed;

enum mw_op {
	/* guest -> host requests (each gets MW_RESULT with the same id) */
	MW_HELLO = 1,
	MW_SCAN = 2,
	MW_ABORT_SCAN = 3,
	MW_CONNECT = 4,
	MW_DISCONNECT = 5,
	MW_SET_MAC = 6,
	MW_ADD_KEY = 7,
	MW_DEL_KEY = 8,
	MW_SET_DEFAULT_KEY = 9,
	MW_SET_DEFAULT_MGMT_KEY = 10,
	MW_SET_STATION = 11,
	MW_GET_STATION = 12,
	MW_SET_POWER_SAVE = 13,
	MW_SET_CQM = 14,

	/* host -> guest */
	MW_RESULT = 64,		/* STATUS (+ station info for GET_STATION) */
	MW_HELLO_REPLY = 65,	/* VERSION, CHANNELS, ATTACHED_CHANNELS */
	MW_SCAN_RESULT = 66,	/* BSSID, FREQ, SIGNAL_MBM, CAPABILITY, BEACON_INT, TSF, IES */
	MW_SCAN_DONE = 67,	/* ABORTED */
	MW_CONNECT_RESULT = 68,	/* STATUS(u16 as s32), BSSID, FREQ, REQ_IES, RESP_IES, TIMED_OUT, bss fields */
	MW_DISCONNECTED = 69,	/* REASON, LOCALLY_GENERATED */
	MW_CQM = 70,		/* RSSI_EVENT, SIGNAL */
	MW_ATTACHED = 71,	/* ATTACHED_CHANNELS */
	MW_DETACHED = 72,	/* DETACH_REASON */
};

enum mw_attr {
	MWA_UNSPEC,
	MWA_VERSION = 1,		/* string */
	MWA_STATUS = 2,			/* s32 */
	MWA_SSID = 3,			/* binary */
	MWA_BSSID = 4,			/* 6 bytes */
	MWA_BSSID_HINT = 5,		/* 6 bytes */
	MWA_FREQ = 6,			/* u32 MHz */
	MWA_FREQ_HINT = 7,		/* u32 MHz */
	MWA_IES = 8,			/* binary */
	MWA_AUTH_TYPE = 9,		/* u32 enum nl80211_auth_type */
	MWA_WPA_VERSIONS = 10,		/* u32 */
	MWA_CIPHERS_PAIRWISE = 11,	/* u32[] */
	MWA_CIPHER_GROUP = 12,		/* u32 */
	MWA_AKMS = 13,			/* u32[] */
	MWA_CONTROL_PORT_ETHERTYPE = 14,/* u16 (0 = no control port) */
	MWA_MFP = 15,			/* u32 enum nl80211_mfp */
	MWA_REASON = 16,		/* u16 */
	MWA_MAC = 17,			/* 6 bytes */
	MWA_KEY_IDX = 18,		/* u8 */
	MWA_PAIRWISE = 19,		/* u8 bool */
	MWA_CIPHER = 20,		/* u32 */
	MWA_KEY = 21,			/* binary */
	MWA_SEQ = 22,			/* binary */
	MWA_UNICAST = 23,		/* u8 bool */
	MWA_MULTICAST = 24,		/* u8 bool */
	MWA_AUTHORIZED = 25,		/* u8 bool */
	MWA_ENABLED = 26,		/* u8 bool */
	MWA_THRESHOLD = 27,		/* s32 dBm */
	MWA_HYSTERESIS = 28,		/* u32 dB */
	MWA_SIGNAL_MBM = 29,		/* s32 */
	MWA_CAPABILITY = 30,		/* u16 */
	MWA_BEACON_INT = 31,		/* u16 */
	MWA_TSF = 32,			/* u64 */
	MWA_BSS_IES = 33,		/* binary */
	MWA_REQ_IES = 34,		/* binary */
	MWA_RESP_IES = 35,		/* binary */
	MWA_TIMED_OUT = 36,		/* u8 bool */
	MWA_LOCALLY_GENERATED = 37,	/* u8 bool */
	MWA_RSSI_EVENT = 38,		/* u32 enum nl80211_cqm_rssi_threshold_event */
	MWA_CHANNELS = 39,		/* u32[] MHz: every channel this slot may ever get */
	MWA_ATTACHED_CHANNELS = 40,	/* u32[] MHz: channels of the bands attached now */
	MWA_SSIDS = 41,			/* repeated [u8 len][ssid] */
	MWA_FREQS = 42,			/* u32[] MHz */
	MWA_ABORTED = 43,		/* u8 bool */
	MWA_TX_BITRATE = 44,		/* u32, 100 kbit/s */
	MWA_RX_BITRATE = 45,		/* u32, 100 kbit/s */
	MWA_TX_PACKETS = 46,		/* u32 */
	MWA_RX_PACKETS = 47,		/* u32 */
	MWA_TX_FAILED = 48,		/* u32 */
	MWA_SIGNAL = 49,		/* s32 dBm */
	MWA_DETACH_REASON = 50,		/* u32: 1 user, 2 screen, 3 stop, 4 revoked, 5 gone */
	__MWA_MAX,
};

#endif
