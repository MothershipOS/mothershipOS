//! The modem slot's AT dialect: Cuttlefish's modem simulator's, the one the stock Cuttlefish RIL speaks
//! (device/google/cuttlefish/host/commands/modem_simulator). The VM sees a modem that behaves exactly like the
//! simulator, with the phone's real network behind it:
//!
//! - **answered here** from the modem's state (QMI): signal (`+CSQ`, the simulator's 22 values incl. LTE and NR),
//!   technology (`+CTEC`), registration (`+CREG` / `+CGREG` / `+CEREG`), operator (`+COPS?`), ICCID (`+CICCID`),
//!   IMEI (`+CGSN`), the packet-data commands (contexts are the host's: each is a QMI session routed to the VM's
//!   data NIC), SMS delivery (`+CNMI` / `+CNMA`: incoming SMS come over QMI WMS);
//! - **unsolicited** results from the modem's indications: registration, technology, signal, physical channels
//!   (`%CGFPCCFG`: an NR secondary next to LTE is 5G NSA), incoming SMS (`+CMT`), call events (`RING`,
//!   `NO CARRIER`);
//! - **everything else is standard 27.007** (SIM access, PIN, calls, DTMF, USSD, supplementary services, SMS send,
//!   network selection) and goes to the modem's AT port as is; its unsolicited results that are ours to give are
//!   dropped there (`modem_line`).
//!
//! A VM that doesn't hold the handle talks to an **empty slot**: a modem without a SIM card and without service
//! (`empty_slot`), so its radio stack settles instead of retrying.

use std::collections::BTreeMap;

use crate::nas::{Net, Rat, Reg};

/// The simulator's "not available" (INT_MAX).
const NA: i64 = 0x7fff_ffff;
/// `AT+CTEC=?`: GSM, WCDMA, LTE, NR (technology indices)
const TECHS: &str = "+CTEC: 0,1,5,6";
/// preferred technologies (`+CTEC` mask, one byte per priority level): GSM | WCDMA | LTE | NR
const DEFAULT_PREFERRED: u32 = 0x63;

/// A context the VM defined (`AT+CGDCONT=`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub pdp: String,
    pub apn: String,
}

/// An active context's session, as the VM sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Active {
    /// "a.b.c.d/len"
    pub address: String,
    pub gateway: String,
    pub dns: Vec<String>,
}

/// The modem's identity.
#[derive(Clone, Debug, Default)]
pub struct Ids {
    pub iccid: Option<String>,
    pub imei: Option<String>,
}

/// What a command takes.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// answered here: the response lines, final result included
    Reply(Vec<String>),
    /// to the modem's AT port, as is
    Modem(Vec<u8>),
    /// start data on these contexts (answer: OK / ERROR)
    Activate(Vec<u8>),
    /// end the VM's data on these contexts (none: all); answer OK
    Deactivate(Vec<u8>),
    /// the incoming SMS delivered last: received (true) or refused; answer OK
    SmsAck(bool),
    /// preferred technologies, as a QMI NAS mode preference (answer: `+CTEC: DONE` / ERROR)
    Mode(u16),
}

fn ok(lines: impl IntoIterator<Item = String>) -> Route {
    Route::Reply(lines.into_iter().chain(["OK".to_string()]).collect())
}

fn ints(s: &str) -> Vec<i64> {
    s.split(',').map(|x| x.trim().trim_matches('"').parse().unwrap_or(-1)).collect()
}

/// What the VM was last told, to report changes only.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Told {
    cs: (Reg, u32, u32, Option<Rat>, Option<String>),
    ps: (Reg, u32, u32, Option<Rat>),
    rat: Option<Rat>,
    csq: String,
    channels: Vec<String>,
}

pub struct Radio {
    /// unsolicited registration modes (`AT+CREG=<n>` ...): 0 off, 1 status, 2 status and cell
    creg: u8,
    cgreg: u8,
    cereg: u8,
    preferred: u32,
    pub contexts: BTreeMap<u8, Context>,
    /// the context defined last (`ATD*99***1#` dials it, as the simulator's RIL expects)
    defined: Option<u8>,
    told: Option<Told>,
}

impl Default for Radio {
    fn default() -> Self {
        Self { creg: 0, cgreg: 0, cereg: 0, preferred: DEFAULT_PREFERRED, contexts: BTreeMap::new(), defined: None, told: None }
    }
}

impl Radio {
    /// One command unit from the VM (a command line, or an SMS PDU after `+CMGS=`).
    pub fn command(&mut self, unit: &[u8], net: &Net, ids: &Ids, active: &BTreeMap<u8, Active>) -> Route {
        if unit.last().is_some_and(|c| *c == 0x1a || *c == 0x1b) {
            return Route::Modem(unit.to_vec()); // an SMS PDU
        }
        let raw = String::from_utf8_lossy(unit).trim().to_string();
        let t = raw.to_ascii_uppercase();
        let Some(cmd) = t.strip_prefix("AT") else { return Route::Modem(unit.to_vec()) };
        match cmd {
            "+CSQ" => return ok([csq(net)]),
            "+CTEC?" => return ok([format!("+CTEC: {},{:x}", tech(net.rat).unwrap_or(5), self.preferred)]),
            "+CTEC=?" => return ok([TECHS.to_string()]),
            "+CREG?" => return ok([format!("+CREG: {},{}", self.creg, reg_body(self.creg, net.cs, net, net.rat))]),
            // the simulator details the data registration by the voice registration's mode (its RIL reads the
            // data technology from there)
            "+CGREG?" => return ok([format!("+CGREG: {},{}", self.cgreg, reg_body(self.creg, net.ps, net, net.rat))]),
            "+CEREG?" => return ok([format!("+CEREG: {},{}", self.cereg, reg_body(self.creg, net.ps, net, net.rat))]),
            "+COPS?" => return ok([cops(net, 0)]),
            "+COPS=3,0;+COPS?;+COPS=3,1;+COPS?;+COPS=3,2;+COPS?" => return ok([cops(net, 0), cops(net, 1), cops(net, 2)]),
            "+CICCID" => {
                return match &ids.iccid {
                    Some(i) => ok([i.clone()]),
                    None => Route::Reply(vec!["+CME ERROR: 10".into()]), // SIM not inserted
                };
            }
            "+CGSN" => {
                if let Some(i) = &ids.imei {
                    return ok([i.clone()]);
                }
            }
            // contexts 1 to the highest defined, each with its state: the simulator's RIL indexes its data call list
            // by cid - 1 and sizes it by these lines (the simulator lists every context ever defined)
            "+CGACT?" => {
                let top = self.contexts.keys().chain(active.keys()).max().copied().unwrap_or(0);
                return ok((1..=top).map(|c| format!("+CGACT: {c},{}", active.contains_key(&c) as u8)));
            }
            "+CGDCONT?" => {
                return ok(self.contexts.iter().map(|(c, x)| {
                    let addr = active.get(c).map(|a| a.address.as_str()).unwrap_or("");
                    format!("+CGDCONT: {c},\"{}\",\"{}\",\"{addr}\",0,0", x.pdp, x.apn)
                }));
            }
            "+CGQREQ=1" | "+CGQMIN=1" | "+CGEREP=1,0" => return ok([]),
            _ => {}
        }
        if let Some(n) = cmd.strip_prefix("+CREG=") {
            // the RIL's screen state: 1 while the screen is off, 2 when it is on again. On again: the current signal
            // and channels are reported anew (Android waits for them; nothing was reported while off)
            let n = n.trim().parse().unwrap_or(0);
            if n >= 2
                && self.creg < 2
                && let Some(t) = self.told.as_mut()
            {
                t.csq.clear();
                t.channels.clear();
            }
            self.creg = n;
            return ok([]);
        }
        if let Some(n) = cmd.strip_prefix("+CGREG=") {
            self.cgreg = n.trim().parse().unwrap_or(0);
            return ok([]);
        }
        if let Some(n) = cmd.strip_prefix("+CEREG=") {
            self.cereg = n.trim().parse().unwrap_or(0);
            return ok([]);
        }
        if let Some(rest) = cmd.strip_prefix("+CTEC=") {
            // current,preferred (hex mask): the preference is the modem's mode preference
            let pref = rest.split(',').nth(1).and_then(|p| u32::from_str_radix(p.trim().trim_matches('"'), 16).ok());
            return match pref {
                Some(p) if p != 0 => {
                    self.preferred = p;
                    Route::Mode(mode_preference(p))
                }
                _ => Route::Reply(vec!["ERROR".into()]),
            };
        }
        if let Some(rest) = raw.get(3..).filter(|_| cmd.starts_with("+CGDCONT=")).and_then(|r| r.strip_prefix("CGDCONT=")) {
            // cid,"type","apn",...: the APN keeps its case
            let f: Vec<&str> = rest.split(',').map(|x| x.trim().trim_matches('"')).collect();
            return match f.first().and_then(|c| c.parse::<u8>().ok()) {
                Some(cid) if cid > 0 => {
                    let ctx = Context { pdp: f.get(1).unwrap_or(&"IP").to_string(), apn: f.get(2).unwrap_or(&"").to_string() };
                    self.contexts.insert(cid, ctx);
                    self.defined = Some(cid);
                    ok([])
                }
                _ => Route::Reply(vec!["ERROR".into()]),
            };
        }
        if let Some(rest) = cmd.strip_prefix("D*99") {
            // ATD*99***<cid>#: the simulator's RIL dials 1 for whichever context it just defined
            let dialed = rest.trim_end_matches('#').rsplit('*').next().and_then(|c| c.parse().ok());
            return Route::Activate(vec![self.defined.or(dialed).unwrap_or(1)]);
        }
        if let Some(rest) = cmd.strip_prefix("+CGACT=") {
            let mut v = ints(rest);
            let state = if v.is_empty() { 0 } else { v.remove(0) };
            let cids: Vec<u8> = v.into_iter().filter(|c| *c > 0 && *c < 256).map(|c| c as u8).collect();
            return match (state, cids.is_empty()) {
                (1, true) => ok([]), // "AT+CGACT=1,0": the simulator's RIL resetting, nothing to do
                (1, false) => Route::Activate(cids),
                _ => Route::Deactivate(cids),
            };
        }
        if cmd.starts_with("+CGDATA") {
            return Route::Reply(vec!["ERROR".into()]);
        }
        if let Some(rest) = cmd.strip_prefix("+CGCONTRDP=") {
            let cid: u8 = rest.trim().parse().unwrap_or(0);
            return match (active.get(&cid), self.contexts.get(&cid)) {
                (Some(a), Some(c)) => ok([format!("+CGCONTRDP: {cid},5,{},{},{},{}", c.apn, a.address, a.gateway, a.dns.join(" "))]),
                _ => Route::Reply(vec!["+CME ERROR: 21".into()]), // invalid index
            };
        }
        if let Some(rest) = cmd.strip_prefix("+CNMA") {
            // AT+CNMA / AT+CNMA=1: received; AT+CNMA=2[,...]: refused
            return Route::SmsAck(!rest.trim_start_matches('=').starts_with('2'));
        }
        if cmd.starts_with("+CNMI=") {
            return ok([]); // incoming SMS always reach the VM (over WMS); the modem's routes stay ours
        }
        if cmd.starts_with("+REMOTE") {
            return ok([]); // the simulator's host-side controls
        }
        Route::Modem(unit.to_vec())
    }

    /// The unsolicited results for what changed since the VM was last told. The first time only the signal and the
    /// physical channels: the RIL queries registration itself, but Android waits for signal reports. While the
    /// guest's screen is off (`AT+CREG=1`) signal and channels are not reported: they would only wake it.
    pub fn changes(&mut self, net: &Net, active: &BTreeMap<u8, Active>) -> Vec<String> {
        let awake = self.creg >= 2 || self.told.is_none();
        let mut now = Told {
            cs: (net.cs, net.area, net.cell, net.rat, net.plmn.as_ref().map(|p| p.numeric())),
            ps: (net.ps, net.area, net.cell, net.rat),
            rat: net.rat,
            csq: csq(net),
            channels: channels(net, active),
        };
        if !awake && let Some(old) = &self.told {
            now.csq = old.csq.clone();
            now.channels = old.channels.clone();
        }
        let Some(old) = self.told.replace(now.clone()) else { return [vec![now.csq], now.channels].concat() };
        let mut out = vec![];
        if old.cs != now.cs && self.creg > 0 {
            out.push(format!("+CREG: {}", reg_body(self.creg, net.cs, net, net.rat)));
        }
        if old.ps != now.ps {
            if self.cgreg > 0 {
                out.push(format!("+CGREG: {}", reg_body(self.cgreg, net.ps, net, net.rat)));
            }
            if self.cereg > 0 {
                out.push(format!("+CEREG: {}", reg_body(self.cereg, net.ps, net, net.rat)));
            }
        }
        if old.rat != now.rat
            && let Some(t) = tech(net.rat)
        {
            out.push(format!("+CTEC: {t}"));
        }
        if old.csq != now.csq {
            out.push(now.csq.clone());
        }
        if old.channels != now.channels {
            out.extend(now.channels);
        }
        out
    }

    /// Forget what the VM was told (it reconnected: its RIL starts over).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The answer of an empty slot (a modem with no SIM card, no service, no calls) to one command unit.
pub fn empty_slot(unit: &[u8]) -> Vec<String> {
    const NO_SIM: &[&str] = &["+CPIN", "+CRSM", "+CSIM", "+CLCK", "+CIMI", "+CICCID", "+CCHO", "+CCHC", "+CGLA", "+CPWD",
                              "+CPINR", "+CNUM", "+CMGS", "+CMGW", "+CUSD=", "+CCFC", "+CCWA=", "+CLIR=", "^MBAU"];
    let t = String::from_utf8_lossy(unit).trim().to_ascii_uppercase();
    if unit.last().is_some_and(|c| *c == 0x1a || *c == 0x1b) {
        return vec!["+CME ERROR: 10".into()];
    }
    let Some(cmd) = t.strip_prefix("AT") else { return vec!["ERROR".into()] };
    let line = match cmd {
        "+CSQ" => Some(csq(&Net::default())),
        "+CFUN?" => Some("+CFUN: 1".into()),
        "+CTEC?" => Some(format!("+CTEC: 5,{DEFAULT_PREFERRED:x}")),
        "+CTEC=?" => Some(TECHS.into()),
        "+CREG?" => Some("+CREG: 0,0".into()),
        "+CGREG?" => Some("+CGREG: 0,0".into()),
        "+CEREG?" => Some("+CEREG: 0,0".into()),
        "+COPS?" => Some("+COPS: 0".into()),
        "+COPS=3,0;+COPS?;+COPS=3,1;+COPS?;+COPS=3,2;+COPS?" => return vec!["+COPS: 0".into(), "+COPS: 0".into(), "+COPS: 0".into(), "OK".into()],
        _ => None,
    };
    if let Some(l) = line {
        return vec![l, "OK".into()];
    }
    if NO_SIM.iter().any(|p| cmd.starts_with(p)) {
        return vec!["+CME ERROR: 10".into()]; // SIM not inserted
    }
    if cmd.starts_with('D') || cmd == "A" || cmd.starts_with("+CGACT=1") || cmd.starts_with("+CGDATA") {
        return vec!["NO CARRIER".into()];
    }
    vec!["OK".into()] // settings, queries with nothing to list (calls, contexts)
}

/// A line from the modem's AT port: passed to the VM, or dropped because the result is ours to give.
pub fn modem_line(line: &str) -> bool {
    let l = line.trim_start();
    const OURS: &[&str] = &["+CREG:", "+CGREG:", "+CEREG:", "+CSQ:", "+CTEC:", "+CMT:", "+CMTI:", "+CDS:", "+CDSI:", "RING", "+CRING:"];
    if OURS.iter().any(|p| l.starts_with(p)) {
        return false;
    }
    // +CLCC: <id>,<dir>,<stat>,<mode>,...: only voice calls (mode 0); the modem also lists its data bearer
    if let Some(r) = l.strip_prefix("+CLCC:") {
        return r.split(',').nth(3).is_none_or(|m| m.trim() == "0");
    }
    true
}

/// +CTEC technology index.
fn tech(rat: Option<Rat>) -> Option<u8> {
    rat.map(|r| match r {
        Rat::Gsm => 0,
        Rat::Umts => 1,
        Rat::Lte => 5,
        Rat::Nr => 6,
    })
}

/// 27.007 <AcT> as the simulator reports it.
fn act(rat: Option<Rat>) -> u8 {
    match rat {
        Some(Rat::Gsm) | None => 3, // EGPRS
        Some(Rat::Umts) => 6,       // HSPA
        Some(Rat::Lte) => 7,        // E-UTRAN
        Some(Rat::Nr) => 11,        // NR (5GCN)
    }
}

/// The body of a registration result (after "+CREG: " and, for the query, "<n>,"): `<stat>[,"<lac>","<ci>",<AcT>]`.
fn reg_body(n: u8, stat: Reg, net: &Net, rat: Option<Rat>) -> String {
    if n >= 2 && stat.registered() {
        format!("{},\"{:x}\",\"{:x}\",{}", stat as u8, net.area, net.cell, act(rat))
    } else {
        format!("{}", stat as u8)
    }
}

/// `+COPS:` in format 0 (long name), 1 (short name), 2 (numeric).
fn cops(net: &Net, format: u8) -> String {
    let Some(p) = net.plmn.as_ref().filter(|_| net.cs.registered() || net.ps.registered()) else { return "+COPS: 0".into() };
    let name = match format {
        0 if !p.long.is_empty() => p.long.clone(),
        1 if !p.short.is_empty() => p.short.clone(),
        0 | 1 if !p.short.is_empty() || !p.long.is_empty() => if p.long.is_empty() { p.short.clone() } else { p.long.clone() },
        _ => p.numeric(),
    };
    let format = if name == p.numeric() { 2 } else { format };
    format!("+COPS: 0,{format},\"{name}\"")
}

/// The simulator's `+CSQ`: gsm_rssi, gsm_ber, cdma_dbm, cdma_ecio, evdo_dbm, evdo_ecio, evdo_snr, lte_rssi,
/// lte_rsrp, lte_rsrq, lte_rssnr, lte_cqi, lte_ta, tdscdma_rscp, wcdma_rssi, wcdma_ber, nr_ss_rsrp, nr_ss_rsrq,
/// nr_ss_sinr, nr_csi_rsrp, nr_csi_rsrq, nr_csi_sinr (RSRP / RSRQ as positive numbers, INT_MAX = not available).
pub fn csq(net: &Net) -> String {
    let s = &net.signal;
    let asu = |dbm: Option<i32>| dbm.map(|d| ((d as i64 + 113) / 2).clamp(0, 31)).unwrap_or(NA);
    let neg = |v: Option<i32>, lo: i64, hi: i64| v.map(|v| -(v as i64)).filter(|v| (lo..=hi).contains(v)).unwrap_or(NA);
    let lte = net.rat == Some(Rat::Lte);
    let lte_or = |v: i64| if lte { v } else { NA };
    let v = [
        if net.rat == Some(Rat::Gsm) { asu(s.gsm_rssi) } else { NA },
        NA,
        NA,
        NA,
        NA,
        NA,
        NA,
        lte_or(asu(s.lte_rssi)),
        lte_or(neg(s.lte_rsrp, 44, 140)),
        lte_or(neg(s.lte_rsrq, 3, 20)),
        lte_or(s.lte_snr.map(|v| v as i64).filter(|v| (-200..=300).contains(v)).unwrap_or(NA)),
        NA,
        NA,
        NA,
        if net.rat == Some(Rat::Umts) { asu(s.umts_rssi) } else { NA },
        NA,
        neg(s.nr_rsrp, 44, 140),
        neg(s.nr_rsrq, 3, 20),
        s.nr_snr.map(|v| v as i64 / 10).filter(|v| (-23..=40).contains(v)).unwrap_or(NA),
        NA,
        NA,
        NA,
    ];
    format!("+CSQ: {}", v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","))
}

/// Physical channel configurations (`%CGFPCCFG: <status>,<bandwidth>,<technology>,<frequency range>,<cid>`) per
/// active context: the serving cell (1 = primary), and with EN-DC the NR cell (2 = secondary). Bandwidth and
/// frequency range are not known here (0).
fn channels(net: &Net, active: &BTreeMap<u8, Active>) -> Vec<String> {
    let Some(rat) = net.rat.filter(|_| net.ps.registered()) else { return vec![] };
    let primary = match rat {
        Rat::Gsm => 1,
        Rat::Umts => 2,
        Rat::Lte => 32,
        Rat::Nr => 64,
    };
    let mut out = vec![];
    for cid in active.keys() {
        out.push(format!("%CGFPCCFG: 1,0,{primary},0,{cid}"));
        if net.endc() {
            out.push(format!("%CGFPCCFG: 2,0,64,0,{cid}"));
        }
    }
    out
}

/// A `+CTEC` preferred mask (bits: 0 GSM, 1 WCDMA, 2 CDMA, 3 EvDo, 4 TD-SCDMA, 5 LTE, 6 NR, in any priority byte)
/// as a QMI NAS mode preference (bits: 0 1x, 1 EvDo, 2 GSM, 3 UMTS, 4 LTE, 5 TD-SCDMA, 6 NR5G).
fn mode_preference(mask: u32) -> u16 {
    let m = (0..4).fold(0u32, |a, i| a | (mask >> (8 * i)) & 0xff);
    [(0, 2), (1, 3), (2, 0), (3, 1), (4, 5), (5, 4), (6, 6)].iter().filter(|(b, _)| m & (1 << b) != 0).fold(0, |a, (_, q)| a | 1 << q)
}

/// The +CMT unsolicited result (PDU mode) for a 3GPP TPDU as WMS gives it (no SMSC address): the PDU gets an empty
/// SMSC address ("00"), the length counts the TPDU only.
pub fn cmt(tpdu: &[u8]) -> Option<String> {
    if tpdu.is_empty() {
        return None;
    }
    let hex: String = tpdu.iter().map(|b| format!("{b:02X}")).collect();
    Some(format!("+CMT: ,{}\r\n00{hex}", tpdu.len()))
}

/// The result for a QMI VOICE All Call Status "Call Information" (count, then per call: id, state, type, direction,
/// mode, multipart, ALS): RING while a call is incoming or waiting, NO CARRIER when one ended.
pub fn call_event(info: &[u8]) -> Option<&'static str> {
    let n = *info.first()? as usize;
    let states: Vec<u8> = info.get(1..1 + 7 * n)?.chunks(7).map(|c| c[1]).collect();
    if states.iter().any(|s| *s == 0x02 || *s == 0x07) {
        Some("RING")
    } else if states.contains(&0x09) {
        Some("NO CARRIER")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nas::{Plmn, Signal};

    fn lte() -> Net {
        Net {
            cs: Reg::Home,
            ps: Reg::Home,
            rat: Some(Rat::Lte),
            area: 0x2a3b,
            cell: 0x123_4567,
            plmn: Some(Plmn { mcc: 231, mnc: 6, mnc3: false, long: "O2 - SK".into(), short: "O2".into() }),
            signal: Signal { lte_rssi: Some(-63), lte_rsrp: Some(-98), lte_rsrq: Some(-12), lte_snr: Some(130), ..Signal::default() },
        }
    }

    fn reply(r: Route) -> Vec<String> {
        match r {
            Route::Reply(l) => l,
            other => panic!("not a reply: {other:?}"),
        }
    }

    #[test]
    fn network_answers() {
        let (mut r, net, ids, none) = (Radio::default(), lte(), Ids::default(), BTreeMap::new());
        let q = |c: &str, r: &mut Radio| reply(r.command(c.as_bytes(), &net, &ids, &none));
        assert_eq!(q("AT+CSQ", &mut r)[0], "+CSQ: 2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,25,98,12,130,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647,2147483647");
        assert_eq!(q("AT+CREG=2", &mut r), ["OK"]);
        assert_eq!(q("AT+CREG?", &mut r)[0], "+CREG: 2,1,\"2a3b\",\"1234567\",7");
        assert_eq!(q("AT+CGREG?", &mut r)[0], "+CGREG: 0,1,\"2a3b\",\"1234567\",7"); // detailed by +CREG's mode
        assert_eq!(q("AT+CTEC?", &mut r)[0], "+CTEC: 5,63");
        assert_eq!(q("AT+COPS=3,0;+COPS?;+COPS=3,1;+COPS?;+COPS=3,2;+COPS?", &mut r),
                   ["+COPS: 0,0,\"O2 - SK\"", "+COPS: 0,1,\"O2\"", "+COPS: 0,2,\"23106\"", "OK"]);
        assert_eq!(q("AT+CICCID", &mut r), ["+CME ERROR: 10"]);
        assert_eq!(r.command(b"AT+CPIN?\r", &net, &ids, &none), Route::Modem(b"AT+CPIN?\r".to_vec()));
    }

    #[test]
    fn data() {
        let (mut r, net, ids) = (Radio::default(), lte(), Ids::default());
        let mut act = BTreeMap::new();
        assert_eq!(reply(r.command(b"AT+CGDCONT=3,\"IP\",\"o2internet\",,0,0", &net, &ids, &act)), ["OK"]);
        assert_eq!(r.command(b"ATD*99***1#", &net, &ids, &act), Route::Activate(vec![3]));
        act.insert(3, Active { address: "10.0.0.2/30".into(), gateway: "10.0.0.1".into(), dns: vec!["1.1.1.1".into(), "8.8.8.8".into()] });
        assert_eq!(reply(r.command(b"AT+CGACT?", &net, &ids, &act)), ["+CGACT: 1,0", "+CGACT: 2,0", "+CGACT: 3,1", "OK"]);
        assert_eq!(reply(r.command(b"AT+CGDCONT?", &net, &ids, &act))[0], "+CGDCONT: 3,\"IP\",\"o2internet\",\"10.0.0.2/30\",0,0");
        assert_eq!(reply(r.command(b"AT+CGCONTRDP=3", &net, &ids, &act))[0], "+CGCONTRDP: 3,5,o2internet,10.0.0.2/30,10.0.0.1,1.1.1.1 8.8.8.8");
        assert_eq!(r.command(b"AT+CGACT=0,3", &net, &ids, &act), Route::Deactivate(vec![3]));
        assert_eq!(reply(r.command(b"AT+CGACT=1,0", &net, &ids, &act)), ["OK"]);
    }

    #[test]
    fn unsolicited_changes() {
        let (mut r, mut net, ids) = (Radio::default(), lte(), Ids::default());
        let mut act = BTreeMap::new();
        for c in ["AT+CREG=2", "AT+CGREG=1"] {
            r.command(c.as_bytes(), &net, &ids, &act);
        }
        let first = r.changes(&net, &act); // first look: the signal only (the RIL queries registration)
        assert!(first.len() == 1 && first[0].starts_with("+CSQ: "));
        act.insert(1, Active { address: String::new(), gateway: String::new(), dns: vec![] });
        net.signal.nr_rsrp = Some(-90); // an NR leg: 5G NSA
        let c = r.changes(&net, &act);
        assert!(c.iter().any(|l| l.starts_with("+CSQ: ") && l.contains(",90,")));
        assert_eq!(c.iter().filter(|l| l.starts_with("%CGFPCCFG")).collect::<Vec<_>>(), ["%CGFPCCFG: 1,0,32,0,1", "%CGFPCCFG: 2,0,64,0,1"]);
        net.cs = Reg::Searching;
        net.ps = Reg::Searching;
        let c = r.changes(&net, &act);
        assert!(c.contains(&"+CREG: 2".to_string()) && c.contains(&"+CGREG: 2".to_string()));
        // screen off: no signal reports; on again: the current signal at once
        net.cs = Reg::Home;
        r.command(b"AT+CREG=1", &net, &ids, &act);
        net.signal.lte_rsrp = Some(-110);
        assert!(!r.changes(&net, &act).iter().any(|l| l.starts_with("+CSQ")));
        r.command(b"AT+CREG=2", &net, &ids, &act);
        assert!(r.changes(&net, &act).iter().any(|l| l.starts_with("+CSQ") && l.contains(",110,")));
    }

    #[test]
    fn empty() {
        assert_eq!(empty_slot(b"AT+CPIN?\r"), ["+CME ERROR: 10"]);
        assert_eq!(empty_slot(b"AT+CREG?\r"), ["+CREG: 0,0", "OK"]);
        assert_eq!(empty_slot(b"ATE0Q0V1\r"), ["OK"]);
        assert_eq!(empty_slot(b"AT+CLCC\r"), ["OK"]);
        assert_eq!(empty_slot(b"ATD112;\r"), ["NO CARRIER"]);
    }

    #[test]
    fn modem_lines() {
        assert!(!modem_line("+CREG: 1"));
        assert!(!modem_line("+CLCC: 1,1,0,1,0,\"\",128")); // the data bearer
        assert!(modem_line("+CLCC: 2,0,3,0,0,\"+421900000000\",145"));
        assert!(modem_line("OK"));
    }

    #[test]
    fn preferences() {
        assert_eq!(mode_preference(0x63), 0b101_1100); // GSM UMTS LTE NR5G
        assert_eq!(mode_preference(0x2000), 1 << 4); // LTE only (priority byte 1)
    }

    #[test]
    fn sms_and_calls() {
        assert_eq!(cmt(&[0x04, 0x0c, 0x91, 0xaa]).unwrap(), "+CMT: ,4\r\n00040C91AA");
        assert_eq!(call_event(&[1, 1, 2, 0, 2, 3, 0, 0]), Some("RING"));
        assert_eq!(call_event(&[1, 1, 9, 0, 2, 3, 0, 0]), Some("NO CARRIER"));
        assert_eq!(call_event(&[1, 1, 3, 0, 2, 3, 0, 0]), None);
    }
}
