# Search syntax

Plain words are path terms: each word may match the result name or one of its
ancestor directories. Quotes preserve spaces, and `*` or `?` selects wildcard
matching.

```text
report                         name or ancestor contains "report"
projects report                both terms occur somewhere along the path
"annual report"                preserve the phrase as one term
projects\atlas                 path separators split components into terms
*.toml                         wildcard leaf-name search
report?.pdf                    `?` matches one character
```

Matching is case-insensitive. Multiple path terms are combined with AND, but
they may be satisfied by different components of the same path. A query accepts
at most 16 terms.

Metadata predicates may be combined with path terms:

| Predicate | Meaning | Example |
|---|---|---|
| `type:file` | files only | `report type:file` |
| `type:dir` | directories only | `work type:dir` |
| `ext:` | one of several extensions | `ext:rs,toml` |
| `size:` | exact known size comparison | `video size:>100mb` |
| `modified:` | age comparison | `modified:<7d` |

Comparisons can be combined:

```text
report type:file ext:pdf,docx        PDF or DOCX files under matching paths
assets type:dir                      matching directories only
video size:>=100mb size:<2gb         files from 100 MB up to 2 GB
notes modified:<7d                   modified within the last seven days
archive modified:>52w                older than 52 weeks
```

Size operators are `=`, `>`, `>=`, `<`, and `<=`. Units are `b`, `kb`, `mb`,
`gb`, and `tb`. Age uses `<` for newer than and `>` for older than; units are
`s`, `m`, `h`, `d`, and `w`. Unknown or incomplete sizes do not satisfy a size
predicate.

## CLI examples

```powershell
scry 'projects report type:file ext:pdf,docx'
scry --sort recent 'notes modified:<7d'
scry --sort size 'video type:file size:>500mb'
scry --interactive 'source ext:rs'
```

Predicates are evaluated inside the index scan before the result limit, so a
selective filter does not discard better matches after top-k selection.

