# Rust client SDK

Add `scry-client` to the application and connect to the daemon:

```rust,no_run
use scry_client::Client;
use scry_core::protocol::QueryKind;

let mut client = Client::connect()?;
let results = client.query(QueryKind::PathTerms, "report type:file ext:pdf", 50)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

For search-as-you-type interfaces, keep one `SearchSession` alive. Submitting a
new query supersedes pending work; `poll_latest` never blocks the UI thread.

```rust,no_run
use scry_client::Client;
use scry_core::protocol::QueryKind;

let client = Client::connect()?;
let mut session = client.into_search_session(50);
session.submit(QueryKind::PathTerms, "rep")?;
if let Some(results) = session.poll_latest()? {
    // Render only the newest completed generation.
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

Reuse the client or session. Opening a new pipe for every keystroke adds avoidable
latency and prevents cancellation from doing useful work.
