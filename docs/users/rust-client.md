# Rust client

Add the client directly from GitHub:

```toml
[dependencies]
scry-client = { git = "https://github.com/Ariaryy/scry-search" }
```

For reproducible builds, pin a release tag matching the installed daemon, for
example `tag = "v0.1.0-alpha.3"`. Without a tag, `Cargo.lock` pins the resolved
commit until dependencies are updated.

Add `scry-client` to the application and connect to the daemon:

```rust,no_run
use scry_client::{Client, QueryKind};

let mut client = Client::connect()?;
let results = client.query(QueryKind::PathTerms, "report type:file ext:pdf", 50)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

For search-as-you-type interfaces, keep one `SearchSession` alive. Submitting a
new query supersedes pending work; `poll_latest` never blocks the UI thread.

```rust,no_run
use scry_client::{Client, QueryKind};

let client = Client::connect()?;
let mut session = client.into_search_session(50);
session.submit(QueryKind::PathTerms, "rep")?;
if let Some(results) = session.poll_latest()? {
    // Render only the newest completed generation.
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Runnable example

The repository includes a small
[`SearchSession` example](../../examples/src/main.rs) that keeps one connection
alive and accepts queries from standard input. With `scryd` running, launch it
from the repository root:

```powershell
cargo run -p scry-example --release
```

See the [`examples/` guide](../../examples/README.md) for suggested queries.

Reuse the client or session. Opening a new pipe for every keystroke adds avoidable
latency and prevents cancellation from doing useful work.
