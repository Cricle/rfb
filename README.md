# RFB

[![Release](https://github.com/Cricle/rfb/actions/workflows/release.yml/badge.svg)](https://github.com/Cricle/rfb/actions/workflows/release.yml)
[![Real-VM E2E](https://github.com/Cricle/rfb/actions/workflows/e2e.yml/badge.svg)](https://github.com/Cricle/rfb/actions/workflows/e2e.yml)
[![Crates.io](https://img.shields.io/crates/v/rfb-sdk)](https://crates.io/crates/rfb-sdk)
[![PyPI](https://img.shields.io/pypi/v/rfb-sdk)](https://pypi.org/project/rfb-sdk/)
[![NuGet](https://img.shields.io/nuget/v/Rfb.Sdk)](https://www.nuget.org/packages/Rfb.Sdk/)
[![Maven Central](https://img.shields.io/maven-central/v/io.github.cricle/rfb-sdk)](https://central.sonatype.com/artifact/io.github.cricle/rfb-sdk)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**English** | [简体中文](README.zh.md)

RFB (Runtime/Resource/Filesystem Boundary) is a sandbox-oriented Rust workspace that defines typed contracts, transports, and runtime integrations between a host and guest VMs. It is not a chat service, a model proxy, or an application session layer.

## Install (Rust)

```toml
[dependencies]
rfb-sdk = "0.0.1"
rfb-rig = "0.0.1"
# Only needed for the guest/Firecracker runtime:
rfb-runtime = { version = "0.0.1", default-features = false, features = ["host-vsock"] }
```

CLI: `cargo install rfb-sdk --features cli` (binary: `rfb-cli`). The prebuilt **linux-x64** `rfb-cli` binary is also distributed as a binary-only package (no wrapper code):

```bash
dotnet add package Rfb.Cli   # NuGet (binary in tools/, copied to output)
npm install -g rfb-cli       # npm (bin entry points straight at the ELF)
```

### Custom images from a `build.rfb` script

One TOML file describes the image — interpreters, offline packages, prebuilt Rust apps, extra files — and `rfb-cli image build-script` drives the whole chain (static runtime build → rootfs assembly → verification):

```toml
schema = "rfb-build/v1"
mode = "zeroboot-zbrt"
output = "out/my-sandbox.ext4"
force = true

[interpreters]
python = true        # /bin/python3 (embeds RustPython)
lua = true           # /bin/lua (embeds mlua 5.4)

[packages]
py-site = "sites/py" # offline pure-Python packages -> /usr/lib/python3/site-packages
lua-lib = "sites/lua"

[rust]
apps = ["hello"]     # prebuilt musl binaries -> /usr/local/bin

[files]
"assets/banner.txt" = "/etc/motd"
```

See [docs/example-build.rfb](docs/example-build.rfb) for the annotated reference script. Interpreter features are fail-closed: a runtime built without `rustpython`/`mlua` is rejected at image build time instead of installing dead applets.

Published crates contain Rust sources and crate resources only — no Firecracker binary, Linux kernel, ext4 images, snapshots, or forkd service. Those must be provisioned explicitly by the deployment system.

## Crates

| Crate | Responsibility |
|---|---|
| `rfb-sdk` | Minimal sandbox API, capability / filesystem / guest protocol abstractions; optional forkd, ZBRT, and `rfb-cli` integration. |
| `rfb-rig` | Adapts RFB sandbox capabilities as Rig 0.42 tools; does not own an agent loop. |
| `rfb-runtime` | RFB1 guest/runtime, workspace executor, vsock, Firecracker controller, and runtime tests; also hosts library code used by build/acceptance flows. |

Dependency direction: `rfb-rig` / `rfb-runtime` → `rfb-sdk`; `rfb-sdk` never depends on Rig. The application owns agents, prompts, models, HTTP/SSE, credentials, and session storage.

## Multi-language SDKs

The same sandbox surface ships as:

| Language | Package |
|---|---|
| Python | [`rfb-sdk`](https://pypi.org/project/rfb-sdk/) (`import rfb_sdk`) |
| Java | [`io.github.cricle:rfb-sdk`](https://central.sonatype.com/artifact/io.github.cricle/rfb-sdk) (Maven Central) |
| C#/.NET | [`Rfb.Sdk`](https://www.nuget.org/packages/Rfb.Sdk/) (NuGet) |
| Node.js | [`rfb-sdk`](https://www.npmjs.com/package/rfb-sdk) (`import { RfbClient } from 'rfb-sdk'`) |

All four suites (Python / C# / Java / Node.js) are exercised end-to-end in CI on a clean Debian 12 container (see `sdk/e2e/debian-e2e.sh`).

## Features, platforms, and MSRV

By default `rfb-sdk` enables no backend. `forkd` enables the TCP/NDJSON forkd client, `zeroboot` enables ZBRT, and `cli` enables every backend plus the `rfb-cli` binary. `rfb-runtime` defaults to `core`; `guest` adds the guest/vsock side, `host-vsock` the host side, `forkd` the guest forkd agent, `firecracker` the Linux/KVM controller, and `cli` everything the CLI needs.

`tokio-vsock`, KVM ioctls, and the Firecracker controller are Linux-only. Windows builds core/CLI (no Linux KVM/vsock); macOS builds everything that does not require a Linux backend. Real-VM acceptance needs Linux/WSL, KVM, and matching Firecracker, kernel, and rootfs assets. MSRV is Rust 1.93 (edition 2021), tracked by `Cargo.lock`.

## Quick start

```bash
cargo add rfb-sdk
cargo test -p rfb-sdk
cargo test -p rfb-runtime --no-default-features --features core
cargo build -p rfb-sdk --features cli
rfb-cli doctor --json   # host capability report
```

Protocol lines — RFB1 (framed vsock), forkd (TCP/NDJSON), and ZBRT (ZeroBoot binary frame) — are not interchangeable. Missing capabilities must fail closed.

The ZeroBoot provider keeps one VM but opens a pool of ZBRT connections into it, because the V1 contract allows one active command per connection. Four environment variables size that VM for a host (defaults in parentheses): `RFB_ZBRT_VM_MEM_MIB` (128; the measured Firecracker floor is 48), `RFB_ZBRT_VM_VCPU` (1), `RFB_ZBRT_SESSIONS` (8, the number of commands that can run at once), and `RFB_ZBRT_SNAPSHOT_SHARDS` (2, only meaningful with hot start). `rfb-ben zbrt --vcpus 1,2 --mem-mib 512` measures the resulting scaling curve on a real KVM host.

Setting `RFB_ZBRT_SNAPSHOT_DIR=<dir>` (on tmpfs) turns on snapshot hot start: the provider boots its parent VM once, pauses it, and snapshots it into `<dir>/parent/`; every later sandbox restores from that snapshot instead of paying a cold boot (~1 s -> a few hundred ms). The snapshot carries an identity fingerprint (kernel, rootfs, machine shape, port), so a config change rebuilds it automatically. Children share the parent's memory file copy-on-write and its rootfs image (guest writes land on the per-VM workspace tmpfs), so neither a memory nor an image copy is paid per create. Note: the vsock relay path is baked into the snapshot by the patched Firecracker build in `resx/`, so hot-mode creates serialize per snapshot dir; `RFB_ZBRT_SNAPSHOT_SHARDS=N` (default 2) gives each shard its own parent snapshot and lock, running restores fully in parallel (measured: 100 concurrent creates, 100/100 ok — ~106 ms/create at 2 shards, ~54 ms at 4 shards; gated in CI by `tests/zeroboot_concurrency.rs`). Snapshot files are owner-only (0700/0600) — `memory.bin` contains the VM's full memory.

## Distributed / cluster sandboxes

`rfb::cluster::ClusterProvider` spans N forkd controller nodes (`forkd-controller serve` on each host — no extra server component): it schedules sandbox creation to the least-in-flight healthy node (or round-robin), fails over on transport errors, 5xx, and undecodable responses, trips a node after 3 consecutive failures, and lets it back in with one lazy half-open probe per `recovery_interval` (default 30 s; `probe()` checks on demand, immediately). 4xx propagates to the caller with no failure strike and no failover (no silent double-create); a transport/decode failure reconciles the node's sandbox list and deletes a unique orphan (a miss is preferred over a wrong delete). The returned handle releases its in-flight slot on drop (RAII, no manual `release`). `preflight()` verifies every node has the snapshot ready/bootable with matching digests; `list_all()` + `delete_on()` find and clean up orphans after an owner crash. Exec/stream/filesystem traffic goes directly to the sandbox's guest address; teardown routes to the owning node. All nodes boot the same snapshot tag. See `sdk/BACKEND_SETUP.md` §7 and `rfb/tests/cluster.rs`.

## Sandbox fork (zeroboot)

`ZeroBootSandbox::fork(self)` checkpoints a live VM (pause → full snapshot) and restores TWO fresh VMs from that checkpoint, returning `(continued original, fork)` — both inherit the full live state, because the workspace tmpfs is guest memory captured by the dump (real-VM verified: files written before the fork are visible in the fork and in the fork of the fork). One fork costs ~0.5-1 s (the patched Firecracker build has no incremental diff, unlike forkd's live-fork), the checkpoint dir is shared-owned by both siblings and removed with the last one, and the sandbox is forkable only in hot mode (`RFB_ZBRT_SNAPSHOT_DIR`). See `sdk/BACKEND_SETUP.md` §6b-4 and `rfb/tests/zeroboot_fork.rs`.

## Runtime asset boundary

Protocol code, build code, image manifest examples, and docs live inside the git/crates.io boundary. Large, platform-specific artifacts are deliberately NOT published as crate dependencies: Firecracker binaries, Linux kernels, ext4 rootfs images, snapshots, forkd agent images, runtime static binaries, logs, and build directories are external artifacts. A controlled artifact store or deployment pipeline downloads them per target platform, verifies SHA-256, and injects them via absolute paths or environment variables (for example `RFB_RUNTIME_BIN`, `FORKD_ROOTFS`).

This repository keeps `resx/firecracker/firecracker-v1.16.1-x86_64.tgz` as the only full Firecracker release asset, verified with `sha256sum -c resx/firecracker/firecracker-v1.16.1-x86_64.tgz.sha256`. The bundled LICENSE, NOTICE, and THIRD-PARTY files must accompany any redistribution. Never commit credentials, real snapshots, or user workspace content.

## CI/CD

- **CI** — every PR and non-main push: Rust checks (fmt, clippy, tests, docs with `--all-features`, boundary checks) plus full SDK builds and tests (Python / C# / Java / Node.js) in a clean Debian 12 container.
- **Real-VM E2E** — pushes to `main`: boots the real Firecracker/forkd stack on a KVM runner, creates a snapshot from the forkd-agent rootfs, and runs the full `#[ignore]`-gated real-VM test suite.
- **Release** — pushing a `vMAJOR.MINOR.PATCH` tag publishes `rfb-runtime` → `rfb-sdk` → `rfb-rig` to crates.io, `rfb-sdk` to PyPI, `io.github.cricle:rfb-sdk` to Maven Central (GPG-signed), and `Rfb.Sdk` to NuGet, and `rfb-sdk` (TypeScript) plus the `rfb-cli` binary to npm, then attaches the `rfb-cli` binary and SHA256SUMS to a GitHub Release. Publishing is idempotent: already-published artifacts are skipped on re-runs.

## Release & CI

See [docs/RELEASE.md](docs/RELEASE.md) for the full release pipeline, one-time setup and a failure quick-reference.

## License

MIT — see [LICENSE-MIT](LICENSE-MIT).
