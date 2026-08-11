# IPC protocol

Transport: Win32 named pipe, `\\.\pipe\scry` (`scry_ipc::PIPE_NAME`), byte-mode, duplex.
Framing (`scry-ipc/src/lib.rs`): every message is a u32 LE length prefix followed by that
many bytes. `Pipe::read_frame`/`write_frame` handle this identically on both ends.

This is deliberately not rkyv/flatbuffers — request/result payloads are small (a query
string, a page of paths), so a hand-rolled cursor-based encoding (`scry-core/src/protocol.rs`)
is simpler than schema-generated code. rkyv is reserved for the large index payload where
zero-copy actually matters.

## Request

```
u8      QueryKind   0 = Prefix, 1 = Substring, 2 = Wildcard
string  pattern     u32 LE length prefix + UTF-8 bytes
u32     limit
```

## Results

```
u32     count
repeated count times:
    string  path
    u64     size
    u8      is_dir  (0 or 1)
```

Decoding (`decode_request`/`decode_results`) is bounds-checked throughout via a private
`Cursor` that returns `Option` on any out-of-range read — a truncated or malformed frame
decodes to `None` rather than panicking or reading out of bounds.

## Server-side access control

`scryd` typically runs elevated (MFT/USN access requires it). A named pipe created by an
elevated process inherits a DACL that blocks unelevated clients by default — silently, as
`ERROR_ACCESS_DENIED` on `CreateFileW`. `PipeServer` works around this by building an explicit
security descriptor from SDDL `D:(A;;GA;;;WD)` (`Everyone`: generic all) and passing it to
every `CreateNamedPipeW` call. This only affects local access — `\\.\pipe\...` names aren't
network-reachable regardless.

## Wire compatibility

There is no version field. `scry-client` and `scryd` are expected to be built from the same
commit; this is not a stable cross-version protocol.
