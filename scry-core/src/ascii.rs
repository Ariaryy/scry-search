/// ASCII-case-insensitive byte comparison helpers.
use std::cmp::Ordering;

pub fn cmp_ci(a: &[u8], b: &[u8]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let lo_x = x.to_ascii_lowercase();
        let lo_y = y.to_ascii_lowercase();
        match lo_x.cmp(&lo_y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

pub fn starts_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len()
        && hay
            .iter()
            .zip(needle.iter())
            .all(|(h, n)| h.eq_ignore_ascii_case(n))
}

/// Whether `hay` contains `needle_lower` under ASCII case folding of `hay`.
///
/// `needle_lower` must already be lowercase; an ASCII-uppercase needle byte
/// can never equal a folded haystack byte. Candidate starts are located with
/// SIMD `memchr2` over both cases of the first needle byte, so only those
/// positions pay for the byte-wise compare.
pub fn contains_ci(hay: &[u8], needle_lower: &[u8]) -> bool {
    let Some((&first, rest)) = needle_lower.split_first() else {
        return true;
    };
    if hay.len() < needle_lower.len() || first.is_ascii_uppercase() {
        return false;
    }
    let last_start = hay.len() - needle_lower.len();
    let upper = first.to_ascii_uppercase();
    let mut from = 0;
    while from <= last_start {
        let Some(offset) = memchr::memchr2(first, upper, &hay[from..=last_start]) else {
            return false;
        };
        let i = from + offset;
        if hay[i + 1..i + needle_lower.len()]
            .iter()
            .zip(rest)
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
        {
            return true;
        }
        from = i + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cmp_ci() {
        assert_eq!(cmp_ci(b"ABC", b"abc"), Ordering::Equal);
        assert_eq!(cmp_ci(b"abc", b"abd"), Ordering::Less);
        assert_eq!(cmp_ci(b"abc", b"ab"), Ordering::Greater);
    }

    #[test]
    fn test_starts_with_ci() {
        assert!(starts_with_ci(b"README.md", b"readme"));
        assert!(!starts_with_ci(b"read", b"readme"));
    }

    #[test]
    fn test_contains_ci() {
        assert!(contains_ci(b"Report_FINAL.docx", b"final"));
        assert!(contains_ci(b"abc", b""));
    }

    /// The original byte-at-a-time scan, kept as the reference behaviour.
    fn contains_ci_reference(hay: &[u8], needle_lower: &[u8]) -> bool {
        needle_lower.is_empty()
            || hay.windows(needle_lower.len()).any(|window| {
                window
                    .iter()
                    .zip(needle_lower)
                    .all(|(h, n)| h.to_ascii_lowercase() == *n)
            })
    }

    #[test]
    fn contains_ci_matches_the_reference_scan() {
        // Small alphabet with case pairs, a repeated first byte, punctuation
        // and non-ASCII UTF-8 so overlapping and near-miss starts are common.
        const ALPHABET: &[u8] = b"aAbB.\xC3\xA9";
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let hay: Vec<u8> = (0..next() % 12)
                .map(|_| ALPHABET[next() as usize % ALPHABET.len()])
                .collect();
            let needle: Vec<u8> = (0..next() % 5)
                .map(|_| ALPHABET[next() as usize % ALPHABET.len()])
                .collect();
            for needle in [needle.to_ascii_lowercase(), needle] {
                assert_eq!(
                    contains_ci(&hay, &needle),
                    contains_ci_reference(&hay, &needle),
                    "hay={hay:?} needle={needle:?}"
                );
            }
        }
    }
}
