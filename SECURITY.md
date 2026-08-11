# Security policy

Scry is an early alpha and currently supports the latest tagged release only.

Please do not open a public issue for a vulnerability involving privilege
boundaries, malformed filesystem data, named-pipe access, or shared-section
validation. Use GitHub's private vulnerability reporting feature. Include a
minimal reproduction, affected version, and expected impact; do not include
personal file names or live index data.

The daemon runs elevated while clients normally do not. Treat every filesystem
record, IPC frame, duplicated handle, mapped archive, and delta overlay as
untrusted input.

