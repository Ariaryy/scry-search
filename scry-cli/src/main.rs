//! `scry <query>` — thin CLI over scry-client.
//!
//! Query syntax: bare words are path terms. Metadata predicates include
//! `type:file`, `type:dir`, `ext:rs,txt`, `size:>10mb`, and `modified:<7d`.
//! `*`/`?` in the pattern switch to a wildcard search automatically.
//! `--interactive` types the query live against the daemon's pipelined
//! as-you-type endpoint instead of running one query and exiting.
//! `--sort recent|size|relevance` picks the result ordering (default
//! relevance).

mod console;
mod interactive;

use scry_client::Client;
use scry_core::protocol::{Order, QueryKind, ResultEntry};
use std::io::IsTerminal;

const DEFAULT_LIMIT: u32 = 50;
const DEFAULT_INTERACTIVE_LIMIT: u32 = 12;

fn main() -> anyhow::Result<()> {
    let t0 = std::time::Instant::now();
    let mut shared = false;
    let mut verbose = false;
    let mut interactive = false;
    let mut stats = false;
    let mut explicit_kind = None;
    let mut order = Order::default();
    let mut limit = None;
    let mut terms = Vec::new();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--version" | "-V" => {
                println!("scry {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
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
            "--limit" => {
                let value = arguments.next().unwrap_or_default();
                let parsed = value.parse::<u32>().ok().filter(|value| *value > 0);
                limit = Some(
                    parsed.ok_or_else(|| anyhow::anyhow!("--limit expects a positive integer"))?,
                );
            }
            _ => terms.push(argument),
        }
    }
    let query = terms.join(" ");

    let t_args = t0.elapsed();
    let mut client = Client::connect().map_err(|e| anyhow::anyhow!("{e}\nis scryd running?"))?;
    let t_connect = t0.elapsed();

    if interactive {
        let mut session = client.into_search_session(limit.unwrap_or(DEFAULT_INTERACTIVE_LIMIT));
        session.set_order(order);
        return interactive::run(session, explicit_kind, query);
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
        client.search_local_ordered(kind, &query, limit.unwrap_or(DEFAULT_LIMIT), order)?
    } else {
        if verbose {
            eprintln!("scry: RPC query path");
        }
        client.query_ordered(kind, &query, limit.unwrap_or(DEFAULT_LIMIT), order)?
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

fn print_help() {
    println!(
        "scry {}\n\nUsage: scry [OPTIONS] <QUERY>\n\n  --interactive       Search while typing; arrows select and Enter opens\n  --limit N           Maximum results (default 50; interactive 12)\n  --prefix            Force prefix matching\n  --substring         Force substring matching\n  --wildcard          Force wildcard matching\n  --sort VALUE        relevance, recent, or size\n  --shared-index      Prefer validated local execution\n  --no-shared-index   Force daemon RPC\n  --verbose           Print client phase timings\n  --stats             Print daemon query statistics\n  -h, --help          Print help\n  -V, --version       Print version",
        env!("CARGO_PKG_VERSION")
    );
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
    if std::io::stdout().is_terminal() {
        let path = visible_terminal_text(&entry.path);
        let color = if entry.is_dir { "36" } else { "37" };
        println!(
            "  \x1b[1;{color}m{path}{marker}\x1b[0m  \x1b[2m{}  ·  {}\x1b[0m",
            display_size(entry),
            display_mtime(entry.mtime)
        );
    } else {
        println!(
            "{}{marker}\t{}\t{}",
            entry.path,
            display_size(entry),
            display_mtime(entry.mtime)
        );
    }
}

fn visible_terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

pub(crate) fn display_size(entry: &ResultEntry) -> String {
    if !(entry.size_exact || entry.is_dir && entry.size > 0) {
        return "—".into();
    }
    let prefix = if entry.size_exact { "" } else { "≥" };
    format!("{prefix}{}", human_size(entry.size))
}

fn human_size(kib: u64) -> String {
    const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];
    if kib == 0 {
        return "0 B".into();
    }
    let mut value = kib as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 10.0 || value.fract() < 0.05 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub(crate) fn display_mtime(mtime: u32) -> String {
    console::format_local_time(mtime).unwrap_or_else(|| "unknown date".into())
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
    fn sizes_distinguish_exact_lower_bound_and_unknown() {
        let entry = |size, is_dir, size_exact| ResultEntry {
            path: "item".into(),
            size,
            mtime: 0,
            is_dir,
            size_exact,
        };
        assert_eq!(display_size(&entry(42, false, true)), "42 KB");
        assert_eq!(display_size(&entry(0, false, true)), "0 B");
        assert_eq!(display_size(&entry(42, true, false)), "≥42 KB");
        assert_eq!(display_size(&entry(0, false, false)), "—");
        assert_eq!(human_size(1_536), "1.5 MB");
    }

    #[test]
    fn terminal_text_cannot_inject_control_sequences() {
        assert_eq!(visible_terminal_text("safe\u{1b}[2J"), "safe�[2J");
    }
}
