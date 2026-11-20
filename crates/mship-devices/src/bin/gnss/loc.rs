//! The GNSS engine as QMI LOC (service 0x10) offers it: one client for the whole life of the engine (a session ends
//! with its client), events decoded into [`Event`]s. Requests answer twice: a response, then the result as an
//! indication with the same message id.

use std::time::Duration;

use mship_devices::qrtr::{QmiClient, QmiMsg, tlv, tlv_u8, tlv_u32};

use crate::nmea::{Fix, Sat, System};

pub const SERVICE: u32 = 0x10;
const TIMEOUT: Duration = Duration::from_secs(10);
const SESSION: u8 = 1;

mod msg {
    pub const REGISTER_EVENTS: u16 = 0x0021;
    pub const START: u16 = 0x0022;
    pub const STOP: u16 = 0x0023;
    pub const POSITION_REPORT: u16 = 0x0024;
    pub const SV_INFO: u16 = 0x0025;
    pub const TIME_REQUEST: u16 = 0x0028;
    pub const XTRA_REQUEST: u16 = 0x0029;
    pub const ENGINE_STATE: u16 = 0x002b;
    pub const INJECT_PREDICTED_ORBITS: u16 = 0x0035;
    pub const INJECT_XTRA_DATA: u16 = 0x00a7;
    pub const PREDICTED_ORBITS_SOURCE: u16 = 0x0036;
    pub const INJECT_UTC_TIME: u16 = 0x0038;
    pub const SET_ENGINE_LOCK: u16 = 0x003a;
}

/// event registration mask (TLV 0x01 of Register Events)
const EVENTS: u64 = 1 << 0 // position report
    | 1 << 1 // satellite info
    | 1 << 4 // inject time request
    | 1 << 5 // inject predicted orbits (XTRA) request
    | 1 << 7; // engine state

#[derive(Debug)]
pub enum Event {
    /// a final position: the fix, its UTC time (ms), the satellites used
    Position(Fix, Option<i64>, Vec<(System, u16)>),
    /// the satellites in view (none marked used: that comes with positions)
    Sats(Vec<Sat>),
    /// the engine wants the time
    TimeRequest,
    /// the engine wants predicted orbits
    XtraRequest,
    Engine(bool),
}

pub struct Engine {
    c: QmiClient,
}

/// The satellite numbering the engine uses (GPS 1-32, SBAS 33-64 or 120-158, GLONASS 65-96, QZSS 193-200,
/// BeiDou 201-263, Galileo 301-336) as (system, NMEA id).
pub fn satellite(id: u16) -> Option<(System, u16)> {
    Some(match id {
        1..=32 => (System::Gps, id),
        33..=64 => (System::Sbas, id),
        65..=96 => (System::Glonass, id),
        120..=158 => (System::Sbas, id - 87),
        193..=200 => (System::Qzss, id - 192),
        201..=263 => (System::Beidou, id - 200),
        301..=336 => (System::Galileo, id - 300),
        _ => return None,
    })
}

/// A satellite of the SV info list, by its system (1 GPS, 2 Galileo, 3 SBAS, 4 BeiDou, 5 GLONASS) and number:
/// some engines number within the system (GLONASS slot 1-24, BeiDou 1-63, Galileo 1-36) instead of globally.
pub fn satellite_in(system: u32, id: u16) -> Option<(System, u16)> {
    Some(match (system, id) {
        (1, 1..=32) => (System::Gps, id),
        (1, 193..=200) => (System::Qzss, id - 192),
        (2, 1..=36) => (System::Galileo, id),
        (2, 301..=336) => (System::Galileo, id - 300),
        (3, 33..=64) => (System::Sbas, id),
        (3, 120..=158) => (System::Sbas, id - 87),
        (4, 1..=63) => (System::Beidou, id),
        (4, 201..=263) => (System::Beidou, id - 200),
        (5, 1..=24) => (System::Glonass, id + 64),
        (5, 65..=96) => (System::Glonass, id),
        _ => return None,
    })
}

fn f32_at(v: &[u8], at: usize) -> Option<f32> {
    v.get(at..at + 4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).filter(|x| x.is_finite())
}

fn f32_tlv(m: &QmiMsg, t: u8) -> Option<f32> {
    m.tlvs.get(&t).and_then(|v| f32_at(v, 0))
}

fn f64_tlv(m: &QmiMsg, t: u8) -> Option<f64> {
    m.tlvs.get(&t).and_then(|v| v.get(..8)).map(|b| f64::from_le_bytes(b.try_into().unwrap()))
}

/// A radius at confidence `pct` % of a circular normal error, scaled to 68 % (what the contract reports).
fn to_68(r: f32, pct: Option<u8>) -> f32 {
    let k = |p: f64| (-2.0 * (1.0 - p).ln()).sqrt();
    match pct.filter(|p| (1..100).contains(p)) {
        Some(p) => (r as f64 * k(0.68) / k(p as f64 / 100.0)) as f32,
        None => r,
    }
}

/// Position Report: a final fix only (session status 0) that has a position.
fn position(m: &QmiMsg) -> Option<Event> {
    if m.u32(0x01)? != 0 {
        return None;
    }
    let (lat, lon) = (f64_tlv(m, 0x10)?, f64_tlv(m, 0x11)?);
    let dop = m.tlvs.get(&0x24);
    let fix = Fix {
        lat,
        lon,
        alt_msl: f32_tlv(m, 0x1b),
        alt_ellipsoid: f32_tlv(m, 0x1a),
        h_acc: f32_tlv(m, 0x12).map(|r| to_68(r, m.tlvs.get(&0x16).and_then(|v| v.first().copied()))),
        v_acc: f32_tlv(m, 0x1c).map(|r| to_68(r, m.tlvs.get(&0x1d).and_then(|v| v.first().copied()))),
        speed: f32_tlv(m, 0x18),
        course: f32_tlv(m, 0x20),
        pdop: dop.and_then(|v| f32_at(v, 0)),
        hdop: dop.and_then(|v| f32_at(v, 4)),
        vdop: dop.and_then(|v| f32_at(v, 8)),
    };
    let utc = m.tlvs.get(&0x25).and_then(|v| v.get(..8)).map(|b| u64::from_le_bytes(b.try_into().unwrap()) as i64);
    // satellites used: count u8, ids u16
    let used = m.tlvs.get(&0x2c).map(|v| {
        let n = v.first().copied().unwrap_or(0) as usize;
        (0..n).filter_map(|i| v.get(1 + 2 * i..3 + 2 * i)).filter_map(|b| satellite(u16::from_le_bytes([b[0], b[1]]))).collect()
    });
    Some(Event::Position(fix, utc, used.unwrap_or_default()))
}

/// GNSS SV Info: count u8, then 28-byte entries (valid mask u32, system u32, id u16, health u8, status u32,
/// navigation data u8, elevation f32, azimuth f32, C/N0 f32).
fn sats(m: &QmiMsg) -> Option<Event> {
    let v = m.tlvs.get(&0x10)?;
    let n = *v.first()? as usize;
    let mut out = vec![];
    for i in 0..n {
        let e = v.get(1 + 28 * i..1 + 28 * (i + 1))?;
        let valid = u32::from_le_bytes(e[..4].try_into().unwrap());
        let sys = u32::from_le_bytes(e[4..8].try_into().unwrap());
        let Some((system, id)) = satellite_in(sys, u16::from_le_bytes([e[8], e[9]])) else { continue };
        let has = |bit: u32| valid & bit != 0;
        out.push(Sat {
            system,
            id,
            elevation: f32_at(e, 16).filter(|_| has(1 << 5)),
            azimuth: f32_at(e, 20).filter(|_| has(1 << 6)),
            cn0: f32_at(e, 24).filter(|c| has(1 << 7) && *c > 0.0),
            used: false,
        });
    }
    Some(Event::Sats(out))
}

impl Engine {
    /// A client for the engine at (node, port): events registered, the engine unlocked (it ships locked for
    /// sessions the phone starts: "MI GPS is locked"; stock Android clears it at every boot).
    pub fn open(node: u32, port: u32) -> Result<Self, String> {
        let mut c = QmiClient::new(SERVICE, node, port).map_err(|e| e.to_string())?;
        c.request(msg::REGISTER_EVENTS, &[tlv(0x01, &EVENTS.to_le_bytes())], TIMEOUT)?;
        let mut e = Self { c };
        // lock type 1: none
        match e.c.request_ind(msg::SET_ENGINE_LOCK, &[tlv_u32(0x01, 1)], TIMEOUT) {
            Ok(i) if i.u32(0x01) == Some(0) => {}
            Ok(i) => return Err(format!("engine lock: status {:?}", i.u32(0x01))),
            Err(err) => return Err(format!("engine lock: {err}")),
        }
        Ok(e)
    }

    pub fn fd(&self) -> std::os::fd::RawFd {
        self.c.fd()
    }

    /// Periodic fixes, one at least every `interval_ms` (a running session is replaced).
    pub fn start(&mut self, interval_ms: u32) -> Result<(), String> {
        // 0x10 fix recurrence (1 periodic), 0x12 intermediate reports (2 off: final fixes only), 0x13 interval
        self.c.request(msg::START, &[tlv_u8(0x01, SESSION), tlv_u32(0x10, 1), tlv_u32(0x12, 2), tlv_u32(0x13, interval_ms)], TIMEOUT)
            .map(|_| ())
    }

    pub fn stop(&mut self) -> Result<(), String> {
        self.c.request(msg::STOP, &[tlv_u8(0x01, SESSION)], TIMEOUT).map(|_| ())
    }

    /// The host's UTC time, if its clock can be trusted, with its uncertainty.
    pub fn inject_time(&mut self, utc_ms: u64, uncertainty_ms: u32) -> Result<(), String> {
        match self.c.request_ind(msg::INJECT_UTC_TIME, &[tlv(0x01, &utc_ms.to_le_bytes()), tlv_u32(0x02, uncertainty_ms)], TIMEOUT)?.u32(0x01) {
            Some(0) => Ok(()),
            s => Err(format!("status {s:?}")),
        }
    }

    /// Where the engine wants its predicted orbits from: (servers, max file size, max part size).
    pub fn xtra_source(&mut self) -> Result<(Vec<String>, u32, u32), String> {
        let i = self.c.request_ind(msg::PREDICTED_ORBITS_SOURCE, &[], TIMEOUT)?;
        if i.u32(0x01) != Some(0) {
            return Err(format!("status {:?}", i.u32(0x01)));
        }
        let sizes = i.tlvs.get(&0x10).filter(|v| v.len() >= 8);
        let max_file = sizes.map(|v| u32::from_le_bytes(v[..4].try_into().unwrap())).unwrap_or(0);
        let max_part = sizes.map(|v| u32::from_le_bytes(v[4..8].try_into().unwrap())).unwrap_or(0);
        Ok((i.tlvs.get(&0x11).map(|v| strings(v)).unwrap_or_default(), max_file, max_part))
    }

    /// Predicted orbits (an XTRA file), in parts of at most `max_part` bytes, each confirmed before the next. Newer
    /// engines take them as Inject XTRA Data (0xA7), older ones as Inject Predicted Orbits Data (0x35, format XTRA).
    pub fn inject_xtra(&mut self, data: &[u8], max_part: u32) -> Result<(), String> {
        match self.inject_parts(msg::INJECT_XTRA_DATA, data, max_part) {
            Err(e) if e.contains("error 0x5e") || e.contains("error 0x47") => self.inject_parts(msg::INJECT_PREDICTED_ORBITS, data, max_part),
            r => r,
        }
    }

    fn inject_parts(&mut self, id: u16, data: &[u8], max_part: u32) -> Result<(), String> {
        let part = (max_part as usize).clamp(256, 60_000);
        let parts = data.len().div_ceil(part);
        for (n, chunk) in data.chunks(part).enumerate() {
            let mut d = (chunk.len() as u16).to_le_bytes().to_vec();
            d.extend_from_slice(chunk);
            let mut tlvs = vec![tlv_u32(0x01, data.len() as u32), tlv(0x02, &(parts as u16).to_le_bytes()), tlv(0x03, &(n as u16 + 1).to_le_bytes()), tlv(0x04, &d)];
            if id == msg::INJECT_PREDICTED_ORBITS {
                tlvs.push(tlv_u32(0x10, 0)); // format: XTRA
            }
            let i = self.c.request_ind(id, &tlvs, TIMEOUT)?;
            if i.u32(0x01) != Some(0) {
                return Err(format!("{id:#06x} part {}/{parts}: status {:?}", n + 1, i.u32(0x01)));
            }
        }
        Ok(())
    }

    /// What arrived (without blocking). Err: the engine's service is gone.
    pub fn events(&mut self) -> std::io::Result<Vec<Event>> {
        let ind = self.c.indications()?;
        if std::env::var_os("MSHIP_GNSS_DEBUG").is_some() {
            for m in &ind {
                let tlvs: Vec<String> = m.tlvs.iter().map(|(t, v)| format!("{t:#04x}:{}", v.len())).collect();
                eprintln!("loc indication {:#06x} [{}]", m.msg, tlvs.join(" "));
            }
        }
        Ok(ind.iter().filter_map(|m| match m.msg {
            msg::POSITION_REPORT => position(m),
            msg::SV_INFO => sats(m),
            msg::TIME_REQUEST => Some(Event::TimeRequest),
            msg::XTRA_REQUEST => Some(Event::XtraRequest),
            msg::ENGINE_STATE => m.u32(0x01).map(|s| Event::Engine(s == 1)),
            _ => None,
        }).collect())
    }
}

/// count u8, then strings each prefixed by a u8 length
fn strings(v: &[u8]) -> Vec<String> {
    let mut out = vec![];
    let mut at = 1;
    for _ in 0..v.first().copied().unwrap_or(0) {
        let Some(&n) = v.get(at) else { break };
        let Some(s) = v.get(at + 1..at + 1 + n as usize) else { break };
        out.push(String::from_utf8_lossy(s).trim_end_matches('\0').to_string());
        at += 1 + n as usize;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mship_devices::qrtr::{QMI_INDICATION, qmi_encode};

    #[test]
    fn numbering() {
        assert_eq!(satellite(7), Some((System::Gps, 7)));
        assert_eq!(satellite(70), Some((System::Glonass, 70)));
        assert_eq!(satellite(123), Some((System::Sbas, 36)));
        assert_eq!(satellite(195), Some((System::Qzss, 3)));
        assert_eq!(satellite(219), Some((System::Beidou, 19)));
        assert_eq!(satellite(312), Some((System::Galileo, 12)));
        assert_eq!(satellite(0), None);
        assert_eq!(satellite_in(4, 15), Some((System::Beidou, 15)));
        assert_eq!(satellite_in(5, 3), Some((System::Glonass, 67)));
        assert_eq!(satellite_in(1, 15), Some((System::Gps, 15)));
    }

    #[test]
    fn confidence() {
        assert!((to_68(10.0, Some(68)) - 10.0).abs() < 1e-4);
        assert!((to_68(10.0, Some(95)) - 6.167).abs() < 0.01); // 95 % radius 2.448 sigma, 68 % 1.510 sigma
        assert_eq!(to_68(10.0, None), 10.0);
    }

    #[test]
    fn decode() {
        let f = |v: f32| v.to_le_bytes().to_vec();
        let pos = qmi_encode(QMI_INDICATION, 0, msg::POSITION_REPORT, &[
            tlv_u32(0x01, 0), tlv_u8(0x02, 1), tlv(0x10, &48.5f64.to_le_bytes()), tlv(0x11, &(-11.25f64).to_le_bytes()),
            tlv(0x12, &f(4.0)), tlv(0x1b, &f(500.0)), tlv(0x24, &[f(1.5), f(0.9), f(1.2)].concat()),
            tlv(0x25, &1_000u64.to_le_bytes()), tlv(0x2c, &[2, 5, 0, 0x2c, 1]),
        ]);
        let Some(Event::Position(fix, utc, used)) = position(&QmiMsg::decode(&pos).unwrap()) else { panic!() };
        assert_eq!((fix.lat, fix.lon, fix.h_acc, fix.alt_msl, fix.hdop, utc), (48.5, -11.25, Some(4.0), Some(500.0), Some(0.9), Some(1000)));
        assert_eq!(used, vec![(System::Gps, 5)]); // 0x12c = 300: not a satellite
        let busy = qmi_encode(QMI_INDICATION, 0, msg::POSITION_REPORT, &[tlv_u32(0x01, 1), tlv(0x10, &48.5f64.to_le_bytes())]);
        assert!(position(&QmiMsg::decode(&busy).unwrap()).is_none());
        // one GLONASS satellite: valid mask with elevation, azimuth, C/N0
        let mut e = vec![1];
        e.extend((0xe3u32).to_le_bytes());
        e.extend(5u32.to_le_bytes());
        e.extend(70u16.to_le_bytes());
        e.push(1);
        e.extend(2u32.to_le_bytes());
        e.push(0);
        e.extend([f(30.0), f(200.0), f(33.5)].concat());
        let sv = qmi_encode(QMI_INDICATION, 0, msg::SV_INFO, &[tlv(0x10, &e)]);
        let Some(Event::Sats(s)) = sats(&QmiMsg::decode(&sv).unwrap()) else { panic!() };
        assert_eq!(s, vec![Sat { system: System::Glonass, id: 70, elevation: Some(30.0), azimuth: Some(200.0), cn0: Some(33.5), used: false }]);
        assert_eq!(strings(&[2, 3, b'a', b'b', b'c', 1, b'd']), vec!["abc", "d"]);
    }
}
