//! Fingerprints of spec text.

/// 64-bit FNV-1a: stable across Rust versions and platforms (unlike `DefaultHasher`), so the
/// fingerprints saved in `.codecraft/spec-state.json` keep meaning after an upgrade.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Fingerprint of prose parts: runs of whitespace collapse, so re-wrapping a paragraph or
/// changing indentation does not change it, and the parts stay apart (`["ab", "c"]` ≠ `["a", "bc"]`).
pub fn fingerprint(parts: &[&str]) -> String {
    let mut norm = String::new();
    for part in parts {
        let mut first = true;
        for word in part.split_whitespace() {
            if !first {
                norm.push(' ');
            }
            first = false;
            norm.push_str(word);
        }
        norm.push('\n');
    }
    format!("{:016x}", fnv1a(norm.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_matches_the_reference_vectors() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn whitespace_does_not_change_a_fingerprint_but_words_do() {
        assert_eq!(fingerprint(&["a  b\n c"]), fingerprint(&[" a b c "]));
        assert_ne!(fingerprint(&["a b c"]), fingerprint(&["a b d"]));
        assert_ne!(fingerprint(&["ab", "c"]), fingerprint(&["a", "bc"]));
        assert_eq!(fingerprint(&["x"]).len(), 16);
    }
}
