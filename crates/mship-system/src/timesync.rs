//! The clock from the HTTP Date header of a public server, directly (Wi-Fi) or through the dev PC's proxy
//! (`mship-dev proxy` on 10.42.0.1:3128), then the RTC offset saved so the time survives reboots without network.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::{Res, set_clock};

/// must send a Date header (connectivity checks do not)
const URL_HOST: &str = "archive.ubuntu.com";
const URL_PATH: &str = "/ubuntu/";

/// "Sun, 25 Oct 2026 21:47:03 GMT" -> unix time
pub fn parse_http_date(s: &str) -> Option<i64> {
    let w: Vec<&str> = s.split_whitespace().collect();
    let [_, d, mon, y, hms, ..] = w.as_slice() else { return None };
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let m = months.iter().position(|x| x == mon)? as i64 + 1;
    let (d, y): (i64, i64) = (d.parse().ok()?, y.parse().ok()?);
    let t: Vec<i64> = hms.split(':').filter_map(|x| x.parse().ok()).collect();
    let [hh, mm, ss] = t.as_slice() else { return None };
    // days from civil (Howard Hinnant)
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

/// The server's Date, asked directly or through an HTTP proxy ("host:port").
fn date(proxy: Option<&str>) -> Option<i64> {
    let target = proxy.map_or(format!("{URL_HOST}:80"), String::from);
    let addr = target.to_socket_addrs().ok()?.next()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let path = if proxy.is_some() { format!("http://{URL_HOST}{URL_PATH}") } else { URL_PATH.into() };
    write!(s, "HEAD {path} HTTP/1.0\r\nHost: {URL_HOST}\r\nUser-Agent: mothershipOS\r\n\r\n").ok()?;
    let mut b = vec![0u8; 8192];
    let n = s.read(&mut b).ok()?;
    String::from_utf8_lossy(&b[..n])
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("date")).map(|(_, v)| v.trim().to_string()))
        .and_then(|d| parse_http_date(&d))
}

pub fn sync() -> Res<()> {
    let proxy = std::env::var("MSHIP_PROXY").unwrap_or_else(|_| "10.42.0.1:3128".into());
    let proxy = proxy.trim_start_matches("http://").trim_end_matches('/').to_string();
    for _ in 0..30 {
        for (via, p) in [("direct", None), ("proxy", Some(proxy.as_str()))] {
            if let Some(t) = date(p) {
                set_clock(t)?;
                println!("clock set from {URL_HOST} ({via}): {t}");
                return crate::rtc::save();
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(format!("could not get the time (direct or via {proxy})"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn http_date() {
        assert_eq!(super::parse_http_date("Sun, 25 Oct 2026 21:47:03 GMT"), Some(1792964823));
        assert_eq!(super::parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(super::parse_http_date("Tue, 29 Feb 2028 12:00:00 GMT"), Some(1835438400));
        assert_eq!(super::parse_http_date("garbage"), None);
    }
}
