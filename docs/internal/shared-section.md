# Shared-index queries

The daemon exposes the immutable arena through an anonymous pagefile-backed
section. A client asks over its existing named-pipe connection; the daemon gets
the peer PID from `GetNamedPipeClientProcessId` and duplicates a handle with
only `FILE_MAP_READ | SECTION_QUERY`. No section name or client-supplied PID is
accepted.

The base section is cached by base generation and rebuilt only after full
reindex or delta compaction. Each response also carries the current serialized
delta overlay and one generation counter. This keeps tombstones and newly added
records coherent with the base without copying the full arena after every USN
batch. The client validates each fresh archive mapping and validates the delta's
lengths, parents, UTF-8, and base cardinality before searching.

After mapping a generation, the client includes that generation in later
capability requests. If it is still current, the daemon returns only a small
header with a zero handle; it does not duplicate a handle or serialize the
overlay again. Full transfer occurs only when publication changed.

Clients retain a mapping while the daemon reports that generation as current.
When the daemon announces a replacement, the client installs it only after the
section and overlay validate. If replacement validation fails, the client
discards its older generation, keeps the handshake retry immediately due, and
uses RPC until a current mapping installs; it never knowingly answers from the
superseded snapshot.

The wire addition uses request discriminant 4. Discriminants 0–2 retain their
original byte encoding, and 3 remains reserved for path-term queries. Shared
responses begin with `SCRYSHR1`; old or malformed responses cannot be mistaken
for result frames.

Required invariants are covered by section round-trips and replacement tests,
bounded generation caching, failed-remap fallback, and local-versus-RPC search
equivalence with a non-empty overlay.
