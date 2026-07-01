// SPDX-License-Identifier: GPL-3.0-or-later

//! Versioned save states: snapshot and restore the full emulated machine.
//!
//! A state captures everything the deterministic core needs to resume
//! exactly where it left off: the CPU core, the whole `Bus` (RAM, ROM,
//! chipset, CIAs, floppy images in memory, expansion boards, CD state),
//! and the machine-level timing carries. Host-side state is deliberately
//! excluded and survives the load instead: audio/serial sinks, debugger
//! instrumentation, and diagnostic trace files. File-backed hard-drive and
//! CD images are stored as paths and reopened on load, so their sector
//! contents are NOT part of the state -- a guest that wrote to a hard
//! drive after the snapshot will see those writes after restoring too.
//!
//! Save and load must happen at an emulated-frame boundary; mid-frame the
//! beam-event capture buffers and slice accounting are not in a resumable
//! state. The emulator wrappers (`Emulator::save_state`/`load_state`) are
//! called from the frame loop between frames, which satisfies this.
//!
//! File format: an 8-byte magic, a little-endian u32 format version, an
//! (uncompressed) bincode `MachineDescriptor` naming the machine the state was
//! produced on, then a zlib stream of bincode-encoded components in the fixed
//! order written by `M68kMachine::write_state`. The descriptor lets a load
//! detect that the state belongs to a different machine than the running
//! config and reconfigure the host to match it; the serialized components
//! already carry the actual hardware, so the machine itself always rebuilds
//! from the state regardless.

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::config::MachineDescriptor;
use crate::cpu::M68kMachine;

const STATE_MAGIC: &[u8; 8] = b"CLSSTATE";

/// Save-state format version. The payload is bincode of the live state
/// structs, so ANY shape change to a serialized struct (Bus, the chipset
/// modules, CpuCore, floppy/expansion state, ...) -- fields added, removed,
/// reordered, or retyped -- makes old states unreadable: bump this whenever
/// that happens so stale files fail with a clear version message instead of
/// a confusing decode error.
// Version history:
//   1: initial format
//   2: keyboard MCU model replaced the Bus kbd_queue byte path
//   3: keyboard MCU clock-based handshake timing (state shape change)
//   4: PollStats.custom HashMap replaced by a flat Vec table
//   5: MachineDescriptor header (machine-shape guard rail)
//   6: Memory gained the A1000 WCS (wcs + wcs_write_protected)
//   7: Bus.a2091 Option replaced by Bus.devices Vec<BoardDevice>; the
//      BoardBacking::A2091 variant became BoardBacking::Device(slot)
//   8: BoardDevice gained Wasm and A2065 variants (enum layout change)
//   9: CpuCore.fpr retyped f64 -> FloatX80 (80-bit extended FPU registers)
//  10: CpuCache backing arrays became Vec (variable line count for the
//      68040's 4 KB caches vs the 020/030's 256 bytes)
//  11: CpuCore MMU registers collapsed (removed tc/urp/srp/mmusr duplicates;
//      mmu_sr retyped u16->u32) so the 040 MOVEC path and the page-table
//      walker share one register set
//  12: Paula audio channels gained deferred AUDxEN-disable state so a DMA
//      clear is observed at the current word boundary
pub const STATE_VERSION: u32 = 12;

/// Default state file name, timestamped like the screenshot/recorder names.
pub fn auto_filename() -> std::path::PathBuf {
    let ts = crate::timestamp::compact_now();
    std::path::PathBuf::from(format!("copperline-state-{ts}.clstate"))
}

/// Write the machine's emulated state to `path`, stamped with `descriptor`
/// (the shape of the machine that produced it). Call only between emulated
/// frames.
pub fn save(machine: &M68kMachine, descriptor: &MachineDescriptor, path: &Path) -> Result<()> {
    let file =
        File::create(path).with_context(|| format!("creating save state {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer.write_all(STATE_MAGIC)?;
    writer.write_all(&STATE_VERSION.to_le_bytes())?;
    // The descriptor sits uncompressed ahead of the zlib stream so it can be
    // read (and a mismatch detected) without decompressing the whole machine.
    bincode::serialize_into(&mut writer, descriptor)
        .map_err(|e| anyhow!("serializing machine descriptor: {e}"))?;
    let mut encoder = ZlibEncoder::new(writer, Compression::fast());
    machine.write_state(&mut encoder)?;
    encoder
        .finish()
        .and_then(|mut w| w.flush())
        .with_context(|| format!("writing save state {}", path.display()))?;
    Ok(())
}

/// Restore the machine from a state written by `save`, returning the machine
/// descriptor the state was stamped with so the caller can compare it against
/// the running machine and reconfigure the host. The live machine is left
/// untouched if the file is unreadable, has the wrong magic/version, or any
/// referenced disk image cannot be reopened. Call only between emulated
/// frames, and re-anchor real-time pacing afterwards (`Emulator::load_state`
/// does both).
pub fn load(machine: &mut M68kMachine, path: &Path) -> Result<MachineDescriptor> {
    let file =
        File::open(path).with_context(|| format!("opening save state {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut magic = [0u8; STATE_MAGIC.len()];
    reader
        .read_exact(&mut magic)
        .with_context(|| format!("reading save state {}", path.display()))?;
    if &magic != STATE_MAGIC {
        bail!("{} is not a Copperline save state", path.display());
    }
    let mut version_bytes = [0u8; 4];
    reader
        .read_exact(&mut version_bytes)
        .with_context(|| format!("reading save state {}", path.display()))?;
    let version = u32::from_le_bytes(version_bytes);
    if version != STATE_VERSION {
        bail!(
            "save state {} is format version {version}; this build reads version {}",
            path.display(),
            STATE_VERSION
        );
    }
    // Read the descriptor straight from the BufReader; bincode consumes exactly
    // its encoded bytes, leaving the reader positioned at the zlib stream.
    let descriptor: MachineDescriptor = bincode::deserialize_from(&mut reader)
        .map_err(|e| anyhow!("reading machine descriptor from {}: {e}", path.display()))?;
    let mut decoder = ZlibDecoder::new(reader);
    machine
        .apply_state(&mut decoder)
        .with_context(|| format!("loading save state {}", path.display()))?;
    Ok(descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::NullSink;
    use crate::bus::Bus;
    use crate::chipset::paula::Paula;
    use crate::config::CpuModel;
    use crate::floppy::FloppyController;
    use crate::memory::{Memory, ROM_SIZE};
    use crate::serial::NullSerialSink;
    use crate::zorro::ZorroChain;

    /// Minimal machine: reset vectors into ROM, where a `bra.s` spins.
    fn test_machine() -> M68kMachine {
        let mut rom = vec![0u8; ROM_SIZE];
        rom[0..4].copy_from_slice(&0x0000_4000u32.to_be_bytes()); // SP
        rom[4..8].copy_from_slice(&0x00F8_0010u32.to_be_bytes()); // PC
        rom[0x10..0x12].copy_from_slice(&0x60FEu16.to_be_bytes()); // bra.s self
        let bus = Bus::new(
            Memory {
                chip_ram: vec![0u8; 512 * 1024],
                slow_ram: Vec::new(),
                rom,
                overlay: false,
                zorro: ZorroChain::default(),
                extended_rom: Vec::new(),
                extended_rom_base: 0,
                wcs: Vec::new(),
                wcs_write_protected: false,
            },
            Paula::new(Box::new(NullSerialSink), Box::new(NullSink)),
            FloppyController::default(),
        );
        crate::cpu::build(bus, CpuModel::M68000, false, 2, false).unwrap()
    }

    fn temp_state(name: &str) -> std::path::PathBuf {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "copperline-savestate-{}-{unique}-{name}.clstate",
            std::process::id()
        ))
    }

    #[test]
    fn rejects_files_without_the_state_magic() {
        let path = temp_state("magic");
        std::fs::write(&path, b"NOTASTATEFILE").unwrap();
        let mut machine = test_machine();
        let before_pc = machine.pc();
        let err = load(&mut machine, &path).unwrap_err();
        assert!(err.to_string().contains("not a Copperline save state"));
        // A failed load leaves the live machine untouched.
        assert_eq!(machine.pc(), before_pc);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_other_format_versions() {
        let path = temp_state("version");
        let mut bytes = STATE_MAGIC.to_vec();
        bytes.extend_from_slice(&(STATE_VERSION + 1).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let mut machine = test_machine();
        let err = load(&mut machine, &path).unwrap_err();
        assert!(err.to_string().contains("format version"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncated_payload_leaves_the_machine_untouched() {
        let save_path = temp_state("full");
        let truncated_path = temp_state("truncated");
        let mut machine = test_machine();
        machine.step_slice(500).unwrap();
        save(&machine, &MachineDescriptor::default(), &save_path).unwrap();
        let bytes = std::fs::read(&save_path).unwrap();
        std::fs::write(&truncated_path, &bytes[..bytes.len() / 2]).unwrap();

        machine.step_slice(500).unwrap();
        let before_pc = machine.pc();
        let before_secs = machine.bus().emulated_seconds();
        assert!(load(&mut machine, &truncated_path).is_err());
        assert_eq!(machine.pc(), before_pc);
        assert_eq!(machine.bus().emulated_seconds(), before_secs);

        // The intact file still loads after the failed attempt.
        load(&mut machine, &save_path).unwrap();
        let _ = std::fs::remove_file(&save_path);
        let _ = std::fs::remove_file(&truncated_path);
    }

    #[test]
    fn round_trips_the_machine_descriptor() {
        let path = temp_state("descriptor");
        let descriptor = MachineDescriptor {
            cpu: CpuModel::M68EC020,
            chip_ram_bytes: 2 * 1024 * 1024,
            fast_ram_bytes: 8 * 1024 * 1024,
            slow_ram_bytes: 0,
            chipset: crate::config::Chipset::Aga,
            video_standard: crate::chipset::agnus::VideoStandard::Ntsc,
            machine: Some(crate::config::MachineModel::A1200),
            rom: crate::config::RomId::of(b"a fake kickstart image"),
            extended_rom: Some(crate::config::RomId::of(b"a fake extended rom")),
        };
        let mut machine = test_machine();
        save(&machine, &descriptor, &path).unwrap();
        // The descriptor the load reports is the one the state was stamped
        // with, not the (default) shape of the machine being loaded into.
        let loaded = load(&mut machine, &path).unwrap();
        assert_eq!(loaded, descriptor);
        assert!(!MachineDescriptor::default().differences(&loaded).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cd_controller_travels_in_the_state() {
        // A state taken on a CD machine carries its CD controller, so loading
        // it into a machine that had none makes the CD drive appear. This is
        // what lets the status bar's CD controls (keyed on
        // `Bus::cd_drive_present`) show up after loading, e.g., a CD32 state
        // over an A500 session.
        let path = temp_state("cd-controller");
        let mut cd_machine = test_machine();
        cd_machine
            .bus_mut()
            .attach_akiko(crate::akiko::Akiko::new());
        assert!(cd_machine.bus().cd_drive_present());
        save(&cd_machine, &MachineDescriptor::default(), &path).unwrap();

        // A fresh machine with no CD controller gains one from the load.
        let mut plain_machine = test_machine();
        assert!(!plain_machine.bus().cd_drive_present());
        load(&mut plain_machine, &path).unwrap();
        assert!(plain_machine.bus().cd_drive_present());
        let _ = std::fs::remove_file(&path);
    }
}
