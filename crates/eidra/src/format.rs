//! Number formatting for reports.

/// `1234567` as `1,234,567`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A count in a few characters: `950`, `10.7K`, `1.2M`.
pub fn compact(n: f64) -> String {
    match n {
        n if n >= 1e6 => format!("{:.1}M", n / 1e6),
        n if n >= 1e4 => format!("{:.0}K", n / 1e3),
        n if n >= 1e3 => format!("{:.1}K", n / 1e3),
        n => format!("{n:.0}"),
    }
}

/// Seconds as `45 s`, `3.2 min` or `1.5 h`.
pub fn duration(seconds: f64) -> String {
    match seconds {
        s if s >= 5400.0 => format!("{:.1} h", s / 3600.0),
        s if s >= 90.0 => format!("{:.1} min", s / 60.0),
        s => format!("{s:.0} s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(225_280), "225,280");
        assert_eq!(compact(950.0), "950");
        assert_eq!(compact(10_696.0), "11K");
        assert_eq!(compact(1_234.0), "1.2K");
        assert_eq!(compact(1_200_000.0), "1.2M");
        assert_eq!(duration(66.2), "66 s");
        assert_eq!(duration(600.0), "10.0 min");
        assert_eq!(duration(7200.0), "2.0 h");
    }
}
