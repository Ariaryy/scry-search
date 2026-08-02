//! `scry <query>` — thin CLI over scry-client.
//!
//! Query syntax: a bare pattern (e.g. `report`) is a name prefix search.
//! `*`/`?` in the pattern switch to a wildcard search automatically.

use scry_client::Client;
use scry_core::protocol::QueryKind;

fn main() -> anyhow::Result<()> {
    let mut no_shared = false;
    let mut verbose = false;
    let mut explicit_kind = None;
    let mut terms = Vec::new();
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--no-shared-index" => no_shared = true,
            "--verbose" => verbose = true,
            "--prefix" => explicit_kind = Some(QueryKind::Prefix),
            "--substring" => explicit_kind = Some(QueryKind::Substring),
            "--wildcard" => explicit_kind = Some(QueryKind::Wildcard),
            _ => terms.push(argument),
        }
    }
    let query = terms.join(" ");
    if query.is_empty() {
        eprintln!("usage: scry <query>");
        std::process::exit(1);
    }

    let mut client = Client::connect().map_err(|e| anyhow::anyhow!("{e}\nis scryd running?"))?;

    let kind = explicit_kind.unwrap_or(QueryKind::PathTerms);

    let results = if no_shared {
        if verbose {
            eprintln!("scry: RPC query path");
        }
        client.query(kind, &query, 200)?
    } else {
        if verbose {
            eprintln!("scry: shared-index query path (automatic RPC fallback)");
        }
        client.search_local(kind, &query, 200)?
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
