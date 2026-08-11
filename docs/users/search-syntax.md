# Search syntax

Plain words are path terms: each word may match the result name or one of its
ancestor directories. Quotes preserve spaces, and `*` or `?` selects wildcard
matching.

Metadata predicates may be combined with path terms:

| Predicate | Meaning | Example |
|---|---|---|
| `type:file` | files only | `report type:file` |
| `type:dir` | directories only | `work type:dir` |
| `ext:` | one of several extensions | `ext:rs,toml` |
| `size:` | exact known size comparison | `video size:>100mb` |
| `modified:` | age comparison | `modified:<7d` |

Size units are `b`, `kb`, `mb`, `gb`, and `tb`. Age units are `s`, `m`, `h`,
`d`, and `w`. Unknown or incomplete sizes do not satisfy a size predicate.
Predicates are evaluated inside the index scan before the result limit, so a
selective filter does not discard better matches after top-k selection.

