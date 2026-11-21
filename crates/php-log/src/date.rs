// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The log timestamp `php_format_date("d-M-Y H:i:s e", ts, localtime)`
//! when no timezone applies: before the modules are activated
//! (`localtime = 0`) or without the date extension.

/// `ts` as `d-M-Y H:i:s e` in UTC (`01-Jan-1970 00:00:00 UTC`).
#[must_use]
pub fn format_log_date_utc(ts: i64) -> String {
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{d:02}-{}-{y} {:02}:{:02}:{:02} UTC",
        MON[usize::try_from(m - 1).unwrap_or(0)],
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates() {
        assert_eq!(format_log_date_utc(0), "01-Jan-1970 00:00:00 UTC");
        assert_eq!(format_log_date_utc(951_782_400), "29-Feb-2000 00:00:00 UTC");
        assert_eq!(
            format_log_date_utc(1_793_728_800),
            "03-Nov-2026 18:00:00 UTC"
        );
        assert_eq!(format_log_date_utc(-1), "31-Dec-1969 23:59:59 UTC");
    }
}
