use crate::arena::ArchivedArena;
use crate::cancel::{Cancellation, CHECK_INTERVAL};
use crate::literals::required_literals;
use regex_automata::meta::Regex;
use regex_automata::util::syntax;

#[derive(Debug, Clone)]
pub enum Query {
    /// Anchored prefix — binary search over name-sorted records, O(log n + k).
    Prefix(String),
    /// Unanchored substring, accelerated by a trigram block filter.
    Substring(String),
    /// Wildcard (`*`/`?`) or regex, compiled to a DFA once and reused across the scan.
    Regex(String),
    /// AND-ed literal terms that may match the leaf or any ancestor.
    PathTerms(Vec<String>),
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
pub fn search_base(arena: &ArchivedArena, query: &Query, limit: usize) -> Vec<u32> {
    search_base_impl(arena, query, limit, None)
}

/// As `search_base`, but abandons the scan (returning an empty result) once
/// `cancel` reports a newer request superseded this one. Used only by the
/// daemon's interactive query path — a one-shot caller has nothing that could
/// supersede it, so it always uses the plain `search_base` above.
pub(crate) fn search_base_cancellable(
    arena: &ArchivedArena,
    query: &Query,
    limit: usize,
    cancel: Cancellation,
) -> Vec<u32> {
    search_base_impl(arena, query, limit, Some(cancel))
}

fn search_base_impl(
    arena: &ArchivedArena,
    query: &Query,
    limit: usize,
    cancel: Option<Cancellation>,
) -> Vec<u32> {
    match query {
        Query::Prefix(prefix) => {
            let range = arena.prefix_range(prefix);
            range.take(limit).collect()
        }
        Query::Substring(needle) => {
            let needle_lower: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
            let finder = memchr::memmem::Finder::new(&needle_lower);
            let mut results = Vec::new();
            let stopped = std::cell::Cell::new(false);
            let cancelled = std::cell::Cell::new(false);
            let mut scratch = Vec::new();
            let mut checked: u32 = 0;
            let mut visit = |idx, name: &[u8]| {
                if is_cancelled_periodically(cancel, &mut checked) {
                    cancelled.set(true);
                    stopped.set(true);
                    return std::ops::ControlFlow::Break(());
                }
                scratch.clear();
                scratch.extend(name.iter().map(|b| b.to_ascii_lowercase()));
                if finder.find(&scratch).is_some() {
                    results.push(idx);
                    if results.len() >= limit {
                        stopped.set(true);
                        return std::ops::ControlFlow::Break(());
                    }
                }
                std::ops::ControlFlow::Continue(())
            };
            match arena.candidate_blocks(&needle_lower) {
                None => arena.for_each_name(&mut visit),
                Some(blocks) => {
                    for block in blocks {
                        let start = block * crate::trigram::TRIGRAM_BLOCK as u32;
                        let end =
                            (start + crate::trigram::TRIGRAM_BLOCK as u32).min(arena.len() as u32);
                        arena.for_each_name_in(start..end, &mut visit);
                        if stopped.get() {
                            break;
                        }
                    }
                }
            }
            if cancelled.get() {
                return Vec::new();
            }
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
            let stopped = std::cell::Cell::new(false);
            let cancelled = std::cell::Cell::new(false);
            let mut checked: u32 = 0;
            let mut visit = |idx, name: &[u8]| {
                if is_cancelled_periodically(cancel, &mut checked) {
                    cancelled.set(true);
                    stopped.set(true);
                    return std::ops::ControlFlow::Break(());
                }
                if re.is_match(name) {
                    results.push(idx);
                    if results.len() >= limit {
                        stopped.set(true);
                        return std::ops::ControlFlow::Break(());
                    }
                }
                std::ops::ControlFlow::Continue(())
            };
            match required_literals(pattern)
                .and_then(|clauses| arena.candidate_blocks_for_clauses(&clauses))
            {
                None => arena.for_each_name(&mut visit),
                Some(blocks) => {
                    for block in blocks {
                        let start = block * crate::trigram::TRIGRAM_BLOCK as u32;
                        let end =
                            (start + crate::trigram::TRIGRAM_BLOCK as u32).min(arena.len() as u32);
                        arena.for_each_name_in(start..end, &mut visit);
                        if stopped.get() {
                            break;
                        }
                    }
                }
            }
            if cancelled.get() {
                return Vec::new();
            }
            results
        }
        Query::PathTerms(_) => Vec::new(),
    }
}

/// Advances `checked` and, once every `CHECK_INTERVAL` calls, reports whether
/// `cancel` has been superseded. Cheap in the common (non-cancellable) case:
/// `cancel` is `None` for every one-shot caller, so this is a single branch.
#[inline]
pub(crate) fn is_cancelled_periodically(cancel: Option<Cancellation>, checked: &mut u32) -> bool {
    let Some(cancel) = cancel else {
        return false;
    };
    *checked = checked.wrapping_add(1);
    (*checked).is_multiple_of(CHECK_INTERVAL) && cancel.is_cancelled()
}

#[cfg(test)]
mod tests {
    use super::{search_base, Query};
    use crate::arena::{ArchivedArena, Arena};
    use crate::store::{save, ArenaStore};
    use regex_automata::meta::Regex;
    use regex_automata::util::syntax;

    fn generated_store(count: usize) -> (tempfile::TempDir, ArenaStore) {
        let mut builder = Arena::builder();
        for i in 0..count {
            let name = match i % 10 {
                0 => format!("Report_{i:06}.PDF"),
                1 => format!("summary_{i:06}.pdf"),
                2 => format!("image_{i:06}.png"),
                3 => format!("photo_{i:06}.jpeg"),
                4 => format!("a{i:06}b.c"),
                _ => format!("unrelated_{i:06}.dat"),
            };
            builder.push(&name, 0, false);
        }
        let arena = builder.build().0;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("regex-filter.rkyv");
        save(&arena, &path).unwrap();
        let store = ArenaStore::open(&path).unwrap();
        (dir, store)
    }

    fn regex_scan(arena: &ArchivedArena, pattern: &str, limit: usize) -> Vec<u32> {
        let re = Regex::builder()
            .syntax(syntax::Config::new().case_insensitive(true))
            .build(pattern)
            .unwrap();
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

    #[test]
    fn regex_results_are_identical_with_and_without_the_filter() {
        let (_dir, store) = generated_store(20_000);
        let arena = store.archived();
        let patterns = [
            r"^.*\.pdf$",
            r"^report.*$",
            r"^.*report.*$",
            r"^.*\.(png|jpeg)$",
            r"^a.b\.c$",
            r"^(ab|report).*$",
            r".*",
            r"^does-not-exist$",
        ];
        for pattern in patterns {
            for limit in [1, 7, usize::MAX] {
                assert_eq!(
                    search_base(arena, &Query::Regex(pattern.into()), limit),
                    regex_scan(arena, pattern, limit),
                    "{pattern}, limit={limit}"
                );
            }
        }

        let clauses = crate::literals::required_literals(r"^.*\.jpeg$").unwrap();
        let selected = arena.candidate_blocks_for_clauses(&clauses).unwrap().len();
        let total = arena.len().div_ceil(crate::trigram::TRIGRAM_BLOCK);
        assert!(
            selected * 5 < total,
            "filter selected {selected}/{total} blocks"
        );
    }

    #[test]
    fn wildcard_results_are_identical_with_and_without_the_filter() {
        let (_dir, store) = generated_store(20_000);
        let arena = store.archived();
        for wildcard in ["*.pdf", "report*", "*report*", "*.?ng", "*"] {
            let Query::Regex(pattern) = Query::wildcard(wildcard) else {
                unreachable!()
            };
            for limit in [1, 7, usize::MAX] {
                assert_eq!(
                    search_base(arena, &Query::Regex(pattern.clone()), limit),
                    regex_scan(arena, &pattern, limit),
                    "{wildcard}, limit={limit}"
                );
            }
        }
    }

    #[test]
    #[ignore = "million-record release benchmark"]
    fn bench_regex_filtered_vs_scan() {
        let (_dir, store) = generated_store(1_000_000);
        let arena = store.archived();
        for pattern in [
            r"^.*\.pdf$",
            r"^report.*$",
            r"^.*report.*$",
            r"^.*\.(png|jpeg)$",
            r".*",
        ] {
            let start = std::time::Instant::now();
            let filtered = search_base(arena, &Query::Regex(pattern.into()), usize::MAX);
            let filtered_elapsed = start.elapsed();
            let start = std::time::Instant::now();
            let scanned = regex_scan(arena, pattern, usize::MAX);
            let scan_elapsed = start.elapsed();
            assert_eq!(filtered, scanned, "{pattern}");

            let selected = crate::literals::required_literals(pattern)
                .and_then(|clauses| arena.candidate_blocks_for_clauses(&clauses))
                .map(|blocks| blocks.len());
            eprintln!(
                "regex {pattern:?}: filtered={filtered_elapsed:?}, scan={scan_elapsed:?}, blocks={selected:?}/{}",
                arena.len().div_ceil(crate::trigram::TRIGRAM_BLOCK)
            );
        }
    }
}
