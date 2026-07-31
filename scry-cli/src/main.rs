//! `scry <query>` — thin CLI over scry-client.
//!
//! Query syntax: a bare pattern (e.g. `report`) is a name prefix search.
//! `*`/`?` in the pattern switch to a wildcard search automatically.

use scry_client::Client;
use scry_core::protocol::QueryKind;

fn main() -> anyhow::Result<()> {
    let query: String = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if query.is_empty() {
        eprintln!("usage: scry <query>");
        std::process::exit(1);
    }

    let client = Client::connect()
        .map_err(|e| anyhow::anyhow!("{e}\nis scryd running?"))?;

    let kind = if query.contains('*') || query.contains('?') {
        QueryKind::Wildcard
    } else {
        QueryKind::Prefix
    };

    let results = client.query(kind, &query, 200)?;
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
