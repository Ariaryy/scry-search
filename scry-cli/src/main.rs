//! `scry <query>` — thin CLI over scry-client. Placeholder — see task #6.

fn main() -> anyhow::Result<()> {
    let query: String = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if query.is_empty() {
        eprintln!("usage: scry <query>");
        std::process::exit(1);
    }
    let _client = scry_client::Client::connect()?;
    println!("query: {query}");
    Ok(())
}
