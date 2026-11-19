//! The network as the modem sees it (QMI NAS): registration, radio technology, cell, operator, signal.
//!
//! Decoded from NAS Get Serving System / Serving System indications (registration, PLMN, cell), Get Signal Info /
//! Signal Info indications (signal, and whether an NR leg is up: EN-DC) and Get PLMN Name (operator names).

use mship_devices::qrtr::QmiMsg;

/// 27.007 registration status (<stat> of +CREG / +CGREG / +CEREG).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reg {
    #[default]
    NotRegistered = 0,
    Home = 1,
    Searching = 2,
    Denied = 3,
    Unknown = 4,
    Roaming = 5,
}

impl Reg {
    pub fn registered(self) -> bool {
        matches!(self, Reg::Home | Reg::Roaming)
    }
}

/// The serving radio access technology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rat {
    Gsm,
    Umts,
    Lte,
    /// NR standalone
    Nr,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plmn {
    pub mcc: u16,
    pub mnc: u16,
    /// the MNC has three digits
    pub mnc3: bool,
    pub long: String,
    pub short: String,
}

impl Plmn {
    pub fn numeric(&self) -> String {
        if self.mnc3 { format!("{:03}{:03}", self.mcc, self.mnc) } else { format!("{:03}{:02}", self.mcc, self.mnc) }
    }
}

/// Signal values in their natural units; None = not measured / not on that technology.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Signal {
    /// dBm
    pub gsm_rssi: Option<i32>,
    /// dBm
    pub umts_rssi: Option<i32>,
    /// dBm
    pub lte_rssi: Option<i32>,
    /// dBm
    pub lte_rsrp: Option<i32>,
    /// dB
    pub lte_rsrq: Option<i32>,
    /// 0.1 dB
    pub lte_snr: Option<i32>,
    /// dBm
    pub nr_rsrp: Option<i32>,
    /// dB
    pub nr_rsrq: Option<i32>,
    /// 0.1 dB
    pub nr_snr: Option<i32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Net {
    /// circuit-switched (voice) registration
    pub cs: Reg,
    /// packet-switched (data) registration
    pub ps: Reg,
    pub rat: Option<Rat>,
    /// LAC, or the TAC on LTE / NR
    pub area: u32,
    pub cell: u32,
    pub plmn: Option<Plmn>,
    pub signal: Signal,
}

impl Net {
    /// An NR leg next to LTE (EN-DC, "5G NSA"): the modem measures NR while serving on LTE.
    pub fn endc(&self) -> bool {
        self.rat == Some(Rat::Lte) && self.signal.nr_rsrp.is_some()
    }

    /// Serving System (message or indication): registration, PLMN, cell. The TLV numbers differ between the two from
    /// 0x19 on (the indication has an extra TLV there).
    pub fn serving_system(&mut self, m: &QmiMsg, indication: bool) {
        let Some(ss) = m.tlvs.get(&0x01) else { return };
        // registration state (0 not registered, 1 registered, 2 searching, 3 denied, 4 unknown), CS / PS attach
        // (1 attached), selected network, radio interfaces
        let (Some(&reg), Some(&cs_attach), Some(&ps_attach)) = (ss.first(), ss.get(1), ss.get(2)) else { return };
        let roaming = m.tlvs.get(&0x10).and_then(|v| v.first()).is_some_and(|r| *r == 0); // 0: roaming on
        let base = match reg {
            1 if roaming => Reg::Roaming,
            1 => Reg::Home,
            2 => Reg::Searching,
            3 => Reg::Denied,
            0 => Reg::NotRegistered,
            _ => Reg::Unknown,
        };
        let attached = |a: u8| if base.registered() && a != 1 { Reg::NotRegistered } else { base };
        self.cs = attached(cs_attach);
        self.ps = attached(ps_attach);
        let radios: &[u8] = ss.get(5..5 + *ss.get(4).unwrap_or(&0) as usize).unwrap_or(&[]);
        // QMI radio interfaces: 4 GSM, 5 UMTS, 8 LTE, 12 NR5G
        self.rat = if radios.contains(&12) {
            Some(Rat::Nr)
        } else if radios.contains(&8) {
            Some(Rat::Lte)
        } else if radios.contains(&5) {
            Some(Rat::Umts)
        } else if radios.contains(&4) {
            Some(Rat::Gsm)
        } else {
            None
        };
        if let Some(p) = m.tlvs.get(&0x12).filter(|p| p.len() >= 5) {
            let mcc = u16::from_le_bytes([p[0], p[1]]);
            let mnc = u16::from_le_bytes([p[2], p[3]]);
            let desc = p.get(5..5 + p[4] as usize).map(|d| String::from_utf8_lossy(d).trim().to_string()).unwrap_or_default();
            let same = self.plmn.as_ref().is_some_and(|o| o.mcc == mcc && o.mnc == mnc);
            if !same {
                self.plmn = Some(Plmn { mcc, mnc, mnc3: mnc > 99, long: desc.clone(), short: desc });
            }
        } else if !base.registered() {
            self.plmn = None;
        }
        let (lac, cid, pcs, tac) = if indication { (0x1d, 0x1e, 0x29, 0x25) } else { (0x1c, 0x1d, 0x27, 0x24) };
        if let (Some(p), Some(plmn)) = (m.tlvs.get(&pcs).filter(|p| p.len() >= 5), self.plmn.as_mut()) {
            plmn.mnc3 = p[4] != 0;
        }
        let u16_at = |t: u8| m.tlvs.get(&t).filter(|v| v.len() >= 2).map(|v| u16::from_le_bytes([v[0], v[1]]) as u32);
        if let Some(c) = m.tlvs.get(&cid).filter(|v| v.len() >= 4) {
            self.cell = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        let area = if matches!(self.rat, Some(Rat::Lte | Rat::Nr)) { u16_at(tac).or(u16_at(lac)) } else { u16_at(lac) };
        if let Some(a) = area {
            self.area = a;
        }
        if !base.registered() {
            self.area = 0;
            self.cell = 0;
        }
    }

    /// Signal Info (message or indication).
    pub fn signal_info(&mut self, m: &QmiMsg) {
        let i8_ = |v: &[u8], o: usize| v.get(o).map(|b| *b as i8 as i32);
        let i16_ = |v: &[u8], o: usize| v.get(o..o + 2).map(|b| i16::from_le_bytes([b[0], b[1]]) as i32);
        let t = |id: u8| m.tlvs.get(&id).map(|v| v.as_slice());
        let mut s = Signal { gsm_rssi: t(0x12).and_then(|v| i8_(v, 0)), umts_rssi: t(0x13).and_then(|v| i8_(v, 0)), ..Signal::default() };
        if let Some(v) = t(0x14) {
            // RSSI i8 dBm, RSRQ i8 dB, RSRP i16 dBm, SNR i16 0.1 dB
            s.lte_rssi = i8_(v, 0);
            s.lte_rsrq = i8_(v, 1);
            s.lte_rsrp = i16_(v, 2);
            s.lte_snr = i16_(v, 4);
        }
        // NR: RSRP i16 dBm, SNR i16 0.1 dB; -32768 = not measured (no NR leg)
        if let Some(v) = t(0x17) {
            s.nr_rsrp = i16_(v, 0).filter(|r| (-160..0).contains(r));
            s.nr_snr = i16_(v, 2).filter(|r| *r != -32768).filter(|_| s.nr_rsrp.is_some());
        }
        s.nr_rsrq = t(0x18).and_then(|v| i16_(v, 0)).filter(|r| (-45..0).contains(r)).filter(|_| s.nr_rsrp.is_some());
        self.signal = s;
    }

    /// Get PLMN Name: the operator's long and short names (EONS) for the current PLMN.
    pub fn plmn_name(&mut self, m: &QmiMsg) {
        let (Some(v), Some(plmn)) = (m.tlvs.get(&0x10), self.plmn.as_mut()) else { return };
        // SPN: encoding u8, len u8, bytes; short: encoding, country initials, spare bits, len, bytes; long: same
        let mut o = 0;
        let mut field = |with_ci: bool| -> Option<String> {
            let enc = *v.get(o)?;
            let skip = if with_ci { 3 } else { 1 };
            let spare = if with_ci { *v.get(o + 2)? } else { 0 };
            let n = *v.get(o + skip)? as usize;
            let b = v.get(o + skip + 1..o + skip + 1 + n)?;
            o += skip + 1 + n;
            Some(decode_name(enc, b, spare))
        };
        let _spn = field(false);
        let short = field(true).unwrap_or_default();
        let long = field(true).unwrap_or_default();
        if !long.is_empty() {
            plmn.long = long;
        }
        if !short.is_empty() {
            plmn.short = short;
        }
    }
}

/// A network name as the modem gives it: encoding 1 = UCS-2 (little-endian), else 8-bit text (the modem has
/// already unpacked GSM 7-bit names; `spare`, the unused bits of the packed form, no longer applies).
fn decode_name(enc: u8, b: &[u8], _spare: u8) -> String {
    let s = if enc == 1 {
        let u: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        String::from_utf16_lossy(&u)
    } else {
        b.iter().map(|c| if c.is_ascii_graphic() || *c == b' ' { *c as char } else { '?' }).collect()
    };
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mship_devices::qrtr::{QMI_INDICATION, qmi_encode, tlv};

    fn msg(tlvs: &[Vec<u8>]) -> QmiMsg {
        QmiMsg::decode(&qmi_encode(QMI_INDICATION, 0, 0x24, tlvs)).unwrap()
    }

    #[test]
    fn serving_system_lte() {
        let mut n = Net::default();
        let m = msg(&[
            tlv(0x01, &[1, 1, 1, 2, 1, 8]),                 // registered, CS+PS attached, 3GPP, LTE
            tlv(0x10, &[1]),                                // roaming off
            tlv(0x12, &[231, 0, 6, 0, 6, b'R', b'a', b'd', b'o', b's', b't']),
            tlv(0x1e, &0x0123_4567u32.to_le_bytes()),       // cell
            tlv(0x25, &0x2a3bu16.to_le_bytes()),            // TAC
            tlv(0x29, &[231, 0, 6, 0, 0]),                  // 2-digit MNC
        ]);
        n.serving_system(&m, true);
        assert_eq!((n.cs, n.ps, n.rat), (Reg::Home, Reg::Home, Some(Rat::Lte)));
        assert_eq!((n.area, n.cell), (0x2a3b, 0x0123_4567));
        let p = n.plmn.as_ref().unwrap();
        assert_eq!((p.numeric().as_str(), p.long.as_str()), ("23106", "Radost"));
        // searching: not registered, no cell
        n.serving_system(&msg(&[tlv(0x01, &[2, 0, 0, 2, 0])]), true);
        assert_eq!((n.cs, n.rat, n.cell, n.plmn.is_none()), (Reg::Searching, None, 0, true));
    }

    #[test]
    fn signal_and_endc() {
        let mut n = Net { rat: Some(Rat::Lte), ..Net::default() };
        let lte = [-63i8 as u8, -12i8 as u8, 0x9e, 0xff, 0x82, 0x00]; // RSRP -98, SNR 13.0 dB
        n.signal_info(&msg(&[tlv(0x14, &lte), tlv(0x17, &[0x00, 0x80, 0x00, 0x80])]));
        assert_eq!((n.signal.lte_rsrp, n.signal.lte_rsrq, n.signal.lte_snr), (Some(-98), Some(-12), Some(130)));
        assert!(!n.endc());
        n.signal_info(&msg(&[tlv(0x14, &lte), tlv(0x17, &[0xa6, 0xff, 0x64, 0x00])])); // NR RSRP -90, SNR 10 dB
        assert_eq!((n.signal.nr_rsrp, n.signal.nr_snr), (Some(-90), Some(100)));
        assert!(n.endc());
    }

    #[test]
    fn eons() {
        let mut n = Net { plmn: Some(Plmn { mcc: 231, mnc: 6, ..Plmn::default() }), ..Net::default() };
        let mut v = vec![0, 6];
        v.extend(b"Radost");
        for name in [&b"O2"[..], &b"O2 Slovakia"[..]] {
            v.extend([0, 0, 6, name.len() as u8]);
            v.extend(name);
        }
        n.plmn_name(&msg(&[tlv(0x10, &v)]));
        let p = n.plmn.unwrap();
        assert_eq!((p.short.as_str(), p.long.as_str()), ("O2", "O2 Slovakia"));
    }

    #[test]
    fn names() {
        assert_eq!(decode_name(1, &[b'O', 0, b'2', 0], 0), "O2");
        assert_eq!(decode_name(0, b"Radost", 6), "Radost");
    }
}
