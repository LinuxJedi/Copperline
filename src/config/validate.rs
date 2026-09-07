//! Validation and conversion from the raw TOML view into a checked
//! [`Config`], plus the value-syntax parsers shared with the CLI flags.
use super::*;
use crate::bus::PortDevice;
use crate::chipset::agnus::{AgnusRevision, VideoStandard};
use crate::chipset::denise::DeniseRevision;
use crate::memory::RamInit;
use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
impl TryFrom<RawConfig> for Config {
    type Error = anyhow::Error;

    fn try_from(raw: RawConfig) -> Result<Self> {
        let machine = match raw.machine.profile.as_deref() {
            None => None,
            Some(s) => Some(parse_machine_model(s)?),
        };
        let defaults = machine.map_or_else(Config::default, machine_profile_defaults);
        // Independent validation failures accumulate here so a single pass
        // reports them all; parse failures whose value later checks depend on
        // still fail fast. On any accumulated error the fallback values never
        // reach a running machine.
        let mut errors: Vec<anyhow::Error> = Vec::new();
        let cpu = match raw.cpu.model.as_deref() {
            None => defaults.cpu,
            Some(s) => parse_cpu(s)?,
        };
        let fpu = raw.cpu.fpu.unwrap_or_else(|| cpu.default_fpu());
        let cpu_clock_mhz = match raw.cpu.clock_mhz {
            Some(mhz) if mhz.is_finite() && mhz > 0.0 => mhz,
            Some(_) => {
                errors.push(anyhow!("[cpu] clock_mhz must be a positive number"));
                cpu.default_clock_mhz()
            }
            // With the whole [cpu] pair absent, the profile's clock stands:
            // the A1200/CD32 profiles pin the authentic 14.18 MHz (4x the
            // PAL colour clock), where the generic 020 default is 14.0. An
            // explicit [cpu] model is a different part in the socket, so it
            // takes its own stock speed instead.
            None if raw.cpu.model.is_none() => defaults.cpu_clock_mhz,
            None => cpu.default_clock_mhz(),
        };
        if fpu && matches!(cpu, CpuModel::M68000 | CpuModel::M68010) {
            errors.push(anyhow!(
                "[cpu] fpu = true needs the 68020+ coprocessor interface; \
                 a 68000/68010 cannot drive a 68881/68882"
            ));
        }
        // The on-chip caches are silicon: model them by default whenever the
        // CPU has them (AmigaOS turns them on via CACR), so a 020/030 matches
        // real hardware instead of contending with chip-bus DMA on every
        // instruction fetch. `[cpu] icache`/`dcache` still force either way.
        let cpu_icache = raw
            .cpu
            .icache
            .unwrap_or_else(|| cpu.has_instruction_cache());
        let cpu_dcache = raw.cpu.dcache.unwrap_or_else(|| cpu.has_data_cache());
        let cpu_unimplemented = match raw.cpu.unimplemented.as_deref() {
            None => UnimplementedPolicy::Trap,
            Some(s) => {
                let policy = match s.trim().to_ascii_lowercase().as_str() {
                    "trap" => UnimplementedPolicy::Trap,
                    "native" => UnimplementedPolicy::Native,
                    _ => {
                        errors.push(anyhow!(
                            "[cpu] unimplemented must be \"trap\" or \"native\", got {:?}",
                            s
                        ));
                        UnimplementedPolicy::Trap
                    }
                };
                if cpu != CpuModel::M68060 {
                    errors.push(anyhow!(
                        "[cpu] unimplemented applies only to the 68060 \
                         (other models implement their full instruction sets)"
                    ));
                }
                policy
            }
        };
        if cpu_icache && !cpu.has_instruction_cache() {
            errors.push(anyhow!(
                "[cpu] icache = true needs a 68020/68EC020/68030/68040 \
                 (the 68000 has no instruction cache)"
            ));
        }
        if cpu_dcache && !cpu.has_data_cache() {
            errors.push(anyhow!(
                "[cpu] dcache = true needs a 68030 or 68040 \
                 (the 68000/68020 have no data cache)"
            ));
        }
        let cpu_jit = raw.cpu.jit.unwrap_or(false);
        if let Some(speed) = raw.emulation.speed.as_deref() {
            log::warn!(
                "[emulation] speed = {speed:?} is deprecated and ignored: the \
                 deterministic cycle-driven core is the only timing model"
            );
        }
        let warp_boot = raw
            .emulation
            .warp_boot
            .unwrap_or(defaults.emulation.warp_boot);
        let warp_until = match raw.emulation.warp_until {
            None => defaults.emulation.warp_until,
            Some(secs) if secs > 0.0 && secs.is_finite() => Some(secs),
            Some(secs) => {
                bail!("[emulation] warp_until must be a positive number of seconds, got {secs}")
            }
        };
        if warp_boot && warp_until.is_some() {
            bail!(
                "[emulation] warp_boot and warp_until are mutually exclusive: \
                 pick the storage-idle heuristic or the fixed timestamp"
            );
        }
        let emulation = Emulation {
            power_on: raw
                .emulation
                .power_on
                .unwrap_or(defaults.emulation.power_on),
            auto_launch: raw
                .emulation
                .auto_launch
                .unwrap_or(defaults.emulation.auto_launch),
            pacing_budget: match raw.emulation.pacing_budget.as_deref() {
                None => defaults.emulation.pacing_budget,
                Some(s) => parse_pacing_budget(s)?,
            },
            realtime_priority: raw
                .emulation
                .realtime_priority
                .unwrap_or(defaults.emulation.realtime_priority),
            warp_speed: match raw.emulation.warp_speed.as_deref() {
                None => defaults.emulation.warp_speed,
                Some(s) => parse_warp_speed(s)?,
            },
            warp_boot,
            warp_boot_idle: match raw.emulation.warp_boot_idle {
                None => defaults.emulation.warp_boot_idle,
                Some(secs) if secs > 0.0 && secs.is_finite() => secs,
                Some(secs) => {
                    bail!("[emulation] warp_boot_idle must be a positive number of seconds, got {secs}")
                }
            },
            warp_until,
            uaelib: raw.emulation.uaelib.unwrap_or(defaults.emulation.uaelib),
            uaelib_files: raw
                .emulation
                .uaelib_files
                .unwrap_or(defaults.emulation.uaelib_files),
            rewind: raw.emulation.rewind.unwrap_or(defaults.emulation.rewind),
            rewind_budget_mb: match raw.emulation.rewind_budget_mb {
                None => defaults.emulation.rewind_budget_mb,
                // A ring that cannot hold a single snapshot has no anchor to
                // rewind to, so reject the degenerate value rather than
                // silently recording nothing.
                Some(0) => bail!("[emulation] rewind_budget_mb must be at least 1 MiB"),
                Some(mb) => mb,
            },
            rewind_interval_frames: match raw.emulation.rewind_interval_frames {
                None => defaults.emulation.rewind_interval_frames,
                Some(0) => bail!("[emulation] rewind_interval_frames must be at least 1"),
                Some(n) => n,
            },
            run_ahead_frames: match raw.emulation.run_ahead_frames {
                None => defaults.emulation.run_ahead_frames,
                Some(n) => {
                    if n > crate::config::RUN_AHEAD_MAX_FRAMES {
                        bail!(
                            "[emulation] run_ahead_frames must be 0..={}",
                            crate::config::RUN_AHEAD_MAX_FRAMES
                        );
                    }
                    n
                }
            },
        };
        let chip_ram_bytes = match raw.memory.chip.as_deref() {
            None => defaults.chip_ram_bytes,
            Some(s) => parse_size(s, "chip RAM")?,
        };
        let fast_ram_bytes = match raw.memory.fast.as_deref() {
            None => defaults.fast_ram_bytes,
            Some(s) => parse_size(s, "fast RAM")?,
        };
        let slow_ram_bytes = match raw.memory.slow.as_deref() {
            None => defaults.slow_ram_bytes,
            Some(s) => parse_size(s, "slow RAM")?,
        };
        let ram_init = match raw.memory.init.as_deref() {
            None => defaults.ram_init,
            Some(s) => parse_ram_init(s)?,
        };
        let mb_ram_bytes = match raw.memory.motherboard.as_deref() {
            None => defaults.mb_ram_bytes,
            Some(s) => parse_size(s, "motherboard RAM")?,
        };
        let accel_ram_bytes = match raw.memory.accelerator.as_deref() {
            None => defaults.accel_ram_bytes,
            Some(s) => parse_size(s, "accelerator RAM")?,
        };
        let z3_ram_bytes = match raw.memory.z3.as_deref() {
            None => defaults.z3_ram_bytes,
            Some(s) => parse_size(s, "Zorro III RAM")?,
        };
        let mut zorro_boards = Vec::new();
        let mut wasm_boards = Vec::new();
        for entry in &raw.zorro {
            match crate::zorro::load_board_metadata(Path::new(&entry.metadata))? {
                crate::zorro::LoadedZorroBoard::Ram(spec) => zorro_boards.push(spec),
                crate::zorro::LoadedZorroBoard::Wasm {
                    spec,
                    wasm_path,
                    mut manifest,
                    default_config,
                    options: _,
                } => {
                    // Effective config = manifest defaults, with the user's
                    // per-board overrides layered on top.
                    let mut config = default_config;
                    if let Some(overrides) = &entry.config {
                        for (key, value) in overrides {
                            config.insert(key.clone(), crate::zorro::toml_value_to_string(value));
                        }
                    }
                    manifest.config = config;
                    // The bundled-ROM sentinel is reserved for the board
                    // [hostsocket] expands to: a metadata board's file-typed
                    // config value naming it must fail fast here (the module
                    // path sentinel is rejected in load_board_metadata), or
                    // the plugin host would silently hand it the embedded
                    // HostSocket stub ROM.
                    for key in &manifest.file_keys {
                        if manifest.config.get(key).map(String::as_str)
                            == Some(crate::hostsocket::BUNDLED_HOSTSOCKET_ROM)
                        {
                            return Err(anyhow!(
                                "{}: config {key:?} value {:?} is reserved for the bundled \
                                 [hostsocket] board",
                                entry.metadata,
                                crate::hostsocket::BUNDLED_HOSTSOCKET_ROM,
                            ));
                        }
                    }
                    wasm_boards.push(WasmBoardConfig {
                        spec,
                        wasm_path,
                        manifest,
                    });
                }
            }
        }
        let chipset = match raw.chipset.revision.as_deref() {
            None => defaults.chipset,
            Some(s) => parse_chipset(s)?,
        };
        let video_standard = match raw.chipset.video.as_deref() {
            None => defaults.video_standard,
            Some(s) => parse_video_standard(s)?,
        };
        let audio = AudioConfig {
            floppy_sounds: raw
                .audio
                .floppy_sounds
                .unwrap_or(defaults.audio.floppy_sounds),
            floppy_sounds_volume: match raw.audio.floppy_sounds_volume {
                None => defaults.audio.floppy_sounds_volume,
                Some(v) if v <= 100 => v as u8,
                Some(v) => {
                    errors.push(anyhow!(
                        "[audio] floppy_sounds_volume must be 0-100, got {v}"
                    ));
                    defaults.audio.floppy_sounds_volume
                }
            },
            output_device: raw
                .audio
                .output_device
                .clone()
                .filter(|name| !name.trim().is_empty()),
            output_enabled: raw
                .audio
                .output_enabled
                .unwrap_or(defaults.audio.output_enabled),
            channel_mode: match raw.audio.channel_mode.as_deref() {
                None => defaults.audio.channel_mode,
                Some(s) => match parse_channel_mode(s) {
                    Ok(mode) => mode,
                    Err(e) => {
                        errors.push(e);
                        defaults.audio.channel_mode
                    }
                },
            },
            stereo_separation: match raw.audio.stereo_separation {
                None => defaults.audio.stereo_separation,
                Some(v) if v <= 100 => v as u8,
                Some(v) => {
                    errors.push(anyhow!("[audio] stereo_separation must be 0-100, got {v}"));
                    defaults.audio.stereo_separation
                }
            },
            filter: match raw.audio.audio_filter.as_deref() {
                None => defaults.audio.filter,
                Some(s) => match parse_audio_filter_mode(s) {
                    Ok(mode) => mode,
                    Err(e) => {
                        errors.push(e);
                        defaults.audio.filter
                    }
                },
            },
            stem_granularity: match raw.audio.stem_granularity.as_deref() {
                None => defaults.audio.stem_granularity.clone(),
                Some(s) => match crate::audio::mux::StemGranularity::parse_list(s) {
                    Ok(list) => Some(list),
                    Err(e) => {
                        errors.push(anyhow!("[audio] stem_granularity: {e}"));
                        defaults.audio.stem_granularity.clone()
                    }
                },
            },
        };
        let (floppy, floppy_connected, floppy_playlists) =
            parse_floppy(raw.floppy, defaults.floppy_connected)?;
        let overscan = match raw.display.overscan.as_deref() {
            None => defaults.overscan,
            Some(s) => parse_overscan(s)?,
        };
        let mut tv_centre_axis = |key: &str, raw: Option<i32>, range: i32, default: i32| match raw {
            None => default,
            Some(v) if (-range..=range).contains(&v) => v,
            Some(v) => {
                errors.push(anyhow!(
                    "[display] {key} must be between -{range} and {range}, got {v}"
                ));
                default
            }
        };
        let tv_centre = TvCentre {
            h: tv_centre_axis(
                "tv_h_centre",
                raw.display.tv_h_centre,
                TV_H_CENTRE_RANGE,
                defaults.tv_centre.h,
            ),
            v: tv_centre_axis(
                "tv_v_centre",
                raw.display.tv_v_centre,
                TV_V_CENTRE_RANGE,
                defaults.tv_centre.v,
            ),
        };
        let pixel_aspect = match raw.display.pixel_aspect.as_deref() {
            None => defaults.pixel_aspect,
            Some(s) => parse_pixel_aspect(s)?,
        };
        let scaling = match raw.display.scaling.as_deref() {
            None => defaults.scaling,
            Some(s) => parse_display_scaling(s)?,
        };
        let autocrop = raw.display.autocrop.unwrap_or(defaults.autocrop);
        let deinterlace = raw.display.deinterlace.unwrap_or(defaults.deinterlace);
        let phosphor = match raw.display.phosphor {
            None => defaults.phosphor,
            Some(p) if (0.0..=0.95).contains(&p) => p,
            Some(p) => {
                errors.push(anyhow!(
                    "[display] phosphor must be between 0.0 and 0.95, got {p}"
                ));
                defaults.phosphor
            }
        };
        let shader = match raw.display.shader.as_deref() {
            None => defaults.shader.clone(),
            Some(s) => parse_shader(s)?,
        };
        let shader_strength = match raw.display.shader_strength {
            None => defaults.shader_strength,
            Some(p) if (0.0..=1.0).contains(&p) => p,
            Some(p) => {
                errors.push(anyhow!(
                    "[display] shader_strength must be between 0.0 and 1.0, got {p}"
                ));
                defaults.shader_strength
            }
        };
        let bezel = match &raw.display.bezel {
            None => defaults.bezel,
            Some(raw) => match raw.resolve() {
                Ok(style) => style,
                Err(e) => {
                    errors.push(e);
                    defaults.bezel
                }
            },
        };
        let bezel_stickers = match raw.display.bezel_stickers.as_deref().map(str::trim) {
            None => defaults.bezel_stickers.clone(),
            Some("") => None,
            Some(p) => Some(PathBuf::from(p)),
        };
        let perf_overlay = raw.display.perf_overlay.unwrap_or(defaults.perf_overlay);
        let tint = match raw.display.tint.as_deref() {
            None => defaults.tint,
            Some(s) => parse_tint(s)?,
        };
        let menu_scale = match raw.display.menu_scale.as_deref() {
            None => defaults.menu_scale,
            Some(s) => parse_menu_scale(s)?,
        };
        let full_screen = raw.display.full_screen.unwrap_or(defaults.full_screen);
        let status_bar = raw.display.status_bar.unwrap_or(defaults.status_bar);
        let joystick_input_mode = match raw.input.joystick.as_deref() {
            None => defaults.joystick_input_mode,
            Some(s) => parse_joystick_input_mode(s)?,
        };
        let mouse_sensitivity = match raw.input.mouse_sensitivity {
            None => defaults.mouse_sensitivity,
            Some(v) if v <= 100 => v as u8,
            Some(v) => {
                errors.push(anyhow!("[input] mouse_sensitivity must be 0-100, got {v}"));
                defaults.mouse_sensitivity
            }
        };
        let mouse_capture = match raw.input.mouse_capture.as_deref() {
            None => defaults.mouse_capture,
            Some(s) => parse_mouse_capture(s)?,
        };
        // An implausibly fast autofire is a typo, not a preference: at more
        // than ~30 Hz the pulse is shorter than the frame the guest samples
        // it on, so the button would read as noise or as never pressed.
        let autofire_hz = match raw.input.autofire_hz {
            None => defaults.autofire_hz,
            Some(hz) if hz <= AUTOFIRE_MAX_HZ => hz,
            Some(hz) => {
                errors.push(anyhow!(
                    "[input] autofire_hz must be 0 (off) to {AUTOFIRE_MAX_HZ}, got {hz}"
                ));
                defaults.autofire_hz
            }
        };
        // The profile carries the default wiring (mouse + joystick, with a
        // CD32 pad on the CD32 profile); an explicit key beats it either
        // way -- a real CD32 accepts any controller too.
        let port_devices = [
            match raw.input.port1.as_deref() {
                None => defaults.port_devices[0],
                Some(s) => parse_port_device(s, "port1")?,
            },
            match raw.input.port2.as_deref() {
                None => defaults.port_devices[1],
                Some(s) => parse_port_device(s, "port2")?,
            },
        ];
        let serial = SerialConfig {
            mode: match raw.serial.mode.as_deref() {
                None => defaults.serial.mode,
                Some(s) => parse_serial_mode(s)?,
            },
            midi_out: raw.serial.midi_out.clone(),
            midi_in: raw.serial.midi_in.clone(),
            mt32_control_rom: raw.serial.mt32_control_rom.as_ref().map(PathBuf::from),
            mt32_lcd: match raw.serial.mt32_lcd.as_deref() {
                None => defaults.serial.mt32_lcd,
                Some(s) => parse_mt32_lcd(s)?,
            },
            mt32_pcm_rom: raw.serial.mt32_pcm_rom.as_ref().map(PathBuf::from),
            mt32_panel: raw.serial.mt32_panel.unwrap_or(defaults.serial.mt32_panel),
            coppersynth_soundfont: raw.serial.coppersynth_soundfont.as_ref().map(PathBuf::from),
            coppersynth_mt32_mode: raw.serial.coppersynth_mt32_mode.clone(),
            coppersynth_panel: raw
                .serial
                .coppersynth_panel
                .unwrap_or(defaults.serial.coppersynth_panel),
            listen: raw.serial.listen.clone(),
            connect: raw.serial.connect.clone(),
            telnet: raw.serial.telnet.or(defaults.serial.telnet),
            phonebook: raw
                .serial
                .phonebook
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            session: raw.serial.session.as_ref().map(PathBuf::from),
        };
        if serial.mode == SerialMode::Modem && raw.serial.connect.is_some() {
            bail!(
                "[serial] connect is for mode = \"tcp-connect\"; in modem mode the guest \
                 dials with ATD<host:port>"
            );
        }
        if raw.serial.telnet.is_some() && serial.mode != SerialMode::Modem {
            bail!(
                "[serial] telnet is for mode = \"modem\" (it sets AT*T1's default at \
                 power-on); got mode = {:?}",
                serial.mode.label()
            );
        }
        if raw.serial.phonebook.is_some() && serial.mode != SerialMode::Modem {
            bail!(
                "[serial] phonebook is for mode = \"modem\" (a guest's ATD looks numbers up \
                 there); got mode = {:?}",
                serial.mode.label()
            );
        }
        if raw.serial.session.is_some() && serial.mode != SerialMode::Modem {
            bail!(
                "[serial] session is for mode = \"modem\" (it replays a canned dial-out \
                 session instead of dialing out over TCP); got mode = {:?}",
                serial.mode.label()
            );
        }
        if serial.session.is_some() && raw.serial.listen.is_some() {
            bail!(
                "[serial] session and [serial] listen cannot be combined: the scripted \
                 transport has no inbound-call support (dial-out replay only)"
            );
        }
        if let Some(phonebook) = raw.serial.phonebook.as_ref() {
            for (number, target) in phonebook {
                if number.is_empty()
                    || !number.chars().all(|c| {
                        c.is_ascii_digit() || matches!(c, '-' | ' ' | '(' | ')' | '#' | '*')
                    })
                {
                    errors.push(anyhow!(
                        "[serial.phonebook] {number:?} is not a valid phone number: only \
                         digits, the DTMF symbols # and *, and the separators a dialer sends \
                         (-, space, parentheses) are allowed"
                    ));
                }
                let trimmed = target.trim();
                if trimmed.is_empty() {
                    errors.push(anyhow!(
                        "[serial.phonebook] {number:?} has an empty value; expected \
                         \"host:port\" or a bare host (the modem's default port is appended)"
                    ));
                } else if let Some((_, port)) = trimmed.rsplit_once(':') {
                    if port.parse::<u16>().is_err() {
                        errors.push(anyhow!(
                            "[serial.phonebook] {number:?} = {target:?}: {port:?} is not a \
                             valid port (0-65535)"
                        ));
                    }
                }
            }
        }
        if let Some(mode) = serial.coppersynth_mt32_mode.as_deref() {
            let m = mode.trim();
            if !(m.eq_ignore_ascii_case("auto")
                || m.eq_ignore_ascii_case("on")
                || m.eq_ignore_ascii_case("off"))
            {
                bail!(
                    "[serial] coppersynth_mt32_mode must be \"auto\", \"on\", or \"off\", \
                     got {mode:?}"
                );
            }
        }

        let ide = IdeConfig {
            master: raw.ide.master.map(drive_image).transpose()?,
            slave: raw.ide.slave.map(drive_image).transpose()?,
        };
        // Two machines have an IDE port: a Gayle one (A600/A1200) and the
        // A4000's, which is the same ATA cable off the Fat Gary bus. `[ide]`
        // fits either; nothing else has anywhere to put the drives.
        let has_ide_port = defaults.gate_array.gayle_id().is_some() || defaults.ide_a4000;
        if (ide.master.is_some() || ide.slave.is_some()) && !has_ide_port {
            errors.push(anyhow!(
                "[ide] images need a machine with an IDE port: set [machine] profile = \"A600\" \
                 (or A1200, or A4000)"
            ));
        }
        let scsi_controller_named = raw.scsi.controller.is_some();
        let scsi_controller = match raw.scsi.controller.as_deref() {
            // A machine with a Super DMAC already has a SCSI bus, so drives go
            // on it unless the config asks for a Zorro board instead.
            None if defaults.sdmac => ScsiController::A3000,
            None => ScsiController::A2091,
            Some(raw_ctrl) => match raw_ctrl.trim().to_ascii_lowercase().as_str() {
                "a2091" => ScsiController::A2091,
                "a4091" => ScsiController::A4091,
                "a3000" => ScsiController::A3000,
                _ => {
                    errors.push(anyhow!(
                        "[scsi] controller = {raw_ctrl:?} is not known \
                         (expected \"a2091\", \"a4091\", or \"a3000\")"
                    ));
                    ScsiController::A2091
                }
            },
        };
        let rtg = match raw.rtg.card.as_deref() {
            None => defaults.rtg,
            Some(raw_card) => match raw_card.trim().to_ascii_lowercase().as_str() {
                "none" => RtgCard::None,
                "z3660" => RtgCard::Z3660,
                "picasso2" => RtgCard::Picasso2,
                "picasso2plus" | "picasso2+" => RtgCard::Picasso2Plus,
                "graffityz2" => RtgCard::GraffityZ2,
                "graffityz3" => RtgCard::GraffityZ3,
                _ => {
                    errors.push(anyhow!(
                        "[rtg] card = {raw_card:?} is not known \
                         (expected \"z3660\", \"picasso2\", \"picasso2plus\", \
                         \"graffityz2\", \"graffityz3\", or \"none\")"
                    ));
                    RtgCard::None
                }
            },
        };
        // Only the Picasso II and Graffity cards have configurable display
        // memory; other cards ignore [rtg] vram entirely, so a leftover
        // value must not fail an unrelated configuration.
        let rtg_vram_bytes = if matches!(
            rtg,
            RtgCard::Picasso2 | RtgCard::Picasso2Plus | RtgCard::GraffityZ2 | RtgCard::GraffityZ3
        ) {
            match raw.rtg.vram.as_deref() {
                None => defaults.rtg_vram_bytes,
                Some(value) => parse_size(value, "RTG VRAM")?,
            }
        } else {
            defaults.rtg_vram_bytes
        };

        let mut scsi = ScsiConfig {
            controller: scsi_controller,
            rom: raw.scsi.rom.map(PathBuf::from),
            rom_odd: raw.scsi.rom_odd.map(PathBuf::from),
            units: [
                raw.scsi.unit0.map(drive_image).transpose()?,
                raw.scsi.unit1.map(drive_image).transpose()?,
                raw.scsi.unit2.map(drive_image).transpose()?,
                raw.scsi.unit3.map(drive_image).transpose()?,
                raw.scsi.unit4.map(drive_image).transpose()?,
                raw.scsi.unit5.map(drive_image).transpose()?,
                raw.scsi.unit6.map(drive_image).transpose()?,
            ],
        };
        // Explicit A2091/A4091 boards, and a non-empty SCSI bus on machines
        // which default to one, use their bundled open ROMs (resolved to real
        // paths later). A completely absent [scsi] section must remain
        // disabled; otherwise every ordinary A500 configuration would grow a
        // controller merely because A2091 is the enum's default personality.
        let scsi_requested = scsi_controller_named
            || scsi.rom.is_some()
            || scsi.rom_odd.is_some()
            || scsi.units.iter().any(Option::is_some);
        if scsi_requested
            && scsi.controller == ScsiController::A2091
            && scsi.rom.is_none()
            && scsi.rom_odd.is_none()
        {
            scsi.rom = Some(PathBuf::from(BUNDLED_A2091_ROM));
        }
        if scsi_requested && scsi.controller == ScsiController::A4091 && scsi.rom.is_none() {
            scsi.rom = Some(PathBuf::from(BUNDLED_A4091_ROM));
        }
        if scsi.enabled() && scsi.rom.is_none() && scsi.controller.is_zorro_board() {
            errors.push(anyhow!(
                "[scsi] drives need a boot ROM: set [scsi] rom = \"...\""
            ));
        }
        // The motherboard SCSI is silicon, not a card: it has no boot ROM (the
        // Kickstart carries its driver), and it only exists where the Super
        // DMAC does.
        if scsi.controller == ScsiController::A3000 {
            if !defaults.sdmac {
                errors.push(anyhow!(
                    "[scsi] controller = \"a3000\" is the motherboard SCSI: set \
                     [machine] profile = \"A3000\", or fit a Zorro board with \
                     controller = \"a2091\" (or \"a4091\")"
                ));
            }
            if scsi.rom.is_some() {
                errors.push(anyhow!(
                    "[scsi] rom does not apply to the A3000 motherboard SCSI: it has no \
                     boot ROM, Kickstart's own scsi.device drives it"
                ));
            }
        }
        if scsi.rom_odd.is_some() && scsi.controller != ScsiController::A2091 {
            errors.push(anyhow!(
                "[scsi] rom_odd is an A2091 split-EPROM option; the A4091 has a single rom"
            ));
        }
        if scsi.rom_odd.is_some() && scsi.rom.is_none() {
            errors.push(anyhow!("[scsi] rom_odd needs rom (the even EPROM half)"));
        }

        // `[copperhf]`: Copperline's own virtual hardfile controller. Same
        // drive-descriptor shape as `[ide]`/`[scsi]`/`[lide]`, but it serves
        // hard disks only -- there is no ATAPI/SCSI-CDROM emulation behind
        // it, so a CD image extension is rejected here rather than silently
        // attaching nothing useful.
        let copperhf = CopperhfConfig {
            units: [
                raw.copperhf.unit0.map(copperhf_drive_image).transpose()?,
                raw.copperhf.unit1.map(copperhf_drive_image).transpose()?,
                raw.copperhf.unit2.map(copperhf_drive_image).transpose()?,
                raw.copperhf.unit3.map(copperhf_drive_image).transpose()?,
                raw.copperhf.unit4.map(copperhf_drive_image).transpose()?,
                raw.copperhf.unit5.map(copperhf_drive_image).transpose()?,
                raw.copperhf.unit6.map(copperhf_drive_image).transpose()?,
            ],
        };

        let lide_board = match raw.lide.board.as_deref() {
            None => crate::ide_zorro::LidePersonality::Ripple,
            Some(raw_board) => match raw_board.trim().to_ascii_lowercase().as_str() {
                "ripple" => crate::ide_zorro::LidePersonality::Ripple,
                "ride" => crate::ide_zorro::LidePersonality::Ride,
                "atbus2008" | "at-bus2008" | "atbus" => {
                    crate::ide_zorro::LidePersonality::AtBus2008
                }
                _ => {
                    errors.push(anyhow!(
                        "[lide] board = {raw_board:?} is not known \
                         (expected \"ripple\", \"ride\", or \"atbus2008\")"
                    ));
                    crate::ide_zorro::LidePersonality::Ripple
                }
            },
        };
        let raw_lide_slots = raw.lide.drive_slots();
        let mut lide_drives: [Option<DriveImage>; 4] = Default::default();
        for (slot, raw_drive) in raw_lide_slots.iter().enumerate() {
            if let Some(raw_drive) = raw_drive {
                lide_drives[slot] = Some(drive_image(raw_drive.clone())?);
            }
        }
        // An empty string is an explicit "no ROM" -- the escape hatch back
        // to hardware-only mode -- kept apart from "not set" so the default
        // below only fires when the source config said nothing at all.
        let rom_named = raw.lide.rom.as_deref() != Some("");
        let rom_bank2_named = raw.lide.rom_bank2.as_deref() != Some("");
        let mut lide_rom = raw
            .lide
            .rom
            .as_deref()
            .filter(|r| !r.is_empty())
            .map(PathBuf::from);
        let mut lide_rom_bank2 = raw
            .lide
            .rom_bank2
            .as_deref()
            .filter(|r| !r.is_empty())
            .map(PathBuf::from);
        // A fitted board (named explicitly, or carrying a drive image) with
        // no ROM opinion of its own gets the bundled lide.rom, the same way
        // an explicitly-fitted A4091 gets its bundled ROM below -- this
        // also fits a bare `board = "..."` with no drives yet, the setup
        // for booting a CD inserted at runtime.
        let lide_fitted = raw.lide.board.is_some() || lide_drives.iter().any(Option::is_some);
        if lide_fitted && rom_named && lide_rom.is_none() {
            lide_rom = Some(PathBuf::from(BUNDLED_LIDE_ROM));
        }
        // The CD-filesystem bank only applies where the board has one
        // (RIPPLE/RIDE); AT-Bus 2008 would reject it below, so it is never
        // offered there.
        if lide_rom.is_some()
            && rom_bank2_named
            && lide_rom_bank2.is_none()
            && lide_board != crate::ide_zorro::LidePersonality::AtBus2008
        {
            lide_rom_bank2 = Some(PathBuf::from(BUNDLED_LIDE_CDFS_ROM));
        }
        let lide = LideConfig {
            board: lide_board,
            board_named: raw.lide.board.is_some(),
            rom: lide_rom,
            rom_bank2: lide_rom_bank2,
            drives: lide_drives,
        };
        // Unconditional, not gated on `lide.enabled()`: each checks a
        // specific pair of fields directly (drive count vs. the board's
        // channel count, `rom_bank2` vs. `rom`/`board`), so a `[lide]`
        // section that names only `rom_bank2` -- otherwise `enabled() ==
        // false`, since it looks at `board`/`rom`/`drives` -- still gets
        // "rom_bank2 needs rom" instead of the whole table being silently
        // accepted as a no-op.
        let max_drives = lide_board.max_drives();
        // The deprecated positional array's length is checked whatever else
        // the section sets: a named key must not buy an over-long array a
        // free pass, and `drive_slots` only reads the first four entries,
        // so an unchecked fifth would be dropped without a word.
        if raw.lide.drives.len() > max_drives {
            errors.push(anyhow!(
                "[lide] drives has {} entries; {} only has {max_drives} drive(s)",
                raw.lide.drives.len(),
                lide_board.name()
            ));
        }
        // Named keys are checked per slot rather than by count: with holes
        // expressible, "how many are set" no longer says which ones, and
        // `drive2` on a one-channel board is the error worth naming. Only
        // the named ones -- a slot filled from the legacy array is already
        // covered above, and calling it `driveN` would name a key the
        // config never wrote.
        for (slot, named) in raw.lide.named_drive_slots().iter().enumerate() {
            if named.is_some() && slot >= max_drives {
                errors.push(anyhow!(
                    "[lide] drive{slot} is on channel {}; {} only has {max_drives} drive(s)",
                    slot / 2,
                    lide_board.name()
                ));
            }
        }
        if lide.rom_bank2.is_some() && lide.rom.is_none() {
            errors.push(anyhow!(
                "[lide] rom_bank2 needs rom (the primary bank image)"
            ));
        }
        if lide.rom_bank2.is_some() && lide_board == crate::ide_zorro::LidePersonality::AtBus2008 {
            errors.push(anyhow!(
                "[lide] rom_bank2 does not apply to board = \"atbus2008\": that board has \
                 no flash banking"
            ));
        }

        let a2065_net = match (&raw.a2065.net, &raw.a2065.interface) {
            (None, None) => None,
            (None, Some(_)) => {
                return Err(anyhow!(
                    "[a2065] interface needs net = \"bridge\" (or use --a2065-interface)"
                ));
            }
            (Some(s), interface) => {
                let config = crate::net::parse_net_config(s, interface.as_deref())
                    .map_err(|error| anyhow::anyhow!("[a2065] {error}"))?;
                if interface.is_some() && !matches!(&config, crate::net::NetConfig::Bridge { .. }) {
                    return Err(anyhow!(
                        "[a2065] interface applies only to net = \"bridge\""
                    ));
                }
                Some(config)
            }
        };

        let toccata = raw.toccata.enabled.unwrap_or(defaults.toccata);

        // `[cartridge]`: a model with no image of its own gets the bundled
        // one (resolved to a path by config::resolve); an image with no
        // model is a mistake worth flagging rather than a silent no-op.
        let cartridge = {
            let model = match raw.cartridge.model.as_deref() {
                Some(model) => CartridgeConfig::parse_model(model)
                    .map_err(|error| anyhow!("[cartridge] {error}"))?,
                None => defaults.cartridge.model,
            };
            let rom = raw
                .cartridge
                .rom
                .as_deref()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from);
            if rom.is_some() && model.is_none() {
                return Err(anyhow!(
                    "[cartridge] rom applies only to a fitted cartridge (model = \"hrtmon\")"
                ));
            }
            CartridgeConfig {
                model,
                rom: model.and(rom.or_else(|| Some(PathBuf::from(BUNDLED_HRTMON_ROM)))),
            }
        };
        let mhi = raw.mhi.enabled.unwrap_or(defaults.mhi);

        // `[hostsocket]` expands to the bundled WASM plugin board (see
        // crate::hostsocket), appended after any [[zorro]] metadata boards.
        // `net = "host"` is not one of `crate::net::NetConfig`'s own
        // backends at all -- it selects the Amiberry-style host-socket
        // backend (`crate::hostsocket::board_config`'s own `transport`
        // parameter) instead of routing TCP/UDP through this board's
        // smoltcp stack. The underlying smoltcp interface still needs a
        // real (if now mostly unused -- ICMP and DNS-over-`net` are the
        // only things still on it, see `resolver` below) `net` backend of
        // its own, hardcoded to `"loopback"` under `"host"`: keeping this
        // mode's whole zero-config premise (no interface address, no
        // gateway, no pcap/TAP privileges) means it can't be user-picked
        // here the way `"bridge"`'s own address/gateway are.
        let (hostsocket_net, hostsocket_transport) =
            match (&raw.hostsocket.net, &raw.hostsocket.interface) {
                (None, None) => (None, None),
                (None, Some(_)) => {
                    return Err(anyhow!(
                        "[hostsocket] interface needs net = \"bridge\" (or use \
                         --hostsocket-interface)"
                    ));
                }
                (Some(s), interface) if s.trim().eq_ignore_ascii_case("host") => {
                    if interface.is_some() {
                        return Err(anyhow!(
                            "[hostsocket] interface applies only to net = \"bridge\""
                        ));
                    }
                    if raw.hostsocket.address.is_some() || raw.hostsocket.gateway.is_some() {
                        return Err(anyhow!(
                            "[hostsocket] address/gateway don't apply to net = \"host\" (it \
                             has no interface address of its own -- sockets go out on the \
                             host's own network identity, see docs/guide/configuration.md)"
                        ));
                    }
                    (
                        Some(crate::net::NetConfig::Loopback),
                        Some("host".to_string()),
                    )
                }
                (Some(s), interface) => {
                    let config = crate::net::parse_net_config(s, interface.as_deref())
                        .map_err(|error| anyhow::anyhow!("[hostsocket] {error}"))?;
                    if interface.is_some()
                        && !matches!(&config, crate::net::NetConfig::Bridge { .. })
                    {
                        return Err(anyhow!(
                            "[hostsocket] interface applies only to net = \"bridge\""
                        ));
                    }
                    (Some(config), None)
                }
            };
        let hostsocket_resolver = match raw.hostsocket.resolver.as_deref() {
            // No explicit choice: default to the host OS resolver under
            // net = "nat"/"bridge"/"host" -- the thing that works without
            // any dns_server hand-configured to match the backend
            // (bridge's own default dns_server is NAT's virtual forwarder
            // address, unreachable on a real LAN; "host" mode's own
            // smoltcp interface is hardcoded to "loopback", which
            // wouldn't reach a real dns_server at all). Set
            // resolver = "dns" explicitly (with dns_server, if a specific
            // one is wanted) to opt back into the board's own
            // DNS-over-net query. Plain loopback/none have no sane
            // default here ("host" is rejected there below), so they get
            // no resolver key at all -- gethostbyname() stays whatever it
            // already was for those backends.
            None if hostsocket_transport.as_deref() == Some("host") => Some("host".to_string()),
            None => match &hostsocket_net {
                Some(crate::net::NetConfig::Nat) | Some(crate::net::NetConfig::Bridge { .. }) => {
                    Some("host".to_string())
                }
                _ => None,
            },
            Some(s) => {
                let normalized = s.trim().to_ascii_lowercase();
                if normalized != "dns" && normalized != "host" {
                    return Err(anyhow!(
                        "[hostsocket] resolver {s:?} is not one of \"dns\", \"host\""
                    ));
                }
                if normalized == "host"
                    && hostsocket_transport.as_deref() != Some("host")
                    && !matches!(
                        &hostsocket_net,
                        Some(crate::net::NetConfig::Nat)
                            | Some(crate::net::NetConfig::Bridge { .. })
                    )
                {
                    return Err(anyhow!(
                        "[hostsocket] resolver = \"host\" needs net = \"nat\", \"bridge\", or \
                         \"host\" (it would silently defeat \"loopback\"'s own determinism \
                         guarantee)"
                    ));
                }
                Some(normalized)
            }
        };
        if let Some(net) = &hostsocket_net {
            wasm_boards.push(crate::hostsocket::board_config(
                net.clone(),
                raw.hostsocket.dns_server.as_deref(),
                raw.hostsocket.hostname.as_deref(),
                raw.hostsocket.address.as_deref(),
                raw.hostsocket.gateway.as_deref(),
                hostsocket_resolver.as_deref(),
                hostsocket_transport.as_deref(),
            ));
        }

        // `[zz9k]` expands to the bundled ZZ9000 SDK crypto board (see
        // crate::zz9k), appended the same way. Zorro III by default on a
        // 32-bit CPU (the transport probes Z3 first); Zorro II otherwise,
        // where the window is pinned to 4M -- the only Zorro II size the
        // SDK transport accepts shared-buffer allocations for (its
        // "historical fixed 4 MB" profile).
        if raw.zz9k.enabled.unwrap_or(false) {
            let version = match raw.zz9k.zorro {
                None => {
                    if cpu_has_32bit_bus(cpu) {
                        ZorroVersion::III
                    } else {
                        ZorroVersion::II
                    }
                }
                Some(2) => ZorroVersion::II,
                Some(3) => ZorroVersion::III,
                Some(other) => {
                    errors.push(anyhow!("[zz9k] zorro must be 2 or 3, not {other}"));
                    ZorroVersion::II
                }
            };
            let size_bytes = match (version, &raw.zz9k.size) {
                (ZorroVersion::II, None) => crate::zz9k::Z2_BOARD_SIZE,
                (ZorroVersion::II, Some(s)) => match parse_size(s, "[zz9k] size") {
                    Ok(n) if n == crate::zz9k::Z2_BOARD_SIZE => n,
                    Ok(_) => {
                        errors.push(anyhow!(
                            "[zz9k] size on Zorro II is fixed at 4M: the SDK transport \
                                 refuses shared-buffer allocations through any other Zorro II \
                                 window size"
                        ));
                        crate::zz9k::Z2_BOARD_SIZE
                    }
                    Err(e) => {
                        errors.push(e);
                        crate::zz9k::Z2_BOARD_SIZE
                    }
                },
                (ZorroVersion::III, None) => crate::zz9k::Z2_BOARD_SIZE,
                (ZorroVersion::III, Some(s)) => match parse_size(s, "[zz9k] size") {
                    Ok(n) if n.is_power_of_two() && (0x10_0000..=0x1000_0000).contains(&n) => n,
                    Ok(_) => {
                        errors.push(anyhow!(
                            "[zz9k] size must be a power of two from 1M to 256M"
                        ));
                        crate::zz9k::Z2_BOARD_SIZE
                    }
                    Err(e) => {
                        errors.push(e);
                        crate::zz9k::Z2_BOARD_SIZE
                    }
                },
            };
            if let Some(seed) = &raw.zz9k.seed {
                let trimmed = seed.trim().trim_start_matches("0x");
                if trimmed.is_empty()
                    || trimmed.len() > 64
                    || !trimmed.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    errors.push(anyhow!("[zz9k] seed must be 1 to 64 hex digits"));
                }
            }
            wasm_boards.push(crate::zz9k::board_config(
                version,
                size_bytes,
                raw.zz9k.int2.unwrap_or(false),
                raw.zz9k.seed.as_deref(),
            ));
        } else if raw.zz9k.zorro.is_some()
            || raw.zz9k.size.is_some()
            || raw.zz9k.int2.is_some()
            || raw.zz9k.seed.is_some()
        {
            errors.push(anyhow!(
                "[zz9k] settings need enabled = true to take effect"
            ));
        }

        // The A500 Rev 6A is both the "A500" profile and the no-profile
        // default machine (the most common, most-targeted Amiga): the Fatter
        // 8372A Agnus with the original OCS 8362 Denise. An explicit [chipset]
        // revision is an intentional override, so it re-derives the chips from
        // the generic preset instead -- e.g. revision = "OCS" forces a plain
        // 8371/8362 machine even under profile = "A500". cfg.machine, the gate
        // array, and the descriptor machine id are unaffected.
        let a500_default =
            raw.chipset.revision.is_none() && matches!(machine, None | Some(MachineModel::A500));
        let agnus_revision = match raw.chipset.agnus.as_deref() {
            None => match machine {
                // The A500+ (Rev 8A) and A600 boards have the 2 MB "Super Fat"
                // 8375 soldered on, regardless of preset or fitted chip RAM.
                Some(MachineModel::A500Plus | MachineModel::A600) => AgnusRevision::Ecs8375,
                // The A500 Rev 6A / default machine has the 1 MB 8372A. Pinning
                // it keeps the authentic 1 MiB chip-RAM ceiling, so fitting
                // more is rejected by validate_chip_ram rather than silently
                // promoted to an 8375.
                _ if a500_default => AgnusRevision::Ecs8372Rev4,
                // Everything else -- the A1200's AGA preset (Alice) or an
                // explicit revision preset -- picks by preset + fitted chip RAM.
                _ => default_agnus_revision(chipset, chip_ram_bytes),
            },
            Some(s) => parse_agnus_revision(s)?,
        };
        let denise_revision = match raw.chipset.denise.as_deref() {
            // The A500 Rev 6A / default machine pairs its ECS Agnus with the
            // original 8362 OCS Denise (no superhires/BRDRBLNK). Every other
            // machine, and any explicit revision preset, takes the Denise that
            // matches its preset.
            None if a500_default => DeniseRevision::Ocs,
            None => default_denise_revision(chipset),
            Some(s) => parse_denise_revision(s)?,
        };

        let mem_controller = match raw.machine.mem_controller.as_deref() {
            None => defaults.mem_controller,
            Some("none") => MemController::None,
            Some("ramsey-04") => MemController::Ramsey4,
            Some("ramsey-07") => MemController::Ramsey7,
            Some(other) => anyhow::bail!(
                "[machine] mem_controller {other:?} is not one of \
                 none, ramsey-04, ramsey-07"
            ),
        };

        errors.extend(validate_chip_ram(chip_ram_bytes, chipset, agnus_revision).err());
        errors.extend(validate_fast_ram(fast_ram_bytes, chip_ram_bytes).err());
        errors.extend(validate_slow_ram(slow_ram_bytes).err());
        errors.extend(validate_mb_ram(mb_ram_bytes, mem_controller, cpu).err());
        errors.extend(validate_accel_ram(accel_ram_bytes, cpu).err());
        errors.extend(validate_z3_ram(z3_ram_bytes, cpu).err());
        errors.extend(validate_rtg_card(rtg, rtg_vram_bytes, cpu).err());
        let board_specs = zorro_boards
            .iter()
            .chain(wasm_boards.iter().map(|w| &w.spec));
        for board in board_specs {
            if board.version == ZorroVersion::III && !cpu_has_32bit_bus(cpu) {
                errors.push(anyhow!(
                    "zorro board {:?} is Zorro III, which needs a 32-bit CPU \
                     (68020/68030/68040); {:?} has a 24-bit address bus",
                    board.name,
                    cpu
                ));
            }
        }
        let cd_insert_delay_secs = match raw.cd.insert_delay {
            Some(secs) if secs.is_finite() && secs >= 0.0 => secs,
            Some(_) => {
                errors.push(anyhow!("[cd] insert_delay must be a non-negative number"));
                0.0
            }
            None => 0.0,
        };
        let rtc_seed_unix = match &raw.machine.rtc_time {
            Some(RawRtcTime::Unix(n)) => match u64::try_from(*n) {
                Ok(secs) => Some(secs),
                Err(_) => {
                    errors.push(anyhow!(
                        "[machine] rtc_time must be non-negative Unix seconds \
                         (1970 or later)"
                    ));
                    None
                }
            },
            Some(RawRtcTime::Text(s)) => match crate::rtc::parse_rtc_time(s) {
                Ok(secs) => Some(secs),
                Err(e) => {
                    errors.push(anyhow!("[machine] rtc_time: {e}"));
                    None
                }
            },
            None => None,
        };
        let rtc_frozen = raw.machine.rtc_frozen.unwrap_or(false);
        if rtc_frozen && raw.machine.rtc_time.is_none() {
            errors.push(anyhow!(
                "[machine] rtc_frozen = true needs an rtc_time to freeze at"
            ));
        }
        let rtc_chip = match raw.machine.rtc_chip.as_deref().map(parse_rtc_chip) {
            Some(Ok(chip)) => Some(chip),
            Some(Err(e)) => {
                errors.push(e);
                None
            }
            None => None,
        };

        match errors.len() {
            0 => {}
            1 => return Err(errors.remove(0)),
            _ => {
                let mut msg = String::from("configuration has multiple errors:");
                for e in &errors {
                    msg.push_str(&format!("\n  - {e:#}"));
                }
                bail!("{msg}");
            }
        }

        let rtc_present = match raw.machine.rtc {
            // A configured time on an explicitly unfitted clock would
            // silently do nothing; make the contradiction loud.
            Some(false) if rtc_seed_unix.is_some() => anyhow::bail!(
                "[machine] rtc_time is set but rtc = false leaves the \
                 clock unfitted; drop one of them"
            ),
            // Naming a chip for a socket declared empty is the same
            // contradiction.
            Some(false) if rtc_chip.is_some() => anyhow::bail!(
                "[machine] rtc_chip is set but rtc = false leaves the \
                 clock unfitted; drop one of them"
            ),
            Some(fitted) => fitted,
            None => defaults.rtc_present || rtc_seed_unix.is_some() || rtc_chip.is_some(),
        };
        let rtc_chip = rtc_chip.unwrap_or(defaults.rtc_chip);
        let rp5c01_fitted = rtc_present && rtc_chip == crate::rtc::RtcChip::Rp5c01;
        let battmem_path = match raw.machine.battmem.as_deref() {
            // An empty path keeps the battery registers session-only.
            Some("") => None,
            // A backing file for battery RAM the machine does not have
            // would silently never fill; make the contradiction loud.
            Some(path) if !rp5c01_fitted => anyhow::bail!(
                "[machine] battmem ({path}) backs the RP5C01's battery RAM, \
                 but this machine has no RP5C01 fitted; set \
                 rtc_chip = \"RP5C01\" or drop battmem"
            ),
            Some(path) => Some(PathBuf::from(path)),
            None => rp5c01_fitted.then(crate::paths::battery_ram_file),
        };

        // A SCSI unit exists when something answers on it: a Zorro board
        // that was asked for, or the A3000's own controller with the silicon
        // behind it. This mirrors what `build_machine` actually wires up, so
        // a disk is never accepted onto a bus that will not be built.
        let has_scsi = (scsi.enabled() && scsi.controller.is_zorro_board())
            || (defaults.sdmac && scsi.controller == ScsiController::A3000);
        let host_disks =
            parse_host_disks(&raw.host_disk, &ide, &scsi, &lide, has_ide_port, has_scsi)?;
        // A real host disk is a drive on the port just as an image is, and
        // the ROM's driver is what finds it and mounts what its RDB
        // describes. Counting only images would cull that driver out from
        // under a machine whose only drive is a real one -- which opens
        // perfectly and is then never looked at.
        let host_disk_on_ide = host_disks.iter().any(|disk| !disk.attach.is_scsi());
        let host_disk_on_scsi = host_disks.iter().any(|disk| disk.attach.is_scsi());
        if raw.fmv_rom.is_some() && !defaults.akiko {
            anyhow::bail!("fmv_rom is only valid for a CD32 machine profile");
        }
        Ok(Config {
            host_disks,
            rom_path: raw.rom.map(PathBuf::from).unwrap_or(defaults.rom_path),
            netplay_storage: false,
            netplay_read_only: Vec::new(),
            cpu,
            fpu,
            cpu_clock_mhz,
            cpu_icache,
            cpu_dcache,
            cpu_unimplemented,
            cpu_jit,
            emulation,
            chip_ram_bytes,
            fast_ram_bytes,
            slow_ram_bytes,
            ram_init,
            mb_ram_bytes,
            accel_ram_bytes,
            z3_ram_bytes,
            zorro_boards,
            wasm_boards,
            identify_board: raw.identify.unwrap_or(defaults.identify_board),
            filesys: raw
                .filesys
                .iter()
                .map(|m| crate::filesys::MountSpec {
                    path: std::path::PathBuf::from(&m.path),
                    volume: m.volume.clone().unwrap_or_else(|| {
                        std::path::Path::new(&m.path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "HostFS".into())
                    }),
                    boot_pri: m.bootpri.unwrap_or(-128),
                    readonly: m.readonly.unwrap_or(false),
                })
                .collect(),
            run_program_dir: raw.run_program_dir,
            chipset,
            agnus_revision,
            denise_revision,
            machine,
            gate_array: defaults.gate_array,
            ide_a4000: defaults.ide_a4000,
            sdmac: defaults.sdmac,
            // The ROM's scsi.device is pure probe time when the machine's
            // built-in disk controller (Gayle or A4000 IDE, A3000 SDMAC SCSI)
            // has no drives on it: disable it then. With drives it is their
            // boot path and runs; machines with no built-in controller carry
            // no scsi.device in ROM, so there is nothing to disable.
            rom_scsi_device_disable: raw.machine.rom_scsi_device_disable.unwrap_or({
                let builtin_drives = (has_ide_port
                    && (ide.master.is_some() || ide.slave.is_some() || host_disk_on_ide))
                    || (defaults.sdmac
                        && scsi.controller == ScsiController::A3000
                        && (scsi.units.iter().any(Option::is_some) || host_disk_on_scsi));
                (has_ide_port || defaults.sdmac) && !builtin_drives
            }),
            akiko: defaults.akiko,
            cdtv_cd: defaults.cdtv_cd,
            extended_rom_path: raw
                .extended_rom
                .map(PathBuf::from)
                .or(defaults.extended_rom_path),
            fmv_rom_path: match raw.fmv_rom.as_deref() {
                Some("") => None,
                Some(path) => Some(PathBuf::from(path)),
                None => defaults.fmv_rom_path,
            },
            cd_image_path: raw.cd.image.map(PathBuf::from),
            cd_insert_delay_secs,
            cd32_nvram_path: raw
                .cd
                .nvram
                .map(PathBuf::from)
                .or_else(|| defaults.akiko.then(crate::paths::akiko_nvram_file)),
            rtc_present,
            rtc_chip,
            rtc_seed_unix,
            rtc_frozen,
            battmem_path,
            log_unmapped: raw
                .debug
                .log_unmapped
                .as_deref()
                .map(parse_log_unmapped)
                .transpose()?,
            validate_chipset: raw.debug.validate_chipset,
            detect_smc: raw.debug.detect_smc,
            mem_controller,
            video_standard,
            audio,
            ide,
            scsi,
            copperhf,
            lide,
            a2065_net,
            toccata,
            cartridge,
            mhi,
            hostsocket_net,
            hostsocket_transport,
            rtg,
            rtg_vram_bytes,
            floppy,
            floppy_connected,
            floppy_playlists,
            overscan,
            tv_centre,
            pixel_aspect,
            scaling,
            autocrop,
            deinterlace,
            phosphor,
            shader,
            shader_strength,
            bezel,
            bezel_stickers,
            perf_overlay,
            tint,
            menu_scale,
            full_screen,
            status_bar,
            joystick_input_mode,
            mouse_sensitivity,
            mouse_capture,
            autofire_hz,
            port_devices,
            serial,
            parallel: resolve_parallel(raw.parallel)?,
            paths: raw.paths,
        })
    }
}

/// Resolve `[parallel]` into a [`ParallelConfig`]. An explicit `device` selects
/// the peripheral; with none set, a bare `output` path implies a printer
/// (back-compat with the original `[parallel] output = "..."`) and otherwise the
/// port is empty. Rejects a printer with no capture path and an out-of-range
/// sampler gain.
fn resolve_parallel(raw: RawParallel) -> Result<ParallelConfig> {
    let device = match raw.device.as_deref() {
        Some(s) => parse_parallel_device(s)?,
        None if raw.output.is_some() => ParallelDevice::Printer,
        None => ParallelDevice::None,
    };
    if device == ParallelDevice::Printer && raw.output.is_none() {
        bail!("[parallel] device = \"printer\" needs an output path (output = \"...\")");
    }
    let sampler_gain_db = raw.sampler_gain.unwrap_or(0.0);
    let gain_range = crate::sampler::MIN_SAMPLER_GAIN_DB..=crate::sampler::MAX_SAMPLER_GAIN_DB;
    if device == ParallelDevice::Sampler
        && (!sampler_gain_db.is_finite() || !gain_range.contains(&sampler_gain_db))
    {
        bail!(
            "[parallel] sampler_gain must be between {} and {} dB, got {sampler_gain_db}",
            crate::sampler::MIN_SAMPLER_GAIN_DB,
            crate::sampler::MAX_SAMPLER_GAIN_DB
        );
    }
    Ok(ParallelConfig {
        device,
        printer_output: raw.output.map(PathBuf::from),
        sampler_input: raw.sampler_input,
        sampler_gain_db,
    })
}

pub(crate) fn parse_parallel_device(s: &str) -> Result<ParallelDevice> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" | "off" => Ok(ParallelDevice::None),
        "printer" => Ok(ParallelDevice::Printer),
        "sampler" => Ok(ParallelDevice::Sampler),
        other => bail!(
            "[parallel] device must be \"none\", \"printer\", or \"sampler\", got \"{other}\""
        ),
    }
}

pub(crate) fn parse_overscan(s: &str) -> Result<Overscan> {
    match s.trim().to_ascii_lowercase().as_str() {
        "full" => Ok(Overscan::Full),
        "tv" => Ok(Overscan::Tv),
        other => bail!("[display] overscan must be \"full\" or \"tv\", got \"{other}\""),
    }
}

pub(crate) fn parse_pixel_aspect(s: &str) -> Result<PixelAspect> {
    match s.trim().to_ascii_lowercase().as_str() {
        "tv" => Ok(PixelAspect::Tv),
        "square" => Ok(PixelAspect::Square),
        other => bail!("[display] pixel_aspect must be \"tv\" or \"square\", got \"{other}\""),
    }
}

pub(crate) fn parse_display_scaling(s: &str) -> Result<DisplayScaling> {
    match s.trim().to_ascii_lowercase().as_str() {
        "smooth" => Ok(DisplayScaling::Smooth),
        "integer" => Ok(DisplayScaling::Integer),
        other => bail!("[display] scaling must be \"smooth\" or \"integer\", got \"{other}\""),
    }
}

/// Parse a `[display] shader` value: a preset name ("off" is accepted for
/// "none", so [`ShaderKind::label`] round-trips), or the path of a `.wgsl`
/// file, which is kept verbatim since host paths are case-sensitive.
/// Whether the file exists is the loader's business, not the parser's:
/// a missing custom shader falls back to no shader rather than failing
/// the whole config.
pub(crate) fn parse_shader(s: &str) -> Result<ShaderMode> {
    let s = s.trim();
    match s.to_ascii_lowercase().as_str() {
        "none" | "off" => Ok(ShaderMode::None),
        "scanlines" => Ok(ShaderMode::Scanlines),
        "mask" => Ok(ShaderMode::Mask),
        "crt" => Ok(ShaderMode::Crt),
        lower if lower.ends_with(".wgsl") => Ok(ShaderMode::Custom(PathBuf::from(s))),
        _ => Err(anyhow!(
            "[display] shader must be \"none\", \"scanlines\", \"mask\", \"crt\", \
             or a \".wgsl\" file path, got {:?}",
            s
        )),
    }
}

/// Parse a `[display] bezel` name ("off" is accepted for "none", so
/// [`BezelStyle::label`] round-trips).
pub(crate) fn parse_bezel(s: &str) -> Result<BezelStyle> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" | "off" => Ok(BezelStyle::None),
        "1084" => Ok(BezelStyle::Model1084),
        "classic" => Ok(BezelStyle::Classic),
        other => Err(anyhow!(
            "[display] bezel must be \"off\", \"1084\" or \"classic\", got {other:?}"
        )),
    }
}

/// Parse a `[display] tint` value ("off" is accepted for "none", so
/// [`Tint::label`] round-trips).
pub(crate) fn parse_tint(s: &str) -> Result<Tint> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" | "off" => Ok(Tint::None),
        "bw" => Ok(Tint::Bw),
        "green" => Ok(Tint::Green),
        "amber" => Ok(Tint::Amber),
        "sepia" => Ok(Tint::Sepia),
        other => bail!(
            "[display] tint must be \"none\", \"bw\", \"green\", \"amber\", \
             or \"sepia\", got \"{other}\""
        ),
    }
}

/// Parse a `[display] menu_scale` value.
pub(crate) fn parse_menu_scale(s: &str) -> Result<MenuScale> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1x" | "1" | "normal" => Ok(MenuScale::Normal),
        "2x" | "2" | "large" => Ok(MenuScale::Large),
        other => bail!("[display] menu_scale must be \"1x\" or \"2x\", got \"{other}\""),
    }
}

pub(crate) fn parse_port_device(s: &str, key: &str) -> Result<PortDevice> {
    let device = PortDevice::parse(s).ok_or_else(|| {
        anyhow!(
            "[input] {key} must be \"mouse\", \"gamepad-mouse\", \"joystick\", \
             \"cd32\", \"analogue\", or \"none\", got {s:?}"
        )
    })?;
    // A mouse belongs in port 1, and a gamepad driving one is still a
    // mouse: Workbench and nearly every game read the pointer there, so
    // port 2 is told plainly rather than left to behave oddly.
    if device == PortDevice::GamepadMouse && key != "port1" {
        bail!(
            "[input] {key} cannot be \"gamepad-mouse\": only port 1 takes a mouse a gamepad drives"
        );
    }
    Ok(device)
}

/// Display label for a mouse sensitivity value: the neutral midpoint shows as
/// "Default" in the GUI and OSD, every other value as its number. The config
/// and CLI still use the number 50.
#[cfg(feature = "frontend")]
pub(crate) fn mouse_sensitivity_label(sensitivity: u8) -> String {
    if sensitivity == 50 {
        "Default".to_string()
    } else {
        sensitivity.to_string()
    }
}

pub(crate) fn parse_joystick_input_mode(s: &str) -> Result<JoystickInputMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        // "auto" is retained as a backward-compatibility alias for older configs
        // and `--joystick auto`; the auto-detect mode was removed in favour of
        // the two explicit, always-visible modes, so it now maps to the default.
        "auto" | "gamepad" | "pad" | "joystick" | "joy" => Ok(JoystickInputMode::Gamepad),
        "keyboard" | "kbd" | "key" => Ok(JoystickInputMode::Keyboard),
        _ => Err(anyhow!(
            "unknown [input] joystick {:?}: expected \"gamepad\" or \"keyboard\"",
            s
        )),
    }
}

pub(crate) fn parse_mouse_capture(s: &str) -> Result<MouseCapture> {
    match s.trim().to_ascii_lowercase().as_str() {
        "click" | "on-click" => Ok(MouseCapture::Click),
        "auto" | "focus" => Ok(MouseCapture::Auto),
        "manual" | "off" | "none" => Ok(MouseCapture::Manual),
        _ => Err(anyhow!(
            "unknown [input] mouse_capture {:?}: expected \"click\", \"auto\", or \"manual\"",
            s
        )),
    }
}

pub(crate) fn parse_serial_mode(s: &str) -> Result<SerialMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => Ok(SerialMode::Off),
        "stdout" | "terminal" => Ok(SerialMode::Stdout),
        "midi" => Ok(SerialMode::Midi),
        "tcp" => Ok(SerialMode::Tcp),
        "tcp-connect" => Ok(SerialMode::TcpConnect),
        "pty" => Ok(SerialMode::Pty),
        "modem" => Ok(SerialMode::Modem),
        _ => Err(anyhow!(
            "unknown [serial] mode {:?}: expected \"off\", \"stdout\", \"midi\", \"tcp\", \
             \"tcp-connect\", \"pty\", or \"modem\"",
            s
        )),
    }
}

fn parse_pacing_budget(s: &str) -> Result<PacingBudget> {
    match s.trim().to_ascii_lowercase().as_str() {
        "cycles" | "m68k-cycles" => Ok(PacingBudget::Cycles),
        "instructions" | "retired-instructions" => Ok(PacingBudget::Instructions),
        _ => Err(anyhow!(
            "unknown emulation pacing_budget {:?}: expected \"cycles\" or \"instructions\"",
            s
        )),
    }
}

fn parse_warp_speed(s: &str) -> Result<WarpSpeed> {
    match s.trim().to_ascii_lowercase().as_str() {
        "2x" | "2" => Ok(WarpSpeed::X2),
        "4x" | "4" => Ok(WarpSpeed::X4),
        "8x" | "8" => Ok(WarpSpeed::X8),
        "16x" | "16" => Ok(WarpSpeed::X16),
        "max" | "unlimited" => Ok(WarpSpeed::Max),
        _ => Err(anyhow!(
            "unknown emulation warp_speed {:?}: expected \"2x\", \"4x\", \"8x\", \"16x\", or \"max\"",
            s
        )),
    }
}

fn parse_cpu(s: &str) -> Result<CpuModel> {
    let norm = s.trim().to_ascii_lowercase().replace(['m', '_', '-'], "");
    match norm.as_str() {
        "68000" | "000" => Ok(CpuModel::M68000),
        "68010" | "010" => Ok(CpuModel::M68010),
        "68ec020" | "ec020" => Ok(CpuModel::M68EC020),
        "68020" | "020" => Ok(CpuModel::M68020),
        "68030" | "030" => Ok(CpuModel::M68030),
        "68040" | "040" => Ok(CpuModel::M68040),
        "68060" | "060" => Ok(CpuModel::M68060),
        _ => Err(anyhow!(
            "unknown cpu model {:?}: expected 68000 / 68010 / 68EC020 / 68020 / 68030 / 68040 / 68060",
            s
        )),
    }
}

fn parse_chipset(s: &str) -> Result<Chipset> {
    match s.trim().to_ascii_uppercase().as_str() {
        "OCS" => Ok(Chipset::Ocs),
        "ECS" => Ok(Chipset::Ecs),
        "AGA" => Ok(Chipset::Aga),
        _ => Err(anyhow!("unknown chipset {:?}: expected OCS / ECS / AGA", s)),
    }
}

/// Parse `[debug] log_unmapped`: `all`, or a hex `START-END` range with an
/// inclusive end (e.g. `"DD0000-DEFFFF"`, or `"0x00DD0000-0x00DEFFFF"`).
pub(crate) fn parse_log_unmapped(s: &str) -> Result<std::ops::RangeInclusive<u32>> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("all") {
        return Ok(0..=u32::MAX);
    }
    let hex = |v: &str| -> Result<u32> {
        let v = v.trim();
        let digits = v
            .strip_prefix("0x")
            .or_else(|| v.strip_prefix("0X"))
            .unwrap_or(v);
        u32::from_str_radix(digits, 16)
            .with_context(|| format!("[debug] log_unmapped: {v:?} is not a hex address"))
    };
    let (start, end) = s
        .split_once('-')
        .ok_or_else(|| anyhow!("[debug] log_unmapped {s:?}: expected \"all\" or \"START-END\""))?;
    let (start, end) = (hex(start)?, hex(end)?);
    if start > end {
        bail!("[debug] log_unmapped {s:?}: start must not be above end");
    }
    Ok(start..=end)
}

/// Parse a machine model name (`"A500"`, `"A1200"`, ...) as the `--model`
/// flag and `[machine] profile` accept it: case-insensitive, with `_`/`-`/
/// spaces ignored. Public for alternative frontends (the browser build) that
/// take a model name from their own UI.
pub fn parse_machine_model(s: &str) -> Result<MachineModel> {
    let norm = s.trim().to_ascii_uppercase().replace(['_', '-', ' '], "");
    match norm.as_str() {
        "A1000" => Ok(MachineModel::A1000),
        "A500" => Ok(MachineModel::A500),
        "A500OCS" => Ok(MachineModel::A500Ocs),
        "A500PLUS" | "A500+" => Ok(MachineModel::A500Plus),
        "A600" => Ok(MachineModel::A600),
        "A1200" => Ok(MachineModel::A1200),
        "A3000" => Ok(MachineModel::A3000),
        "A4000" => Ok(MachineModel::A4000),
        "CDTV" => Ok(MachineModel::Cdtv),
        "CD32" => Ok(MachineModel::Cd32),
        _ => Err(anyhow!(
            "unknown machine model {:?}: expected A1000 / A500 / A500OCS / A500Plus / A600 / A1200 / A3000 / A4000 / CDTV / CD32",
            s
        )),
    }
}

/// The defaults a `[machine] profile` supplies before the explicit
/// `[cpu]`/`[chipset]`/`[memory]` sections override them. Also the way an
/// alternative frontend builds a stock machine of a given model without a
/// config file, as the desktop launcher and the browser build do.
pub fn machine_profile_defaults(model: MachineModel) -> Config {
    let mut d = Config {
        machine: Some(model),
        ..Config::default()
    };
    match model {
        // The A500 Rev 6A board: the ECS "Fatter" 8372A Agnus (1 MiB chip
        // reach plus the software PAL/NTSC switch) paired with the original
        // OCS 8362 Denise, and the common 512 KiB chip + 512 KiB trapdoor
        // slow RAM. The 8372A makes up to 1 MiB chip RAM possible (chip =
        // "1M" / --chip 1M); the Denise stays OCS, so this is an Agnus-only
        // ECS machine, not a full-ECS A500+. The 8372A/8362 pairing is pinned
        // in the agnus/denise derivation below. A bare 512 KiB machine is
        // still available with `[memory] slow = "0"` or `--slow 0`.
        MachineModel::A500 => {
            d.chipset = Chipset::Ecs;
        }
        // The original Amiga: OCS 8361/8367 Agnus + OCS 8362 Denise, 256 KiB
        // stock chip RAM, no trapdoor slow RAM, no RTC. The `rom` is the
        // 64 KiB bootstrap ROM and the Kickstart disk goes in DF0; the boot
        // ROM loads it into the WCS at $FC0000 (see Memory::load_a1000).
        MachineModel::A1000 => {
            d.chipset = Chipset::Ocs;
            d.chip_ram_bytes = 256 * 1024;
            d.slow_ram_bytes = 0;
            // No RTC (inherits the default-off).
        }
        // The early A500 (Rev 3/5) / A2000: the 512 KiB OCS "Fat Agnus"
        // (8370/8371) and OCS 8362 Denise, with the same 512 KiB chip +
        // 512 KiB trapdoor slow RAM. This is the pre-Rev-6A machine the
        // default used to be; `revision = "OCS"` gives the same chips.
        MachineModel::A500Ocs => {
            d.chipset = Chipset::Ocs;
        }
        // The A500+ (Rev 8A) has a battery-backed OKI RTC soldered to the
        // motherboard -- one of the few models that ships with a clock.
        MachineModel::A500Plus => {
            d.chipset = Chipset::Ecs;
            d.chip_ram_bytes = 1024 * 1024;
            d.slow_ram_bytes = 0;
            d.rtc_present = true;
        }
        // The base A600 shipped without an RTC (only the A600HD added one);
        // it inherits the default-off, so `[machine] rtc = true` re-fits it.
        MachineModel::A600 => {
            d.chipset = Chipset::Ecs;
            d.chip_ram_bytes = 1024 * 1024;
            d.slow_ram_bytes = 0;
            d.gate_array = GateArray::GayleA600;
        }
        MachineModel::A1200 => {
            d.chipset = Chipset::Aga;
            d.chip_ram_bytes = 2 * 1024 * 1024;
            d.slow_ram_bytes = 0;
            d.cpu = CpuModel::M68EC020;
            d.cpu_clock_mhz = 14.18;
            d.gate_array = GateArray::GayleA1200;
        }
        // The A3000: ECS on a big-box board, a 25 MHz 68030 with a real MMU,
        // and Ramsey-04 in front of the motherboard DRAM. Gary, not Gayle, so
        // no PCMCIA and no Gayle IDE. Its SCSI is a Super DMAC at $DD0000
        // driving a WD33C93; `[scsi]` fits drives to it.
        MachineModel::A3000 => {
            d.chipset = Chipset::Ecs;
            d.chip_ram_bytes = 2 * 1024 * 1024;
            d.slow_ram_bytes = 0;
            // Stock motherboard fast RAM: four banks of 256Kx4 ZIPs.
            d.mb_ram_bytes = 4 * 1024 * 1024;
            d.cpu = CpuModel::M68030;
            d.cpu_clock_mhz = 25.0;
            d.mem_controller = MemController::Ramsey4;
            d.gate_array = GateArray::FatGary;
            d.rtc_present = true;
            // The big boxes carry the Ricoh clock part, not the OKI one --
            // and Linux/m68k hard-assumes RP5C01 on these models.
            d.rtc_chip = crate::rtc::RtcChip::Rp5c01;
            d.sdmac = true;
        }
        // The A4000: the same board a generation later -- AGA, a 25 MHz 68040,
        // and Ramsey-07. Its IDE at $DD2020 is Gayle's ATA cable without the
        // gate array; `[ide]` fits drives to it.
        MachineModel::A4000 => {
            d.chipset = Chipset::Aga;
            d.chip_ram_bytes = 2 * 1024 * 1024;
            d.slow_ram_bytes = 0;
            // Stock motherboard fast RAM: one 4 MiB bank of 1Mx4 SIMMs.
            d.mb_ram_bytes = 4 * 1024 * 1024;
            d.cpu = CpuModel::M68040;
            d.cpu_clock_mhz = 25.0;
            d.mem_controller = MemController::Ramsey7;
            d.gate_array = GateArray::FatGary;
            d.rtc_present = true;
            d.rtc_chip = crate::rtc::RtcChip::Rp5c01;
            d.ide_a4000 = true;
        }
        // CDTV: A500-class board with the 1 MB ECS Agnus and 1 MB chip
        // RAM, plus the 256 KiB extended ROM at $F00000 (configure it via
        // extended_rom = "..."). No Gayle. It carries a battery-backed clock.
        MachineModel::Cdtv => {
            d.chipset = Chipset::Ecs;
            d.chip_ram_bytes = 1024 * 1024;
            d.slow_ram_bytes = 0;
            d.floppy_connected = [false; 4];
            d.cdtv_cd = true;
            d.rtc_present = true;
        }
        // CD32: AGA, 68EC020 at 14 MHz, 2 MB chip RAM, Akiko, and the
        // 512 KiB extended ROM at $E00000. No Gayle, no RTC (default-off).
        MachineModel::Cd32 => {
            d.chipset = Chipset::Aga;
            d.chip_ram_bytes = 2 * 1024 * 1024;
            d.slow_ram_bytes = 0;
            d.cpu = CpuModel::M68EC020;
            d.cpu_clock_mhz = 14.18;
            d.floppy_connected = [false; 4];
            d.akiko = true;
            #[cfg(feature = "cd32-fmv")]
            {
                d.fmv_rom_path = Some(PathBuf::from(BUNDLED_FMV_ROM));
            }
            // The bundled controller: lowlevel.library expects the pad's
            // serial button protocol on port 2.
            d.port_devices[1] = PortDevice::Cd32Pad;
        }
    }
    // Hardware that follows from the parts picked above, derived exactly as
    // the raw-config pipeline derives it when the matching [chipset]/[cpu]
    // keys are absent, so a profile built directly (the browser build, the
    // launcher's fallback) is the same machine as a config file naming the
    // profile. Skipping this is how the browser's first A1200 ended up an
    // AGA machine with a 1 MiB-reach ECS Agnus: the chip window mirrors by
    // Agnus reach, so the guest sized 1 MiB of its 2 MiB chip RAM.
    // machine_profile_defaults_match_bare_profile_configs pins the parity.
    d.agnus_revision = match model {
        // The A500+/A600 boards have the 2 MB "Super Fat" 8375 soldered on,
        // regardless of fitted chip RAM.
        MachineModel::A500Plus | MachineModel::A600 => AgnusRevision::Ecs8375,
        // The A500 Rev 6A keeps its pinned 8372A/OCS-Denise pairing (also
        // the no-profile default machine's chips).
        MachineModel::A500 => AgnusRevision::Ecs8372Rev4,
        _ => default_agnus_revision(d.chipset, d.chip_ram_bytes),
    };
    d.denise_revision = match model {
        MachineModel::A500 => DeniseRevision::Ocs,
        _ => default_denise_revision(d.chipset),
    };
    // The FPU and on-chip caches are silicon: present whenever the CPU has
    // them, exactly like the pipeline's [cpu] defaults.
    d.fpu = d.cpu.default_fpu();
    d.cpu_icache = d.cpu.has_instruction_cache();
    d.cpu_dcache = d.cpu.has_data_cache();
    // An RTG card comes fitted wherever the machine can host one, so RTG
    // needs no config step beyond installing the guest driver. The Z3660 is
    // a Zorro III board, so the gate is the same one Zorro III RAM uses: a
    // CPU with a 32-bit address bus. That is the A3000 and A4000 today, and
    // any future profile that qualifies, without a model list to maintain.
    if cpu_has_32bit_bus(d.cpu) {
        d.rtg = RtgCard::Z3660;
    }
    d
}

/// Preset to Agnus mapping: the ECS preset picks the 2 MB 8375 only when
/// more than 1 MB of chip RAM is fitted, so identification and DMA pointer
/// gating match what such a machine would really carry. AGA selects Alice.
fn default_agnus_revision(chipset: Chipset, chip_ram_bytes: usize) -> AgnusRevision {
    match chipset {
        Chipset::Ocs => AgnusRevision::Ocs,
        Chipset::Ecs => {
            if chip_ram_bytes > 1024 * 1024 {
                AgnusRevision::Ecs8375
            } else {
                AgnusRevision::Ecs8372Rev4
            }
        }
        Chipset::Aga => AgnusRevision::AgaAlice,
    }
}

fn default_denise_revision(chipset: Chipset) -> DeniseRevision {
    match chipset {
        Chipset::Ocs => DeniseRevision::Ocs,
        Chipset::Ecs => DeniseRevision::Ecs8373,
        Chipset::Aga => DeniseRevision::AgaLisa,
    }
}

/// Parse `[machine] rtc_chip`. "RF5C01A" is accepted as an alias for the
/// Ricoh part because that is what AmigaOS-lineage sources call it.
fn parse_rtc_chip(s: &str) -> Result<crate::rtc::RtcChip> {
    match s.trim().to_ascii_uppercase().as_str() {
        "MSM6242" | "MSM6242B" | "OKI" => Ok(crate::rtc::RtcChip::Msm6242),
        "RP5C01" | "RP5C01A" | "RF5C01A" | "RICOH" => Ok(crate::rtc::RtcChip::Rp5c01),
        _ => Err(anyhow!(
            "unknown machine rtc_chip {:?}: expected MSM6242 / RP5C01",
            s
        )),
    }
}

fn parse_agnus_revision(s: &str) -> Result<AgnusRevision> {
    match s.trim().to_ascii_uppercase().as_str() {
        "OCS" | "8370" | "8371" => Ok(AgnusRevision::Ocs),
        "8372" | "8372A" => Ok(AgnusRevision::Ecs8372Rev4),
        "8375" | "8372B" => Ok(AgnusRevision::Ecs8375),
        "8374" | "ALICE" => Ok(AgnusRevision::AgaAlice),
        _ => Err(anyhow!(
            "unknown chipset agnus {:?}: expected OCS / 8370 / 8371 / 8372 / 8372A / 8375 / 8374 / ALICE",
            s
        )),
    }
}

fn parse_denise_revision(s: &str) -> Result<DeniseRevision> {
    match s.trim().to_ascii_uppercase().as_str() {
        "OCS" | "8362" => Ok(DeniseRevision::Ocs),
        "ECS" | "8373" => Ok(DeniseRevision::Ecs8373),
        "LISA" | "4203" => Ok(DeniseRevision::AgaLisa),
        _ => Err(anyhow!(
            "unknown chipset denise {:?}: expected OCS / 8362 / ECS / 8373 / LISA / 4203",
            s
        )),
    }
}

/// Public for the browser frontend (crates/copperline-web), whose `WebEmu`
/// constructor takes the same PAL/NTSC names as the `[chipset] video` key,
/// like `parse_machine_model`.
pub fn parse_video_standard(s: &str) -> Result<VideoStandard> {
    match s.trim().to_ascii_uppercase().as_str() {
        "PAL" => Ok(VideoStandard::Pal),
        "NTSC" => Ok(VideoStandard::Ntsc),
        _ => Err(anyhow!(
            "unknown chipset video {:?}: expected PAL / NTSC",
            s
        )),
    }
}

/// Format a byte count back into the compact human size the config screen
/// writes into `[memory]` (the inverse of [`parse_size`] for the multiples it
/// produces): exact GiB/MiB/KiB get a `G`/`M`/`K` suffix, anything else falls
/// back to a raw byte count. Always emits a 4 KiB-aligned value the parser
/// round-trips.
#[cfg_attr(not(feature = "frontend"), allow(dead_code))]
pub(crate) fn format_size(bytes: usize) -> String {
    const K: usize = 1024;
    const M: usize = 1024 * 1024;
    const G: usize = 1024 * 1024 * 1024;
    if bytes == 0 {
        "0".to_string()
    } else if bytes.is_multiple_of(G) {
        format!("{}G", bytes / G)
    } else if bytes.is_multiple_of(M) {
        format!("{}M", bytes / M)
    } else if bytes.is_multiple_of(K) {
        format!("{}K", bytes / K)
    } else {
        bytes.to_string()
    }
}

/// Parse the diagnostic RAM power-on policy shared by `[memory] init` and
/// `--ram-init`. A named default seed keeps the convenient `random` spelling
/// reproducible, while a fixed pattern accepts either the explicit
/// `pattern:WORD` spelling or a bare hexadecimal word such as `0x5555`.
fn parse_ram_init(s: &str) -> Result<RamInit> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("zero") {
        return Ok(RamInit::Zero);
    }
    if s.eq_ignore_ascii_case("random") {
        return Ok(RamInit::Random {
            seed: DEFAULT_RANDOM_RAM_SEED,
        });
    }
    if s.starts_with("0x") || s.starts_with("0X") {
        return Ok(RamInit::Pattern {
            word: parse_ram_pattern(s).with_context(|| format!("[memory] init {s:?}"))?,
        });
    }
    let Some((mode, value)) = s.split_once(':') else {
        bail!(
            "unknown [memory] init {s:?}: expected zero, random, random:SEED, pattern:WORD, or 0xWORD"
        );
    };
    if mode.trim().eq_ignore_ascii_case("pattern") {
        return Ok(RamInit::Pattern {
            word: parse_ram_pattern(value).with_context(|| format!("[memory] init {s:?}"))?,
        });
    }
    if !mode.trim().eq_ignore_ascii_case("random") {
        bail!(
            "unknown [memory] init {s:?}: expected zero, random, random:SEED, pattern:WORD, or 0xWORD"
        );
    }
    let seed = value.trim().replace('_', "");
    let parsed = if let Some(hex) = seed.strip_prefix("0x").or_else(|| seed.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        seed.parse::<u64>()
    }
    .with_context(|| {
        format!("[memory] init {s:?}: seed must be a decimal or 0x hexadecimal 64-bit integer")
    })?;
    Ok(RamInit::Random { seed: parsed })
}

/// Parse the launcher's fixed 16-bit word. Decimal and `0x` hexadecimal
/// spellings match the seed parser; underscores are accepted for readability.
pub(crate) fn parse_ram_pattern(s: &str) -> Result<u16> {
    let value = s.trim().replace('_', "");
    let parsed = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u16::from_str_radix(hex, 16)
    } else {
        value.parse::<u16>()
    };
    parsed.with_context(|| "pattern must be a decimal or 0x hexadecimal 16-bit word")
}

/// Parse a human size like "512K", "1M", "2 MiB" or a raw byte count.
pub(crate) fn parse_size(s: &str, what: &str) -> Result<usize> {
    let raw = s.trim();
    if raw.is_empty() {
        bail!("{} size is empty", what);
    }
    // Split into numeric prefix + unit suffix.
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (num_str, unit_str) = raw.split_at(split);
    let n: u64 = num_str
        .parse()
        .with_context(|| format!("{} size {:?}: bad number", what, s))?;
    let unit = unit_str.trim().to_ascii_uppercase().replace("IB", "B");
    let bytes = match unit.as_str() {
        "" | "B" => n,
        "K" | "KB" => n * 1024,
        "M" | "MB" => n * 1024 * 1024,
        "G" | "GB" => n * 1024 * 1024 * 1024,
        _ => bail!("{} size {:?}: unknown unit {:?}", what, s, unit_str),
    };
    if bytes % 4096 != 0 {
        bail!("{} size {} bytes must be a multiple of 4 KiB", what, bytes);
    }
    Ok(bytes as usize)
}

fn validate_chip_ram(bytes: usize, chipset: Chipset, agnus: AgnusRevision) -> Result<()> {
    let max = match chipset {
        Chipset::Ocs => 512 * 1024,
        Chipset::Ecs => 2 * 1024 * 1024,
        Chipset::Aga => 2 * 1024 * 1024,
    };
    if bytes == 0 {
        bail!("chip RAM must be > 0");
    }
    if bytes > max {
        bail!(
            "chip RAM {} bytes exceeds {:?} chipset maximum of {} bytes",
            bytes,
            chipset,
            max
        );
    }
    let agnus_max = agnus.dma_addr_capability_mask() as usize + 1;
    if bytes > agnus_max {
        bail!(
            "chip RAM {} bytes exceeds the {:?} Agnus address reach of {} bytes",
            bytes,
            agnus,
            agnus_max
        );
    }
    Ok(())
}

fn validate_fast_ram(fast: usize, chip: usize) -> Result<()> {
    // Standard Zorro II auto-configured fast RAM sits at $00200000,
    // limited to 8 MiB. If chip RAM occupies that space (only happens
    // with 2 MiB chip RAM on ECS/AGA), there's nowhere to put it.
    const FAST_BASE: usize = 0x0020_0000;
    const FAST_LIMIT: usize = 8 * 1024 * 1024;
    if fast == 0 {
        return Ok(());
    }
    if chip > FAST_BASE {
        bail!("fast RAM > 0 incompatible with chip RAM > 2 MiB (no room at $00200000)");
    }
    if fast > FAST_LIMIT {
        bail!(
            "fast RAM {} bytes exceeds Zorro II maximum of {} bytes",
            fast,
            FAST_LIMIT
        );
    }
    if zorro_ii_size_code(fast).is_none() {
        bail!(
            "fast RAM {} bytes is not an autoconfigurable Zorro II size (64K, 128K, 256K, 512K, 1M, 2M, 4M, or 8M)",
            fast
        );
    }
    Ok(())
}

/// Motherboard fast RAM must land on Ramsey's bank layout: four banks of
/// either 256Kx4 parts (1 MiB per bank) or 1Mx4 parts (4 MiB per bank), so
/// 1M-4M in 1M steps or 4M/8M/12M/16M. Beyond the four banks the big-box
/// memory map reserves $04000000-$06FFFFFF for motherboard RAM expansion;
/// filling it (whole 4M steps up to 64M) is an A4000/Ramsey-07 option,
/// sized by the same top-down Kickstart probe. It also needs the Ramsey
/// itself and a CPU whose address bus reaches $07000000 at all.
fn validate_mb_ram(mb: usize, mem_controller: MemController, cpu: CpuModel) -> Result<()> {
    const BANK_1M: usize = 1024 * 1024;
    const BANK_4M: usize = 4 * 1024 * 1024;
    if mb == 0 {
        return Ok(());
    }
    if mem_controller.ramsey_revision().is_none() {
        bail!(
            "motherboard RAM needs a Ramsey memory controller \
             ([machine] mem_controller = \"ramsey-04\" or \"ramsey-07\", \
             fitted by the A3000/A4000 profiles)"
        );
    }
    if !cpu_has_32bit_bus(cpu) {
        bail!(
            "motherboard RAM ends at $08000000, beyond a 24-bit address bus: \
             {:?} cannot reach it (needs a 68020/68030/68040/68060)",
            cpu
        );
    }
    if mb > 4 * BANK_4M {
        if mem_controller.ramsey_revision() != Some(crate::ramsey::RamseyRevision::Rev7) {
            bail!(
                "motherboard RAM beyond 16M fills the $04000000-$06FFFFFF \
                 expansion space, an A4000 option (needs \
                 [machine] mem_controller = \"ramsey-07\")"
            );
        }
        if !mb.is_multiple_of(BANK_4M) || mb > crate::memory::MB_RAM_MAX {
            bail!(
                "motherboard RAM {} bytes does not fill the expansion space \
                 in whole 4M banks (20M-64M in 4M steps)",
                mb
            );
        }
        return Ok(());
    }
    let on_1m_banks = mb.is_multiple_of(BANK_1M) && mb <= 4 * BANK_1M;
    let on_4m_banks = mb.is_multiple_of(BANK_4M) && mb <= 4 * BANK_4M;
    if !(on_1m_banks || on_4m_banks) {
        bail!(
            "motherboard RAM {} bytes does not fill Ramsey banks \
             (1M-4M in 1M steps, or 8M, 12M, 16M; the A4000 extends \
             in 4M steps to 64M)",
            mb
        );
    }
    Ok(())
}

/// CPU-slot (accelerator) fast RAM occupies $08000000-$0FFFFFFF, which only
/// a 32-bit address bus reaches. The bank is whatever DRAM the CPU board
/// carries, so any whole number of megabytes up to the 128M slot space fits.
fn validate_accel_ram(accel: usize, cpu: CpuModel) -> Result<()> {
    const MB: usize = 1024 * 1024;
    if accel == 0 {
        return Ok(());
    }
    if !cpu_has_32bit_bus(cpu) {
        bail!(
            "accelerator RAM sits at $08000000-$0FFFFFFF, beyond a 24-bit \
             address bus: {:?} cannot reach it (needs a 68020/68030/68040/68060)",
            cpu
        );
    }
    if !accel.is_multiple_of(MB) || accel > crate::memory::ACCEL_RAM_MAX {
        bail!(
            "accelerator RAM {} bytes is not a whole number of megabytes \
             up to the 128M CPU-slot space",
            accel
        );
    }
    Ok(())
}

fn cpu_has_32bit_bus(cpu: CpuModel) -> bool {
    matches!(
        cpu,
        CpuModel::M68020 | CpuModel::M68030 | CpuModel::M68040 | CpuModel::M68060
    )
}

fn validate_rtg_card(rtg: RtgCard, vram_bytes: usize, cpu: CpuModel) -> Result<()> {
    if rtg == RtgCard::Z3660 && !cpu_has_32bit_bus(cpu) {
        bail!(
            "[rtg] card = \"z3660\" is a Zorro III board and needs a CPU \
             with a 32-bit address bus (68020/68030/68040/68060); {:?} has \
             a 24-bit bus",
            cpu
        );
    }
    if rtg == RtgCard::GraffityZ3 && !cpu_has_32bit_bus(cpu) {
        bail!(
            "[rtg] card = \"graffityz3\" is a Zorro III board and needs a CPU \
             with a 32-bit address bus (68020/68030/68040/68060); {:?} has \
             a 24-bit bus",
            cpu
        );
    }
    if matches!(
        rtg,
        RtgCard::Picasso2 | RtgCard::Picasso2Plus | RtgCard::GraffityZ2 | RtgCard::GraffityZ3
    ) && !matches!(vram_bytes, 0x10_0000 | 0x20_0000)
    {
        bail!(
            "[rtg] vram for Picasso II and Graffity cards must be \"1M\" or \"2M\", got {} bytes",
            vram_bytes
        );
    }
    Ok(())
}

fn validate_z3_ram(z3: usize, cpu: CpuModel) -> Result<()> {
    if z3 == 0 {
        return Ok(());
    }
    if !cpu_has_32bit_bus(cpu) {
        bail!(
            "Zorro III RAM needs a CPU with a 32-bit address bus \
             (68020/68030/68040); {:?} has a 24-bit bus",
            cpu
        );
    }
    if zorro_iii_size_bits(z3).is_none() {
        bail!(
            "Zorro III RAM {} bytes is not an autoconfigurable size \
             (a power of two from 64K to 1G)",
            z3
        );
    }
    Ok(())
}

fn validate_slow_ram(slow: usize) -> Result<()> {
    const SLOW_LIMIT: usize = 512 * 1024;
    if slow > SLOW_LIMIT {
        bail!(
            "slow RAM {} bytes exceeds A500 trapdoor/fake-fast maximum of {} bytes",
            slow,
            SLOW_LIMIT
        );
    }
    Ok(())
}

fn parse_floppy(
    raw: RawFloppy,
    default_connected: [bool; 4],
) -> Result<(FloppyConfig, [bool; 4], [Vec<PathBuf>; 4])> {
    let connected_count = match raw.drives {
        None => None,
        Some(n @ 0..=4) => Some(usize::from(n)),
        Some(n) => bail!("[floppy] drives must be between 0 and 4, got {n}"),
    };
    let speed = match raw.speed {
        None => 100,
        Some(s)
            if s == crate::floppy::SPEED_TURBO
                || crate::floppy::SUPPORTED_SPEED_PERCENTS.contains(&s) =>
        {
            s
        }
        Some(s) => bail!("[floppy] speed must be 100, 200, 400, 800, or 0 (turbo), got {s}"),
    };
    let raws = [raw.df0, raw.df1, raw.df2, raw.df3];
    let mut drives: [Option<FloppyDriveConfig>; 4] = std::array::from_fn(|_| None);
    let mut connected = match connected_count {
        Some(count) => std::array::from_fn(|idx| idx < count),
        None => default_connected,
    };
    let mut playlists: [Vec<PathBuf>; 4] = std::array::from_fn(|_| Vec::new());
    #[cfg_attr(not(feature = "fluxbridge"), allow(unused_mut))]
    let mut bridges: [Option<FluxBridgeConfig>; 4] = std::array::from_fn(|_| None);
    for (idx, raw_drive) in raws.into_iter().enumerate() {
        let Some(raw_drive) = raw_drive else {
            continue;
        };

        // A physical drive takes the bay instead of an image. A build without
        // the feature has no way to drive one, so the keys are read and
        // ignored rather than rejected: a config file shared between builds
        // stays valid, it just does nothing here.
        #[cfg(not(feature = "fluxbridge"))]
        let _ = &raw_drive.bridge;
        #[cfg(feature = "fluxbridge")]
        if let Some(spec) = raw_drive.bridge.as_deref() {
            let spec = spec.trim();
            if !spec.eq_ignore_ascii_case("off") && !spec.is_empty() {
                if raw_drive.path.is_some() || raw_drive.paths.is_some() {
                    bail!(
                        "floppy.df{idx} has both a bridge and a disk image; a physical drive \
                         supplies its own media, so give one or the other"
                    );
                }
                bridges[idx] = Some(parse_floppy_bridge(idx, spec, &raw_drive)?);
                if let Some(count) = connected_count {
                    if !connected[idx] {
                        bail!(
                            "[floppy] drives = {count} leaves floppy.df{idx} disconnected, \
                             but floppy.df{idx} has a bridge configured"
                        );
                    }
                } else {
                    connected[idx] = true;
                }
                continue;
            }
        }
        // Combine `path` (single) and `paths` (playlist) into one ordered
        // list, with `path` first when both are present.
        let mut raw_images: Vec<String> = Vec::new();
        if let Some(path) = raw_drive.path {
            raw_images.push(path);
        }
        if let Some(paths) = raw_drive.paths {
            raw_images.extend(paths);
        }
        let has_images = !raw_images.is_empty();
        let enabled = raw_drive.enabled.unwrap_or(has_images);
        if !enabled {
            continue;
        }
        if let Some(count) = connected_count {
            if !connected[idx] {
                bail!(
                    "[floppy] drives = {} leaves floppy.df{} disconnected, \
                     but floppy.df{} has media configured",
                    count,
                    idx,
                    idx
                );
            }
        } else {
            connected[idx] = true;
        }
        if !has_images {
            bail!("floppy.df{} is enabled but has no path", idx);
        }
        let mut images = Vec::with_capacity(raw_images.len());
        for image in raw_images {
            if image.trim().is_empty() {
                bail!("floppy.df{} path is empty", idx);
            }
            let image = PathBuf::from(image);
            validate_floppy_image_path(idx, &image)?;
            images.push(image);
        }
        drives[idx] = Some(FloppyDriveConfig {
            path: images[0].clone(),
            write_protected: raw_drive.write_protected.unwrap_or(true),
        });
        playlists[idx] = images;
    }
    Ok((
        FloppyConfig {
            drives,
            bridges,
            speed,
        },
        connected,
        playlists,
    ))
}

/// The interface names this build actually accepts, from the library's own
/// driver table -- so help text, error messages and validation cannot drift
/// from what can really be opened.
#[cfg(feature = "fluxbridge")]
pub fn supported_bridge_drivers() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = crate::fluxbridge::drivers()
        .into_iter()
        .map(|driver| driver.token)
        .collect();
    names.push("off");
    names
}

/// Parse one bay's `bridge = ...` plus its `bridge_*` settings.
#[cfg(feature = "fluxbridge")]
fn parse_floppy_bridge(idx: usize, spec: &str, raw: &RawFloppyDrive) -> Result<FluxBridgeConfig> {
    let driver = match spec
        .to_ascii_lowercase()
        .replace([' ', '-', '_'], "")
        .as_str()
    {
        "drawbridge" | "arduino" => BridgeDriver::DrawBridge,
        "greaseweazle" | "gw" => BridgeDriver::Greaseweazle,
        "supercardpro" | "scp" => BridgeDriver::SupercardPro,
        _ => bail!(
            "floppy.df{idx} bridge = \"{spec}\" is not a known interface ({})",
            supported_bridge_drivers().join(", ")
        ),
    };
    // Recognising a name is not the same as having its driver. FluxBridge is
    // compiled with the drivers Copperline supports, so a configuration
    // naming one this build does not carry is refused here, where the name is
    // read, rather than surviving validation only to fail when the drive is
    // opened.
    if crate::fluxbridge::driver_named(driver.match_token()).is_none() {
        bail!(
            "floppy.df{idx} bridge = \"{spec}\": this build has no {} driver (it has {})",
            driver.label(),
            supported_bridge_drivers().join(", ")
        );
    }

    let mode = match raw.bridge_mode.as_deref().map(str::trim) {
        None => BridgeReadMode::default(),
        Some(s) if s.eq_ignore_ascii_case("normal") => BridgeReadMode::Normal,
        Some(s) if s.eq_ignore_ascii_case("compatible") => BridgeReadMode::Compatible,
        Some(s) if s.eq_ignore_ascii_case("stalling") => BridgeReadMode::Stalling,
        // "turbo" is not a read mode: it intercepts AmigaDOS calls rather
        // than reading the disk, which is not something an emulator modelling
        // the hardware can use. Refused by name rather than quietly
        // substituted, so a config carried over from another emulator says
        // what happened.
        Some(s) if s.eq_ignore_ascii_case("turbo") => bail!(
            "floppy.df{idx} bridge_mode = \"{s}\" answers AmigaDOS calls instead of \
             reading the disk, which Copperline has no use for: it models the drive. \
             Use normal, compatible or stalling."
        ),
        Some(s) => bail!("floppy.df{idx} unknown bridge_mode {s:?}"),
    };
    let density = match raw.bridge_density.as_deref().map(str::trim) {
        None => BridgeDensity::Auto,
        Some(s) if s.eq_ignore_ascii_case("auto") => BridgeDensity::Auto,
        Some(s) if s.eq_ignore_ascii_case("dd") => BridgeDensity::Dd,
        Some(s) if s.eq_ignore_ascii_case("hd") => BridgeDensity::Hd,
        Some(s) => bail!("floppy.df{idx} bridge_density = \"{s}\" is not one of auto, dd, hd"),
    };
    let cable = match raw.bridge_cable.as_deref().map(str::trim) {
        None => BridgeCable::default(),
        Some(s) if s.eq_ignore_ascii_case("a") => BridgeCable::DriveA,
        Some(s) if s.eq_ignore_ascii_case("b") => BridgeCable::DriveB,
        Some("0") => BridgeCable::Shugart0,
        Some("1") => BridgeCable::Shugart1,
        Some("2") => BridgeCable::Shugart2,
        Some("3") => BridgeCable::Shugart3,
        Some(s) => bail!("floppy.df{idx} bridge_cable = \"{s}\" is not one of a, b, 0, 1, 2, 3"),
    };
    let port = raw
        .bridge_port
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Replays of captured revolutions run at real speed or at double it; a
    // track's first read always streams at the platter's own pace, so finer
    // steps between the two had nothing distinct to mean.
    let speed = match &raw.bridge_speed {
        None => DEFAULT_BRIDGE_SPEED_PERCENT,
        Some(RawReplaySpeed::Word(word)) if word.eq_ignore_ascii_case("normal") => 100,
        Some(RawReplaySpeed::Word(word)) if word.eq_ignore_ascii_case("fast") => 200,
        Some(RawReplaySpeed::Percent(100)) => 100,
        Some(RawReplaySpeed::Percent(200)) => 200,
        Some(other) => bail!(
            "floppy.df{idx} replay_speed = {other:?} is not a replay speed: \
             \"normal\" (real speed) or \"fast\" (double)"
        ),
    };

    Ok(FluxBridgeConfig {
        driver,
        write_protected: raw.write_protected.unwrap_or(true),
        port,
        mode,
        density,
        cable,
        speed,
    })
}

/// Parse `[[host_disk]]` entries and check they can all be honoured.
///
/// Two things are refused here rather than at the point of use. A slot holds
/// one thing, so two host disks cannot share an attachment point, and a slot
/// already given an image cannot also be given a disk -- the same rule a
/// floppy bay follows for a bridge and an image, and for the same reason:
/// silently preferring one would leave the other quietly ignored.
///
/// Whether the disk is actually *there* is deliberately not checked. A
/// configuration is written once and used on a machine whose card reader may
/// be empty, so a missing disk is a condition to report when the machine is
/// built, not a reason to refuse to read the file.
pub(super) fn parse_host_disks(
    raw: &[RawHostDisk],
    ide: &IdeConfig,
    scsi: &ScsiConfig,
    lide: &LideConfig,
    has_ide_port: bool,
    has_scsi: bool,
) -> Result<Vec<HostDiskConfig>> {
    let mut disks: Vec<HostDiskConfig> = Vec::new();
    for (index, entry) in raw.iter().enumerate() {
        let device = entry.device.trim();
        if device.is_empty() {
            bail!("host_disk[{index}] has no device; name one as --list-disks prints it");
        }
        let attach = match entry.attach.as_deref().map(str::trim) {
            None => HostDiskAttach::default(),
            Some(token) => HostDiskAttach::from_token(token).ok_or_else(|| {
                anyhow!(
                    "host_disk[{index}] attach = \"{token}\" is not an attachment point ({})",
                    HostDiskAttach::all()
                        .iter()
                        .map(|a| a.token())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?,
        };
        // A disk attached where the machine has no port is not a disk at
        // all: nothing would ever look at it, so say so now rather than
        // opening it and leaving it unreachable.
        let fitted = match attach {
            HostDiskAttach::IdeMaster | HostDiskAttach::IdeSlave => has_ide_port,
            HostDiskAttach::Scsi(_) => has_scsi,
            HostDiskAttach::LideMaster(ch) | HostDiskAttach::LideSlave(ch) => {
                lide.enabled() && usize::from(ch) < lide.board.channels()
            }
        };
        if !fitted {
            bail!("host_disk[{index}]: {}", attach.requirement());
        }
        // The same medium on two buses is not two drives: the guest mounts
        // and writes the one disk through both, each unaware of the other's
        // cached blocks, which is how a filesystem is destroyed by a machine
        // that never did anything wrong.
        let repeats_identity = disks.iter().any(|disk| {
            disk.device == device
                || entry
                    .fingerprint
                    .as_deref()
                    .filter(|fingerprint| !fingerprint.is_empty())
                    .zip(disk.fingerprint.as_deref())
                    .is_some_and(|(new, earlier)| new == earlier)
        });
        if repeats_identity {
            bail!(
                "host_disk[{index}] names {device}, which is already attached to this \
                 machine; one disk cannot be two drives"
            );
        }
        if let Some(clash) = disks.iter().find(|d| d.attach == attach) {
            bail!(
                "host_disk[{index}] and {} are both attached to {}; a slot holds one disk",
                clash.device,
                attach.label()
            );
        }
        let taken = match attach {
            HostDiskAttach::IdeMaster => ide.master.is_some(),
            HostDiskAttach::IdeSlave => ide.slave.is_some(),
            HostDiskAttach::Scsi(unit) => scsi
                .units
                .get(usize::from(unit))
                .is_some_and(Option::is_some),
            HostDiskAttach::LideMaster(ch) => lide.drives[usize::from(ch) * 2].is_some(),
            HostDiskAttach::LideSlave(ch) => lide.drives[usize::from(ch) * 2 + 1].is_some(),
        };
        if taken {
            bail!(
                "host_disk[{index}] is attached to {}, which already has an image; \
                 a slot holds either a disk or an image",
                attach.label()
            );
        }
        disks.push(HostDiskConfig {
            device: device.to_string(),
            fingerprint: entry.fingerprint.clone(),
            identity_confirmed: entry.identity_confirmed,
            attach,
            writable: !entry.read_only.unwrap_or(true),
        });
    }
    Ok(disks)
}

fn validate_floppy_image_path(idx: usize, path: &Path) -> Result<()> {
    const ADF_SIZE: u64 = 80 * 2 * 11 * 512;
    let meta = std::fs::metadata(path)
        .with_context(|| format!("reading floppy.df{} image {}", idx, path.display()))?;
    if !meta.is_file() {
        bail!("floppy.df{} image {} is not a file", idx, path.display());
    }
    if meta.len() == ADF_SIZE {
        return Ok(());
    }

    let mut sig = [0u8; 8];
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening floppy.df{} image {}", idx, path.display()))?;
    file.read_exact(&mut sig).with_context(|| {
        format!(
            "reading floppy.df{} image signature {}",
            idx,
            path.display()
        )
    })?;
    if sig[..2] == [0x1F, 0x8B]
        || sig[..4] == [0x50, 0x4b, 0x03, 0x04]
        || &sig[..3] == b"SCP"
        || &sig[..4] == b"DMS!"
        || &sig[..4] == b"CAPS"
        || &sig == b"UAE-1ADF"
        || &sig == b"UAE--ADF"
    {
        return Ok(());
    }

    bail!(
        "floppy.df{} image {} is {} bytes, expected {} bytes (standard DD ADF),
        gzip-compressed supported image, UAE extended ADF, IPF, SCP, DMS or single file ZIP",
        idx,
        path.display(),
        meta.len(),
        ADF_SIZE
    );
}
