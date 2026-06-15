// SPDX-License-Identifier: GPL-3.0-or-later

//! Memory regions backing the Amiga bus.

use crate::zorro::ZorroChain;
use anyhow::{Context, Result};
use std::path::Path;

pub const ROM_SIZE: usize = 512 * 1024;
/// A 256 KiB Kickstart part (1.2/1.3) decodes one address line fewer than a
/// 512 KiB part, so it responds across the whole 512 KiB ROM window mirrored.
pub const ROM_SIZE_256K: usize = 256 * 1024;
pub const ROM_BASE: u64 = 0x00F8_0000;
pub const CHIP_RAM_BASE: u64 = 0x0000_0000;
/// Conventional base of the first Zorro II RAM board (the start of the
/// Zorro II expansion space, where the ROM assigns it). Test fixtures
/// pre-configure their fast RAM boards here.
#[cfg_attr(not(test), allow(dead_code))]
pub const FAST_RAM_BASE: u64 = 0x0020_0000;
pub const SLOW_RAM_BASE: u64 = 0x00C0_0000;
pub use crate::zorro::{AUTOCONFIG_BASE, AUTOCONFIG_SIZE};

/// Normalise a boot-ROM image to the full 512 KiB ROM window. A 512 KiB
/// image is taken as-is. A 256 KiB image (Kickstart 1.2/1.3) does not decode
/// A18, so on real hardware the part appears mirrored across the whole
/// $F80000-$FFFFFF space; the 512 KiB images distributed for these versions
/// are simply that 256 KiB ROM doubled. Mirroring a 256 KiB image up to
/// ROM_SIZE makes both forms behave identically.
pub fn normalize_boot_rom(rom: Vec<u8>) -> Result<Vec<u8>> {
    match rom.len() {
        ROM_SIZE => Ok(rom),
        ROM_SIZE_256K => {
            let mut full = Vec::with_capacity(ROM_SIZE);
            full.extend_from_slice(&rom);
            full.extend_from_slice(&rom);
            Ok(full)
        }
        other => anyhow::bail!(
            "ROM size is {} bytes; expected {} (512 KiB) or {} (256 KiB)",
            other,
            ROM_SIZE,
            ROM_SIZE_256K
        ),
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Memory {
    pub chip_ram: Vec<u8>,
    pub slow_ram: Vec<u8>,
    pub rom: Vec<u8>,
    pub overlay: bool,
    /// Zorro expansion boards (autoconfig chain plus their RAM windows).
    pub zorro: ZorroChain,
    /// Extended ROM image (CD32 at $E00000, CDTV at $F00000); empty when
    /// no extended ROM is fitted.
    pub extended_rom: Vec<u8>,
    pub extended_rom_base: u64,
}

impl Memory {
    /// Load the ROM image and allocate chip/slow RAM. At reset the
    /// CPU bus overlays the ROM into the low address range; the chip RAM
    /// backing store itself remains RAM and becomes CPU-visible when CIA-A
    /// releases /OVL. Expansion RAM lives on the `zorro` chain.
    pub fn load(
        rom_path: &Path,
        chip_ram_bytes: usize,
        slow_ram_bytes: usize,
        zorro: ZorroChain,
    ) -> Result<Self> {
        let rom = std::fs::read(rom_path)
            .with_context(|| format!("reading ROM {}", rom_path.display()))?;
        let rom = normalize_boot_rom(rom)?;
        let chip_ram = vec![0u8; chip_ram_bytes];
        let slow_ram = vec![0u8; slow_ram_bytes];
        Ok(Self {
            chip_ram,
            slow_ram,
            rom,
            overlay: true,
            zorro,
            extended_rom: Vec::new(),
            extended_rom_base: 0,
        })
    }

    /// Attach an extended ROM image: 512 KiB maps at $E00000 (CD32),
    /// 256 KiB at $F00000 (CDTV).
    pub fn attach_extended_rom(&mut self, image: Vec<u8>) -> Result<()> {
        self.extended_rom_base = match image.len() {
            0x8_0000 => 0x00E0_0000,
            0x4_0000 => 0x00F0_0000,
            other => anyhow::bail!(
                "extended ROM is {} bytes; expected 512 KiB (CD32, $E00000) \
                 or 256 KiB (CDTV, $F00000)",
                other
            ),
        };
        self.extended_rom = image;
        Ok(())
    }

    /// Remove any fitted extended ROM, returning the $E00000/$F00000 window
    /// to nothing (open bus). Used when a freshly loaded main ROM is not
    /// accompanied by an extended image.
    pub fn detach_extended_rom(&mut self) {
        self.extended_rom = Vec::new();
        self.extended_rom_base = 0;
    }

    /// Return memory to its cold power-on state: clear all RAM and restore
    /// the boot-time ROM overlay and Zorro autoconfig state. Unlike a
    /// warm (keyboard) reset, this does not preserve RAM contents, so the
    /// machine boots as if power had been cycled.
    pub fn power_on_reset(&mut self) {
        self.chip_ram.fill(0);
        self.slow_ram.fill(0);
        self.overlay = true;
        self.zorro.power_on_reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_rom_512k_is_taken_as_is() {
        let image: Vec<u8> = (0..ROM_SIZE).map(|i| i as u8).collect();
        let out = normalize_boot_rom(image.clone()).unwrap();
        assert_eq!(out, image);
    }

    #[test]
    fn boot_rom_256k_is_mirrored_into_full_window() {
        // A 256 KiB Kickstart 1.x part does not decode A18, so it appears
        // mirrored across the whole 512 KiB ROM window. A truncated 256 KiB
        // image must therefore expand to the same bytes as the doubled
        // 512 KiB images distributed for those versions.
        let half: Vec<u8> = (0..ROM_SIZE_256K).map(|i| i as u8).collect();
        let out = normalize_boot_rom(half.clone()).unwrap();
        assert_eq!(out.len(), ROM_SIZE);
        assert_eq!(&out[..ROM_SIZE_256K], &half[..]);
        assert_eq!(&out[ROM_SIZE_256K..], &half[..]);
    }

    #[test]
    fn boot_rom_other_sizes_are_rejected() {
        assert!(normalize_boot_rom(vec![0u8; 1024]).is_err());
        assert!(normalize_boot_rom(vec![0u8; ROM_SIZE + 1]).is_err());
        assert!(normalize_boot_rom(Vec::new()).is_err());
    }
}
