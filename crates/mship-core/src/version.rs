//! Version numbers: contract versions (MAJOR.MINOR) and mothershipOS releases (YYYY.N).

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("bad {kind} {value:?} (want {want})")]
pub struct ParseError {
    kind: &'static str,
    value: String,
    want: &'static str,
}

/// MAJOR.MINOR. Minor = backward-compatible additions, major = anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// `self` (what the host implements) can serve a peer that wants `wanted`: same major, minor >= wanted.
    pub fn satisfies(&self, wanted: Version) -> bool {
        self.major == wanted.major && self.minor >= wanted.minor
    }
}

fn digits(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl FromStr for Version {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let err = || ParseError { kind: "version", value: s.to_string(), want: "MAJOR.MINOR" };
        let parts: Vec<&str> = s.trim().split('.').collect();
        match parts.as_slice() {
            [major] => Ok(Version::new(digits(major).ok_or_else(err)?, 0)),
            [major, minor] => Ok(Version::new(digits(major).ok_or_else(err)?, digits(minor).ok_or_else(err)?)),
            _ => Err(err()),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Channel hello: both sides list versions they speak. Result: the highest common major, minor = the lower of
/// both sides' best minor in that major. `None` if there is no common major.
pub fn negotiate(ours: &[Version], theirs: &[Version]) -> Option<Version> {
    let best = |vs: &[Version], major: u32| vs.iter().filter(|v| v.major == major).map(|v| v.minor).max();
    ours.iter()
        .map(|v| v.major)
        .filter(|&m| best(theirs, m).is_some())
        .max()
        .map(|m| Version::new(m, best(ours, m).unwrap().min(best(theirs, m).unwrap())))
}

/// mothershipOS release YYYY.N (an optional -suffix such as -dev is ignored for ordering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Release {
    pub year: u32,
    pub n: u32,
}

impl FromStr for Release {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let err = || ParseError { kind: "release", value: s.to_string(), want: "YYYY.N" };
        let core = s.trim().split('-').next().unwrap_or("");
        match core.split('.').collect::<Vec<_>>().as_slice() {
            [year, n] if year.len() == 4 => {
                Ok(Release { year: digits(year).ok_or_else(err)?, n: digits(n).ok_or_else(err)? })
            }
            _ => Err(err()),
        }
    }
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.year, self.n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn parse_and_order() {
        assert_eq!(v("1"), Version::new(1, 0));
        assert!(v("1.9") < v("1.10"));
        assert!(v("1.10") < v("2.0"));
        for bad in ["", "1.x", "1.2.3", "-1.0", "v1"] {
            assert!(bad.parse::<Version>().is_err(), "{bad}");
        }
    }

    #[test]
    fn satisfies() {
        assert!(Version::new(1, 3).satisfies(v("1.0")));
        assert!(Version::new(1, 3).satisfies(v("1.3")));
        assert!(!Version::new(1, 3).satisfies(v("1.4"))); // the peer needs a newer minor
        assert!(!Version::new(2, 0).satisfies(v("1.0"))); // majors never cross
    }

    #[test]
    fn negotiate_versions() {
        assert_eq!(negotiate(&[v("1.4"), v("2.1")], &[v("1.2"), v("2.3")]), Some(Version::new(2, 1)));
        assert_eq!(negotiate(&[v("1.4")], &[v("1.2"), v("2.0")]), Some(Version::new(1, 2)));
        assert_eq!(negotiate(&[v("2.0")], &[v("1.5")]), None);
    }

    #[test]
    fn release() {
        let r = |s: &str| s.parse::<Release>().unwrap();
        assert!(r("2026.1") < r("2026.2"));
        assert!(r("2026.9") < r("2027.1"));
        assert_eq!(r("2026.1-dev"), r("2026.1"));
        for bad in ["26.1", "2026", "2026.1.1"] {
            assert!(bad.parse::<Release>().is_err(), "{bad}");
        }
    }
}
