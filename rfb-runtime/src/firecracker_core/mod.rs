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
//! `anyhow` and `serde`, carries none of Firecracker's internal crates
//! (`micro_http`, `logger`, `utils`), and speaks the documented HTTP wire
//! format directly. The review dossier with the dependency-surface analysis
//! and the sign-off checklist is
//! `requirements/RFB/0.1.0/firecracker-core-provenance.md`.

/// Firecracker process, socket, and snapshot management primitives.
pub mod firecracker;
