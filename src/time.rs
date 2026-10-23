//! The timestamps the client reads: key expiry from the account service (ISO 8601) and
//! `Retry-After` from the provider (an HTTP date). Parsed without dependencies; anything
//! malformed is `None`, never a panic.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `z` days after 1970-01-01, as `(year, month, day)`.
#[cfg(test)]
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A decimal number made of ASCII digits only, within `min..=max`.
fn num(s: &str, min: i64, max: i64) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok().filter(|n| (min..=max).contains(n))
}

/// Parses `2026-10-13T12:34:56.789Z`, also without the fraction and with a `±hh:mm` offset,
/// into milliseconds since the Unix epoch.
pub(crate) fn parse_iso_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T').or_else(|| s.split_once(' '))?;
    let mut d = date.split('-');
    let (y, mo, da) = (
        num(d.next()?, 1, 9999)?,
        num(d.next()?, 1, 12)?,
        num(d.next()?, 1, 31)?,
    );
    if d.next().is_some() {
        return None;
    }
    let (time, offset_min) =
        if let Some(t) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
            (t, 0)
        } else {
            let at = rest.rfind(['+', '-'])?;
            let (t, off) = rest.split_at(at);
            let sign = if off.starts_with('-') { -1 } else { 1 };
            let (oh, om) = off
                .get(1..)?
                .split_once(':')
                .unwrap_or((off.get(1..3)?, off.get(3..).unwrap_or("00")));
            (t, sign * (num(oh, 0, 23)? * 60 + num(om, 0, 59)?))
        };
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':');
    let (h, mi, se) = (
        num(t.next()?, 0, 23)?,
        num(t.next()?, 0, 59)?,
        num(t.next()?, 0, 60)?,
    );
    if t.next().is_some() {
        return None;
    }
    let ms = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        let padded = format!("{digits:0<3}");
        num(&padded, 0, 999)?
    };
    let days = days_from_civil(y, mo, da);
    Some(((days * 86_400 + h * 3600 + mi * 60 + se) - offset_min * 60) * 1000 + ms)
}

/// Parses an HTTP date in IMF-fixdate form (`Sun, 06 Nov 1994 08:49:37 GMT`) into milliseconds
/// since the Unix epoch.
pub(crate) fn parse_http_date_ms(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, day, month, year, time, zone] = parts.as_slice() else {
        return None;
    };
    if !zone.eq_ignore_ascii_case("GMT") {
        return None;
    }
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let m = MONTHS.iter().position(|n| n.eq_ignore_ascii_case(month))?;
    let m = i64::try_from(m).ok()? + 1;
    let mut t = time.split(':');
    let (h, mi, se) = (
        num(t.next()?, 0, 23)?,
        num(t.next()?, 0, 59)?,
        num(t.next()?, 0, 60)?,
    );
    let days = days_from_civil(num(year, 1, 9999)?, m, num(day, 1, 31)?);
    Some((days * 86_400 + h * 3600 + mi * 60 + se) * 1000)
}

/// `ms` as an ISO 8601 UTC timestamp with milliseconds.
#[cfg(test)]
pub(crate) fn format_iso_ms(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s, f) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{f:03}Z")
}
