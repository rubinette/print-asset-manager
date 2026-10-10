//! Number and date formatting for asset metadata.

/// 1234567 → "1,234,567".
pub(super) fn group_digits(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Unix nanoseconds → "YYYY-MM-DD" (UTC).
pub(super) fn format_date(mtime_ns: i64) -> String {
    let days = mtime_ns.div_euclid(86_400 * 1_000_000_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    // gpui-kit's glob re-export shadows Rust's #[test]; use the prelude path.
    #[::core::prelude::v1::test]
    fn formats_counts_and_dates() {
        assert_eq!(super::group_digits(0), "0");
        assert_eq!(super::group_digits(999), "999");
        assert_eq!(super::group_digits(1_234_567), "1,234,567");
        assert_eq!(super::group_digits(-12_000), "-12,000");
        assert_eq!(super::format_date(0), "1970-01-01");
        // 2024-02-29T12:00:00Z
        assert_eq!(
            super::format_date(1_709_208_000 * 1_000_000_000),
            "2024-02-29"
        );
        // 2026-09-28T00:00:00Z
        assert_eq!(
            super::format_date(1_790_553_600 * 1_000_000_000),
            "2026-09-28"
        );
    }
}
