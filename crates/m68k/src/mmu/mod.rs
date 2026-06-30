//! MMU emulation (68030/68040 PMMU)

pub mod atc;
mod translation;
pub mod ttr;

use crate::core::cpu::CpuCore;
use crate::core::memory::AddressBus;

pub use atc::Atc;
pub use translation::translate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmuFaultKind {
    ConfigurationError,
    IllegalOperation,
    AccessLevelViolation,
    /// A physical bus error occurred while walking tables / fetching descriptors.
    BusError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmuFault {
    pub kind: MmuFaultKind,
    pub address: u32,
}

pub type MmuResult<T> = Result<T, MmuFault>;

/// Translate a logical address using the CPU's PMMU state (68030/68040 style).
///
/// This is currently based on the (vendored) Musashi PMMU algorithm and focuses on the common
/// CRP/SRP + TC table-walk behavior. Access permission checks and detailed MMUSR bits are TODO.
///
/// The `instruction` parameter indicates whether this is an instruction fetch (true) or
/// data access (false), used for ITT/DTT selection on 68040.
pub fn translate_address<B: AddressBus>(
    cpu: &mut CpuCore,
    bus: &mut B,
    logical: u32,
    write: bool,
    supervisor: bool,
    instruction: bool,
) -> MmuResult<u32> {
    translate(cpu, bus, logical, write, supervisor, instruction)
}
