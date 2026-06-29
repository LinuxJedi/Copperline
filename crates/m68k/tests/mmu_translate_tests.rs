//! 68040 PMMU translation tests: register plumbing, identity walk, and a
//! non-identity remap, driven through the CPU's own read path so the whole
//! translate() dispatch is exercised.

use m68k::core::cpu::CpuCore;
use m68k::core::memory::AddressBus;
use m68k::core::types::CpuType;

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
fn translate_040_unconfigured_walk_falls_back_to_identity() {
    // PHASE1: with no valid tables, the walk falls back to identity rather than
    // faulting (a later phase delivers a resumable access fault instead).
    let mut bus = TestBus::new(0x10000);
    bus.poke_long(0x0000_1000, 0xDEAD_BEEF);

    let mut cpu = enabled_040_cpu();
    cpu.write_control_register(0x807, 0x00FF_0000); // garbage root (unmapped)
    cpu.write_control_register(0x003, 0x0000_8000);

    assert_eq!(cpu.read_32(&mut bus, 0x0000_1000), 0xDEAD_BEEF);
}
