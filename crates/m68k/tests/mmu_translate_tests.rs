//! 68040 PMMU translation tests: register plumbing, identity walk, and a
//! non-identity remap, driven through the CPU's own read path so the whole
//! translate() dispatch is exercised.

use m68k::core::cpu::CpuCore;
use m68k::core::memory::AddressBus;
use m68k::core::types::CpuType;
use m68k::mmu::{translate_address, MmuFaultKind};

/// Flat byte-addressed test memory.
struct TestBus {
    mem: Vec<u8>,
}

impl TestBus {
    fn new(size: usize) -> Self {
        Self { mem: vec![0; size] }
    }
    fn poke_long(&mut self, addr: u32, val: u32) {
        self.write_long(addr, val);
    }
}

impl AddressBus for TestBus {
    fn read_byte(&mut self, addr: u32) -> u8 {
        self.mem.get(addr as usize).copied().unwrap_or(0)
    }
    fn write_byte(&mut self, addr: u32, val: u8) {
        if let Some(m) = self.mem.get_mut(addr as usize) {
            *m = val;
        }
    }
    fn read_word(&mut self, addr: u32) -> u16 {
        ((self.read_byte(addr) as u16) << 8) | self.read_byte(addr.wrapping_add(1)) as u16
    }
    fn write_word(&mut self, addr: u32, val: u16) {
        self.write_byte(addr, (val >> 8) as u8);
        self.write_byte(addr.wrapping_add(1), val as u8);
    }
    fn read_long(&mut self, addr: u32) -> u32 {
        ((self.read_word(addr) as u32) << 16) | self.read_word(addr.wrapping_add(2)) as u32
    }
    fn write_long(&mut self, addr: u32, val: u32) {
        self.write_word(addr, (val >> 16) as u16);
        self.write_word(addr.wrapping_add(2), val as u16);
    }
}

/// Build a one-page 68040 4 KB page table (root -> pointer -> page) mapping the
/// page containing `logical` to `phys_page`. Returns the supervisor root pointer.
fn build_040_table(bus: &mut TestBus, logical: u32, phys_page: u32) -> u32 {
    // Fixed table layout in low memory (all alignments satisfied).
    const ROOT: u32 = 0x2000; // 512-byte aligned
    const PTR: u32 = 0x3000; // 512-byte aligned
    const PAGE: u32 = 0x4000; // 256-byte aligned (4 KB pages)

    let root_idx = (logical >> 25) & 0x7F;
    let ptr_idx = (logical >> 18) & 0x7F;
    let page_idx = (logical >> 12) & 0x3F;

    bus.poke_long(ROOT + root_idx * 4, PTR | 2); // UDT resident -> pointer table
    bus.poke_long(PTR + ptr_idx * 4, PAGE | 2); // UDT resident -> page table
    bus.poke_long(PAGE + page_idx * 4, (phys_page & 0xFFFF_F000) | 1); // PDT resident
    ROOT
}

/// Like `build_040_table` but sets the page descriptor's protection bits:
/// `w` = write-protected (bit 2), `s` = supervisor-only (bit 7).
fn build_040_table_prot(bus: &mut TestBus, logical: u32, phys_page: u32, w: bool, s: bool) -> u32 {
    let root = build_040_table(bus, logical, phys_page);
    let page = 0x4000; // PAGE base in build_040_table
    let page_idx = (logical >> 12) & 0x3F;
    let mut pd = (phys_page & 0xFFFF_F000) | 1;
    if w {
        pd |= 0x0000_0004;
    }
    if s {
        pd |= 0x0000_0080;
    }
    bus.poke_long(page + page_idx * 4, pd);
    root
}

fn enabled_040_cpu() -> CpuCore {
    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68040);
    cpu.set_sr(0x2700); // supervisor, interrupts masked
    assert!(cpu.has_pmmu, "the full 68040 must have a PMMU");
    cpu
}

#[test]
fn movec_040_tc_bit15_enables_and_root_pointers_round_trip() {
    let mut cpu = enabled_040_cpu();
    assert!(!cpu.pmmu_enabled);

    // TC bit 15 (E) is the 68040 enable bit (not bit 31 like the 030).
    cpu.write_control_register(0x003, 0x0000_8000);
    assert!(cpu.pmmu_enabled, "MOVEC TC[15] must enable the PMMU");
    assert_eq!(cpu.read_control_register(0x003), 0x0000_8000);

    cpu.write_control_register(0x003, 0); // disable
    assert!(!cpu.pmmu_enabled);

    // URP (0x806) and SRP (0x807) reach the canonical root-pointer fields that
    // the walker reads, and read back unchanged.
    cpu.write_control_register(0x806, 0x0030_0000);
    cpu.write_control_register(0x807, 0x0040_0000);
    assert_eq!(cpu.read_control_register(0x806), 0x0030_0000);
    assert_eq!(cpu.read_control_register(0x807), 0x0040_0000);
    assert_eq!(cpu.mmu_crp_aptr, 0x0030_0000, "URP stored in mmu_crp_aptr");
    assert_eq!(cpu.mmu_srp_aptr, 0x0040_0000, "SRP stored in mmu_srp_aptr");
}

#[test]
fn translate_040_identity_table_reads_same_address() {
    let mut bus = TestBus::new(0x10000);
    let logical = 0x0000_1000;
    let root = build_040_table(&mut bus, logical, logical); // identity
    bus.poke_long(logical, 0xCAFE_F00D);

    let mut cpu = enabled_040_cpu();
    cpu.write_control_register(0x807, root); // SRP = root (supervisor walk)
    cpu.write_control_register(0x003, 0x0000_8000); // enable, 4 KB pages

    assert_eq!(cpu.read_32(&mut bus, logical), 0xCAFE_F00D);
}

#[test]
fn translate_040_remap_redirects_to_physical_page() {
    let mut bus = TestBus::new(0x10000);
    let logical = 0x0000_1000;
    let phys = 0x0000_8000; // map logical page -> a different physical page
    let root = build_040_table(&mut bus, logical, phys);
    bus.poke_long(phys, 0x1234_5678); // value lives at the physical page
    bus.poke_long(logical, 0x0000_0000); // not at the logical page

    let mut cpu = enabled_040_cpu();
    cpu.write_control_register(0x807, root);
    cpu.write_control_register(0x003, 0x0000_8000);

    assert_eq!(
        cpu.read_32(&mut bus, logical),
        0x1234_5678,
        "translation must redirect logical->physical, not read the logical page"
    );
}

#[test]
fn translate_040_atc_caches_walk_and_honours_flush() {
    // The ATC must serve a second access to the same page from cache, hold that
    // mapping until a flush (real 68040 coherency: a descriptor edit needs a
    // PFLUSH), and pick up the new mapping after a TC write flushes it.
    let mut bus = TestBus::new(0x10000);
    let logical = 0x0000_1000;
    let page_table = 0x4000; // matches build_040_table's layout
    let page_idx = (logical >> 12) & 0x3F;
    let desc_addr = page_table + page_idx * 4;
    let root = build_040_table(&mut bus, logical, logical); // identity
    bus.poke_long(logical, 0xAAAA_0001); // value at the identity page
    bus.poke_long(0x9000, 0xBBBB_0002); // value at the would-be remap target

    let mut cpu = enabled_040_cpu();
    cpu.write_control_register(0x807, root);
    cpu.write_control_register(0x003, 0x0000_8000);

    // First access walks and caches; second is an ATC hit -- both identity.
    assert_eq!(cpu.read_32(&mut bus, logical), 0xAAAA_0001);
    assert_eq!(cpu.read_32(&mut bus, logical), 0xAAAA_0001);

    // Edit the descriptor to remap the page to 0x9000 WITHOUT flushing: the ATC
    // still serves the old (identity) mapping, as on real hardware.
    bus.poke_long(desc_addr, 0x9000 | 1);
    assert_eq!(
        cpu.read_32(&mut bus, logical),
        0xAAAA_0001,
        "stale ATC entry must persist until a flush"
    );

    // A TC write flushes the ATC; the next access re-walks and sees the remap.
    cpu.write_control_register(0x003, 0x0000_8000);
    assert_eq!(cpu.read_32(&mut bus, logical), 0xBBBB_0002);
}

#[test]
fn translate_040_enforces_write_protect_and_supervisor() {
    // Write-protected page: a read translates, a write faults.
    let mut bus = TestBus::new(0x10000);
    let logical = 0x0000_1000;
    let root = build_040_table_prot(&mut bus, logical, logical, true, false);
    let mut cpu = enabled_040_cpu();
    cpu.write_control_register(0x807, root);
    cpu.write_control_register(0x003, 0x0000_8000);

    assert!(translate_address(&mut cpu, &mut bus, logical, false, true, false).is_ok());
    let err = translate_address(&mut cpu, &mut bus, logical, true, true, false).unwrap_err();
    assert_eq!(err.kind, MmuFaultKind::AccessLevelViolation);

    // Supervisor-only page: a supervisor access translates, a user access faults.
    let mut bus = TestBus::new(0x10000);
    let root = build_040_table_prot(&mut bus, logical, logical, false, true);
    let mut cpu = enabled_040_cpu();
    cpu.write_control_register(0x806, root); // URP (user root)
    cpu.write_control_register(0x807, root); // SRP (supervisor root)
    cpu.write_control_register(0x003, 0x0000_8000);

    assert!(translate_address(&mut cpu, &mut bus, logical, false, true, false).is_ok());
    let err = translate_address(&mut cpu, &mut bus, logical, false, false, false).unwrap_err();
    assert_eq!(err.kind, MmuFaultKind::AccessLevelViolation);
}

#[test]
fn translate_030_enforces_write_protect() {
    // A 68030 single-level table whose page descriptor sets WP (bit 2): a read
    // translates (identity), a write faults.
    let mut bus = TestBus::new(0x10000);
    let table = 0x2000;
    // Early-termination page descriptor: mode 1, base 0, WP set (bit 2).
    bus.poke_long(table, 0x0000_0005);

    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68030);
    cpu.set_sr(0x2700);
    cpu.mmu_crp_aptr = table;
    cpu.mmu_crp_limit = 2; // mode 2: 4-byte descriptors
    cpu.mmu_tc = 0x8000_4000; // E (bit 31), IS=0, TIA=4 entries
    cpu.pmmu_enabled = true;

    let logical = 0x0000_1000;
    assert!(translate_address(&mut cpu, &mut bus, logical, false, true, false).is_ok());
    let err = translate_address(&mut cpu, &mut bus, logical, true, true, false).unwrap_err();
    assert_eq!(err.kind, MmuFaultKind::AccessLevelViolation);
}

#[test]
fn ptest_040_reports_resident_and_physical_in_mmusr() {
    // PTESTR (A0) probes the page that A0 points at and reports the physical
    // page + resident (R) bit in MMUSR; an invalid page reports not-resident.
    let mut bus = TestBus::new(0x2_0000);
    let resident = 0x0001_0000;
    let root = build_040_table(&mut bus, resident, resident); // identity resident
    bus.write_word(0x1000, 0xF568); // PTESTR (A0) -- fetched via identity fallback
    bus.write_word(0x1002, 0x4E71); // NOP

    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68040);
    cpu.reset(&mut bus);
    cpu.set_sr(0x2700);
    cpu.write_control_register(0x807, root);
    cpu.write_control_register(0x003, 0x0000_8000);

    let mut hle = m68k::NoOpHleHandler;

    cpu.dar[8] = resident; // A0 -> a mapped page
    cpu.pc = 0x1000;
    cpu.step_with_hle_handler(&mut bus, &mut hle);
    assert_eq!(cpu.mmu_sr, resident | 1, "resident page: R set, physical addr");

    cpu.dar[8] = 0x0005_0000; // A0 -> an unmapped page
    cpu.pc = 0x1000;
    cpu.step_with_hle_handler(&mut bus, &mut hle);
    assert_eq!(cpu.mmu_sr, 0, "invalid page: not resident");
}

#[test]
fn translate_040_write_protect_delivers_resumable_format7_frame() {
    // A write to a write-protected page must vector to BUS_ERROR (vector 2)
    // with a 68040 format-7 access-error frame, leaving the access undone and
    // RTE able to restart the faulting instruction.
    let mut bus = TestBus::new(0x8_0000);
    let target = 0x0001_0000; // write-protected resident page
    let root = build_040_table_prot(&mut bus, target, target, true, false);
    bus.poke_long(0x8, 0x0002_0000); // vector 2 handler (VBR=0, unmapped page -> identity)

    let mut cpu = enabled_040_cpu();
    cpu.write_control_register(0x807, root);
    cpu.write_control_register(0x003, 0x0000_8000);
    let ssp = 0x0000_3000u32;
    cpu.dar[15] = ssp;
    cpu.ppc = 0x0000_1234; // pretend faulting-instruction PC (restart target)
    // No real instruction ran, so make the bus-error rollback a no-op.
    cpu.sr_save = cpu.get_sr();
    cpu.dar_save = cpu.dar;

    cpu.write_32(&mut bus, target, 0xDEAD_BEEF);

    assert_eq!(cpu.pc, 0x0002_0000, "vectored to the access-fault handler");
    let sp = cpu.dar[15];
    assert_eq!(bus.read_long(sp + 2), 0x0000_1234, "stacked restart PC = PPC");
    assert_eq!(bus.read_word(sp + 6), 0x7008, "format 7, vector offset 0x08");
    assert_eq!(bus.read_long(sp + 0x14), target, "fault address");
    assert_eq!(
        bus.read_long(target),
        0,
        "the write-protected store must not have landed"
    );
}

#[test]
fn translate_040_invalid_page_faults_data_but_falls_back_for_fetch() {
    // With no valid tables, a DATA access through the invalid descriptor faults
    // (this is Enforcer's low-memory catch), while an INSTRUCTION fetch falls
    // back to identity so a 68040 enabling TC before its code is mapped does
    // not derail.
    let mut bus = TestBus::new(0x10000);
    let mut cpu = enabled_040_cpu();
    cpu.write_control_register(0x807, 0x00FF_0000); // garbage root (unmapped)
    cpu.write_control_register(0x003, 0x0000_8000);

    let data = translate_address(&mut cpu, &mut bus, 0x0000_1000, false, true, false);
    assert_eq!(data.unwrap_err().kind, MmuFaultKind::AccessLevelViolation);

    let fetch = translate_address(&mut cpu, &mut bus, 0x0000_1000, false, true, true);
    assert_eq!(fetch, Ok(0x0000_1000), "an instruction fetch falls back");
}
