//! Clock reads and the age strings shown next to each command. Every
//! timestamp in padloper is unix seconds.

use std::time::{SystemTime, UNIX_EPOCH};

/// The wall clock in unix seconds. A clock set before the epoch reads as 0
/// rather than failing: a wrong age beats refusing to record a command.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Coarse age for display: now, 5m, 3h, 2d, 4w, 6mo, 2y.
///
/// Buckets are approximate above a week: months are 30 days and years are
/// 365. The column is a glance, not a date.
pub fn relative_time(now: i64, ts: i64) -> String {
    // Clocks move backwards (ntp, suspend), and imported rows can carry a
    // future timestamp. Clamp instead of printing a negative age.
    let d = (now - ts).max(0);
    if d < 60 {
        "now".to_string()
    } else if d < 3600 {
        format!("{}m", d / 60)
    } else if d < 86400 {
        format!("{}h", d / 3600)
    } else if d < 7 * 86400 {
        format!("{}d", d / 86400)
    } else if d < 30 * 86400 {
        format!("{}w", d / 604800)
    } else if d < 365 * 86400 {
        format!("{}mo", d / 2592000)
    } else {
        format!("{}y", d / 31536000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One case per bucket edge. The boundaries are where an off-by-one in a
    // divisor shows up.
    #[test]
    fn each_bucket_boundary_formats_as_expected() {
        assert_eq!(relative_time(1000, 1000), "now");
        assert_eq!(relative_time(1000, 941), "now");
        assert_eq!(relative_time(1000, 940), "1m");
        assert_eq!(relative_time(3600, 0), "1h");
        assert_eq!(relative_time(86400, 0), "1d");
        assert_eq!(relative_time(7 * 86400, 0), "1w");
        assert_eq!(relative_time(30 * 86400, 0), "1mo");
        assert_eq!(relative_time(365 * 86400, 0), "1y");
        assert_eq!(relative_time(2 * 365 * 86400, 0), "2y");
    }

    #[test]
    fn a_timestamp_ahead_of_the_clock_reads_as_now() {
        assert_eq!(relative_time(100, 500), "now");
    }
}
