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
/// Amiga 1000 WCS / WOM: 256 KiB of writable control store at $FC0000 that
/// the boot ROM loads Kickstart into and then write-protects. The 256 KiB
/// boot-ROM window ($F80000-$FBFFFF) sits immediately below it, so a boot
/// ROM echoed to this size and mapped at ROM_BASE ends exactly at WCS_BASE.
pub const WCS_BASE: u64 = 0x00FC_0000;
pub const WCS_SIZE: usize = 256 * 1024;
/// The A1000 bootstrap ROM is 64 KiB; the address latch echoes it across the
/// 256 KiB $F80000-$FBFFFF window.
pub const A1000_BOOT_ROM_SIZE: usize = 64 * 1024;
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

/// Normalise an Amiga 1000 bootstrap ROM (the 64 KiB "Amiga ROM Bootstrap"
/// that loads Kickstart from the Kickstart disk into the WCS). On real
/// hardware the address latch echoes the 64 KiB part across the whole 256 KiB
/// $F80000-$FBFFFF boot-ROM window; echoing it here lets the standard ROM
/// decode at ROM_BASE cover that window, leaving $FC0000-$FFFFFF for the WCS.
pub fn normalize_a1000_boot_rom(rom: Vec<u8>) -> Result<Vec<u8>> {
    if rom.len() != A1000_BOOT_ROM_SIZE {
        anyhow::bail!(
            "A1000 boot ROM is {} bytes; expected {} (64 KiB)",
            rom.len(),
            A1000_BOOT_ROM_SIZE
        );
    }
    let mut full = Vec::with_capacity(WCS_SIZE);
    while full.len() < WCS_SIZE {
        full.extend_from_slice(&rom);
    }
    Ok(full)
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
    /// Amiga 1000 WCS / WOM at $FC0000 (256 KiB writable control store the
    /// boot ROM loads Kickstart into). Empty on every other machine, which is
    /// how the address decode tells an A1000 apart.
    pub wcs: Vec<u8>,
    /// A1000 WCS write-protect latch. A 68000 RESET clears it (WCS writable,
    /// boot ROM mapped at $F80000); a CPU write anywhere in $F80000-$FBFFFF
    /// sets it, after which the boot code runs the Kickstart it loaded.
    pub wcs_write_protected: bool,
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
        Ok(Self::with_rom(
            rom,
            chip_ram_bytes,
            slow_ram_bytes,
            zorro,
            Vec::new(),
        ))
    }

    /// Load an Amiga 1000: the `rom_path` is the 64 KiB bootstrap ROM (echoed
    /// across the $F80000 boot-ROM window), and a 256 KiB WCS is allocated at
    /// $FC0000 for the boot ROM to load Kickstart into from the Kickstart disk.
    pub fn load_a1000(
        rom_path: &Path,
        chip_ram_bytes: usize,
        slow_ram_bytes: usize,
        zorro: ZorroChain,
    ) -> Result<Self> {
        let rom = std::fs::read(rom_path)
            .with_context(|| format!("reading A1000 boot ROM {}", rom_path.display()))?;
        let rom = normalize_a1000_boot_rom(rom)?;
        let wcs = vec![0u8; WCS_SIZE];
        Ok(Self::with_rom(
            rom,
            chip_ram_bytes,
            slow_ram_bytes,
            zorro,
            wcs,
        ))
    }

    fn with_rom(
        rom: Vec<u8>,
        chip_ram_bytes: usize,
        slow_ram_bytes: usize,
        zorro: ZorroChain,
        wcs: Vec<u8>,
    ) -> Self {
        Self {
            chip_ram: vec![0u8; chip_ram_bytes],
            slow_ram: vec![0u8; slow_ram_bytes],
            rom,
            overlay: true,
            zorro,
            extended_rom: Vec::new(),
            extended_rom_base: 0,
            wcs,
            wcs_write_protected: false,
        }
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
        // Cold boot loses the WCS contents and returns the latch to boot mode
        // (WCS writable), so the A1000 reloads Kickstart from disk. A warm
        // (keyboard) reset preserves the WCS, as the real machine does.
        self.wcs.fill(0);
        self.wcs_write_protected = false;
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

    #[test]
    fn a1000_boot_rom_echoes_64k_across_the_256k_window() {
        // The 64 KiB A1000 bootstrap ROM is echoed four times to fill the
        // 256 KiB $F80000-$FBFFFF window, so the standard ROM decode covers it.
        let boot: Vec<u8> = (0..A1000_BOOT_ROM_SIZE).map(|i| i as u8).collect();
        let out = normalize_a1000_boot_rom(boot.clone()).unwrap();
        assert_eq!(out.len(), WCS_SIZE);
        for chunk in out.chunks(A1000_BOOT_ROM_SIZE) {
            assert_eq!(chunk, &boot[..]);
        }
        // Only the 64 KiB part is accepted (a 256/512 KiB Kickstart is not it).
        assert!(normalize_a1000_boot_rom(vec![0u8; ROM_SIZE]).is_err());
        assert!(normalize_a1000_boot_rom(vec![0u8; 32 * 1024]).is_err());
    }
}
