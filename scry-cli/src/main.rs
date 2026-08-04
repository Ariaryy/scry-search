//! `scry <query>` — thin CLI over scry-client.
//!
//! Query syntax: a bare pattern (e.g. `report`) is a name prefix search.
//! `*`/`?` in the pattern switch to a wildcard search automatically.
//! `--interactive` types the query live against the daemon's pipelined
//! as-you-type endpoint instead of running one query and exiting.

mod console;

use scry_client::Client;
use scry_core::protocol::{QueryKind, ResultEntry};

const INTERACTIVE_LIMIT: u32 = 20;

fn main() -> anyhow::Result<()> {
    let mut shared = false;
    let mut verbose = false;
    let mut interactive = false;
    let mut explicit_kind = None;
    let mut terms = Vec::new();
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--shared-index" => shared = true,
            "--no-shared-index" => shared = false,
            "--verbose" => verbose = true,
            "--interactive" => interactive = true,
            "--prefix" => explicit_kind = Some(QueryKind::Prefix),
            "--substring" => explicit_kind = Some(QueryKind::Substring),
            "--wildcard" => explicit_kind = Some(QueryKind::Wildcard),
            _ => terms.push(argument),
        }
    }
    let query = terms.join(" ");

    let mut client = Client::connect().map_err(|e| anyhow::anyhow!("{e}\nis scryd running?"))?;

    if interactive {
        return run_interactive(&mut client, explicit_kind, query);
    }

    if query.is_empty() {
        eprintln!("usage: scry <query>");
        std::process::exit(1);
    }

    let kind = infer_query_kind(explicit_kind, &query);

    let results = if shared {
        if verbose {
            eprintln!("scry: shared-index query path (automatic RPC fallback)");
        }
        client.search_local(kind, &query, 200)?
    } else {
        if verbose {
            eprintln!("scry: RPC query path");
        }
        client.query(kind, &query, 200)?
    };
    if results.is_empty() {
        println!("no matches");
        return Ok(());
    }
    for entry in results {
        let marker = if entry.is_dir { "/" } else { "" };
        println!("{}{marker}\t{}", entry.path, entry.size);
    }
    Ok(())
}

/// Redraws the query line and its current results each keystroke, using the
/// daemon's pipelined `search_interactive` endpoint so a fast typist always
/// sees the answer to their latest keystroke rather than a queued stale one.
fn run_interactive(
    client: &mut Client,
    explicit_kind: Option<QueryKind>,
    initial: String,
) -> anyhow::Result<()> {
    let raw = console::RawMode::enable()
        .ok_or_else(|| anyhow::anyhow!("--interactive requires a real console"))?;

    let mut pattern = initial;
    let mut results = fetch(client, explicit_kind, &pattern)?;
    render(&pattern, &results);

    while let Some(unit) = raw.read_char() {
        match unit {
            0x03 | 0x1B => break, // Ctrl+C / Escape: quit without printing
            0x0D | 0x0A => break, // Enter: stop editing, print current results below
            0x08 | 0x7F => {
                pattern.pop();
            }
            _ => {
                if let Some(Ok(ch)) = char::decode_utf16([unit]).next() {
                    if !ch.is_control() {
                        pattern.push(ch);
                    }
                }
            }
        }
        results = fetch(client, explicit_kind, &pattern)?;
        render(&pattern, &results);
    }
    drop(raw);

    for entry in &results {
        let marker = if entry.is_dir { "/" } else { "" };
        println!("{}{marker}\t{}", entry.path, entry.size);
    }
    Ok(())
}

fn fetch(
    client: &mut Client,
    explicit_kind: Option<QueryKind>,
    pattern: &str,
) -> anyhow::Result<Vec<ResultEntry>> {
    if pattern.is_empty() {
        return Ok(Vec::new());
    }
    let kind = infer_query_kind(explicit_kind, pattern);
    client.search_interactive(kind, pattern, INTERACTIVE_LIMIT)
}

fn render(pattern: &str, results: &[ResultEntry]) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = writeln!(out, "scry> {pattern}");
    if results.is_empty() {
        let _ = writeln!(out, "  no matches");
    } else {
        for entry in results {
            let marker = if entry.is_dir { "/" } else { "" };
            let _ = writeln!(out, "  {}{marker}", entry.path);
        }
    }
    let _ = out.flush();
}

fn infer_query_kind(explicit_kind: Option<QueryKind>, query: &str) -> QueryKind {
    explicit_kind.unwrap_or_else(|| {
        if query.bytes().any(|byte| matches!(byte, b'*' | b'?')) {
            QueryKind::Wildcard
        } else {
            QueryKind::PathTerms
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_wildcard_from_glob_metacharacters() {
        assert_eq!(infer_query_kind(None, "*.pdf"), QueryKind::Wildcard);
        assert_eq!(infer_query_kind(None, "report.*"), QueryKind::Wildcard);
        assert_eq!(infer_query_kind(None, "report?.pdf"), QueryKind::Wildcard);
    }

    #[test]
    fn defaults_plain_queries_to_path_terms() {
        assert_eq!(
            infer_query_kind(None, "project notes"),
            QueryKind::PathTerms
        );
    }

    #[test]
    fn explicit_kind_takes_precedence() {
        assert_eq!(
            infer_query_kind(Some(QueryKind::Prefix), "*.pdf"),
            QueryKind::Prefix
        );
    }
}
