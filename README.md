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

`tokio-vsock`, KVM ioctls, and the Firecracker controller are Linux-only. Windows builds core/CLI (no Linux KVM/vsock); macOS builds everything that does not require a Linux backend. Real-VM acceptance needs Linux/WSL, KVM, and matching Firecracker, kernel, and rootfs assets. MSRV is Rust 1.90 (edition 2021), tracked by `Cargo.lock`.

## Quick start

```bash
cargo add rfb-sdk
cargo test -p rfb-sdk
cargo test -p rfb-runtime --no-default-features --features core
cargo build -p rfb-sdk --features cli
rfb-cli doctor --json   # host capability report
```

Protocol lines — RFB1 (framed vsock), forkd (TCP/NDJSON), and ZBRT (ZeroBoot binary frame) — are not interchangeable. Missing capabilities must fail closed.

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
