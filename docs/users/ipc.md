# IPC integration

The supported Rust API is `scry-client`. It owns framing, cancellation,
shared-index validation, and RPC fallback. Applications in other languages can
implement the named-pipe protocol described in [the contributor protocol
reference](../ipc-protocol.md), but the wire format is not yet covered by a
stable compatibility guarantee.

The transport is request/response RPC over local IPC. A client may also request
a read-only shared section and execute searches locally; if sharing or mapping
validation fails, the Rust client falls back to RPC automatically.

A stable C ABI is not available yet. Consumers that need long-term binary
compatibility should isolate their protocol adapter or wait for the planned SDK
shim.

