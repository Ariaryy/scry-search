# IPC and RPC protocol

Transport is a local duplex byte-mode named pipe. Every frame is `u32` little-
endian payload length followed by that many bytes. The Rust client is the
supported integration boundary; this document records the current internal
wire shape, not a stable cross-version contract.

## Query request

```text
u8      kind       0 prefix, 1 substring, 2 wildcard, 3 path terms,
                   4 shared-index capability, 5 query statistics
u32 LE  limit
u8      order      0 relevance, 1 recent, 2 largest
string  pattern    u32 LE byte length + UTF-8
```

Malformed kinds, orderings, lengths, or UTF-8 are rejected. Server-side limits
are clamped before allocating the bounded top-k heap.

## Query result

```text
u32 LE  count
repeated count times:
    string path
    u64 LE size_bytes
    u32 LE mtime_unix_seconds
    u8     is_directory
optional trailer:
    "SCRE" u8(version=1) repeated count times: u8 size_exact
```

The additive trailer lets a current client distinguish exact empty files,
unknown file sizes, and lower-bound directory totals. A client decoding a
legacy response applies conservative inference. Recognized malformed trailers
are rejected.

## Shared-index capability

Kind 4 returns `SCRYSHR1`, a duplicated read-only section handle, section
length, generation, and serialized live-delta overlay. When the client's
generation is still current, handle zero means reuse the validated mapping.
See [shared-section.md](shared-section.md) for lifecycle and security rules.

## Cancellation and realtime use

One pipe may carry pipelined requests. The daemon associates newer interactive
requests with a generation and abandons superseded work at bounded checkpoints.
`SearchSession` exposes this as submit plus nonblocking poll; consumers should
keep one session rather than reconnect per keystroke.

## Security and compatibility

The elevated daemon creates an explicitly local-accessible pipe so an
unelevated same-machine client can query it. Shared handles are duplicated only
to the PID reported by the pipe itself and receive read/query rights only.

There is no overall protocol-version negotiation. Client and daemon releases
should be packaged together. Additive trailers are individually versioned;
unknown request discriminants and ordering values fail closed.

