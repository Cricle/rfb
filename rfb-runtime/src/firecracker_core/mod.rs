//! Firecracker process, socket, and snapshot management primitives.
//!
//! The submodule contains the low-level lifecycle building blocks used by
//! host-side runtime adapters.
//!
//! # Provenance
//!
//! Self-authored in this repository (2026-08, branch `develop-0.1.0-songzj3`):
//! an original Rust client for the public Firecracker API socket (the
//! documented `PUT /boot-source`, `PUT /drives/*`, `PUT /machine-config`,
//! `PUT /vsock`, `PATCH /vm`, `PUT /actions` and `PUT /snapshot/create`
//! endpoints) plus snapshot/socket file lifecycle helpers. It is **not**
//! derived from the Firecracker source tree: it depends only on `std`,
//! `thiserror` and `serde`, carries none of Firecracker's internal crates
//! (`micro_http`, `logger`, `utils`), and speaks the documented HTTP wire
//! format directly. The review dossier with the dependency-surface analysis
//! and the sign-off checklist is
//! `requirements/RFB/0.1.0/firecracker-core-provenance.md`.
//!
//! 2026-09 Firecracker dual-driver merge: the former
//! `rfb/src/zeroboot/firecracker.rs` driver (also self-authored, same
//! zero-upstream dependency posture) was folded into this module; the `rfb`
//! crate re-exports it behind its `zeroboot` feature. The controller-facing
//! `boot_config`/`boot` entry points keep zero production callers and remain
//! exercised through the tests; historical driver behavior differences are
//! engine parameters with every existing entry point passing its original
//! value.

/// Firecracker process, socket, and snapshot management primitives.
pub mod firecracker;
