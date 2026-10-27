// SPDX-License-Identifier: GPL-2.0
/*
 * mship_wifi: native Wi-Fi for mothershipOS guests (contract mship.wifi 1.x, mothershipOS/docs/contracts/mship.wifi-1.md).
 *
 * The phone's real station interface lives on the host; this driver gives the guest a cfg80211 station
 * ("wlan0") whose operations are forwarded to it:
 *   control: scan / connect / keys / station info over a vsock stream to the host (CID 2, MW_PORT),
 *            messages = struct mw_hdr + netlink-style attributes (mship_wifi_proto.h);
 *   data:    Ethernet frames over a plain virtio-net device whose MAC starts with 02:6d:73 (the host
 *            redirects them to and from the band interface in its kernel), wrapped the way virt_wifi wraps
 *            its lower device.
 * The guest's supplicant drives it like a FullMAC station: connect/disconnect go to the host (cfg80211's SME
 * there does auth/assoc), the 4-way handshake runs in the guest over the data path, keys come back through
 * add_key. Channels of bands the VM doesn't hold are disabled; the wiphy is self-managed (no regulatory
 * changes from the guest).
 */
#include <linux/etherdevice.h>
#include <linux/kthread.h>
#include <linux/module.h>
#include <linux/net.h>
#include <linux/rtnetlink.h>
#include <linux/vm_sockets.h>
#include <net/cfg80211.h>
#include <net/netlink.h>

#include "mship_wifi_proto.h"

static const u8 mw_lower_prefix[3] = { 0x02, 0x6d, 0x73 };

/* ------------------------------------------------------------------ channels */
#define CH2(n) { .band = NL80211_BAND_2GHZ, .center_freq = (n) == 14 ? 2484 : 2407 + 5 * (n), \
		 .hw_value = (n), .max_power = 20, .flags = IEEE80211_CHAN_DISABLED }
#define CH5(n) { .band = NL80211_BAND_5GHZ, .center_freq = 5000 + 5 * (n), .hw_value = (n), \
		 .max_power = 20, .flags = IEEE80211_CHAN_DISABLED }

static struct ieee80211_channel mw_ch2[] = {
	CH2(1), CH2(2), CH2(3), CH2(4), CH2(5), CH2(6), CH2(7), CH2(8), CH2(9), CH2(10), CH2(11), CH2(12),
	CH2(13), CH2(14),
};
static struct ieee80211_channel mw_ch5[] = {
	CH5(36), CH5(40), CH5(44), CH5(48), CH5(52), CH5(56), CH5(60), CH5(64), CH5(100), CH5(104), CH5(108),
	CH5(112), CH5(116), CH5(120), CH5(124), CH5(128), CH5(132), CH5(136), CH5(140), CH5(144), CH5(149),
	CH5(153), CH5(157), CH5(161), CH5(165), CH5(169), CH5(173), CH5(177),
};

static struct ieee80211_rate mw_rates2[] = {
	{ .bitrate = 10 }, { .bitrate = 20 }, { .bitrate = 55 }, { .bitrate = 110 }, { .bitrate = 60 },
	{ .bitrate = 90 }, { .bitrate = 120 }, { .bitrate = 180 }, { .bitrate = 240 }, { .bitrate = 360 },
	{ .bitrate = 480 }, { .bitrate = 540 },
};
static struct ieee80211_rate mw_rates5[] = {
	{ .bitrate = 60 }, { .bitrate = 90 }, { .bitrate = 120 }, { .bitrate = 180 }, { .bitrate = 240 },
	{ .bitrate = 360 }, { .bitrate = 480 }, { .bitrate = 540 },
};

#define MW_HT_CAP { .ht_supported = true, \
	.cap = IEEE80211_HT_CAP_SUP_WIDTH_20_40 | IEEE80211_HT_CAP_SGI_20 | IEEE80211_HT_CAP_SGI_40, \
	.ampdu_factor = 0x3, .ampdu_density = 0x6, \
	.mcs = { .rx_mask = { 0xff, 0xff }, .tx_params = IEEE80211_HT_MCS_TX_DEFINED } }
#define MW_MCS_MAP (IEEE80211_VHT_MCS_SUPPORT_0_9 | IEEE80211_VHT_MCS_SUPPORT_0_9 << 2 | \
	IEEE80211_VHT_MCS_NOT_SUPPORTED << 4 | IEEE80211_VHT_MCS_NOT_SUPPORTED << 6 | \
	IEEE80211_VHT_MCS_NOT_SUPPORTED << 8 | IEEE80211_VHT_MCS_NOT_SUPPORTED << 10 | \
	IEEE80211_VHT_MCS_NOT_SUPPORTED << 12 | IEEE80211_VHT_MCS_NOT_SUPPORTED << 14)

static struct ieee80211_supported_band mw_band2 = {
	.band = NL80211_BAND_2GHZ, .channels = mw_ch2, .n_channels = ARRAY_SIZE(mw_ch2),
	.bitrates = mw_rates2, .n_bitrates = ARRAY_SIZE(mw_rates2), .ht_cap = MW_HT_CAP,
};
static struct ieee80211_supported_band mw_band5 = {
	.band = NL80211_BAND_5GHZ, .channels = mw_ch5, .n_channels = ARRAY_SIZE(mw_ch5),
	.bitrates = mw_rates5, .n_bitrates = ARRAY_SIZE(mw_rates5), .ht_cap = MW_HT_CAP,
	.vht_cap = { .vht_supported = true,
		.cap = IEEE80211_VHT_CAP_MAX_MPDU_LENGTH_11454 | IEEE80211_VHT_CAP_SHORT_GI_80 |
		       IEEE80211_VHT_CAP_RXLDPC,
		.vht_mcs = { .rx_mcs_map = cpu_to_le16(MW_MCS_MAP), .tx_mcs_map = cpu_to_le16(MW_MCS_MAP) } },
};

/* self-managed wiphy: cfg80211 needs a regdomain; which channels are usable is decided by band attach
 * (disabled flags kept in orig_flags survive rule application) and the host's own regulatory domain */
static int mw_set_regd(struct wiphy *wiphy)
{
	struct ieee80211_regdomain *rd = kzalloc(struct_size(rd, reg_rules, 2), GFP_KERNEL);
	int err;

	if (!rd)
		return -ENOMEM;
	rd->n_reg_rules = 2;
	rd->alpha2[0] = '9';	/* "99": built by the driver, not a country */
	rd->alpha2[1] = '9';
	rd->reg_rules[0] = (struct ieee80211_reg_rule)REG_RULE(2402, 2494, 40, 0, 20, 0);
	rd->reg_rules[1] = (struct ieee80211_reg_rule)REG_RULE(5150, 5895, 160, 0, 20, 0);
	err = regulatory_set_wiphy_regd(wiphy, rd);	/* cfg80211 keeps its own copy */
	kfree(rd);
	return err;
}

static const u32 mw_ciphers[] = {
	WLAN_CIPHER_SUITE_TKIP, WLAN_CIPHER_SUITE_CCMP, WLAN_CIPHER_SUITE_CCMP_256, WLAN_CIPHER_SUITE_GCMP,
	WLAN_CIPHER_SUITE_GCMP_256, WLAN_CIPHER_SUITE_AES_CMAC, WLAN_CIPHER_SUITE_BIP_CMAC_256,
	WLAN_CIPHER_SUITE_BIP_GMAC_128, WLAN_CIPHER_SUITE_BIP_GMAC_256,
};

/* ------------------------------------------------------------------ state */
struct mw_netdev {
	struct net_device *upper, *lower;
	bool connecting, connected;
	u8 bssid[ETH_ALEN];
	u32 tx_packets, tx_failed;
};

static struct {
	struct wiphy *wiphy;
	struct mw_netdev *nd;		/* the one wlan0 (rtnl) */
	struct task_struct *thread;
	struct socket *sock;
	struct mutex send_lock;		/* serialises writes and sock changes */
	atomic_t next_id;
	/* one synchronous request at a time (get_station) */
	struct mutex sync_lock;
	u32 sync_id;
	struct completion sync_done;
	int sync_status;
	struct station_info *sync_sinfo;
	/* scan in flight (spinlock: also touched from the rx thread) */
	spinlock_t scan_lock;
	struct cfg80211_scan_request *scan_req;
	u32 scan_id;
	struct work_struct create_work;
	int create_ifindex;
	bool attached;			/* some band is attached: data path open */
} mw;

/* ------------------------------------------------------------------ message building */
struct mw_msg {
	u8 *buf;
	size_t len, cap;
	bool err;
};

static void mw_msg_init(struct mw_msg *m, u16 op, u32 id)
{
	struct mw_hdr *h;

	m->cap = 512;
	m->buf = kzalloc(m->cap, GFP_KERNEL);
	m->err = !m->buf;
	m->len = sizeof(*h);
	if (m->err)
		return;
	h = (struct mw_hdr *)m->buf;
	h->op = cpu_to_le16(op);
	h->id = cpu_to_le32(id);
}

static void mw_put(struct mw_msg *m, u16 type, const void *data, size_t len)
{
	size_t need = m->len + NLA_ALIGN(NLA_HDRLEN + len);
	struct nlattr *a;

	if (m->err)
		return;
	if (need > MW_MAX_MSG) {
		m->err = true;
		return;
	}
	if (need > m->cap) {
		u8 *n = krealloc(m->buf, max(need, m->cap * 2), GFP_KERNEL);

		if (!n) {
			m->err = true;
			return;
		}
		m->buf = n;
		m->cap = max(need, m->cap * 2);
	}
	a = (struct nlattr *)(m->buf + m->len);
	a->nla_len = NLA_HDRLEN + len;
	a->nla_type = type;
	if (len)
		memcpy(nla_data(a), data, len);
	memset((u8 *)nla_data(a) + len, 0, NLA_ALIGN(NLA_HDRLEN + len) - NLA_HDRLEN - len);
	m->len = need;
}

#define mw_put_u8(m, t, v)  do { u8 __v = (v); mw_put(m, t, &__v, 1); } while (0)
#define mw_put_u16(m, t, v) do { u16 __v = (v); mw_put(m, t, &__v, 2); } while (0)
#define mw_put_u32(m, t, v) do { u32 __v = (v); mw_put(m, t, &__v, 4); } while (0)
#define mw_put_s32(m, t, v) do { s32 __v = (v); mw_put(m, t, &__v, 4); } while (0)

/* sends and frees the message; -ENOTCONN when the host isn't there */
static int mw_send(struct mw_msg *m)
{
	struct msghdr msg = { .msg_flags = MSG_NOSIGNAL };
	struct kvec iov;
	int ret;

	if (m->err) {
		kfree(m->buf);
		return -ENOMEM;
	}
	((struct mw_hdr *)m->buf)->len = cpu_to_le32(m->len);
	iov.iov_base = m->buf;
	iov.iov_len = m->len;
	mutex_lock(&mw.send_lock);
	ret = mw.sock ? kernel_sendmsg(mw.sock, &msg, &iov, 1, m->len) : -ENOTCONN;
	mutex_unlock(&mw.send_lock);
	kfree(m->buf);
	return ret < 0 ? ret : 0;
}

static u32 mw_id(void)
{
	return (u32)atomic_inc_return(&mw.next_id);
}

/* ------------------------------------------------------------------ attribute parsing */
struct mw_attrs {
	const struct nlattr *a[__MWA_MAX];
};

static void mw_parse(struct mw_attrs *at, const u8 *data, int len)
{
	const struct nlattr *a;
	int rem;

	memset(at, 0, sizeof(*at));
	nla_for_each_attr(a, (const struct nlattr *)data, len, rem) {
		if (nla_type(a) < __MWA_MAX)
			at->a[nla_type(a)] = a;	/* unknown attributes (newer minor) are ignored */
	}
}

static u32 mw_u32(const struct mw_attrs *at, int t, u32 def)
{
	return at->a[t] && nla_len(at->a[t]) >= 4 ? *(const u32 *)nla_data(at->a[t]) : def;
}

static u16 mw_u16(const struct mw_attrs *at, int t, u16 def)
{
	return at->a[t] && nla_len(at->a[t]) >= 2 ? *(const u16 *)nla_data(at->a[t]) : def;
}

static u64 mw_u64(const struct mw_attrs *at, int t)
{
	u64 v = 0;

	if (at->a[t] && nla_len(at->a[t]) >= 8)
		memcpy(&v, nla_data(at->a[t]), 8);
	return v;
}

static bool mw_flag(const struct mw_attrs *at, int t)
{
	return at->a[t] && nla_len(at->a[t]) >= 1 && *(const u8 *)nla_data(at->a[t]);
}

static const u8 *mw_bin(const struct mw_attrs *at, int t, size_t *len)
{
	*len = at->a[t] ? nla_len(at->a[t]) : 0;
	return at->a[t] ? nla_data(at->a[t]) : NULL;
}

static const u8 *mw_mac(const struct mw_attrs *at, int t)
{
	return at->a[t] && nla_len(at->a[t]) == ETH_ALEN ? nla_data(at->a[t]) : NULL;
}

/* ------------------------------------------------------------------ bands */
/* enable exactly the listed channels (wiphy lock taken here) */
static void mw_set_channels(const u32 *freqs, int n)
{
	struct ieee80211_supported_band *bands[] = { &mw_band2, &mw_band5 };
	int b, c, i;

	wiphy_lock(mw.wiphy);
	for (b = 0; b < ARRAY_SIZE(bands); b++)
		for (c = 0; c < bands[b]->n_channels; c++) {
			struct ieee80211_channel *ch = &bands[b]->channels[c];
			bool on = false;

			for (i = 0; i < n; i++)
				on |= freqs[i] == ch->center_freq;
			if (on)
				ch->flags &= ~IEEE80211_CHAN_DISABLED;
			else
				ch->flags |= IEEE80211_CHAN_DISABLED;
			ch->orig_flags = ch->flags;
		}
	wiphy_unlock(mw.wiphy);
}

static void mw_attach_channels(const struct mw_attrs *at)
{
	size_t len;
	const u8 *f = mw_bin(at, MWA_ATTACHED_CHANNELS, &len);
	mw_set_channels((const u32 *)f, len / 4);
	WRITE_ONCE(mw.attached, len >= 4);
	pr_info("mship_wifi: %zu channels attached\n", len / 4);
}

/* ------------------------------------------------------------------ host events */
static void mw_scan_finish(bool aborted)
{
	struct cfg80211_scan_info info = { .aborted = aborted };
	struct cfg80211_scan_request *req;

	spin_lock_bh(&mw.scan_lock);
	req = mw.scan_req;
	mw.scan_req = NULL;
	spin_unlock_bh(&mw.scan_lock);
	if (req)
		cfg80211_scan_done(req, &info);
}

static void mw_inform_bss(const struct mw_attrs *at, int ies_attr)
{
	struct ieee80211_channel *ch = ieee80211_get_channel(mw.wiphy, mw_u32(at, MWA_FREQ, 0));
	const u8 *bssid = mw_mac(at, MWA_BSSID);
	struct cfg80211_bss *bss;
	const u8 *ies;
	size_t ies_len;

	if (!ch || (ch->flags & IEEE80211_CHAN_DISABLED) || !bssid)
		return;
	ies = mw_bin(at, ies_attr, &ies_len);
	bss = cfg80211_inform_bss(mw.wiphy, ch, CFG80211_BSS_FTYPE_UNKNOWN, bssid, mw_u64(at, MWA_TSF),
				  mw_u16(at, MWA_CAPABILITY, WLAN_CAPABILITY_ESS),
				  mw_u16(at, MWA_BEACON_INT, 100), ies, ies_len,
				  (s32)mw_u32(at, MWA_SIGNAL_MBM, (u32)DBM_TO_MBM(-80)), GFP_KERNEL);
	if (bss)
		cfg80211_put_bss(mw.wiphy, bss);
}

static void mw_connect_result(const struct mw_attrs *at)
{
	struct mw_netdev *nd = mw.nd;
	const u8 *bssid = mw_mac(at, MWA_BSSID);
	size_t req_len, resp_len;
	const u8 *req = mw_bin(at, MWA_REQ_IES, &req_len);
	const u8 *resp = mw_bin(at, MWA_RESP_IES, &resp_len);
	int status = (s32)mw_u32(at, MWA_STATUS, WLAN_STATUS_UNSPECIFIED_FAILURE);

	if (!nd || !nd->connecting)
		return;
	nd->connecting = false;
	if (bssid && status == WLAN_STATUS_SUCCESS)
		mw_inform_bss(at, MWA_BSS_IES);	/* cfg80211 must know the BSS it connected to */
	if (mw_flag(at, MWA_TIMED_OUT)) {
		cfg80211_connect_timeout(nd->upper, bssid, req, req_len, GFP_KERNEL, NL80211_TIMEOUT_UNSPECIFIED);
		return;
	}
	cfg80211_connect_result(nd->upper, bssid, req, req_len, resp, resp_len, (u16)status, GFP_KERNEL);
	if (status == WLAN_STATUS_SUCCESS) {
		nd->connected = true;
		if (bssid)
			ether_addr_copy(nd->bssid, bssid);
		netif_carrier_on(nd->upper);
	}
}

static void mw_disconnected(u16 reason, bool local)
{
	struct mw_netdev *nd = mw.nd;

	if (!nd)
		return;
	if (nd->connecting) {
		nd->connecting = false;
		cfg80211_connect_result(nd->upper, NULL, NULL, 0, NULL, 0, WLAN_STATUS_UNSPECIFIED_FAILURE,
					GFP_KERNEL);
	}
	if (nd->connected) {
		nd->connected = false;
		netif_carrier_off(nd->upper);
		cfg80211_disconnected(nd->upper, reason, NULL, 0, local, GFP_KERNEL);
	}
}

static void mw_handle(u16 op, u32 id, const struct mw_attrs *at)
{
	switch (op) {
	case MW_RESULT:
		if (id && id == READ_ONCE(mw.sync_id)) {
			mw.sync_status = (s32)mw_u32(at, MWA_STATUS, (u32)-EIO);
			if (mw.sync_sinfo && !mw.sync_status) {
				struct station_info *s = mw.sync_sinfo;

				s->filled = BIT_ULL(NL80211_STA_INFO_SIGNAL) | BIT_ULL(NL80211_STA_INFO_TX_BITRATE) |
					    BIT_ULL(NL80211_STA_INFO_RX_BITRATE) | BIT_ULL(NL80211_STA_INFO_TX_PACKETS) |
					    BIT_ULL(NL80211_STA_INFO_RX_PACKETS) | BIT_ULL(NL80211_STA_INFO_TX_FAILED);
				s->signal = (s32)mw_u32(at, MWA_SIGNAL, (u32)-80);
				s->txrate.legacy = mw_u32(at, MWA_TX_BITRATE, 10);
				s->rxrate.legacy = mw_u32(at, MWA_RX_BITRATE, 10);
				s->tx_packets = mw_u32(at, MWA_TX_PACKETS, 0);
				s->rx_packets = mw_u32(at, MWA_RX_PACKETS, 0);
				s->tx_failed = mw_u32(at, MWA_TX_FAILED, 0);
			}
			complete(&mw.sync_done);
		} else if (id && id == READ_ONCE(mw.scan_id) && (s32)mw_u32(at, MWA_STATUS, 0) < 0) {
			mw_scan_finish(true);	/* the host refused the scan */
		}
		break;
	case MW_HELLO_REPLY:
		pr_info("mship_wifi: host speaks mship.wifi %.*s\n",
			at->a[MWA_VERSION] ? nla_len(at->a[MWA_VERSION]) : 0,
			at->a[MWA_VERSION] ? (const char *)nla_data(at->a[MWA_VERSION]) : "");
		mw_attach_channels(at);
		break;
	case MW_ATTACHED:
		mw_attach_channels(at);
		break;
	case MW_DETACHED:
		mw_disconnected(WLAN_REASON_DEAUTH_LEAVING, true);
		mw_set_channels(NULL, 0);
		WRITE_ONCE(mw.attached, false);
		mw_scan_finish(true);
		pr_info("mship_wifi: detached (reason %u)\n", mw_u32(at, MWA_DETACH_REASON, 0));
		break;
	case MW_SCAN_RESULT:
		mw_inform_bss(at, MWA_IES);
		break;
	case MW_SCAN_DONE:
		mw_scan_finish(mw_flag(at, MWA_ABORTED));
		break;
	case MW_CONNECT_RESULT:
		mw_connect_result(at);
		break;
	case MW_DISCONNECTED:
		mw_disconnected(mw_u16(at, MWA_REASON, WLAN_REASON_UNSPECIFIED),
				mw_flag(at, MWA_LOCALLY_GENERATED));
		break;
	case MW_CQM:
		if (mw.nd && mw.nd->connected)
			cfg80211_cqm_rssi_notify(mw.nd->upper, mw_u32(at, MWA_RSSI_EVENT, 0),
						 (s32)mw_u32(at, MWA_SIGNAL, 0), GFP_KERNEL);
		break;
	default:
		break;	/* ops from a newer minor */
	}
}

/* ------------------------------------------------------------------ control connection */
static int mw_recv_all(struct socket *sock, void *buf, size_t len)
{
	struct msghdr msg = {};
	struct kvec iov = { .iov_base = buf, .iov_len = len };
	int r = kernel_recvmsg(sock, &msg, &iov, 1, len, MSG_WAITALL);

	return r == len ? 0 : (r < 0 ? r : -ECONNRESET);
}

static void mw_host_lost(void)
{
	/* the host side went away: nothing is attached any more */
	mw_disconnected(WLAN_REASON_DEAUTH_LEAVING, true);
	mw_set_channels(NULL, 0);
	WRITE_ONCE(mw.attached, false);
	mw_scan_finish(true);
	if (READ_ONCE(mw.sync_id)) {
		mw.sync_status = -ENOTCONN;
		complete(&mw.sync_done);
	}
}

static int mw_thread_fn(void *unused)
{
	struct sockaddr_vm addr = { .svm_family = AF_VSOCK, .svm_cid = MW_HOST_CID, .svm_port = MW_PORT };
	bool logged = false;

	while (!kthread_should_stop()) {
		struct socket *sock;
		struct mw_msg hello;
		struct mw_hdr h;
		u8 *body = NULL;
		int r;

		r = sock_create_kern(&init_net, AF_VSOCK, SOCK_STREAM, 0, &sock);
		if (!r) {
			r = kernel_connect(sock, (struct sockaddr *)&addr, sizeof(addr), 0);
			if (r)
				sock_release(sock);
		}
		if (r) {
			if (!logged)
				pr_info("mship_wifi: host not reachable yet (%d), retrying\n", r);
			logged = true;
			msleep_interruptible(1000);
			continue;
		}
		logged = false;
		mutex_lock(&mw.send_lock);
		mw.sock = sock;
		mutex_unlock(&mw.send_lock);
		pr_info("mship_wifi: connected to host\n");

		mw_msg_init(&hello, MW_HELLO, mw_id());
		mw_put(&hello, MWA_VERSION, MW_VERSION, strlen(MW_VERSION));
		if (mw.nd)
			mw_put(&hello, MWA_MAC, mw.nd->upper->dev_addr, ETH_ALEN);
		mw_send(&hello);

		while (!kthread_should_stop()) {
			u32 len;
			struct mw_attrs at;

			if (mw_recv_all(sock, &h, sizeof(h)))
				break;
			len = le32_to_cpu(h.len);
			if (len < sizeof(h) || len > MW_MAX_MSG)
				break;
			body = kmalloc(len - sizeof(h) + 1, GFP_KERNEL);
			if (!body || mw_recv_all(sock, body, len - sizeof(h))) {
				kfree(body);
				body = NULL;
				break;
			}
			mw_parse(&at, body, len - sizeof(h));
			mw_handle(le16_to_cpu(h.op), le32_to_cpu(h.id), &at);
			kfree(body);
			body = NULL;
		}
		mutex_lock(&mw.send_lock);
		mw.sock = NULL;
		mutex_unlock(&mw.send_lock);
		kernel_sock_shutdown(sock, SHUT_RDWR);
		sock_release(sock);
		pr_info("mship_wifi: host connection lost\n");
		mw_host_lost();
		msleep_interruptible(500);
	}
	return 0;
}

/* synchronous request: waits for MW_RESULT (only get_station needs one) */
static int mw_request_sync(struct mw_msg *m, u32 id, struct station_info *sinfo)
{
	int r;

	mutex_lock(&mw.sync_lock);
	reinit_completion(&mw.sync_done);
	mw.sync_sinfo = sinfo;
	WRITE_ONCE(mw.sync_id, id);
	r = mw_send(m);
	if (!r)
		r = wait_for_completion_timeout(&mw.sync_done, 2 * HZ) ? mw.sync_status : -ETIMEDOUT;
	WRITE_ONCE(mw.sync_id, 0);
	mw.sync_sinfo = NULL;
	mutex_unlock(&mw.sync_lock);
	return r;
}

/* ------------------------------------------------------------------ cfg80211 ops */
static int mw_scan(struct wiphy *wiphy, struct cfg80211_scan_request *req)
{
	struct mw_msg m;
	u32 id = mw_id();
	u8 *ssids;
	u32 *freqs;
	int i, n = 0, r;

	spin_lock_bh(&mw.scan_lock);
	if (mw.scan_req) {
		spin_unlock_bh(&mw.scan_lock);
		return -EBUSY;
	}
	mw.scan_req = req;
	mw.scan_id = id;
	spin_unlock_bh(&mw.scan_lock);

	mw_msg_init(&m, MW_SCAN, id);
	ssids = kmalloc(req->n_ssids * (IEEE80211_MAX_SSID_LEN + 1) + 1, GFP_KERNEL);
	freqs = kmalloc_array(req->n_channels + 1, sizeof(u32), GFP_KERNEL);
	if (ssids && freqs) {
		for (i = 0; i < req->n_ssids; i++) {
			ssids[n++] = req->ssids[i].ssid_len;
			memcpy(ssids + n, req->ssids[i].ssid, req->ssids[i].ssid_len);
			n += req->ssids[i].ssid_len;
		}
		if (n)
			mw_put(&m, MWA_SSIDS, ssids, n);
		for (i = 0; i < req->n_channels; i++)
			freqs[i] = req->channels[i]->center_freq;
		if (req->n_channels)
			mw_put(&m, MWA_FREQS, freqs, req->n_channels * sizeof(u32));
		if (req->ie_len)
			mw_put(&m, MWA_IES, req->ie, req->ie_len);
	} else {
		m.err = true;
	}
	kfree(ssids);
	kfree(freqs);
	r = mw_send(&m);
	if (r) {
		spin_lock_bh(&mw.scan_lock);
		mw.scan_req = NULL;
		spin_unlock_bh(&mw.scan_lock);
	}
	return r;
}

static void mw_abort_scan(struct wiphy *wiphy, struct wireless_dev *wdev)
{
	struct mw_msg m;

	mw_msg_init(&m, MW_ABORT_SCAN, mw_id());
	mw_send(&m);
}

static int mw_connect(struct wiphy *wiphy, struct net_device *dev, struct cfg80211_connect_params *sme)
{
	struct mw_netdev *nd = netdev_priv(dev);
	struct mw_msg m;
	int r;

	if (!READ_ONCE(mw.attached))
		return -ENETDOWN;
	if (!sme->ssid || nd->connecting)
		return -EINVAL;
	mw_msg_init(&m, MW_CONNECT, mw_id());
	mw_put(&m, MWA_SSID, sme->ssid, sme->ssid_len);
	if (sme->bssid)
		mw_put(&m, MWA_BSSID, sme->bssid, ETH_ALEN);
	if (sme->bssid_hint)
		mw_put(&m, MWA_BSSID_HINT, sme->bssid_hint, ETH_ALEN);
	if (sme->channel)
		mw_put_u32(&m, MWA_FREQ, sme->channel->center_freq);
	if (sme->channel_hint)
		mw_put_u32(&m, MWA_FREQ_HINT, sme->channel_hint->center_freq);
	if (sme->ie_len)
		mw_put(&m, MWA_IES, sme->ie, sme->ie_len);
	mw_put_u32(&m, MWA_AUTH_TYPE, sme->auth_type);
	mw_put_u32(&m, MWA_WPA_VERSIONS, sme->crypto.wpa_versions);
	if (sme->crypto.n_ciphers_pairwise)
		mw_put(&m, MWA_CIPHERS_PAIRWISE, sme->crypto.ciphers_pairwise,
		       sme->crypto.n_ciphers_pairwise * sizeof(u32));
	mw_put_u32(&m, MWA_CIPHER_GROUP, sme->crypto.cipher_group);
	if (sme->crypto.n_akm_suites)
		mw_put(&m, MWA_AKMS, sme->crypto.akm_suites, sme->crypto.n_akm_suites * sizeof(u32));
	mw_put_u16(&m, MWA_CONTROL_PORT_ETHERTYPE,
		   sme->crypto.control_port ? be16_to_cpu(sme->crypto.control_port_ethertype) : 0);
	mw_put_u32(&m, MWA_MFP, sme->mfp);
	nd->connecting = true;
	r = mw_send(&m);
	if (r)
		nd->connecting = false;
	return r;
}

static int mw_disconnect(struct wiphy *wiphy, struct net_device *dev, u16 reason)
{
	struct mw_netdev *nd = netdev_priv(dev);
	struct mw_msg m;

	mw_msg_init(&m, MW_DISCONNECT, mw_id());
	mw_put_u16(&m, MWA_REASON, reason);
	mw_send(&m);
	if (nd->connecting) {
		nd->connecting = false;
		cfg80211_connect_result(dev, NULL, NULL, 0, NULL, 0, WLAN_STATUS_UNSPECIFIED_FAILURE, GFP_KERNEL);
	}
	if (nd->connected) {
		nd->connected = false;
		netif_carrier_off(dev);
		cfg80211_disconnected(dev, reason, NULL, 0, true, GFP_KERNEL);
	}
	return 0;
}

static int mw_add_key(struct wiphy *wiphy, struct net_device *dev, int link_id, u8 idx, bool pairwise,
		      const u8 *mac, struct key_params *p)
{
	struct mw_msg m;

	mw_msg_init(&m, MW_ADD_KEY, mw_id());
	mw_put_u8(&m, MWA_KEY_IDX, idx);
	mw_put_u8(&m, MWA_PAIRWISE, pairwise);
	if (mac)
		mw_put(&m, MWA_MAC, mac, ETH_ALEN);
	mw_put_u32(&m, MWA_CIPHER, p->cipher);
	if (p->key_len)
		mw_put(&m, MWA_KEY, p->key, p->key_len);
	if (p->seq_len)
		mw_put(&m, MWA_SEQ, p->seq, p->seq_len);
	return mw_send(&m);
}

static int mw_del_key(struct wiphy *wiphy, struct net_device *dev, int link_id, u8 idx, bool pairwise,
		      const u8 *mac)
{
	struct mw_msg m;

	mw_msg_init(&m, MW_DEL_KEY, mw_id());
	mw_put_u8(&m, MWA_KEY_IDX, idx);
	mw_put_u8(&m, MWA_PAIRWISE, pairwise);
	if (mac)
		mw_put(&m, MWA_MAC, mac, ETH_ALEN);
	return mw_send(&m);
}

static int mw_set_default_key(struct wiphy *wiphy, struct net_device *dev, int link_id, u8 idx, bool uni,
			      bool multi)
{
	struct mw_msg m;

	mw_msg_init(&m, MW_SET_DEFAULT_KEY, mw_id());
	mw_put_u8(&m, MWA_KEY_IDX, idx);
	mw_put_u8(&m, MWA_UNICAST, uni);
	mw_put_u8(&m, MWA_MULTICAST, multi);
	return mw_send(&m);
}

static int mw_set_default_mgmt_key(struct wiphy *wiphy, struct net_device *dev, int link_id, u8 idx)
{
	struct mw_msg m;

	mw_msg_init(&m, MW_SET_DEFAULT_MGMT_KEY, mw_id());
	mw_put_u8(&m, MWA_KEY_IDX, idx);
	return mw_send(&m);
}

static int mw_change_station(struct wiphy *wiphy, struct net_device *dev, const u8 *mac,
			     struct station_parameters *params)
{
	struct mw_msg m;

	if (!(params->sta_flags_mask & BIT(NL80211_STA_FLAG_AUTHORIZED)))
		return 0;
	mw_msg_init(&m, MW_SET_STATION, mw_id());
	mw_put(&m, MWA_MAC, mac, ETH_ALEN);
	mw_put_u8(&m, MWA_AUTHORIZED, !!(params->sta_flags_set & BIT(NL80211_STA_FLAG_AUTHORIZED)));
	return mw_send(&m);
}

static int mw_get_station(struct wiphy *wiphy, struct net_device *dev, const u8 *mac,
			  struct station_info *sinfo)
{
	struct mw_netdev *nd = netdev_priv(dev);
	struct mw_msg m;
	u32 id = mw_id();

	if (!nd->connected || !ether_addr_equal(mac, nd->bssid))
		return -ENOENT;
	mw_msg_init(&m, MW_GET_STATION, id);
	mw_put(&m, MWA_MAC, mac, ETH_ALEN);
	return mw_request_sync(&m, id, sinfo);
}

static int mw_dump_station(struct wiphy *wiphy, struct net_device *dev, int idx, u8 *mac,
			   struct station_info *sinfo)
{
	struct mw_netdev *nd = netdev_priv(dev);

	if (idx || !nd->connected)
		return -ENOENT;
	ether_addr_copy(mac, nd->bssid);
	return mw_get_station(wiphy, dev, mac, sinfo);
}

static int mw_set_power_mgmt(struct wiphy *wiphy, struct net_device *dev, bool enabled, int timeout)
{
	struct mw_msg m;

	mw_msg_init(&m, MW_SET_POWER_SAVE, mw_id());
	mw_put_u8(&m, MWA_ENABLED, enabled);
	return mw_send(&m);
}

static int mw_set_cqm(struct wiphy *wiphy, struct net_device *dev, s32 thold, u32 hyst)
{
	struct mw_msg m;

	mw_msg_init(&m, MW_SET_CQM, mw_id());
	mw_put_s32(&m, MWA_THRESHOLD, thold);
	mw_put_u32(&m, MWA_HYSTERESIS, hyst);
	return mw_send(&m);
}

static const struct cfg80211_ops mw_ops = {
	.scan = mw_scan,
	.abort_scan = mw_abort_scan,
	.connect = mw_connect,
	.disconnect = mw_disconnect,
	.add_key = mw_add_key,
	.del_key = mw_del_key,
	.set_default_key = mw_set_default_key,
	.set_default_mgmt_key = mw_set_default_mgmt_key,
	.change_station = mw_change_station,
	.get_station = mw_get_station,
	.dump_station = mw_dump_station,
	.set_power_mgmt = mw_set_power_mgmt,
	.set_cqm_rssi_config = mw_set_cqm,
};

/* ------------------------------------------------------------------ netdev (virt_wifi-style wrapper) */
static netdev_tx_t mw_xmit(struct sk_buff *skb, struct net_device *dev)
{
	struct mw_netdev *nd = netdev_priv(dev);

	if (!READ_ONCE(mw.attached)) {
		nd->tx_failed++;
		kfree_skb(skb);
		return NETDEV_TX_OK;
	}
	nd->tx_packets++;
	skb->dev = nd->lower;
	return dev_queue_xmit(skb);
}

static int mw_open(struct net_device *dev)
{
	return 0;
}

static int mw_stop(struct net_device *dev)
{
	struct mw_netdev *nd = netdev_priv(dev);

	mw_scan_finish(true);
	if (nd->connected || nd->connecting)
		mw_disconnect(mw.wiphy, dev, WLAN_REASON_DEAUTH_LEAVING);
	netif_carrier_off(dev);
	return 0;
}

static int mw_set_mac(struct net_device *dev, void *p)
{
	struct mw_netdev *nd = netdev_priv(dev);
	struct sockaddr *sa = p;
	struct mw_msg m;

	if (nd->connected || nd->connecting)
		return -EBUSY;
	if (!is_valid_ether_addr(sa->sa_data))
		return -EADDRNOTAVAIL;
	eth_hw_addr_set(dev, sa->sa_data);
	mw_msg_init(&m, MW_SET_MAC, mw_id());
	mw_put(&m, MWA_MAC, sa->sa_data, ETH_ALEN);
	mw_send(&m);	/* the host also gets it in the next hello */
	return 0;
}

static int mw_get_iflink(const struct net_device *dev)
{
	struct mw_netdev *nd = netdev_priv(dev);

	return READ_ONCE(nd->lower->ifindex);
}

static const struct net_device_ops mw_netdev_ops = {
	.ndo_start_xmit = mw_xmit,
	.ndo_open = mw_open,
	.ndo_stop = mw_stop,
	.ndo_set_mac_address = mw_set_mac,
	.ndo_get_iflink = mw_get_iflink,
};

static void mw_destructor(struct net_device *dev)
{
	kfree(dev->ieee80211_ptr);
	dev->ieee80211_ptr = NULL;
}

static void mw_setup(struct net_device *dev)
{
	ether_setup(dev);
	dev->netdev_ops = &mw_netdev_ops;
	dev->needs_free_netdev = true;
	dev->priv_destructor = mw_destructor;
}

static rx_handler_result_t mw_rx(struct sk_buff **pskb)
{
	struct sk_buff *skb = *pskb;
	struct mw_netdev *nd = rcu_dereference(skb->dev->rx_handler_data);

	if (!READ_ONCE(mw.attached))
		return RX_HANDLER_PASS;
	skb = skb_share_check(skb, GFP_ATOMIC);
	if (!skb)
		return RX_HANDLER_CONSUMED;
	*pskb = skb;
	skb->dev = nd->upper;
	/* the station MAC may differ from the lower device's (randomization) */
	if (skb->pkt_type == PACKET_OTHERHOST && ether_addr_equal(eth_hdr(skb)->h_dest, nd->upper->dev_addr))
		skb->pkt_type = PACKET_HOST;
	return RX_HANDLER_ANOTHER;
}

static bool mw_is_lower(struct net_device *dev)
{
	return dev->addr_len == ETH_ALEN && !memcmp(dev->dev_addr, mw_lower_prefix, 3) &&
	       !dev->ieee80211_ptr && dev->netdev_ops != &mw_netdev_ops;
}

/* creates wlan0 on top of the data device (rtnl held) */
static int mw_create(struct net_device *lower)
{
	struct net_device *dev;
	struct mw_netdev *nd;
	int err;

	if (mw.nd)
		return -EEXIST;
	dev = alloc_netdev(sizeof(*nd), "wlan%d", NET_NAME_ENUM, mw_setup);
	if (!dev)
		return -ENOMEM;
	nd = netdev_priv(dev);
	nd->upper = dev;
	nd->lower = lower;
	dev->mtu = lower->mtu;
	eth_hw_addr_inherit(dev, lower);
	SET_NETDEV_DEV(dev, lower->dev.parent);
	dev->ieee80211_ptr = kzalloc(sizeof(*dev->ieee80211_ptr), GFP_KERNEL);
	if (!dev->ieee80211_ptr) {
		free_netdev(dev);
		return -ENOMEM;
	}
	dev->ieee80211_ptr->iftype = NL80211_IFTYPE_STATION;
	dev->ieee80211_ptr->wiphy = mw.wiphy;
	netif_carrier_off(dev);

	err = netdev_rx_handler_register(lower, mw_rx, nd);
	if (err)
		goto free;
	err = register_netdevice(dev);
	if (err)
		goto unhandle;
	err = netdev_upper_dev_link(lower, dev, NULL);
	if (err) {
		unregister_netdevice(dev);
		netdev_rx_handler_unregister(lower);
		return err;
	}
	if (!(lower->flags & IFF_UP))
		dev_change_flags(lower, lower->flags | IFF_UP, NULL);
	mw.nd = nd;
	pr_info("mship_wifi: %s on %s\n", dev->name, lower->name);
	return 0;
unhandle:
	netdev_rx_handler_unregister(lower);
free:
	kfree(dev->ieee80211_ptr);
	dev->ieee80211_ptr = NULL;
	free_netdev(dev);
	return err;
}

static void mw_create_fn(struct work_struct *w)
{
	struct net_device *lower;

	rtnl_lock();
	lower = __dev_get_by_index(&init_net, mw.create_ifindex);
	if (lower && mw_is_lower(lower) && !mw.nd)
		mw_create(lower);
	rtnl_unlock();
}

static int mw_netdev_event(struct notifier_block *nb, unsigned long event, void *ptr)
{
	struct net_device *dev = netdev_notifier_info_to_dev(ptr);

	if (event == NETDEV_REGISTER && !mw.nd && dev_net(dev) == &init_net && mw_is_lower(dev)) {
		mw.create_ifindex = dev->ifindex;	/* can't register a netdev from inside the notifier */
		schedule_work(&mw.create_work);
	} else if (event == NETDEV_UNREGISTER && mw.nd && dev == mw.nd->lower) {
		struct net_device *up = mw.nd->upper;

		netdev_rx_handler_unregister(dev);
		netdev_upper_dev_unlink(dev, up);
		mw.nd = NULL;
		unregister_netdevice(up);
	}
	return NOTIFY_DONE;
}

static struct notifier_block mw_notifier = { .notifier_call = mw_netdev_event };

/* ------------------------------------------------------------------ module */
static int __init mw_init(void)
{
	struct wiphy *w;
	int err;

	mutex_init(&mw.send_lock);
	mutex_init(&mw.sync_lock);
	init_completion(&mw.sync_done);
	spin_lock_init(&mw.scan_lock);
	INIT_WORK(&mw.create_work, mw_create_fn);

	w = wiphy_new(&mw_ops, 0);
	if (!w)
		return -ENOMEM;
	w->interface_modes = BIT(NL80211_IFTYPE_STATION);
	w->signal_type = CFG80211_SIGNAL_TYPE_MBM;
	w->max_scan_ssids = 10;
	w->max_scan_ie_len = 1000;
	w->bands[NL80211_BAND_2GHZ] = &mw_band2;
	w->bands[NL80211_BAND_5GHZ] = &mw_band5;
	w->cipher_suites = mw_ciphers;
	w->n_cipher_suites = ARRAY_SIZE(mw_ciphers);
	w->regulatory_flags |= REGULATORY_WIPHY_SELF_MANAGED;
	wiphy_ext_feature_set(w, NL80211_EXT_FEATURE_MFP_OPTIONAL);
	err = wiphy_register(w);
	if (err) {
		wiphy_free(w);
		return err;
	}
	mw.wiphy = w;
	err = mw_set_regd(w);
	if (err)
		pr_warn("mship_wifi: no regdomain (%d)\n", err);

	err = register_netdevice_notifier(&mw_notifier);	/* also replays already registered devices */
	if (err)
		goto unreg_wiphy;
	mw.thread = kthread_run(mw_thread_fn, NULL, "mship_wifi");
	if (IS_ERR(mw.thread)) {
		err = PTR_ERR(mw.thread);
		goto unreg_notifier;
	}
	return 0;
unreg_notifier:
	unregister_netdevice_notifier(&mw_notifier);
unreg_wiphy:
	wiphy_unregister(w);
	wiphy_free(w);
	return err;
}

static void __exit mw_exit(void)
{
	mutex_lock(&mw.send_lock);
	if (mw.sock)
		kernel_sock_shutdown(mw.sock, SHUT_RDWR);
	mutex_unlock(&mw.send_lock);
	kthread_stop(mw.thread);
	unregister_netdevice_notifier(&mw_notifier);
	flush_work(&mw.create_work);
	rtnl_lock();
	if (mw.nd) {
		struct net_device *up = mw.nd->upper, *low = mw.nd->lower;

		netdev_rx_handler_unregister(low);
		netdev_upper_dev_unlink(low, up);
		mw.nd = NULL;
		unregister_netdevice(up);
	}
	rtnl_unlock();
	wiphy_unregister(mw.wiphy);
	wiphy_free(mw.wiphy);
}

module_init(mw_init);
module_exit(mw_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("mothershipOS native Wi-Fi for VM guests (mship.wifi 1.x)");
MODULE_SOFTDEP("pre: cfg80211");
