//! Random tokens, time, and small text helpers.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Random bytes from the operating system. If that fails (it shouldn't), a time-seeded mix, so a
/// token is never constant; the server refuses to start when the OS generator is broken
/// ([`os_random_works`]).
pub fn fill(buf: &mut [u8]) {
    if getrandom::fill(buf).is_ok() {
        return;
    }
    let mut x = now_ms() as u64 ^ u64::from(std::process::id()).rotate_left(32);
    for b in buf.iter_mut() {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *b = (z ^ (z >> 31)) as u8;
    }
}

pub fn os_random_works() -> bool {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).is_ok()
}

/// `n` random bytes in URL-safe base64 (no padding).
pub fn token(n: usize) -> String {
    let mut b = vec![0u8; n.min(256)];
    fill(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

pub fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    fill(&mut b);
    u64::from_le_bytes(b)
}

/// Milliseconds since 1970.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)).unwrap_or(0)
}

/// `YYYY-MM-DD HH-MM` (UTC) of a time in ms since 1970, for file names.
pub fn stamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
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
    format!("{y:04}-{m:02}-{d:02} {:02}-{:02}", rem / 3600, (rem % 3600) / 60)
}

/// Colours given to people (distinct on light and dark themes), picked from the user id so a
/// person keeps theirs.
pub const PALETTE: [&str; 8] = ["#3B7DD8", "#D8513B", "#2E9E5B", "#9B51E0", "#E0A100", "#1F9FB4", "#D63F8C", "#6B7A8F"];

pub fn color_for(user_id: &str) -> &'static str {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in user_id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let i = usize::try_from(h % PALETTE.len() as u64).unwrap_or(0);
    PALETTE.get(i).copied().unwrap_or("#3B7DD8")
}

/// At most `n` characters, no control characters.
pub fn clean(s: &str, n: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(n).collect()
}

/// HTML-escapes text for an element or a double-quoted attribute.
pub fn html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Percent-encodes everything but unreserved characters (RFC 3986).
pub fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` (and `+` as a space when `form`); `None` for bad escapes or bad UTF-8.
pub fn unpct(s: &str, form: bool) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' if form => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Constant-time comparison of two secrets.
pub fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(token(32).len(), 43);
        assert_ne!(token(16), token(16));
        assert_eq!(color_for("ada"), color_for("ada"));
        assert_eq!(html("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
        assert_eq!(pct("nc:a b/é"), "nc%3Aa%20b%2F%C3%A9");
        assert_eq!(unpct(&pct("nc:a b/é"), false).as_deref(), Some("nc:a b/é"));
        assert_eq!(unpct("a+b", true).as_deref(), Some("a b"));
        assert_eq!(unpct("%zz", false), None);
        assert_eq!(unpct("%4", false), None);
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert_eq!(clean("a\nb", 10), "ab");
        assert_eq!(stamp(0), "1970-01-01 00-00");
        assert_eq!(stamp(1_767_225_600_000 + 3_661_000), "2026-01-01 01-01");
        assert_eq!(stamp(951_782_400_000), "2000-02-29 00-00");
    }
}
