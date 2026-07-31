use crate::arena::ArchivedArena;
use crate::ascii;
use regex_automata::meta::Regex;
use regex_automata::util::syntax;

#[derive(Debug, Clone)]
pub enum Query {
    /// Anchored prefix — binary search over name-sorted records, O(log n + k).
    Prefix(String),
    /// Unanchored substring — linear scan; needle lowercased once before the loop.
    /// An n-gram inverted index is the documented next step if this becomes the
    /// bottleneck (plan 006).
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
            range.take(limit).collect()
        }
        Query::Substring(needle) => {
            // Lowercase once before the scan — not once per record.
            let needle_lower: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
            let mut results = Vec::new();
            arena.for_each_name(|idx, name| {
                if ascii::contains_ci(name, &needle_lower) {
                    results.push(idx);
                    if results.len() >= limit {
                        return std::ops::ControlFlow::Break(());
                    }
                }
                std::ops::ControlFlow::Continue(())
            });
            results
        }
        Query::Regex(pattern) => {
            let re = match Regex::builder()
                .syntax(syntax::Config::new().case_insensitive(true))
                .build(pattern)
            {
                Ok(re) => re,
                Err(_) => return Vec::new(),
            };
            let mut results = Vec::new();
            arena.for_each_name(|idx, name| {
                if re.is_match(name) {
                    results.push(idx);
                    if results.len() >= limit {
                        return std::ops::ControlFlow::Break(());
                    }
                }
                std::ops::ControlFlow::Continue(())
            });
            results
        }
    }
}
