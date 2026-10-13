//! Small helpers: base64, percent-encoding, a non-cryptographic random generator, UUIDs and
//! ISO-8601 timestamps (no extra dependencies).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub(crate) fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3).saturating_mul(4));
    for chunk in data.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let sym = |shift: u32| char::from(B64.get(((n >> shift) & 63) as usize).copied().unwrap_or(b'A'));
        out.push(sym(18));
        out.push(sym(12));
        out.push(if chunk.len() > 1 { sym(6) } else { '=' });
        out.push(if chunk.len() > 2 { sym(0) } else { '=' });
    }
    out
}

/// Percent-encode everything except RFC 3986 unreserved characters.
pub(crate) fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

/// A small xorshift generator seeded from the clock and a global counter (not cryptographic).
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new() -> Self {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        let c = COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        let mut seed = nanos ^ c ^ u64::from(std::process::id()).rotate_left(32);
        if seed == 0 {
            seed = 0x2545_F491_4F6C_DD1D;
        }
        let mut r = Self(seed);
        r.next();
        r
    }

    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish in `lo..hi` (`lo` when the range is empty).
    pub(crate) fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = hi.abs_diff(lo);
        lo.wrapping_add((self.next() % span) as i64)
    }

    pub(crate) fn pick(&mut self, alphabet: &[u8]) -> char {
        if alphabet.is_empty() {
            return 'a';
        }
        let i = (self.next() % alphabet.len() as u64) as usize;
        char::from(alphabet.get(i).copied().unwrap_or(b'a'))
    }
}

/// A random (version 4) UUID.
pub(crate) fn uuid_v4() -> String {
    let mut r = Rng::new();
    let a = r.next();
    let b = r.next();
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let src = if i < 8 { a } else { b };
        *byte = (src >> ((i % 8) * 8)) as u8;
    }
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let part = |r: std::ops::Range<usize>| hex.get(r).unwrap_or_default().to_string();
    format!("{}-{}-{}-{}-{}", part(0..8), part(8..12), part(12..16), part(16..20), part(20..32))
}

/// Seconds since the Unix epoch.
pub(crate) fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// (year, month, day, hour, minute, second) in UTC for Unix seconds.
pub(crate) fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's days-to-civil algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

/// `2026-10-13T12:34:56Z`.
pub(crate) fn iso_timestamp(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn uuid_and_time() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(u.chars().nth(14), Some('4'));
        assert_ne!(uuid_v4(), u);
        assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_timestamp(1_760_358_896), "2025-10-13T12:34:56Z");
        assert_eq!(url_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        let mut r = Rng::new();
        for _ in 0..100 {
            let v = r.range(5, 10);
            assert!((5..10).contains(&v));
        }
        assert_eq!(r.range(3, 3), 3);
        assert!(r.range(i64::MIN, i64::MAX) < i64::MAX);
    }
}
