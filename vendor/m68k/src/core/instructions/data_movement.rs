//! Data movement instructions.
//!
//! MOVE, MOVEA, MOVEM, LEA, PEA, EXG, LINK, UNLK

use crate::core::cpu::CpuCore;
use crate::core::ea::{AddressingMode, EaResult};
use crate::core::memory::AddressBus;
use crate::core::types::{CpuType, Size};

impl CpuCore {
    /// Execute MOVE instruction.
    ///
    /// MOVE <ea>, <ea>
    #[inline]
    pub fn exec_move<B: AddressBus>(
        &mut self,
        bus: &mut B,
        size: Size,
        src_mode: AddressingMode,
        dst_mode: AddressingMode,
    ) -> i32 {
        // Read source value
        let value = self.read_ea(bus, src_mode, size);

        // Write to destination. The 68000 sequences its final prefetch
        // differently per destination mode:
        // - predecrement: prefetch BEFORE the write (the write is last)
        // - absolute long: the address low word is consumed without its np;
        //   both remaining prefetches happen AFTER the write (Class 2)
        // - everything else: write, then the final prefetch (Class 1)
        if self.cpu_type == CpuType::M68000 {
            match dst_mode {
                AddressingMode::PreDecrement(_) => {
                    let ea = self.resolve_ea(bus, dst_mode, size);
                    // A MOVE destination predecrement does not pay the
                    // 2-clock address-computation penalty (it overlaps the
                    // final prefetch); cancel resolve_ea's charge.
                    self.pending_sync_clocks = self.pending_sync_clocks.saturating_sub(2);
                    self.top_up_prefetch(bus);
                    if let EaResult::Memory(addr) = ea {
                        match size {
                            Size::Byte => self.write_8(bus, addr, value as u8),
                            Size::Word => self.write_16(bus, addr, value as u16),
                            // Long predecrement writes descend: low word at
                            // the higher address first, then the high word.
                            Size::Long => {
                                self.write_16(
                                    bus,
                                    addr.wrapping_add(2),
                                    (value & 0xFFFF) as u16,
                                );
                                self.write_16(bus, addr, (value >> 16) as u16);
                            }
                        }
                    }
                }
                AddressingMode::AbsoluteLong => {
                    // Consume the address high word normally, the low word
                    // without its accompanying prefetch.
                    let hi = self.read_imm_16(bus) as u32;
                    self.consume_without_prefetch = true;
                    let lo = self.read_imm_16(bus) as u32;
                    self.consume_without_prefetch = false;
                    let addr = (hi << 16) | lo;
                    match size {
                        Size::Byte => self.write_8(bus, addr, value as u8),
                        Size::Word => self.write_16(bus, addr, value as u16),
                        Size::Long => self.write_32(bus, addr, value),
                    }
                    // The deferred np + final prefetch follow via the
                    // end-of-instruction top-up.
                }
                _ => {
                    self.write_ea(bus, dst_mode, size, value);
                }
            }
        } else {
            self.write_ea(bus, dst_mode, size, value);
        }

        // Set flags
        self.set_logic_flags(value, size);

        // MC68000: base 4 + source-fetch EA + destination-write EA.
        if self.cpu_type == CpuType::M68000 {
            4 + self.ea_source_cycles(src_mode, size) + self.ea_dest_cycles(dst_mode, size)
        } else {
            // 020+: the flat 4 made every MOVE cost the same regardless of
            // operand, so register moves ran a cycle slow and memory reads a
            // couple fast. Model the 020 data-return latency on a memory
            // source. Values are pre-scale (scale_cycles_for_cpu_type applies
            // 5/8) and calibrated to the cycle-exact A1200/FS-UAE reference:
            // Dn,Dm = 2; (An),Dn word read = 6; Dn,(An) write stays bus-bound.
            // (Write-posting, which the bus model lacks, is handled there.)
            let mut c = 2;
            if !src_mode.is_register_direct() {
                c += if size == Size::Long { 11 } else { 7 };
            }
            c
        }
    }

    /// Execute MOVEA instruction.
    ///
    /// MOVEA <ea>, An (no flags affected)
    pub fn exec_movea<B: AddressBus>(
        &mut self,
        bus: &mut B,
        size: Size,
        src_mode: AddressingMode,
        dst_reg: usize,
    ) -> i32 {
        let value = self.read_ea(bus, src_mode, size);

        // Sign extend word to long for MOVEA.W
        let value = if size == Size::Word {
            value as i16 as i32 as u32
        } else {
            value
        };

        self.set_a(dst_reg, value);
        // MC68000: MOVEA base 4 + source-fetch EA (destination An is free).
        if self.cpu_type == CpuType::M68000 {
            4 + self.ea_source_cycles(src_mode, size)
        } else {
            4
        }
    }

    /// Execute LEA instruction.
    ///
    /// LEA <ea>, An
    pub fn exec_lea<B: AddressBus>(
        &mut self,
        bus: &mut B,
        src_mode: AddressingMode,
        dst_reg: usize,
    ) -> i32 {
        // Get effective address (don't read from it)
        let ea = self.get_ea_address(bus, src_mode, Size::Long);
        // Indexed modes spend 2 more internal clocks after the extension
        // fetch, before the final prefetch.
        if matches!(
            src_mode,
            AddressingMode::Index(_) | AddressingMode::PcIndex
        ) {
            self.internal_cycles(2);
        }
        self.set_a(dst_reg, ea);
        4
    }

    /// Execute PEA instruction.
    ///
    /// PEA <ea>
    pub fn exec_pea<B: AddressBus>(&mut self, bus: &mut B, src_mode: AddressingMode) -> i32 {
        let ea = self.get_ea_address(bus, src_mode, Size::Long);
        // Indexed modes spend 2 more internal clocks after the extension
        // fetch, before the final prefetch.
        if matches!(
            src_mode,
            AddressingMode::Index(_) | AddressingMode::PcIndex
        ) {
            self.internal_cycles(2);
        }
        // 68000 bus order: PEA performs its final prefetch BEFORE pushing the
        // address (the pushes are the instruction's last bus accesses).
        self.top_up_prefetch(bus);
        self.push_32(bus, ea);
        12
    }

    /// Execute EXG instruction.
    ///
    /// EXG Rx, Ry
    pub fn exec_exg(&mut self, opcode: u16) -> i32 {
        let rx = ((opcode >> 9) & 7) as usize;
        let ry = (opcode & 7) as usize;
        let mode = (opcode >> 3) & 0x1F;

        match mode {
            0x08 => {
                // EXG Dx, Dy
                let tmp = self.d(rx);
                self.set_d(rx, self.d(ry));
                self.set_d(ry, tmp);
            }
            0x09 => {
                // EXG Ax, Ay
                let tmp = self.a(rx);
                self.set_a(rx, self.a(ry));
                self.set_a(ry, tmp);
            }
            0x11 => {
                // EXG Dx, Ay
                let tmp = self.d(rx);
                self.set_d(rx, self.a(ry));
                self.set_a(ry, tmp);
            }
            _ => {}
        }
        6
    }

    /// Execute LINK instruction.
    ///
    /// LINK An, #<displacement>
    pub fn exec_link<B: AddressBus>(&mut self, bus: &mut B, reg: usize) -> i32 {
        // 68000 bus order: the displacement word is consumed (with its
        // prefetch) BEFORE the An push.
        let disp = self.read_imm_16(bus) as i16 as i32;

        // Push An
        let an = self.a(reg);
        self.push_32(bus, an);

        // An = SP
        self.set_a(reg, self.dar[15]);

        // SP += displacement (16-bit)
        self.dar[15] = (self.dar[15] as i32).wrapping_add(disp) as u32;

        16
    }

    /// Execute LINK.L instruction (68020+).
    ///
    /// LINK.L An, #<displacement> (32-bit displacement)
    pub fn exec_link_long<B: AddressBus>(&mut self, bus: &mut B, reg: usize) -> i32 {
        // Push An
        let an = self.a(reg);
        self.push_32(bus, an);

        // An = SP
        self.set_a(reg, self.dar[15]);

        // SP += displacement (32-bit)
        let disp = self.read_imm_32(bus) as i32;
        self.dar[15] = (self.dar[15] as i32).wrapping_add(disp) as u32;

        16
    }

    /// Execute UNLK instruction.
    ///
    /// UNLK An
    pub fn exec_unlk<B: AddressBus>(&mut self, bus: &mut B, reg: usize) -> i32 {
        // SP = An
        self.dar[15] = self.a(reg);

        // Pop An
        let value = self.pull_32(bus);
        self.set_a(reg, value);

        12
    }

    /// Execute MOVEM instruction (register to memory).
    ///
    /// MOVEM <register list>, <ea>
    pub fn exec_movem_to_mem<B: AddressBus>(
        &mut self,
        bus: &mut B,
        size: Size,
        mode: AddressingMode,
        mask: u16,
    ) -> i32 {
        let mut count = 0;

        // For predecrement mode, bit order is reversed (A7..A0, D7..D0)
        let is_predec = matches!(mode, AddressingMode::PreDecrement(_));

        // Get starting address
        let mut addr = match &mode {
            AddressingMode::PreDecrement(reg) => self.a(*reg as usize),
            _ => self.get_ea_address(bus, mode, size),
        };

        if is_predec {
            // Write in reverse order: A7..A0, D7..D0
            for i in 0..16 {
                if mask & (1 << i) != 0 {
                    let reg_idx = 15 - i; // Reverse: bit 0 = A7, bit 15 = D0
                    let value = self.dar[reg_idx];
                    addr = addr.wrapping_sub(size.bytes());
                    match size {
                        Size::Word => self.write_16(bus, addr, value as u16),
                        Size::Long => self.write_32(bus, addr, value),
                        _ => {}
                    }
                    count += 1;
                }
            }
            // Update address register
            if let AddressingMode::PreDecrement(reg) = mode {
                self.set_a(reg as usize, addr);
            }
        } else {
            // Normal order: D0..D7, A0..A7
            for i in 0..16 {
                if mask & (1 << i) != 0 {
                    let value = self.dar[i];
                    match size {
                        Size::Word => self.write_16(bus, addr, value as u16),
                        Size::Long => self.write_32(bus, addr, value),
                        _ => {}
                    }
                    addr = addr.wrapping_add(size.bytes());
                    count += 1;
                }
            }
        }

        {
            let base = 8 + count * if size == Size::Long { 8 } else { 4 };
            if self.cpu_type == CpuType::M68000 {
                base + self.movem_ea_calc_cycles(mode)
            } else {
                base
            }
        }
    }

    /// Execute MOVEM instruction (memory to register).
    ///
    /// MOVEM <ea>, <register list>
    pub fn exec_movem_to_reg<B: AddressBus>(
        &mut self,
        bus: &mut B,
        size: Size,
        mode: AddressingMode,
        mask: u16,
    ) -> i32 {
        let mut count = 0;
        let is_predec = matches!(mode, AddressingMode::PreDecrement(_));

        // Establish starting address depending on addressing mode.
        let mut addr = match &mode {
            AddressingMode::PostIncrement(reg) => self.a(*reg as usize),
            AddressingMode::PreDecrement(reg) => self.a(*reg as usize),
            _ => self.get_ea_address(bus, mode, size),
        };

        if is_predec {
            // Predecrement source: reverse register order A7..A0, D7..D0
            for i in 0..16 {
                if mask & (1 << i) != 0 {
                    let reg_idx = 15 - i;
                    addr = addr.wrapping_sub(size.bytes());
                    let value = match size {
                        Size::Word => self.read_16(bus, addr) as i16 as i32 as u32,
                        Size::Long => self.read_32(bus, addr),
                        _ => 0,
                    };
                    self.dar[reg_idx] = value;
                    count += 1;
                }
            }
            // Update address register after all reads
            if let AddressingMode::PreDecrement(reg) = mode {
                self.set_a(reg as usize, addr);
            }
        } else {
            // Normal order: D0..D7, A0..A7
            for i in 0..16 {
                if mask & (1 << i) != 0 {
                    let value = match size {
                        Size::Word => self.read_16(bus, addr) as i16 as i32 as u32,
                        Size::Long => self.read_32(bus, addr),
                        _ => 0,
                    };
                    self.dar[i] = value;
                    addr = addr.wrapping_add(size.bytes());
                    count += 1;
                }
            }
            // Update address register for postincrement
            if let AddressingMode::PostIncrement(reg) = mode {
                self.set_a(reg as usize, addr);
            }

            // 68000 quirk: MOVEM memory-to-register always performs one extra
            // word read past the last transferred register (value discarded).
            if self.cpu_type == CpuType::M68000 {
                let _ = self.read_16(bus, addr);
            }
        }

        {
            let base = 12 + count * if size == Size::Long { 8 } else { 4 };
            if self.cpu_type == CpuType::M68000 {
                base + self.movem_ea_calc_cycles(mode)
            } else {
                base
            }
        }
    }

    /// Execute SWAP instruction.
    ///
    /// SWAP Dn
    pub fn exec_swap(&mut self, reg: usize) -> i32 {
        let value = self.d(reg);
        let result = value.rotate_right(16);
        self.set_d(reg, result);

        self.set_logic_flags(result, Size::Long);
        4
    }

    // ========== Helper Methods ==========

    /// Read value from effective address.
    #[inline]
    pub fn read_ea<B: AddressBus>(&mut self, bus: &mut B, mode: AddressingMode, size: Size) -> u32 {
        match self.resolve_ea(bus, mode, size) {
            EaResult::DataReg(reg) => self.d(reg as usize) & size.mask(),
            EaResult::AddrReg(reg) => self.a(reg as usize) & size.mask(),
            EaResult::Memory(addr) => match size {
                Size::Byte => self.read_8(bus, addr) as u32,
                Size::Word => self.read_16(bus, addr) as u32,
                Size::Long => self.read_32(bus, addr),
            },
            // Immediate data was already consumed from the instruction stream
            // by resolve_ea (prefetch queue on 68000).
            EaResult::Immediate(value) => value & size.mask(),
        }
    }

    /// Write value to effective address.
    #[inline]
    pub fn write_ea<B: AddressBus>(
        &mut self,
        bus: &mut B,
        mode: AddressingMode,
        size: Size,
        value: u32,
    ) {
        match self.resolve_ea(bus, mode, size) {
            EaResult::DataReg(reg) => {
                let reg = reg as usize;
                match size {
                    Size::Byte => {
                        self.dar[reg] = (self.dar[reg] & 0xFFFFFF00) | (value & 0xFF);
                    }
                    Size::Word => {
                        self.dar[reg] = (self.dar[reg] & 0xFFFF0000) | (value & 0xFFFF);
                    }
                    Size::Long => {
                        self.dar[reg] = value;
                    }
                }
            }
            EaResult::AddrReg(reg) => {
                // Address registers always get full 32-bit value
                self.dar[8 + reg as usize] = value;
            }
            EaResult::Memory(addr) => match size {
                Size::Byte => self.write_8(bus, addr, value as u8),
                Size::Word => self.write_16(bus, addr, value as u16),
                Size::Long => self.write_32(bus, addr, value),
            },
            EaResult::Immediate(_) => {
                // Can't write to immediate - should not happen
            }
        }
    }

    /// Read value from an already-resolved effective address.
    pub fn read_resolved_ea<B: AddressBus>(
        &mut self,
        bus: &mut B,
        ea: EaResult,
        size: Size,
    ) -> u32 {
        match ea {
            EaResult::DataReg(reg) => self.d(reg as usize) & size.mask(),
            EaResult::AddrReg(reg) => self.a(reg as usize) & size.mask(),
            EaResult::Memory(addr) => match size {
                Size::Byte => self.read_8(bus, addr) as u32,
                Size::Word => self.read_16(bus, addr) as u32,
                Size::Long => self.read_32(bus, addr),
            },
            // Immediate data was already consumed from the instruction stream
            // by resolve_ea (prefetch queue on 68000).
            EaResult::Immediate(value) => value & size.mask(),
        }
    }

    /// Write value to an already-resolved effective address.
    ///
    /// This is the read-modify-write writeback path (the destination was
    /// resolved once and usually read first). On the 68000, RMW instructions
    /// perform their final prefetch BEFORE the writeback (unlike MOVE, whose
    /// write precedes the final prefetch), so the memory arm tops the
    /// prefetch queue up first.
    pub fn write_resolved_ea<B: AddressBus>(
        &mut self,
        bus: &mut B,
        ea: EaResult,
        size: Size,
        value: u32,
    ) {
        match ea {
            EaResult::DataReg(reg) => {
                let reg = reg as usize;
                match size {
                    Size::Byte => self.dar[reg] = (self.dar[reg] & 0xFFFFFF00) | (value & 0xFF),
                    Size::Word => self.dar[reg] = (self.dar[reg] & 0xFFFF0000) | (value & 0xFFFF),
                    Size::Long => self.dar[reg] = value,
                }
            }
            EaResult::AddrReg(reg) => self.dar[8 + reg as usize] = value,
            EaResult::Memory(addr) => {
                self.top_up_prefetch(bus);
                match size {
                    Size::Byte => self.write_8(bus, addr, value as u8),
                    Size::Word => self.write_16(bus, addr, value as u16),
                    // 68000 long RMW writebacks go low word first, then high
                    // word (unlike MOVE.L destinations, which write high
                    // first).
                    Size::Long => {
                        self.write_16(bus, addr.wrapping_add(2), (value & 0xFFFF) as u16);
                        self.write_16(bus, addr, (value >> 16) as u16);
                    }
                }
            }
            EaResult::Immediate(_) => {
                // Can't write to immediate
            }
        }
    }

    /// Get effective address without reading.
    pub fn get_ea_address<B: AddressBus>(
        &mut self,
        bus: &mut B,
        mode: AddressingMode,
        size: Size,
    ) -> u32 {
        match self.resolve_ea(bus, mode, size) {
            EaResult::Memory(addr) => addr,
            // Immediate is not a valid control/address EA (LEA/PEA/JMP/JSR).
            EaResult::Immediate(_) | EaResult::DataReg(_) | EaResult::AddrReg(_) => 0,
        }
    }

    /// Set N, Z flags based on result. Clear V, C.
    pub fn set_logic_flags(&mut self, value: u32, size: Size) {
        let msb = size.msb_mask();
        self.n_flag = if value & msb != 0 { 0x80 } else { 0 };
        self.not_z_flag = value & size.mask();
        self.v_flag = 0;
        self.c_flag = 0;
    }
}
