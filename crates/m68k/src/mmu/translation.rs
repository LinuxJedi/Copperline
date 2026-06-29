//! Address translation (PMMU table walk)

use crate::core::cpu::CpuCore;
use crate::core::memory::{AddressBus, BusFaultKind};

use super::{MmuFault, MmuFaultKind, MmuResult};

fn buserr(address: u32) -> MmuFault {
    MmuFault {
        kind: MmuFaultKind::BusError,
        address,
    }
}

fn access_fault(address: u32) -> MmuFault {
    MmuFault {
        kind: MmuFaultKind::AccessLevelViolation,
        address,
    }
}

fn config_fault(address: u32) -> MmuFault {
    MmuFault {
        kind: MmuFaultKind::ConfigurationError,
        address,
    }
}

fn read_u32_phys<B: AddressBus>(bus: &mut B, addr: u32) -> MmuResult<u32> {
    bus.try_read_long(addr).map_err(|f| {
        if matches!(f.kind, BusFaultKind::BusError) {
            buserr(f.address)
        } else {
            buserr(addr)
        }
    })
}

/// Perform 68030/68040 PMMU translation.
///
/// This implementation follows the structure of Musashi's `pmmu_translate_addr()` algorithm.
/// It currently supports:
/// - CRP/SRP selection via TC bit 25 (0x0200_0000)
/// - Root/table modes 2 (4-byte descriptors) and 3 (8-byte descriptors)
/// - Early-termination descriptors (mode 1) at table A/B/C
/// - Transparent Translation Registers (TTRs) for 68030/68040
///
/// TODO:
/// - Access permission checks and precise MMUSR (`mmu_sr`) bits
/// - Page descriptor root mode (root_limit & 3 == 1)
pub fn translate<B: AddressBus>(
    cpu: &mut CpuCore,
    bus: &mut B,
    logical: u32,
    write: bool,
    supervisor: bool,
    instruction: bool,
) -> MmuResult<u32> {
    // If MMU not enabled, identity-map.
    if !cpu.pmmu_enabled || !cpu.has_pmmu {
        return Ok(logical);
    }

    // During exception processing, bypass translation to prevent recursive faults.
    // Real hardware uses transparent translation or physical addressing for exception frames.
    if cpu.exception_processing {
        return Ok(logical);
    }

    // Check Transparent Translation Registers first - they bypass page table walk.
    if let Some(phys) = super::ttr::check_transparent_translation(cpu, logical, write, instruction)
    {
        return Ok(phys);
    }

    // The 68040 page table is a fixed three-level format unrelated to the
    // 68030's programmable walk below; dispatch to its own walker.
    if cpu.is_040() {
        return translate_040(cpu, bus, logical, write, supervisor, instruction);
    }

    // Root pointer selection: if SRP enabled and supervisor, use SRP; else CRP.
    let use_srp = (cpu.mmu_tc & 0x0200_0000) != 0 && supervisor;
    let (root_aptr, root_limit) = if use_srp {
        (cpu.mmu_srp_aptr, cpu.mmu_srp_limit)
    } else {
        (cpu.mmu_crp_aptr, cpu.mmu_crp_limit)
    };

    // Initial shift / table bits (Musashi):
    // is = tc[19:16], abits=tc[15:12], bbits=tc[11:8], cbits=tc[7:4]
    let is = (cpu.mmu_tc >> 16) & 0xF;
    let abits = (cpu.mmu_tc >> 12) & 0xF;
    let bbits = (cpu.mmu_tc >> 8) & 0xF;
    let cbits = (cpu.mmu_tc >> 4) & 0xF;

    let addr_in = logical;

    #[inline]
    fn top_index(addr: u32, left_shift: u32, bits: u32) -> u32 {
        if bits == 0 {
            return 0;
        }
        // bits is 1..=32. When bits==32, shift right by 0.
        let rshift = 32u32.saturating_sub(bits);
        addr.wrapping_shl(left_shift) >> rshift
    }

    #[inline]
    fn low_bits(addr: u32, shift: u32) -> u32 {
        if shift >= 32 {
            0
        } else {
            addr.wrapping_shl(shift) >> shift
        }
    }

    // Table A offset.
    let mut tofs = top_index(addr_in, is, abits);

    let mut tbl_entry: u32;
    let tamode: u32;

    match root_limit & 3 {
        0 => return Err(config_fault(logical)),
        1 => return Err(config_fault(logical)), // page descriptor root mode not implemented yet
        2 => {
            // 4-byte descriptors
            tofs = tofs.wrapping_mul(4);
            let e = read_u32_phys(bus, tofs.wrapping_add(root_aptr & 0xFFFF_FFFC))?;
            tbl_entry = e;
            tamode = e & 3;
        }
        3 => {
            // 8-byte descriptors: mode in high long, pointer/base in low long
            tofs = tofs.wrapping_mul(8);
            let hi = read_u32_phys(bus, tofs.wrapping_add(root_aptr & 0xFFFF_FFFC))?;
            let lo = read_u32_phys(
                bus,
                tofs.wrapping_add(root_aptr & 0xFFFF_FFFC).wrapping_add(4),
            )?;
            tamode = hi & 3;
            tbl_entry = lo;
        }
        _ => unreachable!(),
    }

    // Table B offset and pointer from A entry.
    tofs = top_index(addr_in, is + abits, bbits);
    let mut tptr = tbl_entry & 0xFFFF_FFF0;
    let tbmode: u32;

    match tamode {
        0 => return Err(access_fault(logical)),
        1 => {
            // Early termination descriptor (Musashi uses &0xffffff00).
            if write && tbl_entry & 0x4 != 0 {
                return Err(access_fault(logical)); // WP: write-protected page
            }
            let base = tbl_entry & 0xFFFF_FF00;
            let shift = is + abits;
            let addr_out = low_bits(addr_in, shift).wrapping_add(base);
            return Ok(addr_out);
        }
        2 => {
            tofs = tofs.wrapping_mul(4);
            tbl_entry = read_u32_phys(bus, tofs.wrapping_add(tptr))?;
            tbmode = tbl_entry & 3;
        }
        3 => {
            tofs = tofs.wrapping_mul(8);
            let hi = read_u32_phys(bus, tofs.wrapping_add(tptr))?;
            let lo = read_u32_phys(bus, tofs.wrapping_add(tptr).wrapping_add(4))?;
            tbmode = hi & 3;
            tbl_entry = lo;
        }
        _ => return Err(access_fault(logical)),
    }

    // Table C
    tofs = top_index(addr_in, is + abits + bbits, cbits);
    tptr = tbl_entry & 0xFFFF_FFF0;
    let tcmode: u32;

    match tbmode {
        0 => return Err(access_fault(logical)),
        1 => {
            if write && tbl_entry & 0x4 != 0 {
                return Err(access_fault(logical)); // WP: write-protected page
            }
            let base = tbl_entry & 0xFFFF_FF00;
            let shift = is + abits + bbits;
            let addr_out = low_bits(addr_in, shift).wrapping_add(base);
            return Ok(addr_out);
        }
        2 => {
            tofs = tofs.wrapping_mul(4);
            tbl_entry = read_u32_phys(bus, tofs.wrapping_add(tptr))?;
            tcmode = tbl_entry & 3;
        }
        3 => {
            tofs = tofs.wrapping_mul(8);
            let hi = read_u32_phys(bus, tofs.wrapping_add(tptr))?;
            let lo = read_u32_phys(bus, tofs.wrapping_add(tptr).wrapping_add(4))?;
            tcmode = hi & 3;
            tbl_entry = lo;
        }
        _ => return Err(access_fault(logical)),
    }

    // Final termination at table C.
    match tcmode {
        1 => {
            if write && tbl_entry & 0x4 != 0 {
                return Err(access_fault(logical)); // WP: write-protected page
            }
            let base = tbl_entry & 0xFFFF_FF00;
            let shift = is + abits + bbits + cbits;
            Ok(low_bits(addr_in, shift).wrapping_add(base))
        }
        _ => Err(access_fault(logical)),
    }
}

/// Perform 68040 PMMU translation.
///
/// The 68040 uses a fixed three-level table (root -> pointer -> page) with
/// 4-byte descriptors, indexed by logical bits [31:25] / [24:18] / [17:12]
/// (4 KB pages) or [17:13] (8 KB pages, TC bit 14 set). The root pointer is
/// URP in user mode and SRP in supervisor mode (no TC bit-25 gate -- that is
/// 68030-only). Table-level descriptors use UDT (bits [1:0]): >=2 = resident;
/// page descriptors use PDT (bits [1:0]): 0 = invalid, 2 = indirect, 1/3 =
/// resident.
///
/// A *data* access through an invalid/unconfigured descriptor raises an access
/// fault (this is how Enforcer/MuForce catch low-memory and freed-memory hits).
/// An *instruction fetch* through an invalid descriptor instead falls back to
/// identity translation: a 68040 enables TC before all of its code is mapped
/// during boot, and faulting the fetch stream there would derail it (the
/// codebase's "safe direction"). Resident pages additionally enforce the W
/// (write-protect) and S (supervisor-only) descriptor bits.
fn translate_040<B: AddressBus>(
    cpu: &mut CpuCore,
    bus: &mut B,
    logical: u32,
    write: bool,
    supervisor: bool,
    instruction: bool,
) -> MmuResult<u32> {
    // Invalid-descriptor outcome: fault on data, identity-fallback on a fetch.
    let invalid = |logical: u32| -> MmuResult<u32> {
        if instruction {
            Ok(logical)
        } else {
            Err(access_fault(logical))
        }
    };
    // Page size: TC bit 14 (P) selects 8 KB, else 4 KB.
    let page_bits = if cpu.mmu_tc & 0x0000_4000 != 0 { 13 } else { 12 };
    let page_mask = (1u32 << page_bits) - 1;

    // ATC fast path: a recent walk for this page avoids the descriptor fetches.
    // A cached entry the access would violate (write to a write-protected page,
    // user access to a supervisor page) misses here, so we re-walk and fault.
    let page_frame = logical >> page_bits;
    if let Some(phys_page) = cpu.atc.lookup(page_frame, supervisor, write) {
        return Ok(phys_page | (logical & page_mask));
    }

    // Root pointer: SRP in supervisor mode, URP (stored in mmu_crp_aptr) in user
    // mode. The 128-entry root table is 512-byte aligned.
    let root = if supervisor {
        cpu.mmu_srp_aptr
    } else {
        cpu.mmu_crp_aptr
    };

    // Level 1: root table, indexed by logical[31:25] (128 x 4 bytes).
    let root_idx = (logical >> 25) & 0x7F;
    let root_desc = read_u32_phys(bus, (root & 0xFFFF_FE00).wrapping_add(root_idx * 4))?;
    if root_desc & 3 < 2 {
        return invalid(logical); // UDT invalid: data faults, fetch falls back
    }

    // Level 2: pointer table (512-byte aligned), indexed by logical[24:18].
    let ptr_table = root_desc & 0xFFFF_FE00;
    let ptr_idx = (logical >> 18) & 0x7F;
    let ptr_desc = read_u32_phys(bus, ptr_table.wrapping_add(ptr_idx * 4))?;
    if ptr_desc & 3 < 2 {
        return invalid(logical); // UDT invalid: data faults, fetch falls back
    }

    // Level 3: page table. With 4 KB pages it has 64 entries (256-byte aligned,
    // indexed by logical[17:12]); with 8 KB pages, 32 entries (128-byte aligned,
    // indexed by logical[17:13]).
    let (page_table_mask, page_idx) = if page_bits == 13 {
        (0xFFFF_FF80u32, (logical >> 13) & 0x1F)
    } else {
        (0xFFFF_FF00u32, (logical >> 12) & 0x3F)
    };
    let page_table = ptr_desc & page_table_mask;
    let mut page_desc = read_u32_phys(bus, page_table.wrapping_add(page_idx * 4))?;

    // PDT: 0 invalid, 2 indirect (the descriptor points to the real page
    // descriptor), 1/3 resident.
    if page_desc & 3 == 2 {
        page_desc = read_u32_phys(bus, page_desc & 0xFFFF_FFFC)?;
    }
    if page_desc & 3 == 0 {
        return invalid(logical); // PDT invalid: data faults, fetch falls back
    }

    // Protection bits: W (write-protect, bit 2) accumulates across the table and
    // page descriptors; S (supervisor-only, bit 7) lives on the page descriptor.
    // A violating access faults (resumable on the 040, vector 2 / format 7).
    let write_protected = (root_desc | ptr_desc | page_desc) & 0x0000_0004 != 0;
    let supervisor_only = page_desc & 0x0000_0080 != 0;
    if (write && write_protected) || (!supervisor && supervisor_only) {
        return Err(access_fault(logical));
    }

    let phys_page = page_desc & !page_mask;
    // Cache only real resident translations -- never the identity fallbacks
    // above, so a page that is later given a valid mapping (after PFLUSH) is not
    // masked by a stale identity entry. The protection bits ride along so a
    // later violating access to the same page is caught on the ATC path too.
    cpu.atc
        .insert(page_frame, supervisor, phys_page, write_protected, supervisor_only);
    Ok(phys_page | (logical & page_mask))
}
