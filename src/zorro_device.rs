// SPDX-License-Identifier: GPL-3.0-or-later

//! The functional-Zorro-board boundary.
//!
//! A [`ZorroDevice`] is an expansion board that *does something* beyond
//! presenting RAM: it answers register reads/writes in its configured window,
//! advances on the colour clock, may assert an interrupt line, and may bus-
//! master DMA into Amiga memory. The in-tree A2091 SCSI controller and CDTV
//! DMAC implement it, and the WASM plugin host (`src/wasmboard.rs`) implements
//! it over a guest module so functional boards can be authored out of tree.
//!
//! [`DeviceHost`] is the narrow host-services view a device is handed on every
//! call. Today it exposes guest memory for DMA; capability hooks (CD audio,
//! networking) are added alongside the devices that need them. It also owns the
//! single 24-bit DMA address decode (chip / slow / Zorro-board RAM) that the
//! A2091 and CDTV bus masters previously each open-coded.

use crate::chipset::paula::CdAudioRing;
use crate::memory::{Memory, SLOW_RAM_BASE};

/// 24-bit DMA bus-master word read: the chip / slow / Zorro-board RAM decode a
/// Zorro DMA controller sees. Returns `None` when the (word-aligned) address is
/// not backed by RAM, leaving the unmapped sentinel and warn policy to the
/// caller. Word reads require both bytes in range, matching the hardware word
/// access; the last odd byte of a region therefore does not satisfy a word.
pub(crate) fn dma_read_word(mem: &Memory, addr: u32) -> Option<u16> {
    let a = addr as usize;
    if a + 1 < mem.chip_ram.len() {
        return Some((u16::from(mem.chip_ram[a]) << 8) | u16::from(mem.chip_ram[a + 1]));
    }
    let slow = SLOW_RAM_BASE as usize;
    if a >= slow && a + 1 < slow + mem.slow_ram.len() {
        let o = a - slow;
        return Some((u16::from(mem.slow_ram[o]) << 8) | u16::from(mem.slow_ram[o + 1]));
    }
    if let Some((board, off)) = mem.zorro.region_at(addr, 2) {
        let ram = mem.zorro.board_ram(board);
        return Some((u16::from(ram[off]) << 8) | u16::from(ram[off + 1]));
    }
    None
}

/// 24-bit DMA bus-master word write. Returns `false` when the target is not
/// backed by RAM (so the caller can warn and drop, as the hardware does).
pub(crate) fn dma_write_word(mem: &mut Memory, addr: u32, w: u16) -> bool {
    let a = addr as usize;
    if a + 1 < mem.chip_ram.len() {
        mem.chip_ram[a] = (w >> 8) as u8;
        mem.chip_ram[a + 1] = w as u8;
        return true;
    }
    let slow = SLOW_RAM_BASE as usize;
    if a >= slow && a + 1 < slow + mem.slow_ram.len() {
        let o = a - slow;
        mem.slow_ram[o] = (w >> 8) as u8;
        mem.slow_ram[o + 1] = w as u8;
        return true;
    }
    if let Some((board, off)) = mem.zorro.region_at(addr, 2) {
        let ram = mem.zorro.board_ram_mut(board);
        ram[off] = (w >> 8) as u8;
        ram[off + 1] = w as u8;
        return true;
    }
    false
}

/// 24-bit DMA bus-master byte read (the CDTV diagnostic peek uses this).
/// Returns `None` when the address is not backed by RAM. Byte reads only need
/// the single byte in range, so the bounds differ from the word read above.
pub(crate) fn dma_read_byte(mem: &Memory, addr: u32) -> Option<u8> {
    let a = addr as usize;
    if a < mem.chip_ram.len() {
        return Some(mem.chip_ram[a]);
    }
    let slow = SLOW_RAM_BASE as usize;
    if a >= slow && a < slow + mem.slow_ram.len() {
        return Some(mem.slow_ram[a - slow]);
    }
    if let Some((board, off)) = mem.zorro.region_at(addr, 1) {
        return Some(mem.zorro.board_ram(board)[off]);
    }
    None
}

/// 24-bit DMA bus-master byte write. Returns `false` when the target is not
/// backed by RAM.
pub(crate) fn dma_write_byte(mem: &mut Memory, addr: u32, b: u8) -> bool {
    let a = addr as usize;
    if a < mem.chip_ram.len() {
        mem.chip_ram[a] = b;
        return true;
    }
    let slow = SLOW_RAM_BASE as usize;
    if a >= slow && a < slow + mem.slow_ram.len() {
        mem.slow_ram[a - slow] = b;
        return true;
    }
    if let Some((board, off)) = mem.zorro.region_at(addr, 1) {
        mem.zorro.board_ram_mut(board)[off] = b;
        return true;
    }
    false
}

/// The host-services view handed to a [`ZorroDevice`] on every call. Wraps the
/// guest [`Memory`] so a board can DMA, and is the place capability hooks are
/// added (CD audio injection, networking) as the boards that need them land.
pub struct DeviceHost<'a> {
    mem: &'a mut Memory,
    /// Paula's CD-audio ring, available only on a host built for the CDTV tick.
    cd_audio: Option<&'a mut CdAudioRing>,
}

impl<'a> DeviceHost<'a> {
    pub fn new(mem: &'a mut Memory) -> Self {
        Self {
            mem,
            cd_audio: None,
        }
    }

    /// A host view that also exposes Paula's CD-audio ring, for the CDTV DMAC
    /// tick (which streams CD audio rather than touching guest memory).
    pub fn with_cd_audio(mem: &'a mut Memory, cd_audio: &'a mut CdAudioRing) -> Self {
        Self {
            mem,
            cd_audio: Some(cd_audio),
        }
    }

    /// The guest memory the board DMAs into.
    pub fn memory_mut(&mut self) -> &mut Memory {
        self.mem
    }

    /// Paula's CD-audio ring. Only present on a host built via
    /// [`DeviceHost::with_cd_audio`] (the CDTV tick path); requesting it
    /// elsewhere is a wiring bug.
    pub fn cd_audio(&mut self) -> &mut CdAudioRing {
        self.cd_audio
            .as_deref_mut()
            .expect("DeviceHost::cd_audio requested without a CD-audio ring")
    }

    // These wrap the shared decode for boards that hold a `DeviceHost` rather
    // than a bare `&Memory`. The in-tree A2091/CDTV call the free functions
    // directly; the WASM plugin host (Phase 2) routes its DMA imports here.
    /// 24-bit DMA word read; `None` when the address is unmapped.
    #[allow(dead_code)]
    pub fn dma_read_word(&self, addr: u32) -> Option<u16> {
        dma_read_word(self.mem, addr)
    }

    /// 24-bit DMA word write; `false` when the target is unmapped.
    #[allow(dead_code)]
    pub fn dma_write_word(&mut self, addr: u32, w: u16) -> bool {
        dma_write_word(self.mem, addr, w)
    }

    /// 24-bit DMA byte read; `None` when the address is unmapped.
    #[allow(dead_code)]
    pub fn dma_read_byte(&self, addr: u32) -> Option<u8> {
        dma_read_byte(self.mem, addr)
    }

    /// Bulk DMA read into `buf` starting at Amiga `addr` (byte-granular 24-bit
    /// decode; unmapped bytes read as 0xFF). The WASM plugin host's `dma_read`
    /// import routes here.
    #[allow(dead_code)]
    pub fn dma_read(&self, addr: u32, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = dma_read_byte(self.mem, addr.wrapping_add(i as u32)).unwrap_or(0xFF);
        }
    }

    /// Bulk DMA write of `buf` to Amiga `addr` (byte-granular 24-bit decode;
    /// unmapped bytes dropped). The WASM plugin host's `dma_write` import
    /// routes here.
    #[allow(dead_code)]
    pub fn dma_write(&mut self, addr: u32, buf: &[u8]) {
        for (i, b) in buf.iter().enumerate() {
            dma_write_byte(self.mem, addr.wrapping_add(i as u32), *b);
        }
    }
}

/// A functional Zorro expansion board.
///
/// Implementors handle register access in their configured window
/// (`read`/`write`, offset-relative), advance on the colour clock (`tick`), and
/// expose level-sensitive interrupt lines the bus polls. DMA goes through the
/// [`DeviceHost`]; the bus owns interrupt latching and recognition latency, so a
/// device only reports its line state and never pulses INTREQ.
//
// Implemented in Phase 1 (A2091, CDTV) and Phase 2 (WASM); defined here so the
// boundary and `DeviceHost` live together.
#[allow(dead_code)]
pub trait ZorroDevice {
    /// Read `size` bytes at window offset `off`.
    fn read(&mut self, off: u32, size: usize, host: &mut DeviceHost) -> u32;

    /// Write `size` bytes of `value` at window offset `off`.
    fn write(&mut self, off: u32, size: usize, value: u32, host: &mut DeviceHost);

    /// Advance the device by `cck` colour clocks.
    fn tick(&mut self, cck: u32, host: &mut DeviceHost);

    /// INT2 (PORTS) line state. Level-sensitive: held true while asserted.
    fn int2_line(&self) -> bool {
        false
    }

    /// INT6 (EXTER) line state. Level-sensitive: held true while asserted.
    fn int6_line(&self) -> bool {
        false
    }

    /// True while the device is quiescent so the bus can skip its tick.
    fn is_idle(&self) -> bool {
        true
    }

    /// Colour clocks until the device's next internal event, if known, so the
    /// scheduler can wake it sparsely instead of polling every cck.
    fn next_event_cck(&self) -> Option<u32> {
        None
    }

    /// Drain and report board activity for the HDD/status LED.
    fn take_activity(&mut self) -> bool {
        false
    }

    /// Return the board to its power-on state (keeps attached media/ROM).
    fn reset(&mut self);

    /// A stable identifier for logging and snapshot matching.
    fn kind(&self) -> &'static str;
}

/// A functional Zorro-chain board the bus owns and serializes inline.
///
/// Each variant is a concrete device implementing [`ZorroDevice`]. The enum
/// (rather than `Box<dyn ZorroDevice>`) keeps the set serde-derivable -- a
/// trait object is not -- so a board's whole state round-trips with the `Bus`
/// like every other device, while still letting the bus hold heterogeneous
/// boards in one `Vec` indexed by [`crate::zorro::BoardBacking::Device`].
// The variants are intentionally different sizes (the A2091 carries its boot
// ROM and SBIC state; a WASM board carries a wasmtime store handle). The bus
// holds only a handful of boards, so the unused tail per element is immaterial;
// boxing would just add an indirection on every register access.
#[allow(clippy::large_enum_variant)]
#[derive(serde::Serialize, serde::Deserialize)]
pub enum BoardDevice {
    A2091(crate::a2091::A2091),
    Wasm(crate::wasmboard::WasmBoard),
}

impl ZorroDevice for BoardDevice {
    fn read(&mut self, off: u32, size: usize, host: &mut DeviceHost) -> u32 {
        match self {
            BoardDevice::A2091(d) => ZorroDevice::read(d, off, size, host),
            BoardDevice::Wasm(d) => ZorroDevice::read(d, off, size, host),
        }
    }

    fn write(&mut self, off: u32, size: usize, value: u32, host: &mut DeviceHost) {
        match self {
            BoardDevice::A2091(d) => ZorroDevice::write(d, off, size, value, host),
            BoardDevice::Wasm(d) => ZorroDevice::write(d, off, size, value, host),
        }
    }

    fn tick(&mut self, cck: u32, host: &mut DeviceHost) {
        match self {
            BoardDevice::A2091(d) => ZorroDevice::tick(d, cck, host),
            BoardDevice::Wasm(d) => ZorroDevice::tick(d, cck, host),
        }
    }

    fn int2_line(&self) -> bool {
        match self {
            BoardDevice::A2091(d) => ZorroDevice::int2_line(d),
            BoardDevice::Wasm(d) => ZorroDevice::int2_line(d),
        }
    }

    fn int6_line(&self) -> bool {
        match self {
            BoardDevice::A2091(d) => ZorroDevice::int6_line(d),
            BoardDevice::Wasm(d) => ZorroDevice::int6_line(d),
        }
    }

    fn is_idle(&self) -> bool {
        match self {
            BoardDevice::A2091(d) => ZorroDevice::is_idle(d),
            BoardDevice::Wasm(d) => ZorroDevice::is_idle(d),
        }
    }

    fn next_event_cck(&self) -> Option<u32> {
        match self {
            BoardDevice::A2091(d) => ZorroDevice::next_event_cck(d),
            BoardDevice::Wasm(d) => ZorroDevice::next_event_cck(d),
        }
    }

    fn take_activity(&mut self) -> bool {
        match self {
            BoardDevice::A2091(d) => ZorroDevice::take_activity(d),
            BoardDevice::Wasm(d) => ZorroDevice::take_activity(d),
        }
    }

    fn reset(&mut self) {
        match self {
            BoardDevice::A2091(d) => ZorroDevice::reset(d),
            BoardDevice::Wasm(d) => ZorroDevice::reset(d),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            BoardDevice::A2091(d) => ZorroDevice::kind(d),
            BoardDevice::Wasm(d) => ZorroDevice::kind(d),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zorro::ZorroChain;

    fn mem_with(chip: usize, slow: usize) -> Memory {
        Memory {
            chip_ram: vec![0u8; chip],
            slow_ram: vec![0u8; slow],
            rom: Vec::new(),
            overlay: false,
            zorro: ZorroChain::default(),
            extended_rom: Vec::new(),
            extended_rom_base: 0,
            wcs: Vec::new(),
            wcs_write_protected: false,
        }
    }

    #[test]
    fn dma_word_round_trips_chip_and_slow_ram() {
        let mut mem = mem_with(0x100, 0x100);
        assert!(dma_write_word(&mut mem, 0x10, 0xBEEF));
        assert_eq!(dma_read_word(&mem, 0x10), Some(0xBEEF));
        assert_eq!(mem.chip_ram[0x10], 0xBE);
        assert_eq!(mem.chip_ram[0x11], 0xEF);

        let slow = SLOW_RAM_BASE as u32;
        assert!(dma_write_word(&mut mem, slow + 0x20, 0x1234));
        assert_eq!(dma_read_word(&mem, slow + 0x20), Some(0x1234));
        assert_eq!(mem.slow_ram[0x20], 0x12);
    }

    #[test]
    fn word_access_needs_both_bytes_in_range() {
        // chip_ram is 0x100 bytes: the last word starts at 0xFE (0xFE,0xFF in
        // range); a word at 0xFF would need byte 0x100, which is out of range,
        // and 0xFF is not in slow or Zorro space either, so it is unmapped.
        let mut mem = mem_with(0x100, 0x100);
        assert!(dma_write_word(&mut mem, 0xFE, 0xAA55));
        assert_eq!(dma_read_word(&mem, 0xFE), Some(0xAA55));
        assert_eq!(dma_read_word(&mem, 0xFF), None);
        assert!(!dma_write_word(&mut mem, 0xFF, 0x0000));
    }

    #[test]
    fn byte_read_includes_the_last_byte_a_word_cannot() {
        // The byte decode (CDTV peek) accepts the final byte 0xFF that the word
        // decode rejects -- the bounds genuinely differ.
        let mut mem = mem_with(0x100, 0x100);
        mem.chip_ram[0xFF] = 0x7E;
        assert_eq!(dma_read_byte(&mem, 0xFF), Some(0x7E));
        assert_eq!(dma_read_byte(&mem, 0x100), None);
    }

    #[test]
    fn unmapped_addresses_report_none() {
        let mem = mem_with(0x100, 0x100);
        // Between chip top and slow base: nothing backs it.
        assert_eq!(dma_read_word(&mem, 0x0040_0000), None);
        assert_eq!(dma_read_byte(&mem, 0x0040_0000), None);
    }
}
