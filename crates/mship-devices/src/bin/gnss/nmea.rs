//! The mship.gnss 1.x wire format (docs/contracts/mship.gnss-1.md): NMEA 0183 4.11 epochs from the host, `$PMSHG`
//! control sentences both ways. Pure functions: no I/O.

use std::fmt::Write;

/// A constellation as NMEA 4.11 names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum System {
    Gps,
    Sbas,
    Glonass,
    Galileo,
    Beidou,
    Qzss,
}

impl System {
    /// GSV talker (SBAS satellites are listed with GPS: NMEA ids 33-64)
    fn talker(self) -> &'static str {
        match self {
            System::Gps | System::Sbas => "GP",
            System::Glonass => "GL",
            System::Galileo => "GA",
            System::Beidou => "GB",
            System::Qzss => "GQ",
        }
    }

    /// GSA system id (4.11)
    fn system_id(self) -> u8 {
        match self {
            System::Gps | System::Sbas => 1,
            System::Glonass => 2,
            System::Galileo => 3,
            System::Beidou => 4,
            System::Qzss => 5,
        }
    }

    /// GSV signal id of the L1-band signal this receiver tracks (GPS L1 C/A, GLONASS G1 C/A, Galileo E1, BeiDou
    /// B1I, QZSS L1 C/A)
    fn signal_id(self) -> u8 {
        if self == System::Galileo { 7 } else { 1 }
    }
}

/// One satellite in view. `id` is its NMEA number within its talker: GPS 1-32, SBAS 33-64, GLONASS 65-96,
/// Galileo 1-36, BeiDou 1-63, QZSS 1-10.
#[derive(Debug, Clone, PartialEq)]
pub struct Sat {
    pub system: System,
    pub id: u16,
    pub elevation: Option<f32>,
    pub azimuth: Option<f32>,
    /// C/N0, dB-Hz
    pub cn0: Option<f32>,
    pub used: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fix {
    pub lat: f64,
    pub lon: f64,
    /// metres above mean sea level
    pub alt_msl: Option<f32>,
    /// metres above the WGS 84 ellipsoid
    pub alt_ellipsoid: Option<f32>,
    /// horizontal accuracy, metres, 68 % (radius)
    pub h_acc: Option<f32>,
    /// vertical accuracy, metres, 68 %
    pub v_acc: Option<f32>,
    /// metres per second
    pub speed: Option<f32>,
    /// degrees from true north
    pub course: Option<f32>,
    pub pdop: Option<f32>,
    pub hdop: Option<f32>,
    pub vdop: Option<f32>,
}

/// What one epoch reports: the time, the fix if there is one, the satellites in view.
#[derive(Debug, Clone, Default)]
pub struct Epoch {
    /// UTC, milliseconds since 1970
    pub utc_ms: i64,
    pub fix: Option<Fix>,
    pub sats: Vec<Sat>,
}

/// `$<body>*<checksum>\r\n`
pub fn sentence(body: &str) -> String {
    let cs = body.bytes().fold(0u8, |a, b| a ^ b);
    format!("${body}*{cs:02X}\r\n")
}

fn opt(v: Option<f32>, prec: usize) -> String {
    v.filter(|x| x.is_finite()).map(|x| format!("{x:.prec$}")).unwrap_or_default()
}

/// ddmm.mmmmmm / dddmm.mmmmmm and the hemisphere
fn angle(v: f64, deg_digits: usize, pos: char, neg: char) -> (String, char) {
    let micro_min = (v.abs() * 60e6).round() as u64; // millionths of a minute: no 60.000000 from rounding
    let (deg, rest) = (micro_min / 60_000_000, micro_min % 60_000_000);
    (format!("{deg:0deg_digits$}{:02}.{:06}", rest / 1_000_000, rest % 1_000_000), if v < 0.0 { neg } else { pos })
}

/// (hhmmss.ss, ddmmyy) of a UTC time
fn utc(ms: i64) -> (String, String) {
    let days = ms.div_euclid(86_400_000);
    let t = ms.rem_euclid(86_400_000);
    let (h, m, s, cs) = (t / 3_600_000, t / 60_000 % 60, t / 1000 % 60, t % 1000 / 10);
    // civil date from days since 1970 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    (format!("{h:02}{m:02}{s:02}.{cs:02}"), format!("{d:02}{mo:02}{:02}", y.rem_euclid(100)))
}

/// One epoch, in the contract's order: GSV (each talker), GSA (each system), GGA, GST, RMC. RMC ends the epoch.
pub fn epoch(e: &Epoch) -> Vec<String> {
    let mut out = vec![];
    let mut sats = e.sats.clone();
    sats.sort_by_key(|s| (s.system.talker(), s.system, s.id));
    // GSV: up to 4 satellites per sentence
    let mut talkers: Vec<&str> = sats.iter().map(|s| s.system.talker()).collect();
    talkers.dedup();
    for t in talkers {
        let list: Vec<&Sat> = sats.iter().filter(|s| s.system.talker() == t).collect();
        let total = list.len().div_ceil(4);
        for (i, chunk) in list.chunks(4).enumerate() {
            let mut b = format!("{t}GSV,{total},{},{:02}", i + 1, list.len());
            for s in chunk {
                let _ = write!(b, ",{:02},{},{},{}", s.id, opt(s.elevation.map(f32::round), 0), opt(s.azimuth.map(f32::round), 0),
                               opt(s.cn0.map(f32::round), 0));
            }
            let _ = write!(b, ",{:X}", chunk[0].system.signal_id());
            out.push(sentence(&b));
        }
    }
    let fix = e.fix.as_ref();
    let (pdop, hdop, vdop) = fix.map(|f| (opt(f.pdop, 1), opt(f.hdop, 1), opt(f.vdop, 1))).unwrap_or_default();
    // GSA: the satellites used, per system, 12 per sentence
    let mut systems: Vec<u8> = sats.iter().filter(|s| s.used).map(|s| s.system.system_id()).collect();
    systems.dedup();
    if systems.is_empty() {
        systems.push(1);
    }
    let mode = match fix {
        Some(f) if f.alt_msl.is_some() => 3,
        Some(_) => 2,
        None => 1,
    };
    for sys in systems {
        let used: Vec<u16> = sats.iter().filter(|s| s.used && s.system.system_id() == sys).map(|s| s.id).collect();
        let groups: Vec<&[u16]> = if used.is_empty() { vec![&[]] } else { used.chunks(12).collect() };
        for g in groups {
            let mut b = format!("GNGSA,A,{mode}");
            for i in 0..12 {
                let _ = write!(b, ",{}", g.get(i).map(|id| format!("{id:02}")).unwrap_or_default());
            }
            let _ = write!(b, ",{pdop},{hdop},{vdop},{sys}");
            out.push(sentence(&b));
        }
    }
    let (time, date) = utc(e.utc_ms);
    let used = sats.iter().filter(|s| s.used).count().min(99);
    match fix {
        Some(f) => {
            let (lat, ns) = angle(f.lat, 2, 'N', 'S');
            let (lon, ew) = angle(f.lon, 3, 'E', 'W');
            let sep = match (f.alt_ellipsoid, f.alt_msl) {
                (Some(e), Some(m)) => Some(e - m),
                _ => None,
            };
            out.push(sentence(&format!("GNGGA,{time},{lat},{ns},{lon},{ew},1,{used:02},{hdop},{},M,{},M,,", opt(f.alt_msl, 1), opt(sep, 1))));
            if let Some(h) = f.h_acc {
                let s = h / std::f32::consts::SQRT_2; // per axis: sqrt(lat² + lon²) = the 68 % radius
                out.push(sentence(&format!("GNGST,{time},,,,,{},{},{}", opt(Some(s), 2), opt(Some(s), 2), opt(f.v_acc, 2))));
            }
            let knots = f.speed.map(|v| v * 3600.0 / 1852.0);
            out.push(sentence(&format!("GNRMC,{time},A,{lat},{ns},{lon},{ew},{},{},{date},,,A,S", opt(knots, 3), opt(f.course, 1))));
        }
        None => {
            out.push(sentence(&format!("GNGGA,{time},,,,,0,{used:02},,,,,,,")));
            out.push(sentence(&format!("GNRMC,{time},V,,,,,,,{date},,,N,V")));
        }
    }
    out
}

/// What a client asks (guest → host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// the contract versions the client speaks
    Hello(Vec<String>),
    /// run the receiver, an epoch at least every this many milliseconds
    Start(u32),
    Stop,
}

/// A `$PMSHG` control sentence; None for anything else. A checksum, if present, must match.
pub fn command(line: &str) -> Option<Command> {
    let line = line.trim();
    let body = line.strip_prefix('$')?;
    let body = match body.rsplit_once('*') {
        Some((b, cs)) => {
            let want = u8::from_str_radix(cs, 16).ok()?;
            (b.bytes().fold(0u8, |a, x| a ^ x) == want).then_some(b)?
        }
        None => body,
    };
    let mut f = body.split(',');
    if f.next()? != "PMSHG" {
        return None;
    }
    match f.next()? {
        "HELLO" => Some(Command::Hello(f.filter(|v| !v.is_empty()).map(String::from).collect())),
        "START" => Some(Command::Start(f.next().and_then(|v| v.parse().ok()).unwrap_or(1000))),
        "STOP" => Some(Command::Stop),
        _ => None,
    }
}

/// `$PMSHG,HELLO,<version>` (or `UNSUPPORTED`)
pub fn hello(version: Option<&str>) -> String {
    sentence(&format!("PMSHG,HELLO,{}", version.unwrap_or("UNSUPPORTED")))
}

/// `$PMSHG,STATE,ATTACHED` / `$PMSHG,STATE,EMPTY[,<reason>]`
pub fn state(attached: bool, reason: Option<&str>) -> String {
    match (attached, reason) {
        (true, _) => sentence("PMSHG,STATE,ATTACHED"),
        (false, Some(r)) => sentence(&format!("PMSHG,STATE,EMPTY,{}", r.to_uppercase())),
        (false, None) => sentence("PMSHG,STATE,EMPTY"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum() {
        // a well-known sentence
        assert_eq!(sentence("GPGGA,092750.000,5321.6802,N,00630.3372,W,1,8,1.03,61.7,M,55.2,M,,"),
                   "$GPGGA,092750.000,5321.6802,N,00630.3372,W,1,8,1.03,61.7,M,55.2,M,,*76\r\n");
    }

    #[test]
    fn time_and_angles() {
        assert_eq!(utc(0), ("000000.00".into(), "010170".into()));
        // 2026-11-19 09:57:01.878 UTC
        assert_eq!(utc(1_795_082_221_878), ("095701.87".into(), "191126".into()));
        assert_eq!(utc(951_782_400_000).1, "290200"); // leap day 2000
        assert_eq!(angle(48.1173, 2, 'N', 'S'), ("4807.038000".into(), 'N'));
        assert_eq!(angle(-11.5166666667, 3, 'E', 'W'), ("01131.000000".into(), 'W'));
        assert_eq!(angle(9.99999999999, 2, 'N', 'S').0, "1000.000000"); // no 60-minute rollover
    }

    #[test]
    fn epochs() {
        let sats = vec![
            Sat { system: System::Gps, id: 5, elevation: Some(45.4), azimuth: Some(120.0), cn0: Some(38.6), used: true },
            Sat { system: System::Glonass, id: 70, elevation: Some(10.0), azimuth: Some(300.0), cn0: None, used: false },
            Sat { system: System::Galileo, id: 11, elevation: Some(60.0), azimuth: Some(10.0), cn0: Some(41.0), used: true },
        ];
        let no_fix = epoch(&Epoch { utc_ms: 0, fix: None, sats: sats.clone() });
        assert!(no_fix.last().unwrap().starts_with("$GNRMC,000000.00,V,"));
        assert!(no_fix.iter().any(|s| s.starts_with("$GPGSV,1,1,01,05,45,120,39,1*")));
        assert!(no_fix.iter().any(|s| s.starts_with("$GLGSV,1,1,01,70,10,300,,1*")));
        assert!(no_fix.iter().any(|s| s.starts_with("$GAGSV,1,1,01,11,60,10,41,7*")));
        let fix = Fix { lat: 48.1173, lon: 11.5166666667, alt_msl: Some(545.4), alt_ellipsoid: Some(592.3), h_acc: Some(5.0),
                        v_acc: Some(8.0), speed: Some(10.0), course: Some(84.4), pdop: Some(1.8), hdop: Some(1.0), vdop: Some(1.5) };
        let e = epoch(&Epoch { utc_ms: 0, fix: Some(fix), sats });
        assert_eq!(e.iter().filter(|s| s.starts_with("$GNGSA,A,3,")).count(), 2); // GPS and Galileo
        assert!(e.iter().any(|s| s.starts_with("$GNGSA,A,3,05,,,,,,,,,,,,1.8,1.0,1.5,1*")));
        assert!(e.iter().any(|s| s.starts_with("$GNGGA,000000.00,4807.038000,N,01131.000000,E,1,02,1.0,545.4,M,46.9,M,,*")));
        assert!(e.iter().any(|s| s.starts_with("$GNGST,000000.00,,,,,3.54,3.54,8.00*")));
        assert!(e.last().unwrap().starts_with("$GNRMC,000000.00,A,4807.038000,N,01131.000000,E,19.438,84.4,010170,,,A,S*"));
    }

    #[test]
    fn commands() {
        assert_eq!(command(&sentence("PMSHG,START,2000")), Some(Command::Start(2000)));
        assert_eq!(command("$PMSHG,STOP\r\n"), Some(Command::Stop));
        assert_eq!(command("$PMSHG,HELLO,1.0,2.0"), Some(Command::Hello(vec!["1.0".into(), "2.0".into()])));
        assert_eq!(command("$PMSHG,STOP*00"), None); // bad checksum
        assert_eq!(command("$GPGGA,1"), None);
        assert_eq!(state(false, Some("revoked")), sentence("PMSHG,STATE,EMPTY,REVOKED"));
    }
}
