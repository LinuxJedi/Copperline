// SPDX-License-Identifier: GPL-3.0-or-later

//! Commodore A2091/A590 SCSI controller: a Zorro II autoconfig board
//! carrying the Commodore DMAC and a WD33C93A SBIC, plus the autoboot ROM
//! whose scsi.device drives them.
//!
//! Board layout (offsets within the configured 64K window, per the
//! A2091 schematics and the register map exercised by the 6.x boot ROMs):
//!
//! - `$0040` ISTR (read-only interrupt status), `$0042` CNTR (control)
//! - `$0080/$0082` WTC word-transfer count, `$0084/$0086` ACR DMA address
//! - `$008E` DAWR, `$0090/$0091` SASR / auxiliary status,
//!   `$0092/$0093` WD33C93 data port
//! - `$00E0` ST_DMA, `$00E2` SP_DMA, `$00E4` CINT, `$00E8` FLUSH strobes
//!   (triggered by reads as well as writes)
//! - `$2000+` the boot ROM, repeating to the end of the window
//!
//! The autoconfig identity (Commodore West Chester / product 3, with the
//! er_InitDiagVec autoboot vector at $2000) comes from the DMAC, not the
//! ROM; the ROM image is required because it carries the DiagArea and the
//! scsi.device driver itself.
//!
//! DMA moves data between the WD33C93 and Amiga memory within the access
//! that completes the handshake (no chip-bus stealing is modeled).
//! TODO: arbitrate DMAC bus-master cycles against the CPU on the expansion
//! bus for cycle-accurate transfer timing.

use crate::memory::{Memory, SLOW_RAM_BASE};
use crate::scsi::{DmaDir, ScsiDisk, Wd33c93};
use anyhow::{bail, Result};
use std::path::Path;

/// The boot ROM appears from this offset to the end of the window.
const ROM_OFFSET: u32 = 0x2000;

// ISTR bits.
const ISTR_INT_F: u8 = 0x80;
const ISTR_INTS: u8 = 0x40;
const ISTR_E_INT: u8 = 0x20;
const ISTR_INT_P: u8 = 0x10;
const ISTR_FE_FLG: u8 = 0x01;

// CNTR bits.
const CNTR_TCEN: u8 = 0x80;
const CNTR_INTEN: u8 = 0x10;

/// The DMAC masters the 24-bit Zorro II address space, word-aligned.
const DMA_ADDR_MASK: u32 = 0x00FF_FFFE;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct A2091 {
    rom: Vec<u8>,
    pub wd: Wd33c93,
    cntr: u8,
    /// End-of-process interrupt latch (DMAC-01 terminal count). The
    /// DMAC-02 modeled here never raises it; CINT/SP_DMA still clear it.
    e_int: bool,
    /// WTC/ACR are live registers the driver reads back mid-transfer.
    wtc: u32,
    acr: u32,
    dawr: u8,
    dma_active: bool,
    dma_warned: bool,
    activity: bool,
}

impl A2091 {
    /// Build the board from its boot ROM image (16K/32K/64K; pass the
    /// interleaved image, or use [`A2091::load_rom`] for split even/odd
    /// dumps).
    pub fn new(rom: Vec<u8>) -> Result<Self> {
        if !matches!(rom.len(), 0x4000 | 0x8000 | 0x1_0000) {
            bail!(
                "A2091 ROM is {} bytes; expected 16K, 32K, or 64K (a merged \
                 even/odd image, or give the [scsi] rom_odd half separately)",
                rom.len()
            );
        }
        Ok(Self {
            rom,
            wd: Wd33c93::new(),
            cntr: 0,
            e_int: false,
            wtc: 0,
            acr: 0,
            dawr: 0,
            dma_active: false,
            dma_warned: false,
            activity: false,
        })
    }

    /// Load a boot ROM image: a single merged file, or separate even/odd
    /// EPROM dumps interleaved (even byte first, matching U13/U12).
    pub fn load_rom(rom: &Path, rom_odd: Option<&Path>) -> Result<Vec<u8>> {
        let even = std::fs::read(rom)
            .map_err(|e| anyhow::anyhow!("reading A2091 ROM {}: {e}", rom.display()))?;
        let Some(odd_path) = rom_odd else {
            return Ok(even);
        };
        let odd = std::fs::read(odd_path)
            .map_err(|e| anyhow::anyhow!("reading A2091 odd ROM {}: {e}", odd_path.display()))?;
        if even.len() != odd.len() {
            bail!(
                "A2091 even/odd ROM halves differ in size ({} vs {} bytes)",
                even.len(),
                odd.len()
            );
        }
        let mut merged = Vec::with_capacity(even.len() * 2);
        for (e, o) in even.iter().zip(odd.iter()) {
            merged.push(*e);
            merged.push(*o);
        }
        Ok(merged)
    }

    pub fn attach_drive(&mut self, unit: usize, disk: ScsiDisk) {
        self.wd.attach_target(unit, disk);
    }

    /// System reset: clear the DMAC and SBIC but keep the mounted drives.
    pub fn reset(&mut self) {
        self.wd.reset();
        self.cntr = 0;
        self.e_int = false;
        self.wtc = 0;
        self.acr = 0;
        self.dawr = 0;
        self.dma_active = false;
    }

    /// Drain the activity latch for the HDD LED.
    pub fn take_activity(&mut self) -> bool {
        std::mem::take(&mut self.activity) | self.wd.take_activity()
    }

    /// The INT2 line into Paula (PORTS): SCSI or end-of-process interrupt,
    /// gated by the CNTR interrupt enable.
    pub fn int2_line(&self) -> bool {
        self.cntr & CNTR_INTEN != 0 && (self.wd.int_asserted() || self.e_int)
    }

    fn istr(&self) -> u8 {
        // The FIFO is always drained in this synchronous model.
        let mut v = ISTR_FE_FLG;
        if self.wd.int_asserted() {
            v |= ISTR_INTS | ISTR_INT_F;
        }
        if self.e_int {
            v |= ISTR_E_INT;
        }
        if self.int2_line() {
            v |= ISTR_INT_P;
        }
        v
    }

    /// Advance emulated time: deliver delayed WD33C93 interrupts and pump
    /// any DMA handshake that became ready.
    pub fn tick(&mut self, cck: u32, mem: &mut Memory) {
        self.wd.tick(cck);
        self.pump_dma(mem);
    }

    // ----- memory-mapped access ------------------------------------------

    /// Read at `off` within the configured board window.
    pub fn read(&mut self, off: u32, size: usize, mem: &mut Memory) -> u32 {
        let value = match size {
            4 => {
                let hi = self.read(off, 2, mem);
                let lo = self.read(off.wrapping_add(2), 2, mem);
                return (hi << 16) | lo;
            }
            2 => u32::from(self.read_unit(off & !1, mem)),
            _ => {
                let word = self.read_unit(off & !1, mem);
                u32::from(if off & 1 == 0 {
                    (word >> 8) as u8
                } else {
                    word as u8
                })
            }
        };
        if off < ROM_OFFSET && crate::envcfg::flag("COPPERLINE_DIAG_A2091") {
            log::info!("a2091 rd {off:#06X}/{size} -> {value:#06X}");
        }
        value
    }

    pub fn write(&mut self, off: u32, size: usize, value: u32, mem: &mut Memory) {
        if off < ROM_OFFSET && crate::envcfg::flag("COPPERLINE_DIAG_A2091") {
            log::info!("a2091 wr {off:#06X}/{size} <- {value:#06X}");
        }
        match size {
            4 => {
                self.write(off, 2, value >> 16, mem);
                self.write(off.wrapping_add(2), 2, value & 0xFFFF, mem);
            }
            2 => self.write_unit(off & !1, value as u16, mem),
            _ => {
                // The registers sit on the low data byte; a byte write to
                // either half carries the value through.
                self.write_unit(off & !1, value as u16 & 0xFF, mem);
            }
        }
    }

    fn read_unit(&mut self, off: u32, mem: &mut Memory) -> u16 {
        if off >= ROM_OFFSET {
            let mask = self.rom.len() - 1;
            let hi = self.rom[off as usize & mask];
            let lo = self.rom[(off as usize + 1) & mask];
            return (u16::from(hi) << 8) | u16::from(lo);
        }
        let v = match off {
            0x40 => u16::from(self.istr()),
            0x42 => u16::from(self.cntr),
            0x80 => (self.wtc >> 16) as u16,
            0x82 => self.wtc as u16,
            0x84 => (self.acr >> 16) as u16,
            0x86 => self.acr as u16,
            0x8E => u16::from(self.dawr),
            0x90 => u16::from(self.wd.read_aux_status()),
            0x92 => u16::from(self.wd.read_data_port()),
            // The strobes fire on reads too.
            0xE0 => {
                self.st_dma(mem);
                0
            }
            0xE2 => {
                self.sp_dma();
                0
            }
            0xE4 => {
                self.cint();
                0
            }
            0xE8 => {
                self.flush();
                0
            }
            // Unpopulated decode (notably the A590's XT drive interface at
            // $A0-$A7, absent on the A2091): the data bus floats high. The
            // boot ROM's drive probe depends on this: it ANDs the four XT
            // status bytes and only skips the XT path when they read $FF.
            _ => 0xFFFF,
        };
        self.pump_dma(mem);
        v
    }

    fn write_unit(&mut self, off: u32, value: u16, mem: &mut Memory) {
        if off >= ROM_OFFSET {
            return;
        }
        match off {
            0x42 => self.cntr = value as u8,
            0x80 => self.wtc = (self.wtc & 0x0000_FFFF) | (u32::from(value) << 16),
            0x82 => self.wtc = (self.wtc & 0xFFFF_0000) | u32::from(value),
            0x84 => self.acr = (self.acr & 0x0000_FFFF) | (u32::from(value) << 16),
            // The DMAC transfers words: the address counter's low bit is
            // not writable.
            0x86 => self.acr = (self.acr & 0xFFFF_0000) | u32::from(value & 0xFFFE),
            0x8E => self.dawr = value as u8,
            0x90 => self.wd.write_sasr(value as u8),
            0x92 => self.wd.write_data_port(value as u8),
            0xE0 => self.st_dma(mem),
            0xE2 => self.sp_dma(),
            0xE4 => self.cint(),
            0xE8 => self.flush(),
            _ => {}
        }
        self.pump_dma(mem);
    }

    // ----- strobes ----------------------------------------------------------

    fn st_dma(&mut self, mem: &mut Memory) {
        self.dma_active = true;
        self.pump_dma(mem);
    }

    fn sp_dma(&mut self) {
        self.dma_active = false;
        self.e_int = false;
    }

    fn cint(&mut self) {
        self.e_int = false;
    }

    fn flush(&mut self) {
        // The FIFO is always drained; FE_FLG is permanently set in ISTR.
    }

    // ----- DMA engine ---------------------------------------------------------

    /// Move every byte the WD33C93 currently offers/wants between its data
    /// path and Amiga memory, a word per DMAC cycle, while DMA is started.
    fn pump_dma(&mut self, mem: &mut Memory) {
        if !self.dma_active {
            return;
        }
        while let Some(dir) = self.wd.dma_request() {
            if self.wd.dma_remaining() == 0 {
                break;
            }
            self.activity = true;
            let addr = self.acr & DMA_ADDR_MASK;
            match dir {
                DmaDir::In => {
                    let b0 = self.wd.dma_in_byte();
                    let b1 = if self.wd.dma_request().is_some() && self.wd.dma_remaining() > 0 {
                        self.wd.dma_in_byte()
                    } else {
                        // Odd byte counts still write a full word; the pad
                        // byte is whatever the FIFO carries (zero here).
                        0
                    };
                    self.dma_write_word(mem, addr, (u16::from(b0) << 8) | u16::from(b1));
                }
                DmaDir::Out => {
                    let w = self.dma_read_word(mem, addr);
                    self.wd.dma_out_byte((w >> 8) as u8);
                    if self.wd.dma_request().is_some() && self.wd.dma_remaining() > 0 {
                        self.wd.dma_out_byte(w as u8);
                    }
                }
            }
            self.acr = (self.acr.wrapping_add(2)) & 0x00FF_FFFF;
            // The word counter only runs with CNTR TCEN set (and only the
            // DMAC-01 raised E_INT at terminal count; this is a DMAC-02).
            if self.cntr & CNTR_TCEN != 0 && self.wtc > 0 {
                self.wtc -= 1;
            }
        }
    }

    fn dma_read_word(&mut self, mem: &Memory, addr: u32) -> u16 {
        let a = addr as usize;
        if a + 1 < mem.chip_ram.len() {
            return (u16::from(mem.chip_ram[a]) << 8) | u16::from(mem.chip_ram[a + 1]);
        }
        let slow = SLOW_RAM_BASE as usize;
        if a >= slow && a + 1 < slow + mem.slow_ram.len() {
            let o = a - slow;
            return (u16::from(mem.slow_ram[o]) << 8) | u16::from(mem.slow_ram[o + 1]);
        }
        if let Some((board, off)) = mem.zorro.region_at(addr, 2) {
            let ram = mem.zorro.board_ram(board);
            return (u16::from(ram[off]) << 8) | u16::from(ram[off + 1]);
        }
        self.warn_dma_target(addr);
        0xFFFF
    }

    fn dma_write_word(&mut self, mem: &mut Memory, addr: u32, w: u16) {
        let a = addr as usize;
        if a + 1 < mem.chip_ram.len() {
            mem.chip_ram[a] = (w >> 8) as u8;
            mem.chip_ram[a + 1] = w as u8;
            return;
        }
        let slow = SLOW_RAM_BASE as usize;
        if a >= slow && a + 1 < slow + mem.slow_ram.len() {
            let o = a - slow;
            mem.slow_ram[o] = (w >> 8) as u8;
            mem.slow_ram[o + 1] = w as u8;
            return;
        }
        if let Some((board, off)) = mem.zorro.region_at(addr, 2) {
            let ram = mem.zorro.board_ram_mut(board);
            ram[off] = (w >> 8) as u8;
            ram[off + 1] = w as u8;
            return;
        }
        self.warn_dma_target(addr);
    }

    fn warn_dma_target(&mut self, addr: u32) {
        if !self.dma_warned {
            self.dma_warned = true;
            log::warn!("a2091: DMA to unmapped address {addr:#08X}; ignoring");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harddrive::SECTOR_SIZE;
    use crate::scsi::{
        ASR_INT, CSR_DISC, CSR_SEL_XFER_DONE, GOOD, WD_COMMAND, WD_COMMAND_PHASE, WD_CONTROL,
        WD_DESTINATION_ID, WD_SCSI_STATUS, WD_TARGET_LUN, WD_TC_LSB, WD_TC_MID, WD_TC_MSB,
    };
    use crate::zorro::{BoardBacking, BoardSpec, ZorroChain, AUTOCONFIG_BASE};
    use std::path::PathBuf;

    // Board register offsets (within the configured 64K window).
    const ISTR: u32 = 0x40;
    const CNTR: u32 = 0x42;
    const ACR_HI: u32 = 0x84;
    const ACR_LO: u32 = 0x86;
    const SASR: u32 = 0x91;
    const SCMD: u32 = 0x93;
    const ST_DMA: u32 = 0xE0;
    const SP_DMA: u32 = 0xE2;

    fn fake_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x4000];
        for (i, b) in rom.iter_mut().enumerate() {
            *b = (i % 253) as u8;
        }
        rom
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        (nanos << 16) | NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_image(sectors: u64) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "copperline-a2091-test-{}-{}.hdf",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::write(&path, vec![0u8; (sectors * SECTOR_SIZE as u64) as usize]).unwrap();
        path
    }

    fn test_memory() -> Memory {
        Memory {
            chip_ram: vec![0u8; 512 * 1024],
            slow_ram: Vec::new(),
            rom: Vec::new(),
            overlay: false,
            zorro: ZorroChain::default(),
            extended_rom: Vec::new(),
            extended_rom_base: 0,
            wcs: Vec::new(),
            wcs_write_protected: false,
        }
    }

    fn board_with_disk(sectors: u64) -> (A2091, PathBuf) {
        let mut board = A2091::new(fake_rom()).unwrap();
        let path = temp_image(sectors);
        board.attach_drive(0, crate::scsi::ScsiDisk::open(&path, 0).unwrap());
        (board, path)
    }

    fn wr_wd(board: &mut A2091, mem: &mut Memory, reg: u8, val: u8) {
        board.write(SASR, 1, u32::from(reg), mem);
        board.write(SCMD, 1, u32::from(val), mem);
    }

    fn rd_wd(board: &mut A2091, mem: &mut Memory, reg: u8) -> u8 {
        board.write(SASR, 1, u32::from(reg), mem);
        board.read(SCMD, 1, mem) as u8
    }

    fn run_until_int2(board: &mut A2091, mem: &mut Memory) {
        for _ in 0..10_000 {
            board.tick(16, mem);
            if board.int2_line() {
                return;
            }
        }
        panic!("no INT2");
    }

    #[test]
    fn autoconfig_identity_carries_diag_vector_and_configures_a_device_window() {
        let mut chain = ZorroChain::default();
        chain.add_board(BoardSpec::a2091()).unwrap();
        // er_Type = Zorro II | DIAGVALID | 64K size code = 0xD1, presented
        // uninverted on the physical nibbles.
        assert_eq!(chain.config_read(AUTOCONFIG_BASE, 1), 0xD0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 2, 1), 0x10);
        // er_Product 3, inverted: 0xFC.
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 4, 1), 0xF0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 6, 1), 0xC0);
        // er_Manufacturer 514 = 0x0202, inverted: FD FD.
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 0x10, 1), 0xF0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 0x12, 1), 0xD0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 0x14, 1), 0xF0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 0x16, 1), 0xD0);
        // er_InitDiagVec 0x2000, inverted: DF FF.
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 0x28, 1), 0xD0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 0x2A, 1), 0xF0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 0x2C, 1), 0xF0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 0x2E, 1), 0xF0);
        // The autoconfig ROM code assigns a Zorro II base: the window must land in the
        // device regions, not the RAM regions.
        chain.config_write(AUTOCONFIG_BASE + 0x48, 2, 0xE900);
        assert_eq!(chain.region_at(0x00E9_0000, 2), None);
        assert_eq!(
            chain.device_region_at(0x00E9_0040, 2),
            Some((BoardBacking::A2091, 0x40))
        );
        assert_eq!(chain.device_region_at(0x00EA_0000, 2), None);
    }

    #[test]
    fn boot_rom_appears_from_offset_2000_and_mirrors() {
        let (mut board, path) = board_with_disk(64);
        let mut mem = test_memory();
        let rom = fake_rom();
        // Word at $2000 is rom[$2000..]; the 16K image mirrors at $6000.
        let w = board.read(0x2000, 2, &mut mem);
        assert_eq!(w, (u32::from(rom[0x2000]) << 8) | u32::from(rom[0x2001]));
        assert_eq!(board.read(0x6000, 2, &mut mem), w);
        // Reads above the image's end wrap to its start.
        assert_eq!(board.read(0x4000, 1, &mut mem) as u8, rom[0]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn acr_round_trips_with_word_aligned_low_bit() {
        let (mut board, path) = board_with_disk(64);
        let mut mem = test_memory();
        board.write(ACR_HI, 2, 0x00FE, &mut mem);
        board.write(ACR_LO, 2, 0x1235, &mut mem);
        assert_eq!(board.read(ACR_HI, 2, &mut mem), 0x00FE);
        assert_eq!(board.read(ACR_LO, 2, &mut mem), 0x1234);
        std::fs::remove_file(&path).ok();
    }

    /// The full boot-ROM driver sequence for a disk read: CDB + transfer
    /// count into the WD33C93, DMA address into the DMAC, ST_DMA, then
    /// Select-and-Transfer; the sector lands in chip RAM and INT2 follows.
    #[test]
    fn dma_read10_lands_sector_in_chip_ram_and_raises_int2() {
        let (board, path) = board_with_disk(64);
        // Pattern in LBA 7 of the image.
        let mut img = std::fs::read(&path).unwrap();
        for i in 0..SECTOR_SIZE {
            img[7 * SECTOR_SIZE + i] = (i % 247) as u8;
        }
        std::fs::write(&path, &img).unwrap();
        let (mut board2, _) = (board, ());
        // Reopen the drive so the new image content is seen.
        board2.attach_drive(0, crate::scsi::ScsiDisk::open(&path, 0).unwrap());
        let mut board = board2;
        let mut mem = test_memory();

        board.write(CNTR, 2, 0x10, &mut mem); // INTEN
        wr_wd(&mut board, &mut mem, WD_CONTROL, 0x80); // DMA mode
        wr_wd(&mut board, &mut mem, WD_DESTINATION_ID, 0);
        wr_wd(&mut board, &mut mem, WD_TARGET_LUN, 0);
        // READ(10), LBA 7, one sector; the CDB registers auto-increment.
        board.write(SASR, 1, 0x03, &mut mem);
        for b in [0x28u8, 0, 0, 0, 0, 7, 0, 0, 1, 0] {
            board.write(SCMD, 1, u32::from(b), &mut mem);
        }
        wr_wd(&mut board, &mut mem, WD_TC_MSB, 0);
        wr_wd(&mut board, &mut mem, WD_TC_MID, (SECTOR_SIZE >> 8) as u8);
        wr_wd(&mut board, &mut mem, WD_TC_LSB, 0);
        board.write(ACR_HI, 2, 0x0000, &mut mem);
        board.write(ACR_LO, 2, 0x4000, &mut mem);
        board.write(ST_DMA, 2, 0, &mut mem);
        wr_wd(&mut board, &mut mem, WD_COMMAND, 0x08);

        run_until_int2(&mut board, &mut mem);
        // ISTR shows the SCSI interrupt, INT_P, and the drained FIFO.
        let istr = board.read(ISTR, 2, &mut mem) as u8;
        assert_eq!(istr & 0x71, 0x51);
        assert_eq!(
            rd_wd(&mut board, &mut mem, WD_SCSI_STATUS),
            CSR_SEL_XFER_DONE
        );
        assert_eq!(rd_wd(&mut board, &mut mem, WD_TARGET_LUN), GOOD);
        assert_eq!(rd_wd(&mut board, &mut mem, WD_COMMAND_PHASE), 0x60);
        for i in 0..SECTOR_SIZE {
            assert_eq!(mem.chip_ram[0x4000 + i], (i % 247) as u8, "byte {i}");
        }
        // ACR advanced past the buffer; the disconnect interrupt follows.
        assert_eq!(board.read(ACR_LO, 2, &mut mem), 0x4200);
        run_until_int2(&mut board, &mut mem);
        assert_eq!(rd_wd(&mut board, &mut mem, WD_SCSI_STATUS), CSR_DISC);
        board.write(SP_DMA, 2, 0, &mut mem);
        assert!(board.take_activity());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn dma_write10_commits_chip_ram_to_the_image() {
        let (mut board, path) = board_with_disk(64);
        let mut mem = test_memory();
        for i in 0..SECTOR_SIZE {
            mem.chip_ram[0x8000 + i] = (i % 239) as u8;
        }
        board.write(CNTR, 2, 0x10, &mut mem);
        wr_wd(&mut board, &mut mem, WD_CONTROL, 0x80);
        wr_wd(&mut board, &mut mem, WD_DESTINATION_ID, 0);
        wr_wd(&mut board, &mut mem, WD_TARGET_LUN, 0);
        board.write(SASR, 1, 0x03, &mut mem);
        for b in [0x2Au8, 0, 0, 0, 0, 3, 0, 0, 1, 0] {
            board.write(SCMD, 1, u32::from(b), &mut mem);
        }
        wr_wd(&mut board, &mut mem, WD_TC_MSB, 0);
        wr_wd(&mut board, &mut mem, WD_TC_MID, (SECTOR_SIZE >> 8) as u8);
        wr_wd(&mut board, &mut mem, WD_TC_LSB, 0);
        board.write(ACR_HI, 2, 0x0000, &mut mem);
        board.write(ACR_LO, 2, 0x8000, &mut mem);
        board.write(ST_DMA, 2, 0, &mut mem);
        wr_wd(&mut board, &mut mem, WD_COMMAND, 0x08);
        run_until_int2(&mut board, &mut mem);
        assert_eq!(
            rd_wd(&mut board, &mut mem, WD_SCSI_STATUS),
            CSR_SEL_XFER_DONE
        );
        assert_eq!(rd_wd(&mut board, &mut mem, WD_TARGET_LUN), GOOD);
        let on_disk = std::fs::read(&path).unwrap();
        for i in 0..SECTOR_SIZE {
            assert_eq!(on_disk[3 * SECTOR_SIZE + i], (i % 239) as u8, "byte {i}");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn int2_is_gated_by_cntr_inten() {
        let (mut board, path) = board_with_disk(64);
        let mut mem = test_memory();
        // CNTR INTEN clear: a completed WD command must not reach INT2.
        wr_wd(&mut board, &mut mem, WD_COMMAND, 0x00); // reset
        for _ in 0..1_000 {
            board.tick(16, &mut mem);
        }
        assert!(board.wd.int_asserted());
        assert!(!board.int2_line());
        let istr = board.read(ISTR, 2, &mut mem) as u8;
        assert_eq!(istr & 0x50, 0x40); // INTS without INT_P
        board.write(CNTR, 2, 0x10, &mut mem);
        assert!(board.int2_line());
        std::fs::remove_file(&path).ok();
    }

    /// The boot ROM's drive probe ANDs the four XT-interface status bytes
    /// at $A1/$A3/$A5/$A7 and only takes the SCSI-only path when they all
    /// read $FF (floating bus; the XT interface is unpopulated on the
    /// A2091). Returning zeros wedges the ROM polling a phantom XT drive.
    #[test]
    fn unpopulated_xt_interface_reads_floating_bus() {
        let (mut board, path) = board_with_disk(64);
        let mut mem = test_memory();
        for off in [0xA1u32, 0xA3, 0xA5, 0xA7] {
            assert_eq!(board.read(off, 1, &mut mem), 0xFF, "offset {off:#x}");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_odd_sized_roms() {
        assert!(A2091::new(vec![0u8; 0x3000]).is_err());
        let merged = {
            let even = vec![0xAAu8; 0x2000];
            let odd = vec![0x55u8; 0x2000];
            let mut m = Vec::new();
            for (e, o) in even.iter().zip(odd.iter()) {
                m.push(*e);
                m.push(*o);
            }
            m
        };
        assert_eq!(merged.len(), 0x4000);
        assert_eq!(merged[0], 0xAA);
        assert_eq!(merged[1], 0x55);
    }

    #[test]
    fn wd_aux_status_visible_at_0x91() {
        let (mut board, path) = board_with_disk(64);
        let mut mem = test_memory();
        wr_wd(&mut board, &mut mem, WD_COMMAND, 0x00);
        for _ in 0..1_000 {
            board.tick(16, &mut mem);
        }
        assert_eq!(board.read(0x91, 1, &mut mem) as u8 & ASR_INT, ASR_INT);
        // The word port carries the chip's byte in the low half.
        assert_eq!(board.read(0x90, 2, &mut mem) as u8 & ASR_INT, ASR_INT);
        std::fs::remove_file(&path).ok();
    }
}
