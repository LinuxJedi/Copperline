//! FPU emulation (68881/68882/68040)

mod operations;
mod packed;
mod softfloat;
mod transcendental;
mod types;

pub use types::*;
