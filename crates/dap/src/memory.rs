//! Memory helpers: base64 for `readMemory`/`writeMemory` (DAP sends bytes as base64), addresses
//! as adapters write them, and hex byte input as people type it.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3).saturating_mul(4));
    for chunk in data.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let sextet = |shift: u32| ALPHABET.get(((n >> shift) & 63) as usize).map_or('A', |c| char::from(*c));
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

/// Decode standard (or URL-safe) base64; whitespace is ignored, padding is optional. `None` for
/// anything else.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut padding = false;
    for c in text.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => {
                padding = true;
                continue;
            }
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        if padding {
            return None;
        }
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// An address as adapters and people write it: `0x7ffe_1234`, `0X10`, `4096`. `None` for
/// anything else (including values that don't fit in 64 bits).
pub fn parse_address(text: &str) -> Option<u64> {
    let text = text.trim();
    let digits: String = text.chars().filter(|c| *c != '_').collect();
    match digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
        Some(hex) if !hex.is_empty() => u64::from_str_radix(hex, 16).ok(),
        Some(_) => None,
        None if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) => digits.parse().ok(),
        None => None,
    }
}

/// The address a value starts with (`0x000000016fdfccf8`, `0x10 "text"`, `(int *) 0x10`).
pub fn leading_address(value: &str) -> Option<u64> {
    let value = value.trim();
    let value = match value.strip_prefix('(') {
        // `(type) 0x…` as lldb prints pointers in some contexts.
        Some(rest) => rest.split_once(')').map_or(value, |(_, after)| after.trim_start()),
        None => value,
    };
    let token: String = value.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    if token.starts_with("0x") || token.starts_with("0X") { parse_address(&token) } else { None }
}

/// The DAP memory reference for an address (`0x…`, the form every adapter accepts).
pub fn reference(address: u64) -> String {
    format!("0x{address:x}")
}

/// Bytes typed as hex: `2a 00 ff`, `2a00ff`, `0x2a,0x00`. `None` if anything else is in it or
/// the digits don't pair up; at most `max` bytes.
pub fn parse_hex_bytes(text: &str, max: usize) -> Option<Vec<u8>> {
    let mut digits = String::new();
    for token in text.split(|c: char| c.is_whitespace() || c == ',') {
        let token = token.strip_prefix("0x").or_else(|| token.strip_prefix("0X")).unwrap_or(token);
        if !token.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        if token.len() % 2 == 1 {
            // A single digit (`a`) means `0a`.
            digits.push('0');
        }
        digits.push_str(token);
    }
    if digits.is_empty() || digits.len() / 2 > max {
        return None;
    }
    let bytes = digits.as_bytes();
    bytes.chunks(2).map(|pair| std::str::from_utf8(pair).ok().and_then(|p| u8::from_str_radix(p, 16).ok())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0, 255, 128, 7]] {
            let encoded = base64_encode(data);
            assert_eq!(base64_decode(&encoded).as_deref(), Some(data), "{encoded}");
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_decode("KQAAAAAAAABoZWxsbwAAAA==").unwrap()[8..13], *b"hello");
        assert_eq!(base64_decode("Zm8").as_deref(), Some(&b"fo"[..]));
        assert!(base64_decode("Zm8=x").is_none());
        assert!(base64_decode("@@").is_none());
    }

    #[test]
    fn addresses_parse() {
        assert_eq!(parse_address("0x16FDFCCF8"), Some(0x0001_6fdf_ccf8));
        assert_eq!(parse_address(" 4096 "), Some(4096));
        assert_eq!(parse_address("0x_10"), Some(16));
        assert_eq!(parse_address("0x"), None);
        assert_eq!(parse_address("0x1ffffffffffffffff"), None);
        assert_eq!(parse_address("&x"), None);
        assert_eq!(parse_address(""), None);
        assert_eq!(leading_address("0x000000016fdfccf8"), Some(0x0001_6fdf_ccf8));
        assert_eq!(leading_address("0x10 \"hi\""), Some(16));
        assert_eq!(leading_address("(int *) 0x20"), Some(32));
        assert_eq!(leading_address("42"), None);
        assert_eq!(leading_address("\"0x10\""), None);
        assert_eq!(reference(255), "0xff");
    }

    #[test]
    fn hex_bytes_parse() {
        assert_eq!(parse_hex_bytes("2a 00 ff", 16), Some(vec![0x2a, 0, 0xff]));
        assert_eq!(parse_hex_bytes("2a00ff", 16), Some(vec![0x2a, 0, 0xff]));
        assert_eq!(parse_hex_bytes("0x2a,0x1", 16), Some(vec![0x2a, 1]));
        assert_eq!(parse_hex_bytes("a", 16), Some(vec![0x0a]));
        assert_eq!(parse_hex_bytes("zz", 16), None);
        assert_eq!(parse_hex_bytes("", 16), None);
        assert_eq!(parse_hex_bytes("00 01 02", 2), None);
        assert_eq!(parse_hex_bytes("é", 2), None);
    }
}
