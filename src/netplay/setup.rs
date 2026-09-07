// SPDX-License-Identifier: GPL-3.0-or-later

//! Validated machine settings and media, without remote host paths.

use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::Path,
};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::chipset::{agnus::AgnusRevision, denise::DeniseRevision};
use crate::{config::*, emulator::Emulator};

pub(super) const MAX_BUNDLE: usize = 512 * 1024 * 1024;
pub(super) const FLOPPY_LIMIT: usize = 16 * 1024 * 1024;
const MANIFEST_LIMIT: usize = 64 * 1024;

/// The guest waits on a default machine, but its host presentation preferences
/// still apply before and after receiving the host's emulated hardware.
pub fn guest_config(local: &RawConfig) -> Result<Config> {
    let mut raw = RawConfig {
        display: local.display.clone(),
        input: RawInput {
            port1: None,
            port2: None,
            ..local.input.clone()
        },
        audio: RawAudio {
            output_enabled: local.audio.output_enabled,
            output_device: local.audio.output_device.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    raw.emulation.realtime_priority = local.emulation.realtime_priority;
    raw.emulation.pacing_budget = local.emulation.pacing_budget.clone();
    raw.serial.mode = Some("off".into());
    Config::try_from(raw)
}

/// Turn directory mounts (including staged WHDLoad/--run volumes) into
/// ordinary Amiga disk volumes. Their names and boot priorities survive;
/// host-directory services and host file authority do not enter the game.
pub(super) fn prepare_sources(cfg: &mut Config) -> Result<()> {
    let mut prepared = cfg.clone();
    prepared.netplay_storage = true;
    prepared.run_program_dir = None;
    prepared.emulation.uaelib_files = false;
    for mount in std::mem::take(&mut prepared.filesys) {
        if mount.readonly {
            prepared.netplay_read_only.push(mount.path.clone());
        }
        let drive = DriveImage {
            path: mount.path,
            volume_name: Some(mount.volume),
            boot_pri: mount.boot_pri,
            filesystem: crate::diskimage::FileSystem::OFS,
        };
        if prepared.gate_array.gayle_id().is_some() || prepared.ide_a4000 {
            if prepared.ide.master.is_none() {
                prepared.ide.master = Some(drive);
                prepared.rom_scsi_device_disable = false;
                continue;
            }
            if prepared.ide.slave.is_none() {
                prepared.ide.slave = Some(drive);
                prepared.rom_scsi_device_disable = false;
                continue;
            }
        }
        let slot = prepared
            .scsi
            .units
            .iter_mut()
            .find(|slot| slot.is_none())
            .context("no free SCSI unit for a netplay directory volume")?;
        *slot = Some(drive);
        if prepared.sdmac && prepared.scsi.rom.is_none() {
            prepared.scsi.controller = ScsiController::A3000;
            prepared.rom_scsi_device_disable = false;
        } else if prepared.scsi.rom.is_none() {
            prepared.scsi.rom = Some(std::path::PathBuf::from(match prepared.scsi.controller {
                ScsiController::A4091 => BUNDLED_A4091_ROM,
                _ => BUNDLED_A2091_ROM,
            }));
        }
    }
    // Persisted clocks are host resources; netplay starts a deterministic,
    // session-only clock on both machines.
    prepared.battmem_path = None;
    prepared.cd32_nvram_path = None;
    super::validate_config(&prepared)?;
    *cfg = prepared;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hardware {
    cpu: RawCpu,
    memory: RawMemory,
    machine: RawMachine,
    chipset: RawChipset,
    audio: RawAudio,
    identify: bool,
    uaelib: bool,
    connected: [bool; 4],
    speed: u16,
    ports: [String; 2],
    rtg: RawRtg,
    boards: Vec<crate::zorro::BoardSpec>,
    scsi: Option<String>,
    lide: Option<String>,
    cartridge: Option<String>,
}

impl Hardware {
    fn capture(cfg: &Config) -> Self {
        let cpu = format!("{:?}", cfg.cpu);
        Self {
            cpu: RawCpu {
                model: Some(cpu.strip_prefix('M').unwrap_or(&cpu).to_owned()),
                fpu: Some(cfg.fpu),
                clock_mhz: Some(cfg.cpu_clock_mhz),
                icache: Some(cfg.cpu_icache),
                dcache: Some(cfg.cpu_dcache),
                unimplemented: (cfg.cpu == CpuModel::M68060)
                    .then(|| format!("{:?}", cfg.cpu_unimplemented).to_ascii_lowercase()),
                jit: Some(false),
            },
            memory: RawMemory {
                chip: Some(cfg.chip_ram_bytes.to_string()),
                fast: Some(cfg.fast_ram_bytes.to_string()),
                slow: Some(cfg.slow_ram_bytes.to_string()),
                init: Some(cfg.ram_init.config_value()),
                motherboard: Some(cfg.mb_ram_bytes.to_string()),
                accelerator: Some(cfg.accel_ram_bytes.to_string()),
                z3: Some(cfg.z3_ram_bytes.to_string()),
            },
            machine: RawMachine {
                profile: cfg.machine.map(|m| format!("{m:?}")),
                rtc: Some(cfg.rtc_present),
                rtc_chip: cfg.rtc_present.then(|| format!("{:?}", cfg.rtc_chip)),
                rtc_time: cfg.rtc_seed_unix.map(|t| RawRtcTime::Unix(t as i64)),
                rtc_frozen: Some(cfg.rtc_frozen),
                battmem: Some(String::new()),
                mem_controller: Some(
                    match cfg.mem_controller {
                        MemController::None => "none",
                        MemController::Ramsey4 => "ramsey-04",
                        MemController::Ramsey7 => "ramsey-07",
                    }
                    .into(),
                ),
                rom_scsi_device_disable: Some(cfg.rom_scsi_device_disable),
            },
            chipset: RawChipset {
                revision: Some(format!("{:?}", cfg.chipset)),
                video: Some(format!("{:?}", cfg.video_standard)),
                agnus: Some(
                    match cfg.agnus_revision {
                        AgnusRevision::Ocs => "OCS",
                        AgnusRevision::Ecs8372Rev4 => "8372A",
                        AgnusRevision::Ecs8375 => "8375",
                        AgnusRevision::AgaAlice => "ALICE",
                    }
                    .into(),
                ),
                denise: Some(
                    match cfg.denise_revision {
                        DeniseRevision::Ocs => "OCS",
                        DeniseRevision::Ecs8373 => "ECS",
                        DeniseRevision::AgaLisa => "LISA",
                    }
                    .into(),
                ),
            },
            audio: RawAudio {
                floppy_sounds: Some(cfg.audio.floppy_sounds),
                floppy_sounds_volume: Some(cfg.audio.floppy_sounds_volume.into()),
                channel_mode: Some(format!("{:?}", cfg.audio.channel_mode)),
                audio_filter: Some(format!("{:?}", cfg.audio.filter)),
                stereo_separation: Some(cfg.audio.stereo_separation.into()),
                ..Default::default()
            },
            identify: cfg.identify_board,
            uaelib: cfg.emulation.uaelib,
            connected: cfg.floppy_connected,
            speed: cfg.floppy.speed,
            ports: cfg.port_devices.map(|p| p.label().to_owned()),
            rtg: RawRtg {
                card: Some(format!("{:?}", cfg.rtg)),
                vram: Some(cfg.rtg_vram_bytes.to_string()),
            },
            boards: cfg.zorro_boards.clone(),
            scsi: cfg
                .scsi
                .enabled()
                .then(|| format!("{:?}", cfg.scsi.controller)),
            lide: cfg.lide.enabled().then(|| cfg.lide.board.name().to_owned()),
            cartridge: cfg.cartridge.model.map(|m| format!("{m:?}")),
        }
    }

    fn config(&self) -> Result<Config> {
        // Validate before the ordinary config parser can resolve any paths.
        ensure!(
            self.machine.battmem.as_deref().is_none_or(str::is_empty),
            "remote battery RAM path is forbidden"
        );
        ensure!(
            self.audio.output_device.is_none()
                && self.audio.output_enabled.is_none()
                && self.audio.stem_granularity.is_none(),
            "remote audio output settings are forbidden"
        );
        ensure!(
            self.boards.len() <= 16
                && self
                    .boards
                    .iter()
                    .all(|b| b.name.len() <= 128 && b.backing == crate::zorro::BoardBacking::Ram),
            "invalid netplay expansion boards"
        );
        let mut raw = RawConfig {
            cpu: self.cpu.clone(),
            memory: self.memory.clone(),
            machine: self.machine.clone(),
            chipset: self.chipset.clone(),
            audio: self.audio.clone(),
            identify: Some(self.identify),
            rtg: self.rtg.clone(),
            ..Default::default()
        };
        raw.emulation.uaelib = Some(self.uaelib);
        if raw
            .machine
            .profile
            .as_ref()
            .is_some_and(|s| s.eq_ignore_ascii_case("CD32"))
        {
            raw.fmv_rom = Some(String::new());
            raw.cd.nvram = Some(String::new());
        }
        raw.floppy.speed = Some(self.speed);
        raw.floppy.drives = Some(0);
        raw.serial.mode = Some("off".into());
        raw.input.port1 = Some(self.ports[0].clone());
        raw.input.port2 = Some(self.ports[1].clone());
        // Controller ROMs are supplied separately. Empty ROM settings opt out
        // of bundled-asset lookup while validating the hardware selection.
        if let Some(controller) = &self.scsi {
            raw.scsi.controller = Some(controller.clone());
            raw.scsi.rom = Some(String::new());
        }
        if let Some(board) = &self.lide {
            raw.lide.board = Some(board.clone());
            raw.lide.rom = Some(String::new());
            raw.lide.rom_bank2 = Some(String::new());
        }
        let mut cfg = Config::try_from(raw)?;
        cfg.netplay_storage = true;
        cfg.floppy_connected = self.connected;
        cfg.battmem_path = None;
        cfg.cd32_nvram_path = None;
        cfg.zorro_boards = self.boards.clone();
        let ram = [
            cfg.chip_ram_bytes,
            cfg.fast_ram_bytes,
            cfg.slow_ram_bytes,
            cfg.mb_ram_bytes,
            cfg.accel_ram_bytes,
            cfg.z3_ram_bytes,
            cfg.rtg_vram_bytes,
        ]
        .into_iter()
        .chain(cfg.zorro_boards.iter().map(|b| b.size_bytes))
        .try_fold(0usize, |sum, n| sum.checked_add(n))
        .context("netplay RAM size overflow")?;
        ensure!(
            ram <= 64 * 1024 * 1024,
            "netplay supports up to 64 MiB of RAM including expansion and video RAM"
        );
        cfg.build_zorro_chain()?;
        super::validate_config(&cfg)?;
        Ok(cfg)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) enum Kind {
    Rom,
    Extended,
    Fmv,
    ScsiRom,
    ScsiOdd,
    LideRom,
    LideBank,
    Cartridge,
    Floppy(u8),
    Ide(u8),
    Scsi(u8),
    Lide(u8),
}

impl Kind {
    fn limit(self) -> Result<usize> {
        Ok(match self {
            Self::Floppy(n) => {
                ensure!(n < 4, "invalid floppy drive");
                FLOPPY_LIMIT
            }
            Self::Ide(n) => {
                ensure!(n < 2, "invalid IDE drive");
                256 * 1024 * 1024
            }
            Self::Scsi(n) => {
                ensure!(n < 7, "invalid SCSI drive");
                256 * 1024 * 1024
            }
            Self::Lide(n) => {
                ensure!(n < 4, "invalid LIDE drive");
                256 * 1024 * 1024
            }
            _ => 2 * 1024 * 1024,
        })
    }
    fn hard_disk(self) -> bool {
        matches!(self, Self::Ide(_) | Self::Scsi(_) | Self::Lide(_))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileInfo {
    kind: Kind,
    size: usize,
    hash: [u8; 32],
    writable: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u16,
    build: String,
    hardware: Hardware,
    files: Vec<FileInfo>,
}

pub(super) struct Bundle {
    manifest: Manifest,
    files: Vec<Vec<u8>>,
}

pub(super) struct Staged {
    pub cfg: Config,
    pub emu: Box<Emulator>,
    // Generated files are scoped to the session and never stored in RawConfig.
    pub directory: tempfile::TempDir,
}

impl Bundle {
    pub fn capture(cfg: &Config, emu: &Emulator) -> Result<Self> {
        let mut bundle = Self {
            manifest: Manifest {
                version: 1,
                build: env!("COPPERLINE_DISPLAY_VERSION").into(),
                hardware: Hardware::capture(cfg),
                files: Vec::new(),
            },
            files: Vec::new(),
        };
        let mut rom = emu.bus().mem.rom.clone();
        if cfg.machine == Some(MachineModel::A1000) {
            rom.truncate(crate::memory::A1000_BOOT_ROM_SIZE);
        }
        bundle.add(Kind::Rom, rom, false)?;
        if !emu.bus().mem.extended_rom.is_empty() {
            bundle.add(Kind::Extended, emu.bus().mem.extended_rom.clone(), false)?;
        }
        for (kind, path) in [
            (Kind::Fmv, cfg.fmv_rom_path.as_ref()),
            (Kind::ScsiRom, cfg.scsi.rom.as_ref()),
            (Kind::ScsiOdd, cfg.scsi.rom_odd.as_ref()),
            (Kind::LideRom, cfg.lide.rom.as_ref()),
            (Kind::LideBank, cfg.lide.rom_bank2.as_ref()),
            (Kind::Cartridge, cfg.cartridge.rom.as_ref()),
        ] {
            if let Some(path) = path {
                bundle.add(kind, read_limited(path, kind.limit()?)?, false)?;
            }
        }
        for drive in 0..4 {
            if let Some(protected) = emu.bus().floppy.disk_image_write_protected(drive) {
                bundle.add(
                    Kind::Floppy(drive as u8),
                    emu.bus().floppy.export_disk_image(drive)?,
                    !protected,
                )?;
            }
        }
        for (kind, drive) in [cfg.ide.master.as_ref(), cfg.ide.slave.as_ref()]
            .into_iter()
            .enumerate()
            .map(|(n, d)| (Kind::Ide(n as u8), d))
            .chain(
                cfg.scsi
                    .units
                    .iter()
                    .enumerate()
                    .map(|(n, d)| (Kind::Scsi(n as u8), d.as_ref())),
            )
            .chain(
                cfg.lide
                    .drives
                    .iter()
                    .enumerate()
                    .map(|(n, d)| (Kind::Lide(n as u8), d.as_ref())),
            )
        {
            if let Some(drive) = drive {
                let unit = match kind {
                    Kind::Ide(n) | Kind::Scsi(n) | Kind::Lide(n) => n,
                    _ => unreachable!(),
                };
                let mut disk = crate::harddrive::HardDriveImage::open_session(
                    &drive.path,
                    &format!("DH{unit}"),
                    match kind {
                        Kind::Ide(_) | Kind::Lide(_) => "ide",
                        _ => "scsi",
                    },
                    drive.volume_name.as_deref(),
                    drive.boot_pri,
                    drive.filesystem,
                )?;
                let len = usize::try_from(disk.total_sectors())?
                    .checked_mul(512)
                    .context("disk size overflow")?;
                ensure!(
                    len <= kind.limit()?,
                    "netplay disk exceeds 256 MiB including partition metadata"
                );
                let mut bytes = vec![0; len];
                for (sector, data) in bytes.chunks_exact_mut(512).enumerate() {
                    disk.read_sector(sector as u64, data)?;
                }
                bundle.add(kind, bytes, !cfg.netplay_read_only.contains(&drive.path))?;
            }
        }
        Ok(bundle)
    }

    fn add(&mut self, kind: Kind, bytes: Vec<u8>, writable: bool) -> Result<()> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= kind.limit()?,
            "invalid netplay media size"
        );
        ensure!(
            self.files.iter().map(Vec::len).sum::<usize>() + bytes.len() <= MAX_BUNDLE,
            "netplay media exceeds 512 MiB"
        );
        self.manifest.files.push(FileInfo {
            kind,
            size: bytes.len(),
            hash: super::digest(&bytes),
            writable,
        });
        self.files.push(bytes);
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let manifest = serde_json::to_vec(&self.manifest)?;
        ensure!(
            manifest.len() <= MANIFEST_LIMIT,
            "netplay manifest too large"
        );
        let mut bytes = (manifest.len() as u32).to_le_bytes().to_vec();
        bytes.extend(manifest);
        for file in &self.files {
            bytes.extend(file);
        }
        ensure!(bytes.len() <= MAX_BUNDLE, "netplay bundle exceeds 512 MiB");
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= 4 && bytes.len() <= MAX_BUNDLE,
            "invalid netplay bundle length"
        );
        let len = u32::from_le_bytes(bytes[..4].try_into()?) as usize;
        ensure!(
            len <= MANIFEST_LIMIT && len <= bytes.len() - 4,
            "invalid netplay manifest length"
        );
        let manifest: Manifest = serde_json::from_slice(&bytes[4..4 + len])?;
        ensure!(
            manifest.version == 1 && manifest.build == env!("COPPERLINE_DISPLAY_VERSION"),
            "netplay requires the same Copperline build on both peers"
        );
        ensure!(
            !manifest.files.is_empty() && manifest.files.len() <= 25,
            "invalid netplay file count"
        );
        manifest.hardware.config()?;
        let mut kinds = BTreeSet::new();
        let mut offset = 4 + len;
        let mut files = Vec::new();
        for file in &manifest.files {
            ensure!(
                kinds.insert(file.kind) && file.size > 0 && file.size <= file.kind.limit()?,
                "invalid or repeated netplay media"
            );
            let end = offset
                .checked_add(file.size)
                .context("media length overflow")?;
            let data = bytes.get(offset..end).context("truncated netplay media")?;
            ensure!(
                super::digest(data) == file.hash,
                "netplay media checksum mismatch"
            );
            files.push(data.to_vec());
            offset = end;
        }
        ensure!(
            offset == bytes.len() && kinds.contains(&Kind::Rom),
            "invalid netplay bundle contents"
        );
        Ok(Self { manifest, files })
    }

    pub fn stage(&self) -> Result<Staged> {
        let mut cfg = self.manifest.hardware.config()?;
        let directory = tempfile::Builder::new()
            .prefix("copperline-netplay-")
            .tempdir()?;
        for (index, (info, bytes)) in self.manifest.files.iter().zip(&self.files).enumerate() {
            let path = directory.path().join(format!(
                "{index}.{}",
                if info.kind.hard_disk() { "hdf" } else { "bin" }
            ));
            if matches!(info.kind, Kind::Floppy(_)) {
                continue;
            }
            std::fs::File::create(&path)?.write_all(bytes)?;
            if info.kind.hard_disk() && !info.writable {
                cfg.netplay_read_only.push(path.clone());
            }
            match info.kind {
                Kind::Rom => cfg.rom_path = path,
                Kind::Extended => cfg.extended_rom_path = Some(path),
                Kind::Fmv => cfg.fmv_rom_path = Some(path),
                Kind::ScsiRom => cfg.scsi.rom = Some(path),
                Kind::ScsiOdd => cfg.scsi.rom_odd = Some(path),
                Kind::LideRom => cfg.lide.rom = Some(path),
                Kind::LideBank => cfg.lide.rom_bank2 = Some(path),
                Kind::Cartridge => {
                    cfg.cartridge.model = CartridgeConfig::parse_model(
                        self.manifest
                            .hardware
                            .cartridge
                            .as_deref()
                            .context("missing cartridge model")?,
                    )?;
                    cfg.cartridge.rom = Some(path);
                }
                Kind::Ide(n) | Kind::Scsi(n) | Kind::Lide(n) => {
                    let drive = Some(DriveImage {
                        path,
                        volume_name: None,
                        boot_pri: HARDFILE_DEFAULT_BOOT_PRI,
                        filesystem: crate::diskimage::FileSystem::FFS,
                    });
                    match info.kind {
                        Kind::Ide(0) => cfg.ide.master = drive,
                        Kind::Ide(_) => cfg.ide.slave = drive,
                        Kind::Scsi(_) => cfg.scsi.units[n as usize] = drive,
                        Kind::Lide(_) => cfg.lide.drives[n as usize] = drive,
                        _ => unreachable!(),
                    }
                }
                Kind::Floppy(_) => unreachable!(),
            }
        }
        let mut emu = Box::new(crate::emulator::build_netplay_machine(
            &cfg,
            Box::new(crate::audio::NullSink),
            true,
        )?);
        for (info, bytes) in self.manifest.files.iter().zip(&self.files) {
            if let Kind::Floppy(drive) = info.kind {
                emu.bus_mut()
                    .floppy
                    .insert_memory_disk_image_bytes_with_limit(
                        drive as usize,
                        bytes.clone(),
                        format!("netplay-df{drive}").into(),
                        !info.writable,
                        FLOPPY_LIMIT,
                    )?;
            }
        }
        emu.bus_mut().floppy.prepare_netplay_images();
        Ok(Staged {
            cfg,
            emu,
            directory,
        })
    }
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        file.metadata()?.len() <= limit as u64,
        "netplay media exceeds its size limit"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "netplay media exceeds its size limit");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_keeps_local_output_preferences_without_loading_local_game_settings() -> Result<()> {
        let mut raw = RawConfig::default();
        raw.cpu.model = Some("invalid local machine".into());
        raw.rom = Some("missing-local.rom".into());
        raw.whdload.game = Some("missing-local-game.lha".into());
        raw.run_program_dir = Some("local-program-files".into());
        raw.audio.output_enabled = Some(false);
        raw.audio.output_device = Some("local output".into());
        raw.display.full_screen = Some(true);
        raw.input.mouse_sensitivity = Some(72);
        raw.input.port1 = Some("analogue".into());
        let cfg = guest_config(&raw)?;
        assert!(!cfg.audio.output_enabled);
        assert_eq!(cfg.audio.output_device.as_deref(), Some("local output"));
        assert!(cfg.full_screen);
        assert_eq!(cfg.mouse_sensitivity, 72);
        assert_eq!(cfg.cpu, Config::default().cpu);
        assert_eq!(cfg.port_devices, Config::default().port_devices);
        assert!(cfg.run_program_dir.is_none());
        assert_eq!(cfg.serial.mode, SerialMode::Off);
        assert_ne!(cfg.rom_path, Path::new("missing-local.rom"));
        Ok(())
    }

    #[test]
    fn hardware_profiles_round_trip_without_host_paths() -> Result<()> {
        for profile in [
            "A500", "A500OCS", "A500Plus", "A600", "A1200", "A3000", "A4000", "CDTV", "CD32",
            "A1000",
        ] {
            let mut raw = RawConfig::default();
            raw.machine.profile = Some(profile.into());
            let mut cfg = Config::try_from(raw)?;
            cfg.serial.mode = SerialMode::Off;
            super::super::prepare_config(&mut cfg)?;
            let hardware = Hardware::capture(&cfg);
            let restored = hardware
                .config()
                .with_context(|| format!("round-tripping {profile}"))?;
            assert_eq!(restored.machine, cfg.machine);
            assert_eq!(restored.cpu, cfg.cpu);
            assert_eq!(restored.floppy_connected, cfg.floppy_connected);
            assert_eq!(restored.rtc_seed_unix, cfg.rtc_seed_unix);
            assert_eq!(restored.agnus_revision, cfg.agnus_revision);
            assert_eq!(restored.port_devices, cfg.port_devices);
            assert!(restored.battmem_path.is_none() && restored.cd32_nvram_path.is_none());
        }
        Ok(())
    }

    #[test]
    fn directory_volumes_keep_names_priorities_and_read_only_access() -> Result<()> {
        for profile in ["A500", "A1200", "A3000", "A4000"] {
            let mut raw = RawConfig::default();
            raw.machine.profile = Some(profile.into());
            raw.serial.mode = Some("off".into());
            for n in 0..3 {
                raw.filesys.push(RawFilesysMount {
                    path: format!("volume-{n}"),
                    volume: Some(format!("Volume{n}")),
                    bootpri: Some(if n == 0 { 6 } else { -128 }),
                    readonly: Some(n == 1),
                });
            }
            let mut cfg = Config::try_from(raw)?;
            prepare_sources(&mut cfg)?;
            assert!(cfg.filesys.is_empty() && cfg.netplay_storage);
            let drives: Vec<_> = [cfg.ide.master.as_ref(), cfg.ide.slave.as_ref()]
                .into_iter()
                .chain(cfg.scsi.units.iter().map(Option::as_ref))
                .flatten()
                .collect();
            assert_eq!(drives.len(), 3);
            for (n, drive) in drives.iter().enumerate() {
                assert_eq!(
                    drive.volume_name.as_deref(),
                    Some(format!("Volume{n}").as_str())
                );
                assert_eq!(drive.boot_pri, if n == 0 { 6 } else { -128 });
                assert!(!drive.filesystem.ffs);
                assert_eq!(cfg.netplay_read_only.contains(&drive.path), n == 1);
            }
            assert_eq!(
                cfg.ide.master.is_some(),
                matches!(profile, "A1200" | "A4000")
            );
            if profile == "A3000" {
                assert_eq!(cfg.scsi.controller, ScsiController::A3000);
            }
        }
        Ok(())
    }

    #[test]
    fn hardware_rejects_host_resources_and_excessive_ram() -> Result<()> {
        let mut hw = Hardware::capture(&Config::default());
        hw.machine.battmem = Some("/tmp/not-a-session-file".into());
        assert!(hw.config().is_err());
        hw.machine.battmem = None;
        hw.audio.output_device = Some("remote choice".into());
        assert!(hw.config().is_err());
        hw.audio.output_device = None;
        hw.cpu.jit = Some(true);
        assert!(hw.config().is_err());
        hw.cpu.jit = Some(false);
        hw.memory.chip = Some("1000000000G".into());
        assert!(hw.config().is_err());
        Ok(())
    }

    #[test]
    fn bundle_rebuilds_the_same_cold_machine_and_rejects_corruption() -> Result<()> {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| -> Result<()> {
                let emu = super::super::tests::emulator()?;
                let cfg = super::super::tests::safe_config()?;
                let bundle = Bundle::capture(&cfg, &emu)?;
                let bytes = bundle.encode()?;
                let host = bundle.stage()?;
                let guest = Bundle::decode(&bytes)?.stage()?;
                assert_eq!(host.emu.netplay_snapshot()?, guest.emu.netplay_snapshot()?);
                assert_ne!(host.directory.path(), guest.directory.path());
                let mut bad = bytes.clone();
                *bad.last_mut().unwrap() ^= 1;
                assert!(Bundle::decode(&bad).is_err());
                assert!(Bundle::decode(&bytes[..bytes.len() - 1]).is_err());
                assert!(Bundle::decode(&u32::MAX.to_le_bytes()).is_err());
                Ok(())
            })?
            .join()
            .unwrap()
    }
}
