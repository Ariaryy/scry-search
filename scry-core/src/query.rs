use crate::arena::ArchivedArena;
use regex_automata::meta::Regex;
use regex_automata::util::syntax;

#[derive(Debug, Clone)]
pub enum Query {
    /// Anchored prefix — hits the sorted name_order index via binary search, O(log n + k).
    Prefix(String),
    /// Unanchored substring — no index for this yet, linear scan over records.
    /// Fine up to a few million entries on modern hardware; an n-gram inverted
    /// index is the documented next step if this becomes the bottleneck.
    Substring(String),
    /// Wildcard (`*`/`?`) or regex, compiled to a DFA once and reused across the scan.
    Regex(String),
}

impl Query {
    pub fn wildcard(pattern: &str) -> Self {
        let mut re = String::with_capacity(pattern.len() + 2);
        re.push('^');
        for c in pattern.chars() {
            match c {
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                '.' | '+' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\' => {
                    re.push('\\');
                    re.push(c);
                }
                c => re.push(c),
            }
        }
        re.push('$');
        Query::Regex(re)
    }
}

/// Search results are returned as arena indices — callers reconstruct paths
/// lazily via `ArchivedArena::full_path`, and only for the slice they actually
/// send over IPC (streaming, not materializing the whole result set).
pub fn search(arena: &ArchivedArena, query: &Query, limit: usize) -> Vec<u32> {
    match query {
        Query::Prefix(prefix) => {
            let range = arena.prefix_range(prefix);
            range
                .take(limit)
                .map(|pos| arena.name_at_sorted(pos))
                .collect()
        }
        Query::Substring(needle) => {
            let needle_lower = needle.to_ascii_lowercase();
            arena
                .records
                .iter()
                .enumerate()
                .filter(|(_, rec)| {
                    rec.name
                        .to_ascii_lowercase()
                        .contains(needle_lower.as_str())
                })
                .take(limit)
                .map(|(i, _)| i as u32)
                .collect()
        }
        Query::Regex(pattern) => {
            let re = match Regex::builder()
                .syntax(syntax::Config::new().case_insensitive(true))
                .build(pattern)
            {
                Ok(re) => re,
                Err(_) => return Vec::new(),
            };
            arena
                .records
                .iter()
                .enumerate()
                .filter(|(_, rec)| re.is_match(rec.name.as_str()))
                .take(limit)
                .map(|(i, _)| i as u32)
                .collect()
        }
    }
}
