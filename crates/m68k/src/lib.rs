//! # m68k
//!
//! A safe Rust M68000 family CPU emulator.
//!
//! Supports: M68000, M68010, M68EC020, M68020, M68EC030, M68030, M68EC040, M68LC040, M68040, SCC68070

pub mod core;
pub mod dasm;
pub mod fpu;
pub mod mmu;

// Re-export commonly used types from core
pub use core::cpu::CpuCore;
pub use core::cpu::{
    CACR_040_DE, CACR_040_IE, CACR_CD, CACR_CED, CACR_CEI, CACR_CI, CACR_ED, CACR_EI, CACR_FD,
    CACR_FI,
};
pub use core::memory::AddressBus;
pub use core::types::{CpuType, HleHandler, NoOpHleHandler, Size, StepResult};