/// ASCII-case-insensitive byte comparison helpers.
use std::cmp::Ordering;

pub fn cmp_ci(a: &[u8], b: &[u8]) -> Ordering {
    let len = a.len().min(b.len());
    for i in 0..len {
        let x = a[i].to_ascii_lowercase();
        let y = b[i].to_ascii_lowercase();
        if x != y {
            return x.cmp(&y);
        }
    }
    a.len().cmp(&b.len())
}

pub fn starts_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    if hay.len() < needle.len() {
        return false;
    }
    for i in 0..needle.len() {
        if hay[i].to_ascii_lowercase() != needle[i].to_ascii_lowercase() {
            return false;
        }
    }
    true
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
