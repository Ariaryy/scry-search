# CLI reference

```text
scry [OPTIONS] <QUERY>

--interactive       realtime selectable search; Enter opens the selected path
--limit N           maximum results (default 50; interactive 12)
--prefix            force anchored leaf-name prefix matching
--substring         force unanchored leaf-name substring matching
--wildcard          force `*` / `?` wildcard matching
--sort VALUE        relevance (default), recent, or size
--shared-index      prefer validated local shared-index execution
--no-shared-index   force ordinary daemon RPC
--verbose           print client-side phase timings
--stats             print daemon query statistics
-h, --help          print help
-V, --version       print version
```

Plain input uses ancestor-aware path terms unless `*` or `?` selects wildcard
matching. Metadata predicates are documented in [search syntax](search-syntax.md).
The CLI process remains unelevated; only `scryd` requires elevation.

Interactive mode uses a bounded persistent search session. Results update after
each edit without blocking input, the latest query latency appears at the right
of the input row, and each result shows a human-readable size and local modified
date. The viewport is fully replaced so shorter result sets do not leave stale
rows. Use `Up`/`Down` to select, `Enter` to open through Windows, `Ctrl+C` to
copy the selected path, `Ctrl+Enter` to reveal it in Explorer, and `Esc` to
close.
