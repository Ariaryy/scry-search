# Updating Scry Search

`scry update` installs the latest non-prerelease GitHub release. Use
`scry update --version v0.1.0` to request an exact release tag instead.
An exact tag never falls back to another release. The target must include the
updater manifest described below. Earlier release archives cannot be installed
by this command.

The original terminal displays progress and waits for the result. Windows UAC
starts a hidden elevated helper under the same Windows account. Rejecting UAC
does not change the installation. The helper runs only during an update.

## Application integration contract

An embedding application should bundle a CLI from this implementation. It may
pin a release tested with its locked Rust client dependency:

```text
scry status --json --version <exact-v-prefixed-tag>
scry update --json --version <exact-v-prefixed-tag>
```

Arguments are separate process arguments, not shell text. JSON mode writes
newline-delimited JSON objects to stdout. Each object has `schema: 1` and an
`event` field. Stderr is diagnostic output, not part of the contract. Unknown
schema versions must be rejected. No executable path or shell command can be
supplied to the elevated helper.

Status emits one object. For example:

```json
{"schema":1,"event":"status","state":"ready","exit_code":0,"detail":{"server_pid":1234,"daemon_version":"0.1.0","ipc_version":1,"index_format":10,"capabilities":1}}
```

| Status state | Meaning | Exit code |
| --- | --- | --- |
| `ready` | RPC compatible and exact requested application version, if supplied | 0 |
| `update_required` | Recognized status, different requested application version | 10 |
| `incompatible` | Recognized status, incompatible RPC version | 10 |
| `unsupported_handshake` | Explicit `SCRYUNS1` discovery rejection | 10 |
| `not_installed` | Pipe absent and no executable in a recognized per-user installation | 11 |
| `stopped` | Pipe absent, recognized installed executable exists | 11 |
| `timeout` | Connected daemon did not finish discovery before the deadline | 11 |
| `access_denied` | Windows denied pipe access | 11 |
| `invalid_response` | Malformed, oversized, or unknown response schema | 11 |
| `transport_error` | Other pipe failure | 11 |

Failure objects have an empty `detail`. Installation discovery checks file
presence without executing the installed binary. It is not an installation
integrity audit. Discovery uses a dedicated connection, a three-second response
deadline, an 85-byte response cap, and the transport's existing two-second busy
pipe wait. Do not classify every timeout or transport error as an obsolete
daemon. The deployed pre-handshake daemon silently consumes unknown request
kinds. The host application owns corroborating legacy detection and its update UI.

Update emits zero or more `progress` objects followed by one confirmed `result`
object. `message` is display text, not an enum. A progress event has `state` equal
to `elevating` or `updating`. A result includes the process `exit_code`:

```json
{"schema":1,"event":"result","state":"updated","message":"Scry Search v0.1.0 is ready","exit_code":0}
```

| Update exit code | Meaning |
| --- | --- |
| 0 | Installed release ready, or interrupted activation recovered |
| 1 | Helper, configuration, or other failure |
| 20 | UAC cancelled |
| 21 | Release metadata or package download failed |
| 22 | Package, manifest, or release verification failed |
| 23 | Unsupported installation, task, architecture, or graceful shutdown |
| 24 | Interrupted transaction cannot yet be recovered |
| 25 | Replacement daemon did not become ready within ten minutes |
| 26 | Release requires another IPC/index migration policy |

Result states are `updated`, `recovered`, `failed`, and `recovery_required`.
Coordinator failures can also report `cancelled`. A caller must treat process
termination without a matching result as failure. On `recovered`, run status
again; recovery may have restored the previous task before activation, so the
requested release is not necessarily installed. Reconnect after a successful
update and verify the pinned version before enabling search.

## Explicit remote discovery

Local status without `--version` checks RPC compatibility only. It never checks
GitHub and does not compare application versions. `--version` requests exact
application-version verification; a mismatch is not an IPC incompatibility.
The SDK handshake reports only application version, IPC version, index format,
and capabilities. It has no remote availability field.

Hosts can explicitly discover compatible releases without installing them:

```text
scry check-update --json --compatible-ipc 1 --compatible-index 10 --channel stable --current-version 0.1.0-alpha.4
```

`--compatible-ipc` and `--compatible-index` are required positive u16 values.
The channel defaults to `stable`; `prerelease` includes both stable releases and
prereleases. Current version defaults to the invoking CLI's application version
and accepts an optional leading `v`. Selection uses semantic version order,
not the API's latest-release pointer or lexical tag order.

Discovery inspects at most 20 recent published releases, at most 2 MiB of release
metadata, and at most 4 KiB per standalone manifest, within a 60-second network
budget. It verifies manifest bytes against GitHub's SHA-256 asset digest and
requires both the manifest and Windows package assets to advertise digests.
Releases missing these assets or using an unknown manifest schema are not
selected. Invalid manifests and digest mismatches fail closed. The newest
compatible release in this bounded window wins, even if a newer release is
incompatible. No package ZIP is downloaded and no installation is changed.

One object is written to stdout:

```json
{"schema":1,"event":"check_update","state":"available","target_version":"v0.2.0","message":"Compatible release v0.2.0 is available","exit_code":0,"detail":{"schema":1,"version":"0.2.0","ipc_version":1,"index_format":10},"inspected_manifests":2}
```

| Discovery state | Meaning | Exit code |
| --- | --- | --- |
| `available` | A newer compatible release was verified | 0 |
| `current` | No newer compatible release in the discovery window | 0 |
| `no_compatible_release` | No eligible verified release in that window | 12 |
| `failed` | Network, validation, or input failure | 21, 22, or 1 |

`target_version` is the selected exact `v` tag for `available` and `current`,
or null otherwise. Selected compatibility fields are `detail.ipc_version` and
`detail.index_format`; `detail.version` has no leading `v`. A `current` result
can select an older release when the installed version is newer than the latest
compatible candidate. It does not request a downgrade. A host may cache results,
choose its own cadence and channel, and show optional updates while continuing
to search with a compatible daemon. These are host policies. Discovery runs
only on explicit invocation and never in the daemon or local status command.

All three JSON contracts use schema 1. Additive fields may appear; consumers
should ignore fields they do not understand, reject unsupported schema versions,
and handle unknown enum values as an explicit error. Existing field meanings
and exit codes are stable within this schema. Display messages can change and
must never be parsed. A future breaking contract requires another schema.

## Installation and recovery

Installed programs live under `%ProgramFiles%\Scry Search\<user SID>`. Updates
use separate release directories so the invoking CLI and existing mapped
binaries can remain open. Administrators and SYSTEM own and write the program
tree. Ordinary users have read and execute access. The per-user elevated task
allows its user to read and run it, not change its action. Index data remains
under `%LOCALAPPDATA%\scry`.

The updater preserves the task XML, including triggers, settings, principal,
and supported daemon arguments. It changes the executable action and applies
the task ACL. Custom actions, working directories, other users' tasks, and
benchmark arguments are unsupported. A missing task creates a normal per-user
installation. A stopped daemon is installed or updated and started.

A running daemon from this implementation accepts an administrator-only stop
event and persists its state before exiting. Pre-updater daemons do not expose
that mechanism. Stop those installations before migration; the updater refuses
to force-terminate them. Development daemons are never stopped. Readiness checks
both the requested version and the pipe server's executable path.

The protected transaction journal records preparation, activation, and commit.
Normal activation and recovery both complete the PATH entry and verified
uninstaller before recording commit.
Rerun the same update command after interruption. Recovery resumes the installed
replacement or restarts the previous task if activation had not occurred.
After replacement startup, failures retain the replacement and journal for
diagnosis and retry. Old release directories remain available; automatic garbage
collection is not included. A host crash can leave a protected staging directory
behind. Normal returns remove the current staging directory.

Only IPC 1 and index format 10 release manifests are accepted. Existing v10
snapshots remain in place. Other formats are refused before stopping the daemon.
Scry normally rebuilds unsupported snapshots rather than migrating them in place.
A future format-changing updater must define a separate rebuild and recovery
policy. Restoring old binaries cannot undo incompatible index writes. This
implementation does not promise a binary rollback after replacement startup.

## Package trust

The helper downloads metadata from the fixed `Ariaryy/scry-search` GitHub release
API over TLS and checks the ZIP against GitHub's SHA-256 asset digest. The
repository release publisher and GitHub are the trust authorities; this is not
an independent offline signing scheme. Missing digests fail closed. Downloads
are streamed and capped at 128 MiB; metadata is capped at 1 MiB. ZIP entry count,
expanded size, paths, duplicate names, and links are checked before extracting
the two executables and manifest into administrator-owned staging. Package
scripts are never executed.

`scry-release.json` has `schema`, `version` without `v`, `ipc_version`, and
`index_format`. The release workflow emits this manifest. The same manifest is published as a standalone release asset for discovery. The
helper checks the verified daemon's `--version` output before changing the installed task.

The parent authenticates the helper's pipe connection using its Windows process
ID. The pipe carries bounded output events only. The helper receives no commands
or paths over IPC, downloads its own verified bytes, and rejects elevation under
a different account. Concurrent updates serialize through an exclusive protected
lock file.
