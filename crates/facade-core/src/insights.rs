//! What the API says about an app's visitors as seen from outside: whether its
//! domains answer, how available it was, the requests that failed, how many
//! people came, and the bytes served.

use serde::{Deserialize, Serialize};

/// Seconds in a day: days are UTC days, counted since the epoch.
pub const DAY: u64 = 86_400;

/// The UTC day (days since the epoch) a time falls in.
pub fn day_of(at: u64) -> u64 {
    at / DAY
}

/// The UTC calendar date of a day: `(year, month 1-12, day 1-31)`.
pub fn civil(day: u64) -> (i64, u32, u32) {
    // Howard Hinnant's days-to-civil.
    let z = day as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The first day of the calendar month `day` is in.
pub fn month_start(day: u64) -> u64 {
    let (_, _, d) = civil(day);
    day - u64::from(d - 1)
}

/// What the outside check found for a domain, loading it as a visitor would:
/// public DNS, the real certificate, through its node's entry point.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reach {
    /// It answered (any HTTP status, a redirect included).
    pub ok: bool,
    /// When it was last checked.
    pub checked_at: u64,
    /// Time to the response's headers, when it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Why not, in a few words (`DNS: no address`, `TLS: certificate expired`,
    /// `timed out`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Since when it has not answered (`None` while it does).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down_since: Option<u64>,
    /// When it last answered, if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok: Option<u64>,
}

/// How an app stood during one minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Uptime {
    Up,
    Down,
    /// Stopped on purpose: not counted.
    Stopped,
    /// Nothing known (the server was down, the app did not exist).
    Unknown,
}

/// A run of minutes in one state, within a day: `[start, end)` minutes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slice {
    pub start: u16,
    pub end: u16,
    pub state: Uptime,
}

/// One day of an app's availability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayUptime {
    pub day: u64,
    /// Minutes up, down, stopped, and unknown (today: up to now).
    pub up: u32,
    pub down: u32,
    pub stopped: u32,
    pub unknown: u32,
    /// Where it was not up: down, stopped and unknown runs, in order.
    #[serde(default)]
    pub slices: Vec<Slice>,
}

impl DayUptime {
    /// Up time over the time it should have been up, in percent.
    pub fn percent(&self) -> Option<f64> {
        uptime_percent(u64::from(self.up), u64::from(self.down))
    }
}

pub fn uptime_percent(up: u64, down: u64) -> Option<f64> {
    (up + down > 0).then(|| up as f64 * 100.0 / (up + down) as f64)
}

/// An app's availability over the last days. Apps with a domain are down when
/// the outside check cannot load their main domain, or their container is
/// crashed or failed; others when their container is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Availability {
    /// Oldest first, today last.
    pub days: Vec<DayUptime>,
    /// Over all of `days`; `None` without a minute up or down.
    pub percent: Option<f64>,
    /// Measured by the outside check of this domain, or by the container state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

/// Visitors on one day: an estimate (within about 2 %) of the distinct clients
/// (address and browser) that sent a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayVisitors {
    pub day: u64,
    pub visitors: u64,
}

/// Bytes through the entry point, as on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bandwidth {
    /// The first day of the calendar month the figures are for.
    pub month_start: u64,
    /// Sent to visitors, and received from them, this month.
    pub month_out: u64,
    pub month_in: u64,
    pub today_out: u64,
    pub today_in: u64,
    /// The previous calendar month, when known.
    #[serde(default)]
    pub last_month_out: u64,
    #[serde(default)]
    pub last_month_in: u64,
}

/// `GET /api/apps/{name}/insights`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppInsights {
    pub availability: Availability,
    /// The last 30 days, oldest first, today last (days without any: 0).
    pub visitors: Vec<DayVisitors>,
    pub bandwidth: Bandwidth,
    /// The outside check of each of its domains.
    #[serde(default)]
    pub reach: Vec<DomainReach>,
    /// Whether the entry point counts any of this (an app it routes to).
    pub routed: bool,
}

impl AppInsights {
    pub fn visitors_on(&self, day: u64) -> Option<u64> {
        self.visitors
            .iter()
            .find(|v| v.day == day)
            .map(|v| v.visitors)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainReach {
    pub domain: String,
    pub reach: Reach,
}

/// `GET /api/nodes/{name}/insights`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeInsights {
    pub bandwidth: Bandwidth,
}

/// The periods failed requests are added up over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorPeriod {
    #[serde(rename = "1h")]
    Hour,
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
}

impl ErrorPeriod {
    pub const ALL: [ErrorPeriod; 3] = [ErrorPeriod::Hour, ErrorPeriod::Day, ErrorPeriod::Week];

    pub fn key(self) -> &'static str {
        match self {
            ErrorPeriod::Hour => "1h",
            ErrorPeriod::Day => "24h",
            ErrorPeriod::Week => "7d",
        }
    }

    pub fn secs(self) -> u64 {
        match self {
            ErrorPeriod::Hour => 3600,
            ErrorPeriod::Day => DAY,
            ErrorPeriod::Week => 7 * DAY,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.key() == s)
    }
}

/// Failed requests with the same status, method and normalised path over the
/// period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorRow {
    pub status: u16,
    pub method: String,
    pub path: String,
    pub count: u64,
    pub first_seen: u64,
    pub last_seen: u64,
}

/// `GET /api/apps/{name}/errors?period=24h`: most frequent first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorsView {
    pub period: ErrorPeriod,
    pub rows: Vec<ErrorRow>,
    /// Rows left out past the most frequent ones returned.
    #[serde(default)]
    pub more: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar() {
        assert_eq!(civil(0), (1970, 1, 1));
        // 2026-11-04
        let day = 20_761;
        assert_eq!(civil(day), (2026, 11, 4));
        assert_eq!(civil(month_start(day)), (2026, 11, 1));
        assert_eq!(civil(day_of(951_782_400)), (2000, 2, 29));
        assert_eq!(month_start(month_start(day)), month_start(day));
    }
}
