//! Minimal **UTC-only** schedule parser for self-host monitors.
//!
//! Two forms are accepted (keeping it simple and correct, per the M6 note):
//!
//! 1. **Fixed interval** — `@every 300s`, `300s`, or a bare integer `300`
//!    (seconds). Next run = `last_or_now + interval`.
//! 2. **Cron** — a standard 5-field expression `min hour dom mon dow`, all in
//!    UTC. Each field is `*`, a single value, a comma list, a `a-b` range, or a
//!    `*/step`. Next run = the next minute boundary at/after `from+60s` whose
//!    fields all match.
//!
//! No external cron crate is used: a self-contained, deterministic UTC walker
//! is simpler to reason about and avoids pulling a scheduler dependency into
//! the open-core tree.

use crate::{MonitorError, MonitorResult};

/// A parsed schedule.
#[derive(Debug, Clone)]
pub enum Schedule {
    /// Fixed interval in seconds (>= 1).
    Interval(u64),
    Cron(CronExpr),
}

impl Schedule {
    /// Parse a schedule string. UTC-only.
    pub fn parse(s: &str) -> MonitorResult<Self> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("@every") {
            return parse_interval(rest.trim()).map(Schedule::Interval);
        }
        // bare "<n>" or "<n>s"
        if s.chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
            && s.split_whitespace().count() == 1
        {
            return parse_interval(s).map(Schedule::Interval);
        }
        CronExpr::parse(s).map(Schedule::Cron)
    }

    /// Compute the next run time (unix seconds, UTC) strictly after `from`
    /// (also unix seconds). For intervals the anchor is `from`.
    pub fn next_after(&self, from: i64) -> i64 {
        match self {
            Schedule::Interval(secs) => from + *secs as i64,
            Schedule::Cron(c) => c.next_after(from),
        }
    }
}

fn parse_interval(s: &str) -> MonitorResult<u64> {
    let digits = s.strip_suffix('s').unwrap_or(s);
    let n: u64 = digits
        .parse()
        .map_err(|_| MonitorError::Schedule(format!("invalid interval '{s}'")))?;
    if n == 0 {
        return Err(MonitorError::Schedule("interval must be >= 1s".into()));
    }
    Ok(n)
}

/// A parsed 5-field cron expression (UTC).
#[derive(Debug, Clone)]
pub struct CronExpr {
    minute: FieldSet, // 0-59
    hour: FieldSet,   // 0-23
    dom: FieldSet,    // 1-31
    month: FieldSet,  // 1-12
    dow: FieldSet,    // 0-6 (Sun=0)
}

impl CronExpr {
    pub fn parse(s: &str) -> MonitorResult<Self> {
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(MonitorError::Schedule(format!(
                "cron must have 5 fields (min hour dom mon dow), got {}",
                parts.len()
            )));
        }
        Ok(CronExpr {
            minute: FieldSet::parse(parts[0], 0, 59)?,
            hour: FieldSet::parse(parts[1], 0, 23)?,
            dom: FieldSet::parse(parts[2], 1, 31)?,
            month: FieldSet::parse(parts[3], 1, 12)?,
            dow: FieldSet::parse(parts[4], 0, 6)?,
        })
    }

    /// Next matching minute boundary strictly after `from` (unix seconds, UTC).
    pub fn next_after(&self, from: i64) -> i64 {
        // Start at the next whole minute strictly after `from`.
        let mut t = (from / 60 + 1) * 60;
        // Bound the search to ~4 years of minutes to avoid an infinite loop on
        // an impossible expression (e.g. Feb 30).
        for _ in 0..(366 * 4 * 24 * 60) {
            let dt = civil_from_unix(t);
            let dow_match = self.dow.contains(dt.weekday as u32);
            let dom_match = self.dom.contains(dt.day as u32);
            // Standard cron semantics: when BOTH dom and dow are restricted
            // (not `*`), the match is the UNION; otherwise the intersection.
            let day_ok = if self.dom.is_wildcard && self.dow.is_wildcard {
                true
            } else if !self.dom.is_wildcard && !self.dow.is_wildcard {
                dom_match || dow_match
            } else if self.dom.is_wildcard {
                dow_match
            } else {
                dom_match
            };
            if self.minute.contains(dt.minute as u32)
                && self.hour.contains(dt.hour as u32)
                && self.month.contains(dt.month as u32)
                && day_ok
            {
                return t;
            }
            t += 60;
        }
        // Fallback: schedule far in the future so the monitor effectively idles.
        from + 60 * 60 * 24 * 365
    }
}

/// A matchable cron field over an inclusive `[min,max]` range.
#[derive(Debug, Clone)]
struct FieldSet {
    allowed: Vec<bool>, // index 0..=max
    base: u32,
    is_wildcard: bool,
}

impl FieldSet {
    fn parse(spec: &str, min: u32, max: u32) -> MonitorResult<Self> {
        let mut allowed = vec![false; (max + 1) as usize];
        let is_wildcard = spec == "*" || spec.starts_with("*/");
        for token in spec.split(',') {
            let (range_part, step) = match token.split_once('/') {
                Some((r, s)) => {
                    let step: u32 = s
                        .parse()
                        .map_err(|_| MonitorError::Schedule(format!("bad step '{token}'")))?;
                    if step == 0 {
                        return Err(MonitorError::Schedule(format!("zero step '{token}'")));
                    }
                    (r, step)
                }
                None => (token, 1),
            };
            let (lo, hi) = if range_part == "*" {
                (min, max)
            } else if let Some((a, b)) = range_part.split_once('-') {
                (
                    a.parse()
                        .map_err(|_| MonitorError::Schedule(format!("bad range '{token}'")))?,
                    b.parse()
                        .map_err(|_| MonitorError::Schedule(format!("bad range '{token}'")))?,
                )
            } else {
                let v: u32 = range_part
                    .parse()
                    .map_err(|_| MonitorError::Schedule(format!("bad value '{token}'")))?;
                (v, v)
            };
            if lo < min || hi > max || lo > hi {
                return Err(MonitorError::Schedule(format!(
                    "cron field '{token}' out of range [{min},{max}]"
                )));
            }
            let mut v = lo;
            while v <= hi {
                allowed[v as usize] = true;
                v += step;
            }
        }
        Ok(FieldSet {
            allowed,
            base: min,
            is_wildcard,
        })
    }

    fn contains(&self, v: u32) -> bool {
        let _ = self.base;
        (v as usize) < self.allowed.len() && self.allowed[v as usize]
    }
}

/// Minimal civil (UTC) date/time decomposition of a unix timestamp.
struct Civil {
    month: u8,   // 1-12
    day: u8,     // 1-31
    hour: u8,    // 0-23
    minute: u8,  // 0-59
    weekday: u8, // 0=Sun
}

/// Convert unix seconds → UTC civil date. Uses Howard Hinnant's
/// `civil_from_days` algorithm; no external dependency.
fn civil_from_unix(secs: i64) -> Civil {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u8;
    let minute = ((secs_of_day % 3600) / 60) as u8;
    // weekday: 1970-01-01 was a Thursday (=4 with Sun=0).
    let weekday = ((days.rem_euclid(7) + 4) % 7) as u8;

    // civil_from_days (days since 1970-01-01).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let _y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8; // [1, 12]

    Civil {
        month,
        day,
        hour,
        minute,
        weekday,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_intervals() {
        assert!(matches!(
            Schedule::parse("@every 300s").unwrap(),
            Schedule::Interval(300)
        ));
        assert!(matches!(
            Schedule::parse("60s").unwrap(),
            Schedule::Interval(60)
        ));
        assert!(matches!(
            Schedule::parse("90").unwrap(),
            Schedule::Interval(90)
        ));
        assert!(Schedule::parse("0s").is_err());
    }

    #[test]
    fn interval_next_after() {
        let s = Schedule::parse("300s").unwrap();
        assert_eq!(s.next_after(1000), 1300);
    }

    #[test]
    fn civil_decode_known_epoch() {
        // 2021-01-01 00:00:00 UTC = 1609459200, a Friday (weekday 5).
        let c = civil_from_unix(1_609_459_200);
        assert_eq!(c.month, 1);
        assert_eq!(c.day, 1);
        assert_eq!(c.hour, 0);
        assert_eq!(c.minute, 0);
        assert_eq!(c.weekday, 5);
    }

    #[test]
    fn cron_every_minute() {
        let s = Schedule::parse("* * * * *").unwrap();
        // next strictly-after a :30 second mark is the next minute boundary.
        assert_eq!(s.next_after(1_609_459_230), 1_609_459_260);
    }

    #[test]
    fn cron_specific_hour_minute() {
        // 03:15 UTC daily. From 2021-01-01 00:00:00 → 2021-01-01 03:15:00.
        let s = Schedule::parse("15 3 * * *").unwrap();
        let next = s.next_after(1_609_459_200);
        let c = civil_from_unix(next);
        assert_eq!((c.hour, c.minute), (3, 15));
    }

    #[test]
    fn cron_step_field() {
        let s = Schedule::parse("*/15 * * * *").unwrap();
        // From 00:00:00, next is 00:15:00.
        let next = s.next_after(1_609_459_200);
        let c = civil_from_unix(next);
        assert_eq!(c.minute, 15);
    }

    #[test]
    fn cron_rejects_bad_field_count() {
        assert!(Schedule::parse("* * *").is_err());
    }

    #[test]
    fn cron_rejects_out_of_range() {
        assert!(Schedule::parse("99 * * * *").is_err());
    }
}
