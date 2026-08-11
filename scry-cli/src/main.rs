//! `scry <query>` — thin CLI over scry-client.
//!
//! Query syntax: a bare pattern (e.g. `report`) is a name prefix search.
//! `*`/`?` in the pattern switch to a wildcard search automatically.
//! `--interactive` types the query live against the daemon's pipelined
//! as-you-type endpoint instead of running one query and exiting.
//! `--sort recent|size|relevance` picks the result ordering (default
//! relevance).

mod console;

use scry_client::{Client, SearchSession};
use scry_core::protocol::{Order, QueryKind, ResultEntry};

const INTERACTIVE_LIMIT: u32 = 20;

fn main() -> anyhow::Result<()> {
    let t0 = std::time::Instant::now();
    let mut shared = false;
    let mut verbose = false;
    let mut interactive = false;
    let mut stats = false;
    let mut explicit_kind = None;
    let mut order = Order::default();
    let mut terms = Vec::new();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--shared-index" => shared = true,
            "--no-shared-index" => shared = false,
            "--verbose" => verbose = true,
            "--interactive" => interactive = true,
            "--stats" => stats = true,
            "--prefix" => explicit_kind = Some(QueryKind::Prefix),
            "--substring" => explicit_kind = Some(QueryKind::Substring),
            "--wildcard" => explicit_kind = Some(QueryKind::Wildcard),
            "--sort" => {
                let value = arguments.next().unwrap_or_default();
                order = parse_order(&value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown --sort value {value:?}; expected relevance, recent or size"
                    )
                })?;
            }
            _ => terms.push(argument),
        }
    }
    let query = terms.join(" ");

    let t_args = t0.elapsed();
    let mut client = Client::connect().map_err(|e| anyhow::anyhow!("{e}\nis scryd running?"))?;
    let t_connect = t0.elapsed();

    if interactive {
        let mut session = client.into_search_session(INTERACTIVE_LIMIT);
        session.set_order(order);
        return run_interactive(session, explicit_kind, query);
    }

    if stats {
        println!("{}", client.stats()?);
        return Ok(());
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
        client.search_local_ordered(kind, &query, 200, order)?
    } else {
        if verbose {
            eprintln!("scry: RPC query path");
        }
        client.query_ordered(kind, &query, 200, order)?
    };
    let t_query = t0.elapsed();
    let empty = results.is_empty();
    if empty {
        println!("no matches");
    } else {
        for entry in results {
            print_entry(&entry);
        }
    }
    let t_print = t0.elapsed();
    if verbose {
        // Measured against a two-volume, ~2.7M-record corpus: `connect` and
        // `print` are consistently sub-millisecond, and `args` is
        // microseconds — process creation (before this process's `main`
        // even starts, so it isn't captured here) and the RPC round trip
        // are where end-to-end latency actually goes. See the "measure CLI
        // overhead" note in AGENTS.md.
        eprintln!(
            "scry: timing args={:?} connect={:?} query={:?} print={:?} total={:?}",
            t_args,
            t_connect - t_args,
            t_query - t_connect,
            t_print - t_query,
            t_print
        );
    }
    Ok(())
}

/// The ordering names accepted by `--sort`. `size` rather than `largest`
/// because that is what the column is called on screen.
fn parse_order(value: &str) -> Option<Order> {
    match value {
        "relevance" => Some(Order::Relevance),
        "recent" => Some(Order::Recent),
        "size" => Some(Order::Largest),
        _ => None,
    }
}

fn print_entry(entry: &ResultEntry) {
    let marker = if entry.is_dir { "/" } else { "" };
    println!("{}{marker}\t{}\t{}", entry.path, entry.size, entry.mtime);
}

fn run_interactive(
    mut session: SearchSession,
    explicit_kind: Option<QueryKind>,
    initial: String,
) -> anyhow::Result<()> {
    let raw = console::RawMode::enable()
        .ok_or_else(|| anyhow::anyhow!("--interactive requires a real console"))?;

    let mut pattern = initial;
    let mut results = Vec::new();
    let mut rendered_lines = 0;
    let mut print_results = false;
    session.submit(infer_query_kind(explicit_kind, &pattern), &pattern)?;
    render(&pattern, &results, true, &mut rendered_lines);

    'outer: loop {
        let mut edited = false;
        while let Some(unit) = raw.try_read_char() {
            match unit {
                0x03 | 0x1B => break 'outer, // Ctrl+C / Escape: quit without printing
                0x0D | 0x0A => {
                    print_results = true;
                    break 'outer;
                }
                0x08 | 0x7F => {
                    pattern.pop();
                    edited = true;
                }
                _ => {
                    if let Some(Ok(ch)) = char::decode_utf16([unit]).next() {
                        if !ch.is_control() {
                            pattern.push(ch);
                            edited = true;
                        }
                    }
                }
            }
        }

        if edited {
            session.submit(infer_query_kind(explicit_kind, &pattern), &pattern)?;
            render(&pattern, &results, true, &mut rendered_lines);
        }

        if let Some(latest) = session.poll_latest()? {
            results = latest;
            render(&pattern, &results, false, &mut rendered_lines);
        }

        std::thread::sleep(std::time::Duration::from_millis(8));
    }

    if print_results && session.is_pending() {
        results = session.wait_latest()?;
    }
    clear_interactive();
    drop(raw);

    if print_results {
        for entry in &results {
            print_entry(entry);
        }
    }
    Ok(())
}

fn render(pattern: &str, results: &[ResultEntry], pending: bool, previous_lines: &mut usize) {
    use std::io::Write;
    let mut out = std::io::stdout();
    render_to(&mut out, pattern, results, pending, previous_lines);
    let _ = out.flush();
}

fn render_to(
    out: &mut impl std::io::Write,
    pattern: &str,
    results: &[ResultEntry],
    pending: bool,
    previous_lines: &mut usize,
) {
    let _ = write!(out, "\x1b[H\x1b[2K\rscry> {pattern}");
    if pending {
        let _ = write!(out, "  searching...");
    }
    let _ = writeln!(out);
    if results.is_empty() {
        let _ = writeln!(out, "\x1b[2K\r  no matches");
    } else {
        for (index, entry) in results.iter().enumerate() {
            let marker = if entry.is_dir { "/" } else { "" };
            let _ = writeln!(out, "\x1b[2K\r  {:>2}  {}{marker}", index + 1, entry.path);
        }
    }
    let lines = results.len().max(1) + 1;
    for _ in lines..*previous_lines {
        let _ = writeln!(out, "\x1b[2K\r");
    }
    *previous_lines = lines;
}

fn clear_interactive() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[2J\x1b[H");
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
    fn sort_names_map_onto_orders() {
        assert_eq!(parse_order("recent"), Some(Order::Recent));
        assert_eq!(parse_order("size"), Some(Order::Largest));
        assert_eq!(parse_order("relevance"), Some(Order::Relevance));
        assert_eq!(parse_order("largest"), None);
    }

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

    #[test]
    fn redraw_keeps_results_while_searching_and_avoids_full_clear() {
        let results = vec![ResultEntry {
            path: "volume\\item".into(),
            is_dir: false,
            size: 0,
            mtime: 0,
        }];
        let mut output = Vec::new();
        let mut lines = 0;
        render_to(&mut output, "it", &results, true, &mut lines);
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("scry> it  searching..."));
        assert!(rendered.contains("volume\\item"));
        assert!(!rendered.contains("\x1b[2J"));
    }
}
