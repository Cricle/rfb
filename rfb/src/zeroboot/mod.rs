//! ZeroBoot Firecracker provider integration.

mod provider;
mod verification;

pub use provider::*;
pub use verification::verify_firecracker_binary;
