//! Strict ZBRT v1 full wire protocol primitives and host/provider contract.
//!
//! Every frame starts with `ZBRT`, version 1, kind, flags, a 128-bit request
//! identifier, and a big-endian payload length. Payload codecs reject both
//! truncation and trailing bytes. This module implements the deterministic
//! protocol contract shared by host, guest, and Firecracker integrations.
//!
//! The canonical implementation lives in `rfb-runtime`'s `zeroboot_protocol`
//! module; this module re-exports it so `rfb`'s host/provider consumers keep a
//! single wire implementation without a `rfb` <-> `rfb-runtime` cycle.
#![allow(missing_docs)]
pub use rfb_runtime::zeroboot_protocol::*;
