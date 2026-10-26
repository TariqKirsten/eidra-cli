//! Text helpers: terminal output, truncation, line numbers, hashing, dates.

use std::fmt::Write as _;

/// Removes ANSI escape sequences, and resolves carriage returns the way a terminal shows them.
pub fn clean_terminal_output(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    let mut out = String::with_capacity(stripped.len());
    for (i, line) in stripped.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // A progress bar redraws its line after \r: keep what was drawn last.
        let line = line.strip_suffix('\r').unwrap_or(line);
        out.push_str(line.rsplit('\r').next().unwrap_or(line));
    }
    out
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then one final character in @..~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        chars.next_if_eq(&'\\');
                        break;
                    }
                }
            }
            // Any other escape is two characters long.
            _ => {}
        }
    }
    out
}

/// `text` cut to about `limit` bytes: its start and its end, with a marker for what was left out.
/// Cuts fall on line breaks where there are any.
pub fn truncate_middle(text: &str, limit: usize, note: &str) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let head = char_floor(text, limit * 2 / 5);
    let head = text[..head].rfind('\n').map_or(head, |i| i + 1);
    let tail = char_ceil(text, text.len() - limit * 3 / 5);
    let tail = text[tail..].find('\n').map_or(tail, |i| tail + i + 1).max(head);
    format!("{}[… {} bytes left out{note} …]\n{}", &text[..head], tail - head, &text[tail..])
}

/// `lines` numbered from `first`, the way `cat -n` shows them.
pub fn numbered<'a>(lines: impl IntoIterator<Item = &'a str>, first: usize) -> String {
    let mut out = String::new();
    for (i, line) in lines.into_iter().enumerate() {
        let _ = writeln!(out, "{:>6}\t{line}", first + i);
    }
    out
}

pub fn char_floor(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn char_ceil(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// FNV-1a: a stable 64-bit hash for noticing changes, not for security.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A UTC date and time for seconds since the epoch: (year, month, day, hour, minute, second).
pub fn utc_datetime(unix_secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (unix_secs / 86_400) as i64;
    let secs = unix_secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, (secs / 3600) as u32, (secs / 60 % 60) as u32, (secs % 60) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_colours_and_progress_bars() {
        let raw = "\x1b[32mok\x1b[0m 3 passed\r\n 10%\r 50%\r100%\n\x1b]0;title\x07done";
        assert_eq!(clean_terminal_output(raw), "ok 3 passed\n100%\ndone");
    }

    #[test]
    fn truncates_on_line_breaks_and_says_so() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let cut = truncate_middle(&text, 200, "; see out.txt");
        assert!(cut.starts_with("line 0\n"));
        assert!(cut.ends_with("line 99\n"));
        assert!(cut.contains("bytes left out; see out.txt …]\n"));
        assert!(cut.len() < 300);
        assert_eq!(truncate_middle("short", 200, ""), "short");
    }

    #[test]
    fn numbers_lines_like_cat() {
        assert_eq!(numbered(["a", "b"], 9), "     9\ta\n    10\tb\n");
    }

    #[test]
    fn dates_from_epoch_seconds() {
        assert_eq!(utc_datetime(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(utc_datetime(1_793_000_000), (2026, 10, 26, 7, 33, 20));
        assert_eq!(utc_datetime(951_782_400), (2000, 2, 29, 0, 0, 0));
    }
}
