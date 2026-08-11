use std::io::BufRead;
use std::time::Duration;

use scry_client::{QueryKind, SearchSession};

fn main() -> anyhow::Result<()> {
    let mut search = SearchSession::connect(20)?;
    eprintln!("Enter queries, for example: report type:file ext:pdf");

    for line in std::io::stdin().lock().lines() {
        let query = line?;
        search.submit(QueryKind::PathTerms, &query)?;

        while search.is_pending() {
            if let Some(results) = search.poll_latest()? {
                for result in results {
                    println!("{}", result.path);
                }
            } else {
                std::thread::sleep(Duration::from_millis(8));
            }
        }
    }
    Ok(())
}
