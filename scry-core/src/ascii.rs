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

pub fn contains_ci(hay: &[u8], needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if hay.len() < needle_lower.len() {
        return false;
    }
    for i in 0..=(hay.len() - needle_lower.len()) {
        let mut match_found = true;
        for j in 0..needle_lower.len() {
            if hay[i + j].to_ascii_lowercase() != needle_lower[j] {
                match_found = false;
                break;
            }
        }
        if match_found {
            return true;
        }
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
}
