//! Time, as Plan 9's `ctime(2)`: seconds since the epoch broken into a
//! [`Tm`], in GMT or the local zone from `/env/timezone`, and back, and
//! `ctime`'s text.
//!
//! A zone file is Plan 9's: standard name and offset in seconds,
//! daylight name and offset, then pairs of local standard times between
//! which daylight time holds:
//!
//! ```text
//! EST -18000 EDT -14400
//!  9943200 25664400 41392800 …
//! ```
//!
//! With no zone, or one that does not parse, time is GMT.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

/// A time broken down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tm {
    /// 0 to 59.
    pub sec: u32,
    pub min: u32,
    pub hour: u32,
    /// 1 to 31.
    pub mday: u32,
    /// 0 to 11.
    pub mon: u32,
    /// Years since 1900.
    pub year: i64,
    /// 0 (Sunday) to 6.
    pub wday: u32,
    /// 0 to 365.
    pub yday: u32,
    /// The zone's name, e.g. `GMT`, `EDT`.
    pub zone: String,
    /// Seconds east of GMT.
    pub tzoff: i64,
}

const DAY: i64 = 86_400;

/// Days since 1970-01-01 of the civil date `y-m-d` (`m` 1 to 12).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    // Howard Hinnant's algorithm, in years starting in March.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of day `z` since 1970-01-01: year, month (1 to 12), day.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 1 to 31, 1 to 12
    let (d, m) = (
        (doy - (153 * mp + 2) / 5 + 1) as u32,
        (if mp < 10 { mp + 3 } else { mp - 9 }) as u32,
    );
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// `t` seconds since the epoch, in GMT.
#[must_use]
pub fn gmtime(t: i64) -> Tm {
    let days = t.div_euclid(DAY);
    let secs = t.rem_euclid(DAY);
    let (year, month, mday) = civil_from_days(days);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // all small and positive
    Tm {
        sec: (secs % 60) as u32,
        min: (secs / 60 % 60) as u32,
        hour: (secs / 3600) as u32,
        mday,
        mon: month - 1,
        year: year - 1900,
        wday: (days + 4).rem_euclid(7) as u32,
        yday: (days - days_from_civil(year, 1, 1)) as u32,
        zone: String::from("GMT"),
        tzoff: 0,
    }
}

/// The seconds since the epoch that `tm` stands for. If `mday` is 0,
/// `yday` gives the day instead. `tzoff` is taken off.
#[must_use]
pub fn tm2sec(tm: &Tm) -> i64 {
    let y = tm.year + 1900;
    let days = if tm.mday != 0 {
        days_from_civil(y, tm.mon + 1, tm.mday)
    } else {
        days_from_civil(y, 1, 1) + i64::from(tm.yday)
    };
    days * DAY + i64::from(tm.hour) * 3600 + i64::from(tm.min) * 60 + i64::from(tm.sec) - tm.tzoff
}

/// A time zone, as a Plan 9 zone file gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    pub std_name: String,
    pub std_off: i64,
    pub dst_name: String,
    pub dst_off: i64,
    /// `[start, end)` in local standard time.
    pub dst: Vec<(i64, i64)>,
}

impl Zone {
    #[must_use]
    pub fn gmt() -> Zone {
        Zone {
            std_name: String::from("GMT"),
            std_off: 0,
            dst_name: String::from("GMT"),
            dst_off: 0,
            dst: Vec::new(),
        }
    }

    /// Reads a zone file; `None` if it is not one.
    #[must_use]
    pub fn parse(text: &[u8]) -> Option<Zone> {
        let text = core::str::from_utf8(text).ok()?;
        let mut w = text.split_ascii_whitespace();
        let name = |s: Option<&str>| s.filter(|s| !s.is_empty() && s.len() <= 3).map(String::from);
        let std_name = name(w.next())?;
        let std_off = w.next()?.parse().ok()?;
        let dst_name = name(w.next())?;
        let dst_off = w.next()?.parse().ok()?;
        let times: Vec<i64> = w.map(str::parse).collect::<Result<_, _>>().ok()?;
        let dst = times
            .chunks(2)
            .filter(|p| p[0] != 0)
            .map(|p| (p[0], *p.get(1).unwrap_or(&i64::MAX)))
            .collect();
        Some(Zone {
            std_name,
            std_off,
            dst_name,
            dst_off,
            dst,
        })
    }

    /// The zone in `/env/timezone`, else GMT.
    #[must_use]
    pub fn local() -> Zone {
        crate::env::getenv_raw(b"timezone")
            .and_then(|z| Zone::parse(&z))
            .unwrap_or_else(Zone::gmt)
    }

    /// `t` in this zone.
    #[must_use]
    pub fn time(&self, t: i64) -> Tm {
        let local = t + self.std_off;
        let dst = self.dst.iter().any(|&(a, b)| local >= a && local < b);
        let (off, name) = if dst {
            (self.dst_off, &self.dst_name)
        } else {
            (self.std_off, &self.std_name)
        };
        let mut tm = gmtime(t + off);
        tm.zone.clone_from(name);
        tm.tzoff = off;
        tm
    }
}

/// `t` in the local zone.
#[must_use]
pub fn localtime(t: i64) -> Tm {
    Zone::local().time(t)
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `tm` as `ctime` gives it: `Thu Jan  1 00:00:00 GMT 1970\n`.
#[must_use]
pub fn asctime(tm: &Tm) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{} {} {:>2} {:02}:{:02}:{:02} {} {}",
        DAYS[tm.wday as usize % 7],
        MONTHS[tm.mon as usize % 12],
        tm.mday,
        tm.hour,
        tm.min,
        tm.sec,
        tm.zone,
        tm.year + 1900
    );
    s
}

/// `t` in the local zone, as text.
#[must_use]
pub fn ctime(t: i64) -> String {
    asctime(&localtime(t))
}

/// Seconds since the epoch, now.
#[must_use]
pub fn time() -> i64 {
    i64::try_from(crate::nsec() / 1_000_000_000).unwrap_or(i64::MAX)
}

/// Days in month `mon` (0 to 11) of year `year` (since 1900).
#[must_use]
pub fn month_days(year: i64, mon: u32) -> u32 {
    match mon {
        1 if is_leap(year + 1900) => 29,
        1 => 28,
        3 | 5 | 8 | 10 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_times() {
        assert_eq!(asctime(&gmtime(0)), "Thu Jan  1 00:00:00 GMT 1970\n");
        // 2000-02-29 23:59:59, a leap day in a century leap year.
        assert_eq!(asctime(&gmtime(951_868_799)), "Tue Feb 29 23:59:59 GMT 2000\n");
        // 2026-10-19 12:34:56.
        let t = gmtime(1_792_413_296);
        assert_eq!(asctime(&t), "Mon Oct 19 12:34:56 GMT 2026\n");
        assert_eq!(t.yday, 291);
        // Before the epoch.
        assert_eq!(asctime(&gmtime(-1)), "Wed Dec 31 23:59:59 GMT 1969\n");
    }

    #[test]
    fn round_trips() {
        for t in [0, 1, -86_401, 951_868_799, 1_792_413_296, 4_102_444_800, -2_208_988_800] {
            assert_eq!(tm2sec(&gmtime(t)), t);
        }
        let mut tm = gmtime(1_792_413_296);
        tm.mday = 0;
        assert_eq!(tm2sec(&tm), 1_792_413_296, "from yday");
    }

    #[test]
    fn zones() {
        let z = Zone::parse(b"EST -18000 EDT -14400\n 100 200 1792395000 1800000000\n").unwrap();
        assert_eq!(z.dst, [(100, 200), (1_792_395_000, 1_800_000_000)]);
        let tm = z.time(1_792_413_296);
        assert_eq!(asctime(&tm), "Mon Oct 19 08:34:56 EDT 2026\n");
        assert_eq!(tm2sec(&tm), 1_792_413_296);
        assert_eq!(z.time(1_000_000).zone, "EST");
        assert!(Zone::parse(b"nonsense").is_none());
        assert!(Zone::parse(b"LONGNAME 0 X 0").is_none());
    }

    #[test]
    fn month_lengths() {
        assert_eq!(month_days(100, 1), 29);
        assert_eq!(month_days(200, 1), 28);
        assert_eq!(month_days(126, 9), 31);
    }
}
