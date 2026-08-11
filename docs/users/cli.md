# CLI reference

```text
scry [OPTIONS] <QUERY>

--interactive       keep one realtime session and update while typing
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
