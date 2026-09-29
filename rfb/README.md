# RFB CLI

`rfb-sdk` crate (lib name `rfb`, CLI binary `rfb-cli`) — command reference. The version follows the workspace release tag.

RFB exposes one protocol line: **ZBRT VERSION=1 full protocol**. The CLI does not maintain a v1/v2 split. `rfb1` is the compatibility command name for the framed-vsock RFB1 StartTurn flow; `zeroboot verify` is the ZeroBoot full-protocol ZBRT verification flow.

## Commands

- `rfb-cli image inspect MANIFEST` reports image metadata and protocol/profile diagnostics.
- `rfb-cli image validate MANIFEST` performs manifest, path, digest, profile, and image-contract validation.
- `rfb-cli image check-kernel KERNEL` validates a kernel ELF and reports its digest.
- `rfb-cli rfb1 acceptance` runs the RFB1 StartTurn/framed-vsock acceptance against a real VM.
- `rfb-cli zeroboot verify` runs ZBRT VERSION=1 full verification (echo, true/false, concurrency, and malformed-frame rejection); `--bench` adds the 100-sample round trip benchmark.
- `rfb-cli forkd preflight` checks host/controller/snapshot readiness; `forkd acceptance` exercises the full forkd gate and always destroys its sandbox.
- `rfb-cli forkd snapshot-bind` verifies the artifact-to-controller snapshot binding. `--require-provenance` requires complete matching controller provenance; acceptance also exposes this flag and fails closed until a verified binding is available.
- `rfb-cli forkd snapshot-create` / `snapshot-info` / `snapshot-delete` are thin wrappers over the official `forkd` binary (`forkd snapshot`, `forkd snapshot-info`, `forkd rmi`). The CLI never re-implements controller internals: kernel/rootfs/tap assets are resolved from `resx/` or the environment, outputs are sanitized, and provenance stays fail-closed under `--require-provenance`.

## Runtime and exit codes

Real VM commands require Linux/WSL, KVM, matching kernel/rootfs, and **Firecracker v1.12.x** (installed from `resx/firecracker/firecracker-v1.12.1`; the version gate rejects v1.16.1 for snapshot-boot compatibility). Exit codes are stable: `0` success or optional VM check skipped, `2` usage error, `3` validation/protocol failure, `4` I/O failure, `5` external tool failure, and `12` required VM prerequisites unavailable. Use `--json` for machine-readable output.
