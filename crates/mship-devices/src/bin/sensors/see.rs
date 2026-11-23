//! Qualcomm SEE (Sensors Execution Environment): the sensor DSP's sensors as QMI service 400 with proto2 messages
//! (sns_client.proto, sns_std*.proto, sns_suid.proto, sns_cal.proto, sns_proximity.proto). One QMI client per stream:
//! a stream ends with its client.
//!
//! QMI: request 0x20, TLV 0x01 = u16 length + `sns_client_request_msg`, TLV 0x10 = use jumbo reports (0);
//! indications 0x21 (report) / 0x22 (jumbo report), TLV 0x02 = u16 length + `sns_client_event_msg`.

use std::time::Duration;

use mship_devices::qrtr::{QmiClient, QmiMsg, tlv, tlv_u8};

pub const SERVICE: u32 = 400;
const TIMEOUT: Duration = Duration::from_secs(5);
const REQ: u16 = 0x20;
const REPORT: u16 = 0x21;
const JUMBO: u16 = 0x22;
/// the lookup sensor's SUID
const SUID_SENSOR: (u64, u64) = (0xabab_abab_abab_abab, 0xabab_abab_abab_abab);

pub mod msg {
    pub const DISABLE: u32 = 10;
    pub const SUID_REQ: u32 = 512;
    pub const SUID_EVENT: u32 = 768;
    pub const STD_CONFIG: u32 = 513;
    pub const ON_CHANGE_CONFIG: u32 = 514;
    pub const STD_EVENT: u32 = 1025;
    pub const CAL_EVENT: u32 = 1022;
    pub const PROXIMITY_EVENT: u32 = 769;
    pub const PROXIMITY_EVENT_RECURRENT: u32 = 1031;
}

// ---------------------------------------------------------------- proto2 wire format

fn varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        out.push(if v != 0 { b | 0x80 } else { b });
        if v == 0 {
            return;
        }
    }
}

fn key(field: u32, wire: u8, out: &mut Vec<u8>) {
    varint(((field as u64) << 3) | wire as u64, out);
}

fn f_varint(field: u32, v: u64, out: &mut Vec<u8>) {
    key(field, 0, out);
    varint(v, out);
}

fn f_fixed32(field: u32, v: u32, out: &mut Vec<u8>) {
    key(field, 5, out);
    out.extend_from_slice(&v.to_le_bytes());
}

fn f_fixed64(field: u32, v: u64, out: &mut Vec<u8>) {
    key(field, 1, out);
    out.extend_from_slice(&v.to_le_bytes());
}

fn f_bytes(field: u32, b: &[u8], out: &mut Vec<u8>) {
    key(field, 2, out);
    varint(b.len() as u64, out);
    out.extend_from_slice(b);
}

/// One decoded field.
#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Bytes(&'a [u8]),
}

fn read_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let c = *b.get(*i)?;
        *i += 1;
        v |= ((c & 0x7f) as u64) << shift;
        if c & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// The fields of a message, in order. None: malformed.
pub fn decode(b: &[u8]) -> Option<Vec<(u32, Value<'_>)>> {
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let k = read_varint(b, &mut i)?;
        let field = (k >> 3) as u32;
        let v = match k & 7 {
            0 => Value::Varint(read_varint(b, &mut i)?),
            1 => {
                let v = u64::from_le_bytes(b.get(i..i + 8)?.try_into().ok()?);
                i += 8;
                Value::Fixed64(v)
            }
            5 => {
                let v = u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?);
                i += 4;
                Value::Fixed32(v)
            }
            2 => {
                let n = read_varint(b, &mut i)? as usize;
                let v = b.get(i..i + n)?;
                i += n;
                Value::Bytes(v)
            }
            _ => return None,
        };
        out.push((field, v));
    }
    Some(out)
}

/// A repeated float field: packed or not.
fn floats(fields: &[(u32, Value)], field: u32) -> Vec<f32> {
    let mut out = vec![];
    for (f, v) in fields {
        if *f != field {
            continue;
        }
        match v {
            Value::Fixed32(x) => out.push(f32::from_bits(*x)),
            Value::Bytes(b) => out.extend(b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c))),
            _ => {}
        }
    }
    out
}

fn varint_field(fields: &[(u32, Value)], field: u32) -> Option<u64> {
    fields.iter().find_map(|(f, v)| match v {
        Value::Varint(x) if *f == field => Some(*x),
        _ => None,
    })
}

fn suid_msg(s: (u64, u64)) -> Vec<u8> {
    let mut b = vec![];
    f_fixed64(1, s.0, &mut b);
    f_fixed64(2, s.1, &mut b);
    b
}

/// `sns_client_request_msg`: to `suid`, message `msg_id`, from the application processor, no wakeup.
pub fn request(suid: (u64, u64), msg_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut susp = vec![];
    f_varint(1, 1, &mut susp); // client processor: APSS
    f_varint(2, 1, &mut susp); // delivery: no wakeup
    let mut std = vec![];
    if !payload.is_empty() {
        f_bytes(2, payload, &mut std);
    }
    let mut b = vec![];
    f_bytes(1, &suid_msg(suid), &mut b);
    f_fixed32(2, msg_id, &mut b);
    f_bytes(3, &susp, &mut b);
    f_bytes(4, &std, &mut b);
    b
}

/// One event of a stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub msg_id: u32,
    /// QTimer ticks (19.2 MHz)
    pub timestamp: u64,
    /// sns_std_sensor_event.data / sns_cal_event.bias
    pub data: Vec<f32>,
    /// sample status 0-3 (unreliable .. high)
    pub status: u8,
    /// proximity events: near
    pub near: Option<bool>,
}

/// The events of one `sns_client_event_msg`.
pub fn events(b: &[u8]) -> Vec<Event> {
    let Some(fields) = decode(b) else { return vec![] };
    let mut out = vec![];
    for (f, v) in &fields {
        let (2, Value::Bytes(e)) = (f, v) else { continue };
        let Some(e) = decode(e) else { continue };
        let msg_id = e.iter().find_map(|(f, v)| matches!((f, v), (1, Value::Fixed32(_))).then(|| match v {
            Value::Fixed32(x) => *x,
            _ => 0,
        }));
        let timestamp = e.iter().find_map(|(f, v)| match (f, v) {
            (2, Value::Fixed64(x)) => Some(*x),
            _ => None,
        });
        let payload = e.iter().find_map(|(f, v)| match (f, v) {
            (3, Value::Bytes(x)) => Some(*x),
            _ => None,
        });
        let (Some(msg_id), Some(payload)) = (msg_id, payload) else { continue };
        let p = decode(payload).unwrap_or_default();
        let ev = match msg_id {
            msg::STD_EVENT => Event { msg_id, timestamp: timestamp.unwrap_or(0), data: floats(&p, 1), status: varint_field(&p, 2).unwrap_or(0) as u8, near: None },
            msg::CAL_EVENT => Event { msg_id, timestamp: timestamp.unwrap_or(0), data: floats(&p, 1), status: varint_field(&p, 4).unwrap_or(0) as u8, near: None },
            msg::PROXIMITY_EVENT | msg::PROXIMITY_EVENT_RECURRENT => Event {
                msg_id,
                timestamp: timestamp.unwrap_or(0),
                data: vec![varint_field(&p, 2).unwrap_or(0) as f32],
                status: varint_field(&p, 3).unwrap_or(0) as u8,
                near: Some(varint_field(&p, 1) == Some(1)),
            },
            _ => continue,
        };
        out.push(ev);
    }
    out
}

/// The payload of a report indication (TLV 0x02: u16 length + message).
fn report_payload(m: &QmiMsg) -> Option<&[u8]> {
    let v = m.tlvs.get(&0x02)?;
    let n = u16::from_le_bytes([*v.first()?, *v.get(1)?]) as usize;
    v.get(2..2 + n)
}

fn send(c: &mut QmiClient, req: &[u8]) -> Result<(), String> {
    let mut t = (req.len() as u16).to_le_bytes().to_vec();
    t.extend_from_slice(req);
    c.request(REQ, &[tlv(0x01, &t), tlv_u8(0x10, 0)], TIMEOUT).map(|_| ())
}

/// The SUID of the default sensor of `data_type` ("accel", "gyro", "mag", "proximity", ...).
pub fn lookup(node: u32, port: u32, data_type: &str) -> Result<(u64, u64), String> {
    let mut c = QmiClient::new(SERVICE, node, port).map_err(|e| e.to_string())?;
    let mut req = vec![];
    f_bytes(1, data_type.as_bytes(), &mut req);
    f_varint(2, 0, &mut req); // no updates
    f_varint(3, 1, &mut req); // the default sensor only
    send(&mut c, &request(SUID_SENSOR, msg::SUID_REQ, &req))?;
    let end = std::time::Instant::now() + TIMEOUT;
    while std::time::Instant::now() < end {
        mship_devices::poll(&[c.fd()], Some(Duration::from_millis(200)));
        for m in c.indications().map_err(|e| e.to_string())? {
            if m.msg != REPORT && m.msg != JUMBO {
                continue;
            }
            let Some(p) = report_payload(&m) else { continue };
            // sns_client_event_msg: events[].payload = sns_suid_event {data_type, suid[]}
            let Some(fields) = decode(p) else { continue };
            for (f, v) in &fields {
                let (2, Value::Bytes(e)) = (f, v) else { continue };
                let Some(e) = decode(e) else { continue };
                if !e.iter().any(|(f, v)| *f == 1 && *v == Value::Fixed32(msg::SUID_EVENT)) {
                    continue;
                }
                let Some(Value::Bytes(payload)) = e.iter().find(|(f, _)| *f == 3).map(|(_, v)| v.clone()) else { continue };
                let suids: Vec<(u64, u64)> = decode(payload).unwrap_or_default().into_iter()
                    .filter_map(|(f, v)| match (f, v) {
                        (2, Value::Bytes(s)) => {
                            let s = decode(s)?;
                            let lo = s.iter().find_map(|(f, v)| match (f, v) { (1, Value::Fixed64(x)) => Some(*x), _ => None })?;
                            let hi = s.iter().find_map(|(f, v)| match (f, v) { (2, Value::Fixed64(x)) => Some(*x), _ => None })?;
                            Some((lo, hi))
                        }
                        _ => None,
                    })
                    .collect();
                return suids.first().copied().ok_or(format!("no {data_type} sensor"));
            }
        }
    }
    Err(format!("{data_type}: no answer from the lookup sensor"))
}

/// A running stream of one sensor (its own client).
pub struct Stream {
    c: QmiClient,
    suid: (u64, u64),
}

impl Stream {
    /// Streaming at `rate` Hz, or on change (None).
    pub fn open(node: u32, port: u32, suid: (u64, u64), rate: Option<f32>) -> Result<Self, String> {
        let mut s = Self { c: QmiClient::new(SERVICE, node, port).map_err(|e| e.to_string())?, suid };
        s.configure(rate)?;
        Ok(s)
    }

    /// A new rate (the stream is reconfigured in place).
    pub fn configure(&mut self, rate: Option<f32>) -> Result<(), String> {
        let req = match rate {
            Some(hz) => {
                let mut p = vec![];
                key(1, 5, &mut p);
                p.extend_from_slice(&hz.to_le_bytes());
                request(self.suid, msg::STD_CONFIG, &p)
            }
            None => request(self.suid, msg::ON_CHANGE_CONFIG, &[]),
        };
        send(&mut self.c, &req)
    }

    pub fn fd(&self) -> std::os::fd::RawFd {
        self.c.fd()
    }

    /// What arrived. Err: the service is gone.
    pub fn events(&mut self) -> std::io::Result<Vec<Event>> {
        Ok(self.c.indications()?.iter().filter(|m| m.msg == REPORT || m.msg == JUMBO).filter_map(report_payload).flat_map(events).collect())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = send(&mut self.c, &request(self.suid, msg::DISABLE, &[]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints() {
        let mut b = vec![];
        varint(300, &mut b);
        assert_eq!(b, [0xac, 0x02]);
        let mut i = 0;
        assert_eq!(read_varint(&b, &mut i), Some(300));
    }

    #[test]
    fn request_layout() {
        let r = request((1, 2), 513, &[0x0d, 0, 0, 0x48, 0x42]);
        let f = decode(&r).unwrap();
        assert_eq!(f[1], (2, Value::Fixed32(513)));
        let Value::Bytes(suid) = f[0].1 else { panic!() };
        assert_eq!(decode(suid).unwrap(), vec![(1, Value::Fixed64(1)), (2, Value::Fixed64(2))]);
    }

    #[test]
    fn decode_events() {
        // captured from the nio: a mag_cal event (packed bias, status 0) and a std sensor event
        let cal = [0x0a, 0x0c, 0x0d, 0xcf, 0x5c, 0x42, 0x08, 0x3d, 0xc6, 0x41, 0xcb, 0x91, 0x49, 0xc3, 0x20, 0x00];
        let std_ev: Vec<u8> = [&[0x0d][..], &1.5f32.to_le_bytes(), &[0x0d], &(-2.0f32).to_le_bytes(), &[0x10, 0x03]].concat();
        let prox = [0x08, 0x01, 0x10, 0x75, 0x18, 0x03];
        let mut msg_b = vec![];
        for (id, p) in [(msg::CAL_EVENT, &cal[..]), (msg::STD_EVENT, &std_ev[..]), (msg::PROXIMITY_EVENT, &prox[..])] {
            let mut e = vec![];
            f_fixed32(1, id, &mut e);
            f_fixed64(2, 42, &mut e);
            f_bytes(3, p, &mut e);
            f_bytes(2, &e, &mut msg_b);
        }
        let mut whole = vec![];
        f_bytes(1, &suid_msg((1, 2)), &mut whole);
        whole.extend(msg_b);
        let ev = events(&whole);
        assert_eq!(ev.len(), 3);
        assert!((ev[0].data[0] - 55.2).abs() < 0.01 && (ev[0].data[2] + 201.57).abs() < 0.01 && ev[0].status == 0);
        assert_eq!((ev[1].data.clone(), ev[1].status, ev[1].timestamp), (vec![1.5, -2.0], 3, 42));
        assert_eq!((ev[2].near, ev[2].status), (Some(true), 3));
    }
}
