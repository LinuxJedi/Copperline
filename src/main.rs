// SPDX-License-Identifier: GPL-3.0-or-later

//! Copperline: Amiga emulator.
//!
//! Usage: copperline [--config FILE] [ROM]
//!   If no --config is given, looks for ./copperline.toml.
//!   If no ROM is given (neither argument nor `rom =` in the config), boots
//!   the bundled AROS open-source Kickstart replacement (see src/romsearch.rs).

use anyhow::{anyhow, Result};
#[cfg(feature = "gdb")]
use copperline::gdbstub;
use copperline::{config, crashlog, debugger, emulator, envcfg, gamepad, priority, video};
use log::{info, warn};
use std::path::Path;
// Only the Linux net-helper setup and the Windows disk broker build
// PathBuf values; other targets would carry an unused-import warning.
#[cfg(any(windows, target_os = "linux"))]
use std::path::PathBuf;
use std::time::{Duration, Instant};

use copperline::audio::{AudioSink, CpalSink, NullSink, WavSink};
use copperline::bus::Bus;
use copperline::chipset::paula::{Paula, DMACON_DMAEN, PAULA_CLOCK_HZ};
use copperline::config::{Chipset, Config, ConfigOverrides};
use copperline::emulator::Emulator;
use copperline::floppy::FloppyController;
use copperline::memory::Memory;
use copperline::serial::StdoutSink;
use copperline::video::window::{App, DiskInsertSpec};
use copperline::video::HOST_SHORTCUT_MODIFIER_LABEL;

mod cli;

#[cfg(test)]
use cli::parse_args_from;
use cli::{parse_args, CliArgs, CliDiskInsert};

fn resolve_disk_insert_after(
    cfg: &mut Config,
    disk_insert_after: Vec<CliDiskInsert>,
) -> Result<Vec<DiskInsertSpec>> {
    let mut out = Vec::new();
    for insert in disk_insert_after {
        match insert {
            CliDiskInsert::Explicit(spec) => {
                if !cfg.floppy_connected[spec.drive_idx] {
                    return Err(anyhow!(
                        "--insert-disk-after df{} needs a connected drive; \
                         use --floppy-drives {} or configure floppy.df{}",
                        spec.drive_idx,
                        spec.drive_idx + 1,
                        spec.drive_idx
                    ));
                }
                out.push(spec);
            }
            CliDiskInsert::Configured { secs, drive_idx } => {
                let Some(drive) = cfg.floppy.drives[drive_idx].take() else {
                    return Err(anyhow!(
                        "--defer-disk-insert df{} requires configured floppy.df{}",
                        drive_idx,
                        drive_idx
                    ));
                };
                out.push(DiskInsertSpec {
                    secs,
                    drive_idx,
                    path: drive.path,
                    write_protected: drive.write_protected,
                });
            }
        }
    }
    Ok(out)
}

fn validate_benchmark_args(cli: &CliArgs) -> Result<()> {
    if cli.benchmark_until.is_none() {
        return Ok(());
    }

    if !cli.screenshot_after.is_empty() {
        return Err(anyhow!(
            "--benchmark-until cannot be combined with --screenshot-after"
        ));
    }
    if !cli.save_state_after.is_empty() {
        return Err(anyhow!(
            "--benchmark-until cannot be combined with --save-state-after"
        ));
    }
    if cli.frame_dump.is_some() {
        return Err(anyhow!(
            "--benchmark-until cannot be combined with --dump-frames"
        ));
    }
    if cli.live_audio_profile_secs.is_some() {
        return Err(anyhow!(
            "--benchmark-until cannot be combined with --profile-live-audio"
        ));
    }
    if !cli.press_after.is_empty()
        || !cli.click_after.is_empty()
        || !cli.joy_after.is_empty()
        || !cli.mouse_after.is_empty()
        || !cli.mouse_to_after.is_empty()
        || !cli.pot_after.is_empty()
        || !cli.freeze_after.is_empty()
    {
        return Err(anyhow!(
            "--benchmark-until cannot be combined with scheduled input events"
        ));
    }
    if cli.record_input.is_some() {
        return Err(anyhow!(
            "--benchmark-until cannot be combined with --record-input"
        ));
    }
    if !cli.disk_insert_after.is_empty() {
        return Err(anyhow!(
            "--benchmark-until cannot be combined with scheduled disk inserts"
        ));
    }

    Ok(())
}

fn validate_run_args(cli: &CliArgs) -> Result<()> {
    if (cli.run_args.is_some() || cli.run_stack.is_some() || cli.run_detach) && cli.run.is_none() {
        return Err(anyhow!(
            "--run-args, --run-stack, and --run-detach need --run"
        ));
    }
    if cli.run.is_some() && cli.whdload.is_some() {
        return Err(anyhow!(
            "--run and --whdload are mutually exclusive: each stages its own boot volume"
        ));
    }
    Ok(())
}

fn validate_gdb_args(cli: &CliArgs) -> Result<()> {
    #[cfg(not(feature = "gdb"))]
    if cli.gdb.is_some() || cli.gdb_gui.is_some() {
        return Err(anyhow!(
            "this build was compiled without the gdb feature; \
             rebuild with --features gdb for --gdb/--gdb-gui"
        ));
    }
    if cli.gdb_dialect.is_some() && cli.gdb.is_none() && cli.gdb_gui.is_none() {
        return Err(anyhow!("--gdb-dialect requires --gdb or --gdb-gui"));
    }
    if cli.gdb.is_some() && cli.gdb_gui.is_some() {
        return Err(anyhow!("--gdb and --gdb-gui cannot be combined"));
    }
    if cli.gdb_gui.is_some() && cli.benchmark_until.is_some() {
        return Err(anyhow!(
            "--gdb-gui cannot be combined with --benchmark-until"
        ));
    }
    if cli.gdb.is_none() {
        return Ok(());
    }

    if cli.benchmark_until.is_some() {
        return Err(anyhow!("--gdb cannot be combined with --benchmark-until"));
    }
    if !cli.screenshot_after.is_empty() {
        return Err(anyhow!("--gdb cannot be combined with --screenshot-after"));
    }
    if !cli.save_state_after.is_empty() {
        return Err(anyhow!("--gdb cannot be combined with --save-state-after"));
    }
    if cli.frame_dump.is_some() {
        return Err(anyhow!("--gdb cannot be combined with --dump-frames"));
    }
    if cli.live_audio_profile_secs.is_some() {
        return Err(anyhow!(
            "--gdb cannot be combined with --profile-live-audio"
        ));
    }
    if !cli.press_after.is_empty()
        || !cli.click_after.is_empty()
        || !cli.joy_after.is_empty()
        || !cli.mouse_after.is_empty()
        || !cli.mouse_to_after.is_empty()
        || !cli.pot_after.is_empty()
        || !cli.freeze_after.is_empty()
    {
        return Err(anyhow!(
            "--gdb cannot be combined with scheduled input events"
        ));
    }
    if cli.record_input.is_some() {
        return Err(anyhow!("--gdb cannot be combined with --record-input"));
    }
    if !cli.disk_insert_after.is_empty() {
        return Err(anyhow!(
            "--gdb cannot be combined with scheduled disk inserts"
        ));
    }
    Ok(())
}

fn validate_control_args(cli: &CliArgs) -> Result<()> {
    #[cfg(not(feature = "control"))]
    if cli.control.is_some() || cli.control_gui.is_some() {
        return Err(anyhow!(
            "this build was compiled without the control feature; \
             rebuild with --features control for --control/--control-gui"
        ));
    }
    if (cli.control.is_some() || cli.control_gui.is_some()) && cli.benchmark_until.is_some() {
        return Err(anyhow!(
            "--control/--control-gui cannot be combined with --benchmark-until"
        ));
    }
    // The headless servers each own the machine's run loop, so --control
    // and --gdb exclude everything else. The two windowed forms share one
    // interactive session: --control-gui with --gdb-gui is allowed.
    if cli.control.is_some() && (cli.gdb.is_some() || cli.gdb_gui.is_some()) {
        return Err(anyhow!("--control cannot be combined with --gdb/--gdb-gui"));
    }
    if cli.control_gui.is_some() && cli.gdb.is_some() {
        return Err(anyhow!("--control-gui cannot be combined with --gdb"));
    }
    if cli.control.is_none() {
        return Ok(());
    }
    // The headless server owns the machine like --gdb does; the windowed
    // App (which fires the scheduled/capture flags) never runs. Input
    // recording IS supported: the server journals injected input itself.
    if !cli.screenshot_after.is_empty() {
        return Err(anyhow!(
            "--control cannot be combined with --screenshot-after (use capture.screenshot)"
        ));
    }
    if !cli.save_state_after.is_empty() {
        return Err(anyhow!(
            "--control cannot be combined with --save-state-after (use state.save)"
        ));
    }
    if cli.frame_dump.is_some() {
        return Err(anyhow!("--control cannot be combined with --dump-frames"));
    }
    if cli.live_audio_profile_secs.is_some() {
        return Err(anyhow!(
            "--control cannot be combined with --profile-live-audio"
        ));
    }
    if !cli.press_after.is_empty()
        || !cli.click_after.is_empty()
        || !cli.joy_after.is_empty()
        || !cli.mouse_after.is_empty()
        || !cli.mouse_to_after.is_empty()
        || !cli.pot_after.is_empty()
        || !cli.freeze_after.is_empty()
    {
        return Err(anyhow!(
            "--control cannot be combined with scheduled input events (use input.* \
             and cartridge.freeze)"
        ));
    }
    if !cli.disk_insert_after.is_empty() {
        return Err(anyhow!(
            "--control cannot be combined with scheduled disk inserts (use media.*)"
        ));
    }
    Ok(())
}

fn run_headless_benchmark(mut emu: Emulator, target_secs: f32) -> Result<()> {
    emu.set_paced(false);
    emu.reset_stats();

    let start_emulated = emu.bus().emulated_seconds();
    let target_secs = f64::from(target_secs);
    if target_secs <= start_emulated {
        return Err(anyhow!(
            "--benchmark-until target {:.3}s is not after current emulated time {:.3}s",
            target_secs,
            start_emulated
        ));
    }

    let start_frames = emu.bus().emulated_frames();
    let started = Instant::now();
    let mut frame_times: Vec<f64> = Vec::new();
    while emu.bus().emulated_seconds() < target_secs {
        let frame_started = Instant::now();
        emu.step_frame()?;
        frame_times.push(frame_started.elapsed().as_secs_f64() * 1_000.0);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let frames = emu.bus().emulated_frames().saturating_sub(start_frames);
    let emulated = emu.bus().emulated_seconds() - start_emulated;
    info!(
        "benchmark: ran {:.3}s emulated to {:.3}s target in {:.3}s wall, {} frames ({:.1}/s)",
        emulated,
        target_secs,
        elapsed,
        frames,
        frames as f64 / elapsed.max(f64::EPSILON)
    );
    report_benchmark_frame_times(start_frames, &frame_times);
    emu.report_stats();
    // Evaluate an untargeted reverse watchpoint at the benchmark's end.
    emu.tt_finalize_reverse_watch()?;
    Ok(())
}

/// A frame slower than this stalls the audio ring on a PAL host (50 Hz = 20 ms
/// per frame, minus headroom for the window render path).
const BENCH_FRAME_BUDGET_MS: f64 = 20.0;

/// Summarize the per-frame wall times of a `--benchmark-until` run: the
/// distribution, and every frame that individually blew the audio budget.
/// Averages hide these spikes, and a single late frame is an audible underrun.
fn report_benchmark_frame_times(start_frame: u64, frame_times: &[f64]) {
    if frame_times.is_empty() {
        return;
    }
    let mut sorted: Vec<f64> = frame_times.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| sorted[((sorted.len() - 1) as f64 * p) as usize];
    info!(
        "benchmark frame times: p50={:.2}ms p90={:.2}ms p99={:.2}ms max={:.2}ms",
        pct(0.50),
        pct(0.90),
        pct(0.99),
        sorted[sorted.len() - 1]
    );
    let over: Vec<(usize, f64)> = frame_times
        .iter()
        .copied()
        .enumerate()
        .filter(|&(_, ms)| ms > BENCH_FRAME_BUDGET_MS)
        .collect();
    if over.is_empty() {
        info!(
            "benchmark frame times: all {} frames within the {:.0}ms budget",
            frame_times.len(),
            BENCH_FRAME_BUDGET_MS
        );
        return;
    }
    info!(
        "benchmark frame times: {} of {} frames over the {:.0}ms budget:",
        over.len(),
        frame_times.len(),
        BENCH_FRAME_BUDGET_MS
    );
    for (idx, ms) in over.iter().take(50) {
        info!("  frame {} ({:.2}ms)", start_frame + *idx as u64, ms);
    }
    if over.len() > 50 {
        info!("  ... and {} more", over.len() - 50);
    }
}

/// Whether to open a live audio sink. `[audio] output_enabled = false` (the GUI
/// "Disabled" option) silences default-on audio, but an explicit `--audio`
/// (`forced_on`) overrides it. `--noaudio` (which clears `audio_live`) and
/// `--audio-wav` still win; those are handled by the caller.
fn live_audio_enabled(audio_live: bool, forced_on: bool, config_enabled: bool) -> bool {
    audio_live && (forced_on || config_enabled)
}

/// The `--audio-stems` source set for this run: Paula (always present, with
/// its four physical channels) and drive sounds (always registered --
/// `[audio] floppy_sounds = false` just means the stem is silent, like a
/// disabled DriveSounds today), plus CD-DA, MT-32, Toccata, and MHI only
/// when this run's config plausibly produces them. A source absent here
/// never gets a stem file at all, even if `--audio-stems-mode` includes
/// `source`/`channel`.
///
/// The CD/MT-32/Toccata/MHI checks are a heuristic, not a perfect "will this
/// run ever make sound" oracle (e.g. a CD swapped into an empty drive
/// mid-run by `--insert-cd-after` or the control protocol is missed) --
/// see docs/internals/audio.md for the exact rule and its limits.
///
/// `state_loaded` must be true when this run passes `--load-state`: the
/// restored machine can describe entirely different hardware than `cfg`
/// (a state's own descriptor can disagree with the config that started
/// this process, and the host reconfigures to match it -- see
/// `Emulator::adopt_loaded_state`), so `cfg` alone cannot say whether the
/// resumed machine has a CD drive, an MT-32, or a Toccata board. Rather
/// than risk silently missing an active source, a state load
/// conservatively registers all three regardless of what `cfg` says.
fn configured_audio_stem_sources(
    cfg: &config::Config,
    state_loaded: bool,
) -> Vec<copperline::audio::mux::SourceSpec> {
    use copperline::audio::mux::SourceSpec;
    let mut sources = vec![
        SourceSpec {
            id: "paula",
            channel_names: &["0", "1", "2", "3"],
        },
        SourceSpec {
            id: "drivesounds",
            channel_names: &[],
        },
    ];
    // A CD image on an [ide]/[lide]/[scsi] drive slot attaches as an
    // ATAPI/SCSI CD-ROM (open_ide_target/open_scsi_target apply this same
    // path test), and its CD-DA feeds the one CdAudioRing like the
    // CD32/CDTV drive does.
    let unit_has_cd_image = |drive: &Option<config::DriveImage>| {
        drive
            .as_ref()
            .is_some_and(|d| config::is_cd_image_path(&d.path))
    };
    let has_cd = state_loaded
        || matches!(
            cfg.machine,
            Some(config::MachineModel::Cd32) | Some(config::MachineModel::Cdtv)
        )
        || cfg.cd_image_path.is_some()
        || unit_has_cd_image(&cfg.ide.master)
        || unit_has_cd_image(&cfg.ide.slave)
        || cfg.lide.drives.iter().any(unit_has_cd_image)
        || cfg.scsi.units.iter().any(unit_has_cd_image);
    if has_cd {
        sources.push(SourceSpec {
            id: "cdda",
            channel_names: &[],
        });
    }
    // ROMs loaded from the menu outlive their session, so they count as
    // configured here too.
    #[cfg(feature = "mt32")]
    let mt32_roms_present = {
        let (control, pcm) = copperline::mt32::rom_overrides();
        (control.is_some() || cfg.serial.mt32_control_rom.is_some())
            && (pcm.is_some() || cfg.serial.mt32_pcm_rom.is_some())
    };
    #[cfg(not(feature = "mt32"))]
    let mt32_roms_present =
        cfg.serial.mt32_control_rom.is_some() && cfg.serial.mt32_pcm_rom.is_some();
    let has_mt32 = state_loaded
        || (config::midi_out_is_mt32(cfg.serial.midi_out.as_deref()) && mt32_roms_present);
    if has_mt32 {
        sources.push(SourceSpec {
            id: "mt32",
            channel_names: &[],
        });
    }
    // Coppersynth's frames arrive under their own name, so its stem
    // registers on the same terms -- it just needs no ROMs to count.
    let has_coppersynth = state_loaded
        || (cfg!(feature = "coppersynth")
            && config::midi_out_is_csynth(cfg.serial.midi_out.as_deref()));
    if has_coppersynth {
        sources.push(SourceSpec {
            id: "coppersynth",
            channel_names: &[],
        });
    }
    if state_loaded || cfg.toccata {
        sources.push(SourceSpec {
            id: "toccata",
            channel_names: &[],
        });
    }
    if state_loaded || cfg.mhi {
        sources.push(SourceSpec {
            id: "mhi",
            channel_names: &[],
        });
    }
    sources
}

/// Print the host audio output devices for `--list-audio-devices`. These are the
/// names `--audio-device` and `[audio] output_device` match against.
fn print_audio_output_devices() -> Result<()> {
    println!("Audio output devices (for --audio-device / [audio] output_device):");
    let devices = copperline::audio::list_output_devices();
    if devices.is_empty() {
        println!("  (none found)");
    }
    for name in devices {
        println!("  {name}");
    }
    Ok(())
}

/// List the host's own disks, for `--host-disk`.
///
/// Enumeration opens nothing and needs no privileges, so this is safe to run
/// at any time; the disk the host is running from is named but marked, since
/// hiding it silently would just look like a missing device.
#[cfg(not(target_arch = "wasm32"))]
fn print_host_disks() -> Result<()> {
    println!("Host disks (name one to --host-disk, or as [[host_disk]] device):");
    let devices = copperline::blockdev::list_devices()?;
    if devices.is_empty() {
        println!("  (none found)");
    }
    for device in devices {
        let usable = if device.safety.openable() {
            ""
        } else {
            "  -- cannot be used"
        };
        println!("  {:<10} {}{usable}", device.id, device.label());
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn print_host_disks() -> Result<()> {
    println!("Host disks: not available in this build");
    Ok(())
}

fn print_net_interfaces() -> Result<()> {
    #[cfg(all(feature = "net-bridge", not(target_arch = "wasm32")))]
    {
        println!("Network interfaces (for --a2065-interface / [a2065] interface):");
        let interfaces = copperline::net::bridge::list_interfaces()?;
        if interfaces.is_empty() {
            println!("  (none found)");
        }
        for interface in interfaces {
            let mut state = Vec::new();
            if interface.up {
                state.push("up");
            } else {
                state.push("down");
            }
            if interface.running {
                state.push("running");
            }
            if interface.loopback {
                state.push("loopback");
            }
            if interface.wireless {
                state.push("wireless; bridging is best-effort");
            }
            println!("  {}\t[{}]", interface.label(), state.join(", "));
        }
        Ok(())
    }
    #[cfg(not(all(feature = "net-bridge", not(target_arch = "wasm32"))))]
    {
        anyhow::bail!("this build has no native bridged-networking support")
    }
}

fn run_net_helper_setup(action: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("FLATPAK_ID").is_some() {
            if action == "status" {
                #[cfg(feature = "net-bridge")]
                {
                    let socket = copperline::net::bridge::linux::helper_socket_path()?;
                    if socket.exists() {
                        println!("host helper socket is visible at {}", socket.display());
                        return Ok(());
                    }
                    anyhow::bail!(
                        "host helper socket is not visible at {}; install and enable \
                         the Linux network-helper companion on the host",
                        socket.display()
                    );
                }
                #[cfg(not(feature = "net-bridge"))]
                anyhow::bail!("this build has no bridged-networking support");
            }
            anyhow::bail!(
                "the Flatpak cannot install a host capability binary from \
                 inside its sandbox; download the Copperline Linux network-helper \
                 companion archive, then run its copperline-net-helper-setup {action}"
            );
        }
        let executable = std::env::current_exe()?;
        let mut candidates = vec![
            executable.with_file_name("copperline-net-helper-setup"),
            executable
                .parent()
                .and_then(Path::parent)
                .map(|prefix| {
                    prefix
                        .join("libexec")
                        .join("copperline")
                        .join("copperline-net-helper-setup")
                })
                .unwrap_or_default(),
            PathBuf::from("packaging/linux/copperline-net-helper-setup"),
        ];
        if let Some(appdir) = std::env::var_os("APPDIR") {
            candidates.insert(
                0,
                PathBuf::from(appdir).join("usr/libexec/copperline/copperline-net-helper-setup"),
            );
        }
        let setup = candidates
            .into_iter()
            .find(|path| path.is_file())
            .ok_or_else(|| {
                anyhow!(
                    "copperline-net-helper-setup was not found; use the Linux \
                     network-helper companion archive from the Copperline release"
                )
            })?;
        let status = std::process::Command::new(&setup).arg(action).status()?;
        if !status.success() {
            anyhow::bail!("{} {action} failed with {status}", setup.display());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = action;
        anyhow::bail!("the capability helper is only used for bridged networking on Linux")
    }
}

/// Print the host audio input devices for `--sampler-list-audio-inputs`. These
/// are the names `--sampler-audio-input` and `[parallel] sampler_input` match
/// against.
fn print_sampler_input_devices() -> Result<()> {
    println!("Audio input devices (for --sampler-audio-input / [parallel] sampler_input):");
    let devices = copperline::sampler::list_input_devices();
    if devices.is_empty() {
        println!("  (none found)");
    }
    for name in devices {
        println!("  {name}");
    }
    Ok(())
}

/// Print the host MIDI endpoints for `--list-midi`. This is how a user finds the
/// names `--midi-out`/`--midi-in` and `[serial]` expect. Without the `midi`
/// feature it says how to get MIDI support rather than printing nothing.
#[cfg(feature = "midi")]
fn list_midi_endpoints() -> Result<()> {
    let endpoints = copperline::midi::enumerate();
    println!("MIDI inputs (sources, for --midi-in):");
    if endpoints.inputs.is_empty() {
        println!("  (none)");
    }
    for e in &endpoints.inputs {
        println!("  {}", e.name);
    }
    println!("MIDI outputs (destinations, for --midi-out):");
    if endpoints.outputs.is_empty() {
        println!("  (none)");
    }
    for e in &endpoints.outputs {
        println!("  {}", e.name);
    }
    Ok(())
}

#[cfg(not(feature = "midi"))]
fn list_midi_endpoints() -> Result<()> {
    println!("This build has no MIDI support; rebuild with --features midi.");
    Ok(())
}

fn main() -> Result<()> {
    let mut log_builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    // Copperline resolves gamepads through gilrs's bundled SDL controller
    // mappings, overridden per-UUID by calibrations from gamepads.toml (see
    // gamepad.rs). For a pad the database misses, gilrs logs "No mapping
    // found for UUID ...; default mapping will be used", but gamepad.rs
    // already prints a clearer calibration prompt for that case. Silence
    // gilrs below error level unless the user has explicitly asked for its
    // logs via RUST_LOG.
    if std::env::var_os("RUST_LOG").is_none() {
        log_builder.filter_module("gilrs", log::LevelFilter::Error);
        // The Cranelift JIT backend logs every compiled trace's full IR
        // listing at info level; with `[cpu] jit` that floods the log.
        // RUST_LOG still opts back in for JIT debugging.
        log_builder.filter_module("cranelift_jit", log::LevelFilter::Warn);
        log_builder.filter_module("cranelift_codegen", log::LevelFilter::Warn);
        // iroh instruments every datagram send with an info-level span.
        // Its log bridge also emits span records under tracing::span, so
        // filter both targets to keep routine netplay quiet. Keep our own
        // connection/route summaries and all transport warnings visible.
        let netplay_level = if copperline::envcfg::flag("COPPERLINE_NETPLAY_DEBUG") {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Warn
        };
        for target in ["iroh", "noq", "netwatch", "tracing::span"] {
            log_builder.filter_module(target, netplay_level);
        }
    }
    log_builder.init();

    crashlog::install();

    let cli = parse_args()?;
    if cli.load_uss.is_some()
        && (cli.load_state.is_some() || cli.run.is_some() || cli.whdload.is_some())
    {
        return Err(anyhow!(
            "--load-uss cannot combine with --load-state, --run or --whdload"
        ));
    }
    validate_benchmark_args(&cli)?;
    validate_run_args(&cli)?;
    validate_gdb_args(&cli)?;
    validate_control_args(&cli)?;
    if cli.calibrate_gamepad {
        return gamepad::run_calibration();
    }
    if cli.list_midi {
        return list_midi_endpoints();
    }
    if cli.list_audio_devices {
        return print_audio_output_devices();
    }
    if cli.list_net_interfaces {
        return print_net_interfaces();
    }
    if cli.list_disks {
        return print_host_disks();
    }
    // Before anything else this process might do: it exists only to open the
    // disks for the Copperline that could not, and it must not open a window,
    // read a configuration, or touch a machine on the way.
    #[cfg(any(windows, target_os = "linux"))]
    if let Some(arguments) = &cli.host_disk_broker {
        // Each host's two halves have their own private arrangement, so the
        // backend reads what its own other half wrote.
        #[cfg(windows)]
        {
            let usage = "the privileged half takes PID REPLY NONCE DEVICE:FINGERPRINT:rw|ro...";
            let parent: u32 = arguments
                .first()
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| anyhow!(usage))?;
            let reply = PathBuf::from(arguments.get(1).ok_or_else(|| anyhow!(usage))?);
            let nonce = arguments.get(2).ok_or_else(|| anyhow!(usage))?;
            let disks = arguments.get(3..).filter(|disks| !disks.is_empty());
            return copperline::blockdev::serve_broker_request(
                disks.ok_or_else(|| anyhow!(usage))?,
                parent,
                &reply,
                nonce,
            );
        }
        #[cfg(target_os = "linux")]
        return copperline::blockdev::serve_broker_request(arguments);
    }
    if let Some(action) = cli.net_helper_action.as_deref() {
        return run_net_helper_setup(action);
    }
    if cli.list_sampler_inputs {
        return print_sampler_input_devices();
    }
    // The synthesizer's battery-backed memory is the frontend's to
    // ask for; `--factory` leaves the file alone in both directions,
    // the same promise the flag makes about the saved default
    // configuration.
    #[cfg(feature = "coppersynth")]
    copperline::csynth::set_persistence(!cli.factory);
    // The modem's stored profile (AT&W/ATZ) is the frontend's to ask for
    // too, same promise as the synth's battery-backed memory above.
    copperline::modem::profile::set_persistence(!cli.factory);
    // With nothing specified, open the configuration screen instead of
    // booting a default machine. Decided before the config is validated --
    // not just before the bundled ROM resolves -- so the launcher opens
    // even when no Kickstart/AROS is present *and* when the saved default
    // no longer validates (a floppy image gone missing, say): the screen
    // is where such a config gets fixed, and its Run button is where the
    // error shows. An auto-launching default rides the same rule -- its
    // failed run lands back on the open screen rather than exiting. (A
    // bare launcher start has no --rom to fold in: launcher_requested
    // requires its absence.)
    if launcher_requested(&cli) {
        return run_configuration_screen(load_raw_config(
            cli.config_path.as_deref(),
            &cli.overrides,
            cli.factory,
        )?);
    }

    let (cfg, mut raw_cfg) = load_config(cli.config_path.as_deref(), &cli.overrides, cli.factory)?;
    if let Some(p) = &cli.rom_path {
        raw_cfg.rom = Some(p.to_string_lossy().into_owned());
    }

    let mut cfg = cfg.with_rom_override(cli.rom_path.clone());
    let uss = cli
        .load_uss
        .as_ref()
        .map(|path| copperline::uss::UssFile::read(path))
        .transpose()?;
    if let Some(uss) = &uss {
        uss.configure(&mut cfg)?;
    }
    // Direct WHDLoad boot: stage the package and derive the machine before
    // the bundled-ROM sentinel resolves, so a Kickstart 3.1 found in the
    // user's collection can serve as the machine ROM. Explicit machine, ROM,
    // and memory choices (config file or CLI overrides, already merged into
    // the raw config) win over the derivation.
    let (config_game, whdload_options) = copperline::whdload::game_and_options(&raw_cfg);
    // An explicit --run outranks a game remembered in [whdload]: the two
    // stage competing boot volumes (--run with --whdload itself is already
    // a validation error).
    let config_game = if (cli.run.is_some() || uss.is_some()) && config_game.is_some() {
        info!("ignoring the configured [whdload] game for this session");
        None
    } else {
        config_game
    };
    if let Some(game) = cli.whdload.clone().or(config_game) {
        let prepared = copperline::whdload::prepare(&game, &whdload_options)?;
        // Derive on a clone: the session keeps the user's own raw config
        // (plus the game itself), so a launcher opened later edits -- and
        // Save writes -- the user's settings, never the derived machine or
        // the two staged mounts, and its own Run restages from scratch.
        let mut derived = raw_cfg.clone();
        copperline::whdload::apply_to_raw(&mut derived, &prepared);
        cfg = Config::try_from(derived)?;
        copperline::whdload::remember_game(&mut raw_cfg, &game);
        info!(
            "whdload: booting {} ({}) from {}, saves persist in {}",
            prepared.slave_rel.display(),
            prepared.slave.name.as_deref().unwrap_or("unnamed slave"),
            game.display(),
            prepared.game_dir.display()
        );
    }
    // Warp launch: stage the minimal boot volume around the executable and
    // mount its directory. Same derive-on-a-clone discipline as WHDLoad so
    // the launcher never sees (or saves) the two staged mounts; unlike
    // WHDLoad nothing else is derived -- the machine is whatever the
    // configuration and CLI flags say.
    let mut run_prog_name: Option<String> = None;
    let mut run_warp: Option<copperline::runprog::WarpLaunch> = None;
    if let Some(program) = &cli.run {
        let prepared = copperline::runprog::prepare_with_options(
            program,
            cli.run_args.as_deref(),
            copperline::runprog::RunOptions {
                stack: cli.run_stack,
                detach: cli.run_detach,
            },
            None,
        )?;
        let mut derived = raw_cfg.clone();
        copperline::runprog::apply_to_raw(&mut derived, &prepared);
        cfg = Config::try_from(derived)?;
        info!(
            "run: booting {} ({} mounted as {}:)",
            prepared.prog_name,
            prepared.prog_dir.display(),
            copperline::runprog::PROG_VOLUME
        );
        run_warp = Some(copperline::runprog::WarpLaunch::new(
            prepared.prog_name.clone(),
            Some(prepared.boot_dir.join(copperline::runprog::DONE_MARKER)),
        ));
        run_prog_name = Some(prepared.prog_name);
    }
    // Only the gdb and control dispatches read the program name; without
    // either feature, keep the binding "used" so the staging code is one
    // shape in every build.
    #[cfg(not(any(feature = "gdb", feature = "control")))]
    let _ = &run_prog_name;
    // --warp-boot/--warp-until (src/warpboot.rs): validate the flag combos
    // before any mode dispatch, so a bad combination errors even on the
    // gdb/control/benchmark paths that return early below. The TOML-level
    // both-set case is rejected by shared config validation; this merged
    // check also catches a CLI flag against a config-file setting. --run
    // has its own gate with a sharper end condition, so combining them is
    // a configuration error rather than a fight between two gates.
    let warp_until = cli.warp_until.or(cfg.emulation.warp_until);
    let warp_boot = cli.warp_boot || cfg.emulation.warp_boot;
    if (warp_boot || warp_until.is_some()) && cli.run.is_some() {
        return Err(anyhow!(
            "--warp-boot/--warp-until cannot combine with --run, which already \
             warp-boots until the program loads"
        ));
    }
    if warp_boot && warp_until.is_some() {
        return Err(anyhow!(
            "--warp-boot and --warp-until are mutually exclusive: pick the \
             storage-idle heuristic or the fixed timestamp"
        ));
    }
    if cli.load_state.is_some() {
        // A save state restores the full ROM image, so a Kickstart file is not
        // required to load one. Still resolve the bundled-AROS sentinel when
        // AROS is installed (best effort, so the banner and any post-load reuse
        // see real paths); build_machine substitutes a placeholder for whatever
        // ROM is still unavailable.
        let _ = config::resolve_bundled_rom(&mut cfg);
    } else {
        config::resolve_bundled_rom(&mut cfg)?;
    }
    let mut disk_insert_after = resolve_disk_insert_after(&mut cfg, cli.disk_insert_after)?;
    if cli.netplay.is_some() {
        copperline::netplay::prepare_config(&mut cfg)?;
    }

    // Name the boot ROM in the banner: a Kickstart image is identified by
    // checksum (src/romdb.rs), so the log says which Kickstart is booting
    // rather than only which file was opened.
    let rom = match config::rom_identification(&cfg.rom_path) {
        Some(id) => format!("{} ({id})", cfg.rom_path.display()),
        None => cfg.rom_path.display().to_string(),
    };
    info!(
        "config: cpu={:?} fpu={} cpu_clock={}MHz chip_ram={}K fast_ram={}K slow_ram={}K z3_ram={}K zorro_boards={} chipset={:?} (agnus={:?} denise={:?}) video={:?} rom={} floppy_drives={}",
        cfg.cpu,
        cfg.fpu,
        cfg.cpu_clock_mhz,
        cfg.chip_ram_bytes / 1024,
        cfg.fast_ram_bytes / 1024,
        cfg.slow_ram_bytes / 1024,
        cfg.z3_ram_bytes / 1024,
        cfg.zorro_boards.len(),
        cfg.chipset,
        cfg.agnus_revision,
        cfg.denise_revision,
        cfg.video_standard,
        rom,
        cfg.floppy_connected
            .iter()
            .filter(|&&connected| connected)
            .count()
    );

    if matches!(cfg.chipset, Chipset::Aga) {
        info!(
            "chipset AGA: bitplanes/palette/RDRAM/FMODE fetch, sprites (wide fetch, manual \
             wide, SSCAN2/BSCAN2 scan doubling, BPLCON3 SPRES, BPLCON4 offsets) and CLXCON2 \
             collisions are implemented; residual gaps: AGA DDF fine granularity, live \
             collisions on the 6-plane decode (docs/internals/chipset.md)"
        );
    }

    if let Some(secs) = cli.live_audio_profile_secs {
        return run_live_audio_profile(secs);
    }

    // Best-effort realtime-like scheduling for the latency-critical threads.
    // Resolved once here (env var overrides the config) so the audio sink can
    // promote its callback thread and the pacer thread can be raised below.
    let realtime_priority = priority::requested(cfg.emulation.realtime_priority);
    if realtime_priority {
        info!("priority: realtime-like thread scheduling requested (best effort)");
    }
    // `[audio] output_enabled = false` (the GUI "Disabled" option) silences
    // default-on audio, but an explicit `--audio` still forces it on and
    // `--noaudio`/`--audio-wav` still win. CLI flags are unchanged.
    let live_audio = live_audio_enabled(
        cli.audio_live,
        cli.audio_live_forced,
        cfg.audio.output_enabled,
    );
    let audio: Box<dyn AudioSink> = if let Some(ref wav_path) = cli.audio_wav {
        Box::new(WavSink::new(wav_path)?)
    } else if live_audio {
        Box::new(CpalSink::new(
            realtime_priority,
            cfg.audio.output_device.as_deref(),
        )?)
    } else {
        // Log the silent path so `--noaudio` (or an output_enabled=false config)
        // is visible alongside the "cpal sink ready" line the live path prints.
        info!("audio: disabled (null sink); no sound");
        Box::new(NullSink)
    };
    // Headless capture runs (screenshot / frame dump) advance the
    // deterministic core unthrottled; the interactive window paces to
    // wall-clock time. The emulated result is identical either way.
    let headless_capture = !cli.screenshot_after.is_empty()
        || cli.frame_dump.is_some()
        || cli.benchmark_until.is_some()
        || cli.gdb.is_some()
        || cli.control.is_some();
    // A real drive on a bridge is the exception: its platter turns in
    // wall-clock time and cannot be hurried. Left unthrottled, the emulated
    // machine outruns it -- spinning the motor up and down faster than it can
    // reach speed, and stepping past tracks before the drive has captured
    // them -- so the guest sees a drive that answers almost nothing. Pace a
    // bridged machine like a real Amiga, whatever else was asked for.
    let bridged = cfg.floppy.bridges.iter().any(Option::is_some);
    let paced = !headless_capture || bridged;
    if bridged && headless_capture {
        info!("emulation timing: paced to wall-clock because a physical floppy drive is attached");
    }
    info!("emulation timing: deterministic core, paced={paced}");
    let mut emu = emulator::build_machine(&cfg, audio, paced, cli.load_state.is_some())?;
    if let Some(uss) = &uss {
        uss.load(&mut emu)?;
    }
    if let Some(path) = &cli.load_state {
        let outcome = emu.load_state(path)?;
        info!(
            "save state loaded: {} ({}, resuming at {:.1}s emulated time)",
            path.display(),
            outcome.summary,
            emu.bus().emulated_seconds()
        );
    }
    if let Some(dir) = &cli.audio_stems {
        let granularities = cli
            .audio_stems_mode
            .as_deref()
            .or(cfg.audio.stem_granularity.as_deref())
            .ok_or_else(|| {
                anyhow!(
                    "--audio-stems requires --audio-stems-mode LIST (e.g. \"master,source\"), \
                     or a [audio] stem_granularity default in the config"
                )
            })?;
        // After any --load-state: the resumed machine can describe
        // different hardware than cfg (see configured_audio_stem_sources'
        // own doc comment), so this reads the config only to *supplement*
        // a conservative state-loaded registration, never to narrow it.
        let sources = configured_audio_stem_sources(&cfg, cli.load_state.is_some());
        emu.bus_mut()
            .paula
            .audio
            .enable_stems(dir, granularities, &sources)?;
    }
    // Arm reverse debugging (snapshot ring + optional one-shot "last writer"
    // watchpoint) from the COPPERLINE_DBG_RR*/RWATCH environment.
    if let Some(rr) = debugger::reverse_config_from_env() {
        if envcfg::var("COPPERLINE_RTC_FIXED_SECS").is_none() {
            warn!(
                "reverse debugging is armed but COPPERLINE_RTC_FIXED_SECS is unset; \
                 the guest RTC reads host wall-clock time, so replay may diverge. \
                 Set COPPERLINE_RTC_FIXED_SECS for deterministic reverse debugging."
            );
        }
        emu.enable_time_travel(rr.budget_mb, rr.interval_frames);
        if let Some(addr) = rr.watch_addr {
            emu.arm_reverse_watch(addr, rr.target_secs);
        }
    }
    if let Some(opts) = cli.waveform {
        emu.machine.ui_wave_start(opts)?;
    }
    if let Some(target_secs) = cli.benchmark_until {
        return run_headless_benchmark(emu, target_secs);
    }
    #[cfg(feature = "gdb")]
    if let Some(listen) = cli.gdb {
        let mut gdb = gdbstub::Config::new(listen);
        gdb.bartman = cli.gdb_dialect.as_deref() == Some("bartman");
        // --run + --gdb: stop at the program's first instruction, the
        // moment the guest OS loads it.
        gdb.stop_on_load = run_prog_name.clone();
        return gdbstub::run(emu, gdb);
    }
    #[cfg(feature = "control")]
    if let Some(listen) = cli.control.clone() {
        let mut config = copperline::control::Config::new(listen);
        config.token = cli.control_token.clone();
        config.info_file = cli.control_info.clone();
        // The headless server owns the machine, so it journals
        // --record-input itself; windowed mode journals through the App.
        config.record_input = cli.record_input.clone();
        // --run + --control: the first resume stops the moment the guest
        // OS loads the program, like --gdb.
        config.stop_on_load = run_prog_name.clone();
        return copperline::control::headless::run(emu, config);
    }
    let disk_write_protected = std::array::from_fn(|idx| {
        cfg.floppy.drives[idx]
            .as_ref()
            .map(|d| d.write_protected)
            .unwrap_or(true)
    });
    video::set_pixel_aspect(config::resolve_pixel_aspect(cfg.pixel_aspect));
    video::set_display_scaling(cfg.scaling);
    video::set_autocrop(cfg.autocrop);
    video::set_menu_scale(cfg.menu_scale);
    // A fascia belongs to a machine that carries the instrument: with the
    // serial port out of MIDI mode, or another device chosen, the strip
    // would be blank glass, so the flag follows the configuration.
    let serial_midi = cfg.serial.mode == config::SerialMode::Midi;
    video::set_mt32_panel_shown(
        cfg.serial.mt32_panel
            && serial_midi
            && config::midi_out_is_mt32(cfg.serial.midi_out.as_deref()),
    );
    #[cfg(feature = "coppersynth")]
    video::set_csynth_panel_shown(
        cfg.serial.coppersynth_panel
            && serial_midi
            && config::midi_out_is_csynth(cfg.serial.midi_out.as_deref()),
    );
    video::set_mt32_lcd(cfg.serial.mt32_lcd);
    // Capture runs (--screenshot-after / --dump-frames) never present a
    // frame, so they skip the host window and event loop entirely: winit's
    // event-loop setup registers with the display server, which aborts or
    // blocks on hosts without one (SSH sessions, sandboxes without
    // window-server access), and a capture run must work anywhere.
    // --control-gui keeps the windowed path: it explicitly asks for an
    // interactive session.
    let windowless_capture = (!cli.screenshot_after.is_empty() || cli.frame_dump.is_some())
        && cli.control_gui.is_none()
        && cli.gdb_gui.is_none();
    // The warp-launch gate belongs to interactive sessions only: a capture
    // run is already unpaced end to end and must never be re-paced when the
    // program loads.
    let run_warp_target = if headless_capture { None } else { run_warp };
    // The general warp-boot gate: interactive sessions only, same rule as
    // the --run gate above (a capture run is already unpaced end to end).
    let warp_boot_gate = if headless_capture {
        None
    } else {
        copperline::warpboot::gate_from_settings(
            warp_boot,
            cfg.emulation.warp_boot_idle,
            warp_until,
        )
    };
    let netplay = if let Some(options) = cli.netplay {
        for disk in disk_insert_after.drain(..) {
            emu.bus_mut().floppy.insert_disk_image(
                disk.drive_idx,
                disk.path,
                disk.write_protected,
            )?;
        }
        let session = copperline::netplay::Session::new(options, &mut emu, &cfg)?;
        #[cfg(feature = "netplay-internet")]
        if let Some(path) = &cli.netplay_invitation_out {
            if let copperline::netplay::ConnectionOptions::Internet(options) = session.options() {
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| std::path::Path::new("."));
                let mut file = tempfile::NamedTempFile::new_in(parent)?;
                std::io::Write::write_all(&mut file, options.invitation.encode()?.as_bytes())?;
                file.persist(path)
                    .map_err(|error| anyhow!("Writing netplay invitation: {}", error.error))?;
                log::info!(
                    "netplay: invitation written to {}; waiting for guest",
                    path.display()
                );
            }
        }
        #[cfg(not(feature = "netplay-internet"))]
        let _ = &cli.netplay_invitation_out;
        Some(session)
    } else {
        None
    };
    let mut app = App::new(
        emu,
        cfg.emulation.power_on,
        cli.screenshot_after,
        cli.save_state_after,
        cli.frame_dump,
        cli.press_after,
        cli.click_after,
        cli.joy_after,
        cli.mouse_after,
        cli.mouse_to_after,
        cli.pot_after,
        disk_insert_after,
        cli.cd_insert_after,
        cli.freeze_after,
        cli.record_input,
        run_warp_target,
        warp_boot_gate,
        cfg.floppy_playlists.clone(),
        disk_write_protected,
        config::resolve_overscan(cfg.overscan),
        cfg.tv_centre,
        config::resolve_deinterlace(cfg.deinterlace),
        config::resolve_phosphor(cfg.phosphor),
        config::resolve_shader(cfg.shader.clone()),
        config::resolve_shader_strength(cfg.shader_strength),
        config::resolve_bezel(cfg.bezel),
        config::resolve_bezel_stickers(cfg.bezel_stickers.clone()),
        config::resolve_perf_overlay(cfg.perf_overlay),
        config::resolve_tint(cfg.tint),
        cfg.full_screen,
        !cfg.status_bar,
        cfg.emulation.warp_speed,
        cfg.joystick_input_mode,
        cfg.mouse_sensitivity,
        cfg.mouse_capture,
        config::about_machine_lines(&cfg),
        raw_cfg,
        if cli.load_state.is_some() {
            Some("loaded save state")
        } else {
            cfg.runahead_machine_block_reason()
        },
        live_audio,
        copperline::sampler::SamplerRequest::from_config(&cfg.parallel),
    );
    if let Some(session) = netplay {
        app.attach_netplay(session);
    }
    #[cfg(feature = "control")]
    if let Some(listen) = cli.control_gui {
        // Bind (and announce) before the window opens so scripts can
        // attach as soon as the endpoint line appears; the socket
        // threads start inside App::run once the event loop exists.
        let mut config = copperline::control::Config::new(listen);
        config.token = cli.control_token;
        config.info_file = cli.control_info;
        // --run + --control-gui: arm the one-shot stop at the program's
        // load before the first frame runs, so a client attaching after
        // the boot cannot miss it.
        config.stop_on_load = run_prog_name.clone();
        let handle = copperline::control::windowed::ControlHandle::bind(&config)?;
        app.attach_control(handle, &config);
    }
    #[cfg(feature = "gdb")]
    if let Some(listen) = cli.gdb_gui {
        // Same shape for the windowed GDB stub: bind before the window
        // opens, socket threads start inside App::run.
        let mut config = gdbstub::Config::new(listen);
        config.bartman = cli.gdb_dialect.as_deref() == Some("bartman");
        // --run + --gdb-gui: stop at the program's first instruction,
        // the moment the guest OS loads it.
        config.stop_on_load = run_prog_name.clone();
        let handle = copperline::gdbstub::windowed::GdbHandle::bind(&config)?;
        app.attach_gdb(handle, &config);
    }

    // Elevate the thread that is about to run the event loop and the pacer.
    // Only when actually pacing to wall-clock time: headless capture advances
    // the core unthrottled, so priority buys it nothing.
    if realtime_priority && paced {
        priority::elevate_pacer_thread();
    }
    if windowless_capture {
        info!("headless capture: running without a window (no display connection)");
        return app.run_headless();
    }
    info!(
        "entering event loop. {HOST_SHORTCUT_MODIFIER_LABEL}+Q to quit, {HOST_SHORTCUT_MODIFIER_LABEL}+S to screenshot, {HOST_SHORTCUT_MODIFIER_LABEL}+G to capture/release mouse."
    );
    app.run()
}

/// Build the minimal placeholder machine that hosts the configuration screen
/// before a real machine is built. It needs no ROM file (a tiny in-memory ROM
/// that immediately stops) and a null audio sink so it claims no audio device
/// while it sits powered off behind the launcher; the user's chosen machine
/// replaces it when they press Run.
fn build_placeholder_machine() -> Result<Emulator> {
    use copperline::memory::{ROM_BASE, ROM_SIZE};
    let mut rom = vec![0u8; ROM_SIZE];
    // Reset vector: a small stack pointer and a PC just past it; the rest is a
    // STOP-then-NOP sled, so the placeholder CPU does nothing if ever stepped.
    rom[0..4].copy_from_slice(&0x0007_FFFEu32.to_be_bytes());
    rom[4..8].copy_from_slice(&(ROM_BASE as u32 + 8).to_be_bytes());
    for word in rom[8..].chunks_exact_mut(2) {
        word.copy_from_slice(&0x4E71u16.to_be_bytes());
    }
    let mem = Memory {
        chip_ram: vec![0u8; 512 * 1024],
        slow_ram: Vec::new(),
        mb_ram: Vec::new(),
        accel_ram: Vec::new(),
        rom,
        overlay: true,
        zorro: copperline::zorro::ZorroChain::default(),
        extended_rom: Vec::new(),
        extended_rom_base: 0,
        wcs: Vec::new(),
        wcs_write_protected: false,
    };
    let bus = Bus::new(
        mem,
        Paula::new(Box::new(StdoutSink::new()), Box::new(NullSink)),
        FloppyController::default(),
    );
    Emulator::new(
        bus,
        copperline::config::CpuModel::M68000,
        false,
        Default::default(),
        copperline::config::PacingBudget::Cycles,
        2,
        true,
    )
}

/// Open the machine-configuration screen (the launcher shown when Copperline is
/// started with no machine specified). A placeholder machine sits powered off
/// behind the panel until the user presses Run, which builds and starts their
/// chosen machine in place.
fn run_configuration_screen(raw_cfg: config::RawConfig) -> Result<()> {
    info!("no machine specified; opening the configuration screen");
    let emu = build_placeholder_machine()?;
    video::set_pixel_aspect(config::resolve_pixel_aspect(config::PixelAspect::Tv));
    // The launcher opens before a machine config is built, so it presents at
    // the default aspect and scaling; the machine it starts applies its own
    // (see start_configured_machine).
    video::set_display_scaling(config::DisplayScaling::Smooth);
    video::set_autocrop(false);
    // The launcher opens before a machine config is built, so the menu size
    // comes straight off the raw file.
    video::set_menu_scale(raw_cfg.menu_scale());
    // The placeholder is always silent; seed the session's audio from the config
    // intent so a state loaded over the launcher gets the configured output.
    let audio_output_enabled = raw_cfg.audio_output_enabled();
    let mut app = App::new(
        emu,
        false,
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        None,
        None,
        None,
        std::array::from_fn(|_| Vec::new()),
        [true; 4],
        config::resolve_overscan(config::Overscan::Tv),
        config::TvCentre::default(),
        config::resolve_deinterlace(true),
        config::resolve_phosphor(0.0),
        config::resolve_shader(config::ShaderMode::None),
        config::resolve_shader_strength(1.0),
        config::resolve_bezel(config::BezelStyle::None),
        config::resolve_bezel_stickers(None),
        config::resolve_perf_overlay(false),
        config::resolve_tint(config::Tint::None),
        // The config-screen placeholder is always a normal windowed UI.
        false,
        false,
        config::WarpSpeed::default(),
        config::JoystickInputMode::default(),
        50,
        // The config screen is a UI to be clicked around; an auto grab
        // belongs to the machine, and run_machine installs the real setting
        // when one is started.
        config::MouseCapture::default(),
        vec![config::ABOUT_PLACEHOLDER_LINE.to_string()],
        raw_cfg,
        None,
        audio_output_enabled,
        // The placeholder runs no sampler; run_machine attaches it on Run.
        copperline::sampler::SamplerRequest::default(),
    );
    app.open_launcher();
    // `[emulation] auto_launch` in the configuration the launcher opened
    // showing: straight to the machine, no configuration screen first.
    app.auto_launch_if_asked();
    app.run()
}

/// Whether to show the configuration screen instead of booting: only on a bare
/// interactive launch with nothing specified (no config file, ROM, overrides,
/// scripted input, headless capture, or save-state load), and with live audio
/// (the launcher's Run path uses the live audio sink).
///
/// `--factory` is deliberately not in the list. It is the flag for somebody
/// whose saved default is not what they want any more, and sending them
/// straight into a machine rather than into the launcher would be the
/// opposite of helpful. A saved default does not suppress the launcher
/// either -- it says what the launcher opens showing, not what to run.
fn launcher_requested(cli: &CliArgs) -> bool {
    cli.netplay.is_none()
        && cli.config_path.is_none()
        && cli.rom_path.is_none()
        && cli.whdload.is_none()
        && cli.run.is_none()
        && cli.overrides.is_empty()
        && !Path::new("copperline.toml").exists()
        && cli.screenshot_after.is_empty()
        && cli.save_state_after.is_empty()
        && cli.frame_dump.is_none()
        && cli.benchmark_until.is_none()
        && cli.gdb.is_none()
        && cli.gdb_gui.is_none()
        && cli.control.is_none()
        && cli.control_gui.is_none()
        && cli.load_state.is_none()
        && cli.load_uss.is_none()
        && cli.press_after.is_empty()
        && cli.click_after.is_empty()
        && cli.joy_after.is_empty()
        && cli.mouse_after.is_empty()
        && cli.mouse_to_after.is_empty()
        && cli.pot_after.is_empty()
        && cli.freeze_after.is_empty()
        && cli.disk_insert_after.is_empty()
        && cli.record_input.is_none()
        && cli.audio_wav.is_none()
        && cli.audio_live
}

fn run_live_audio_profile(secs: f32) -> Result<()> {
    info!(
        "audio profile mode: running Paula DMA to cpal for {:.3}s without window rendering",
        secs
    );
    // This diagnostic mode loads no config, so the realtime knob is env-only
    // and it always uses the default output device.
    let audio = Box::new(CpalSink::new(priority::requested(false), None)?);
    let mut paula = Paula::new(Box::new(StdoutSink::new()), audio);
    paula.set_led_filter_guest(true);

    let mut chip_ram = vec![0u8; 64];
    chip_ram[0] = 0x40;
    chip_ram[1] = 0xC0;
    chip_ram[2] = 0x20;
    chip_ram[3] = 0xE0;

    paula.write_audio_reg(0x00, 0, 0);
    paula.write_audio_reg(0x02, 0, 0);
    paula.write_audio_reg(0x04, 1, 0);
    paula.write_audio_reg(0x06, 400, 0);
    paula.write_audio_reg(0x08, 64, 0);
    paula.write_audio_reg(0x10, 0, 0);
    paula.write_audio_reg(0x12, 2, 0);
    paula.write_audio_reg(0x14, 1, 0);
    paula.write_audio_reg(0x16, 512, 0);
    paula.write_audio_reg(0x18, 48, 0);

    let dmacon = DMACON_DMAEN | 0x0003;
    paula.apply_audio_dmacon_edges(0, dmacon);
    let mut line_cck = 0u32;
    let quantum = Duration::from_millis(5);
    let quantum_cck = (PAULA_CLOCK_HZ as f64 * quantum.as_secs_f64())
        .round()
        .clamp(1.0, u32::MAX as f64) as u32;
    let started = Instant::now();
    let deadline = started + Duration::from_secs_f32(secs);
    let mut chunks = 0u64;

    while Instant::now() < deadline {
        let chunk_started = Instant::now();
        let _ =
            advance_paula_profile_audio(&mut paula, quantum_cck, dmacon, &chip_ram, &mut line_cck);
        chunks = chunks.saturating_add(1);
        if let Some(wait) = quantum.checked_sub(chunk_started.elapsed()) {
            std::thread::sleep(wait);
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    info!(
        "audio profile mode complete: elapsed={:.3}s chunks={} quantum_cck={}",
        elapsed, chunks, quantum_cck
    );
    Ok(())
}

fn advance_paula_profile_audio(
    paula: &mut Paula,
    cck: u32,
    dmacon: u16,
    chip_ram: &[u8],
    line_cck: &mut u32,
) -> u16 {
    // Drive the state machine the way the bus does: service each channel's
    // fixed DMA slot, advance time, and transfer requests at line ends.
    let mut irq = 0;
    for _ in 0..cck {
        let slot = match *line_cck {
            0x00F => Some(0),
            0x011 => Some(1),
            0x013 => Some(2),
            0x015 => Some(3),
            _ => None,
        };
        if let Some(channel) = slot {
            if let Some(request) = paula.audio_dma_request(channel) {
                let word = read_profile_audio_word(chip_ram, request.address);
                irq |= paula.grant_audio_dma(channel, word, dmacon);
            }
        }
        irq |= paula.advance_audio(1, dmacon);
        *line_cck += 1;
        if *line_cck >= 227 {
            *line_cck = 0;
            paula.transfer_audio_dma_requests();
        }
    }
    irq
}

fn read_profile_audio_word(chip_ram: &[u8], address: u32) -> u16 {
    if chip_ram.is_empty() {
        return 0;
    }
    let off = (address as usize) % chip_ram.len();
    ((chip_ram[off] as u16) << 8) | chip_ram[(off + 1) % chip_ram.len()] as u16
}

/// Load the config, returning both the validated [`Config`] used to build the
/// machine and the raw TOML view it came from. The configuration screen keeps
/// the raw view so its "Machine Configuration..." menu item can reopen showing
/// the running machine's settings.
fn load_config(
    explicit: Option<&Path>,
    overrides: &ConfigOverrides,
    factory: bool,
) -> Result<(Config, config::RawConfig)> {
    let raw = load_raw_config(explicit, overrides, factory)?;
    let cfg = Config::try_from(raw.clone())?;
    Ok((cfg, raw))
}

/// The raw half of [`load_config`]: find and parse the file, put its
/// `[paths]` in force, validate nothing. The configuration screen starts
/// from this -- validation belongs to its Run button, where a missing
/// floppy image is a status line to fix rather than an exit before the
/// screen that could fix it ever opens.
fn load_raw_config(
    explicit: Option<&Path>,
    overrides: &ConfigOverrides,
    factory: bool,
) -> Result<config::RawConfig> {
    // Resolve which file (if any) backs the config: the explicit --config
    // path, then ./copperline.toml if present, then the configuration saved
    // with Save default, otherwise the built-in defaults. CLI overrides
    // layer on top of whichever it is.
    let cwd = Path::new("copperline.toml");
    // Only if it was actually saved: most installations have no default, so
    // this is normally one `stat` that finds nothing.
    let saved = (!factory)
        .then(copperline::paths::default_config_file)
        .flatten()
        .filter(|path| path.is_file());
    let path = if explicit.is_some() {
        explicit
    } else if cwd.exists() {
        info!("loading config from {}", cwd.display());
        Some(cwd)
    } else if let Some(saved) = saved.as_deref() {
        info!(
            "loading the saved default configuration {}",
            saved.display()
        );
        Some(saved)
    } else {
        None
    };
    let raw = Config::load_raw(path, overrides)?;
    // Before any conversion: `Config::try_from` resolves the implicit
    // battery-RAM backing files through the paths in force, so adopting
    // afterwards would site this run's NVRAM by the previous answer.
    // Whatever this host cannot reach is dropped here and inherits
    // instead, so a config naming somebody else's memory stick still starts.
    copperline::paths::adopt(raw.paths());
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use copperline::video::window::{
        FrameDumpSpec, JoyButtonKind, KeyPressSpec, MouseButtonKind, DEFAULT_KEY_HOLD_MS,
    };

    fn parse(args: &[&str]) -> Result<CliArgs> {
        parse_args_from(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn placeholder_machine_builds() {
        // The configuration screen's host machine must build without any ROM
        // file or audio device (it sits powered off behind the launcher).
        build_placeholder_machine().expect("placeholder machine builds");
    }

    #[test]
    fn stem_sources_register_cdda_for_every_static_cd_attachment() {
        use std::path::PathBuf;
        let ids = |cfg: &config::Config| -> Vec<&'static str> {
            configured_audio_stem_sources(cfg, false)
                .iter()
                .map(|s| s.id)
                .collect()
        };
        let cd_drive = || {
            Some(config::DriveImage {
                path: PathBuf::from("disc.cue"),
                ..Default::default()
            })
        };

        let cfg_with = |edit: fn(&mut config::Config)| {
            let mut cfg = config::Config::default();
            edit(&mut cfg);
            cfg
        };

        // A bare machine: Paula and drive sounds only, no cdda/mt32.
        assert_eq!(
            ids(&config::Config::default()),
            vec!["paula", "drivesounds"]
        );

        // Each way a CD drive can be statically configured registers cdda:
        // the machine's own drive ([cd] image / a CD32-CDTV profile)...
        let cfg = cfg_with(|c| c.cd_image_path = Some(PathBuf::from("game.iso")));
        assert!(ids(&cfg).contains(&"cdda"));
        let cfg = cfg_with(|c| c.machine = Some(config::MachineModel::Cd32));
        assert!(ids(&cfg).contains(&"cdda"));
        // ...and a CD image on an [ide]/[lide]/[scsi] drive slot, which
        // attaches as an ATAPI/SCSI CD-ROM feeding the same CdAudioRing.
        let mut cfg = cfg_with(|_| {});
        cfg.ide.slave = cd_drive();
        assert!(ids(&cfg).contains(&"cdda"));
        let mut cfg = cfg_with(|_| {});
        cfg.lide.drives[1] = cd_drive();
        assert!(ids(&cfg).contains(&"cdda"));
        let mut cfg = cfg_with(|_| {});
        cfg.scsi.units[3] = cd_drive();
        assert!(ids(&cfg).contains(&"cdda"));

        // A hard-disk image on those same slots is not a CD.
        let mut cfg = cfg_with(|_| {});
        cfg.ide.master = Some(config::DriveImage {
            path: PathBuf::from("workbench.hdf"),
            ..Default::default()
        });
        assert!(!ids(&cfg).contains(&"cdda"));
    }

    #[test]
    fn stem_sources_are_conservative_after_a_state_load() {
        // A bare config says no CD/MT-32/Toccata -- but a --load-state run
        // can resume a machine describing entirely different hardware than
        // this process's own cfg (the host reconfigures to match a
        // state's descriptor), so state_loaded=true must register all
        // three regardless of what the pre-load config says.
        let cfg = config::Config::default();
        let ids: Vec<&'static str> = configured_audio_stem_sources(&cfg, true)
            .iter()
            .map(|s| s.id)
            .collect();
        assert!(ids.contains(&"cdda"), "state loads must not skip cdda");
        assert!(ids.contains(&"mt32"), "state loads must not skip mt32");
        assert!(
            ids.contains(&"toccata"),
            "state loads must not skip toccata"
        );
        assert!(ids.contains(&"mhi"), "state loads must not skip mhi");
        #[cfg(feature = "coppersynth")]
        assert!(
            ids.contains(&"coppersynth"),
            "state loads must not skip coppersynth"
        );

        // Without a state load, the same bare config registers none of them.
        let ids: Vec<&'static str> = configured_audio_stem_sources(&cfg, false)
            .iter()
            .map(|s| s.id)
            .collect();
        assert!(!ids.contains(&"cdda"));
        assert!(!ids.contains(&"mt32"));
        assert!(!ids.contains(&"coppersynth"));
        assert!(!ids.contains(&"toccata"));
        assert!(!ids.contains(&"mhi"));
    }

    #[test]
    fn launcher_shows_only_when_nothing_is_specified() {
        // A bare interactive launch (no config file present in this dir under
        // test) opens the configuration screen...
        let bare = parse(&[]).unwrap();
        assert!(launcher_requested(&bare));
        // ...but specifying a ROM, an override, or a headless capture boots
        // directly instead.
        assert!(!launcher_requested(&parse(&["KICK.ROM"]).unwrap()));
        assert!(!launcher_requested(&parse(&["--model", "A1200"]).unwrap()));
        assert!(!launcher_requested(
            &parse(&["--screenshot-after", "5", "out.png"]).unwrap()
        ));
        assert!(!launcher_requested(&parse(&["--noaudio"]).unwrap()));
        assert!(!launcher_requested(&parse(&["--run", "hello"]).unwrap()));
    }

    #[test]
    fn run_flags_parse_and_validate() {
        let cli = parse(&[
            "--run",
            "build/hello",
            "--run-args",
            "-level 2",
            "--run-stack",
            "32768",
            "--run-detach",
        ])
        .unwrap();
        assert_eq!(cli.run.as_deref(), Some(Path::new("build/hello")));
        assert_eq!(cli.run_args.as_deref(), Some("-level 2"));
        assert_eq!(cli.run_stack, Some(32_768));
        assert!(cli.run_detach);
        assert!(validate_run_args(&cli).is_ok());

        for bytes in [2048, 2049, 2_147_483_644] {
            let cli = parse(&["--run", "hello", "--run-stack", &bytes.to_string()]).unwrap();
            assert_eq!(cli.run_stack, Some(bytes));
        }
        for bytes in [0u64, 4, 2047, 2_147_483_645, 4_294_967_295, 4_294_967_296] {
            let err = parse(&["--run", "hello", "--run-stack", &bytes.to_string()])
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("stack must be between 2048 and 2147483644"),
                "{err}"
            );
        }

        // --run-args without --run is a mistake worth catching.
        let orphan = parse(&["--run-args", "-level 2"]).unwrap();
        assert!(validate_run_args(&orphan)
            .unwrap_err()
            .to_string()
            .contains("--run"));

        // --run and --whdload each stage their own boot volume.
        let both = parse(&["--run", "hello", "--whdload", "game.lha"]).unwrap();
        assert!(validate_run_args(&both)
            .unwrap_err()
            .to_string()
            .contains("mutually exclusive"));
    }

    #[test]
    fn capture_flags_accumulate_instead_of_overwriting() {
        // --screenshot-after and --save-state-after repeat like the
        // scheduled-input flags: a run can bracket several moments. They
        // used to parse into a single slot, so a second occurrence silently
        // replaced the first and the earlier capture never fired.
        let args = parse(&[
            "--screenshot-after",
            "5",
            "early.png",
            "--screenshot-after",
            "10",
            "late.png",
            "--save-state-after",
            "7",
            "mid.clstate",
            "--save-state-after",
            "9",
            "end.clstate",
        ])
        .unwrap();

        let shots: Vec<_> = args
            .screenshot_after
            .iter()
            .map(|(secs, path)| (*secs, path.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            shots,
            vec![(5.0, "early.png".to_owned()), (10.0, "late.png".to_owned())]
        );

        let states: Vec<_> = args
            .save_state_after
            .iter()
            .map(|(secs, path)| (*secs, path.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            states,
            vec![
                (7.0, "mid.clstate".to_owned()),
                (9.0, "end.clstate".to_owned())
            ]
        );

        // A single occurrence still parses to exactly one entry.
        let args = parse(&["--screenshot-after", "5", "out.png"]).unwrap();
        assert_eq!(args.screenshot_after.len(), 1);
        assert!(args.save_state_after.is_empty());
    }

    #[test]
    fn bridge_cli_interface_implies_bridge_and_rejects_conflicts() {
        let args = parse(&["--a2065-interface", "en-test"]).unwrap();
        assert_eq!(args.overrides.a2065_interface.as_deref(), Some("en-test"));
        assert!(args.overrides.a2065_net.is_none());

        let args = parse(&["--a2065-net", "bridge", "--a2065-interface", "en-test"]).unwrap();
        assert_eq!(args.overrides.a2065_net.as_deref(), Some("bridge"));

        let error = parse(&["--a2065-net", "nat", "--a2065-interface", "en-test"]).unwrap_err();
        assert!(error.to_string().contains("conflicts"), "{error:#}");
    }

    fn temp_script(name: &str, contents: &str) -> PathBuf {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "copperline-script-{}-{unique}-{name}.clscript",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn mouse_after_parses_signed_deltas() -> Result<()> {
        let args = parse(&["--mouse-after", "1.5", "-3", "10"])?;
        assert_eq!(args.mouse_after, vec![(1.5, -3, 10, 0)]);
        Ok(())
    }

    #[test]
    fn mouse_to_after_parses_absolute_targets_on_either_port() -> Result<()> {
        let args = parse(&["--mouse-to-after", "3.0", "320", "128"])?;
        assert_eq!(args.mouse_to_after, vec![(3.0, 320, 128, 0)]);
        // The same directive inside a script, with the optional port.
        let path = temp_script("mouse-to", "mouse-to-after 4.5 100 40 2\n");
        let args = parse(&["--script", &path.display().to_string()])?;
        assert_eq!(args.mouse_to_after, vec![(4.5, 100, 40, 1)]);
        std::fs::remove_file(&path).ok();
        Ok(())
    }

    #[test]
    fn scripted_input_flags_take_an_optional_trailing_port() -> Result<()> {
        let args = parse(&[
            "--mouse-after",
            "1.5",
            "-3",
            "10",
            "2",
            "--click-after",
            "5",
            "left",
            "100",
            "2",
            "--joy-after",
            "60",
            "red",
            "300",
            "1",
            "--pot-after",
            "12",
            "50",
            "200",
            "1",
        ])?;
        assert_eq!(args.mouse_after, vec![(1.5, -3, 10, 1)]);
        assert_eq!(args.click_after, vec![(5.0, MouseButtonKind::Left, 100, 1)]);
        assert_eq!(args.joy_after, vec![(60.0, JoyButtonKind::Red, 300, 0)]);
        assert_eq!(args.pot_after, vec![(12.0, 50, 200, 0)]);
        Ok(())
    }

    #[test]
    fn port_token_lookahead_does_not_eat_a_following_flag() -> Result<()> {
        // No trailing port: the next flag must survive as a flag, and the
        // defaults are click/mouse -> port 1, joy/pot -> port 2 (0-based
        // 0/1 in the tuples).
        let args = parse(&[
            "--joy-after",
            "60",
            "red",
            "300",
            "--pot-after",
            "12",
            "50",
            "200",
            "--noaudio",
        ])?;
        assert_eq!(args.joy_after, vec![(60.0, JoyButtonKind::Red, 300, 1)]);
        assert_eq!(args.pot_after, vec![(12.0, 50, 200, 1)]);
        assert!(!args.audio_live);
        Ok(())
    }

    #[test]
    fn script_file_expands_to_the_equivalent_flags() -> Result<()> {
        let path = temp_script(
            "ok",
            "# recorded session\n\
             key-after 14.0 ctrl 500\n\
             press-after 14.1 0x63\n\
             \n\
             click-after 5.0 left 100\n\
             joy-after 60.0 red 300 1\n\
             mouse-after 1.020 -3 10 2\n\
             pot-after 12.0 50 200\n\
             insert-disk-after 30.0 df1 \"/tmp/with space.adf\"\n",
        );
        let args = parse(&["--script", path.to_str().unwrap()])?;
        assert_eq!(args.press_after.len(), 2);
        assert_eq!(args.press_after[0].hold_ms, 500);
        assert_eq!(args.click_after, vec![(5.0, MouseButtonKind::Left, 100, 0)]);
        assert_eq!(args.joy_after, vec![(60.0, JoyButtonKind::Red, 300, 0)]);
        assert_eq!(args.mouse_after, vec![(1.02, -3, 10, 1)]);
        assert_eq!(args.pot_after, vec![(12.0, 50, 200, 1)]);
        assert_eq!(
            args.disk_insert_after,
            vec![CliDiskInsert::Explicit(DiskInsertSpec {
                secs: 30.0,
                drive_idx: 1,
                path: PathBuf::from("/tmp/with space.adf"),
                write_protected: true,
            })]
        );
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn script_files_reject_non_input_directives() {
        // Anything outside the scripted-input set is refused, including
        // nesting another script.
        for line in [
            "config /tmp/evil.toml",
            "script /tmp/other",
            "load-state /tmp/x",
        ] {
            let path = temp_script("bad", line);
            let err = parse(&["--script", path.to_str().unwrap()]).unwrap_err();
            assert!(
                err.to_string().contains("not a scripted-input directive"),
                "{line}: {err}"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn script_lines_with_unterminated_quotes_are_rejected() {
        let path = temp_script("quote", "insert-disk-after 1.0 df0 \"/tmp/unterminated\n");
        let err = parse(&["--script", path.to_str().unwrap()]).unwrap_err();
        assert!(err.to_string().contains("unterminated quote"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recorded_script_round_trips_through_the_parser() -> Result<()> {
        // What the recorder emits must come back as the same scheduled
        // events through --script.
        let mut rec = copperline::inputrec::InputRecorder::new(0.0);
        let mut input = copperline::bus::InputState::default();
        input.set_port_device(1, copperline::bus::PortDevice::Joystick);
        rec.observe(&input, 1.0);
        rec.record_key(0x45, true, 1.5);
        rec.record_key(0x45, false, 1.75);
        input.ports[0].counter_x = 5;
        input.ports[0].fire = true;
        rec.observe(&input, 2.0);
        input.ports[0].fire = false;
        rec.observe(&input, 2.5);
        rec.record_disk_insert(0, Path::new("/tmp/demo.adf"), 3.0);
        let path = temp_script("roundtrip", &rec.finish());

        let args = parse(&["--script", path.to_str().unwrap()])?;
        assert_eq!(args.press_after.len(), 1);
        assert_eq!(args.press_after[0].rawkey, 0x45);
        assert_eq!(args.press_after[0].hold_ms, 250);
        assert_eq!(args.mouse_after, vec![(2.0, 5, 0, 0)]);
        assert_eq!(args.click_after, vec![(2.0, MouseButtonKind::Left, 500, 0)]);
        assert_eq!(args.disk_insert_after.len(), 1);
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn audio_is_enabled_by_default() -> Result<()> {
        let args = parse(&[])?;
        assert!(args.audio_live);
        assert!(args.audio_wav.is_none());
        Ok(())
    }

    #[test]
    fn noaudio_disables_live_audio() -> Result<()> {
        let args = parse(&["--noaudio"])?;
        assert!(!args.audio_live);
        assert!(args.audio_wav.is_none());
        Ok(())
    }

    #[test]
    fn explicit_audio_marks_forced() -> Result<()> {
        assert!(!parse(&[])?.audio_live_forced);
        assert!(parse(&["--audio"])?.audio_live_forced);
        assert!(!parse(&["--noaudio"])?.audio_live_forced);
        Ok(())
    }

    #[test]
    fn config_disable_silences_default_audio_but_not_explicit_audio() {
        // No CLI audio flag: the config's output_enabled decides.
        assert!(live_audio_enabled(true, false, true));
        assert!(!live_audio_enabled(true, false, false));
        // Explicit --audio forces sound on even if the config disabled it.
        assert!(live_audio_enabled(true, true, false));
        // --noaudio (clears audio_live) always wins.
        assert!(!live_audio_enabled(false, false, true));
        assert!(!live_audio_enabled(false, true, true));
    }

    #[test]
    fn audio_wav_selects_wav_output_without_live_audio() -> Result<()> {
        let args = parse(&["--audio-wav", "/tmp/out.wav"])?;
        assert!(!args.audio_live);
        assert_eq!(args.audio_wav, Some(PathBuf::from("/tmp/out.wav")));
        Ok(())
    }

    #[test]
    fn explicit_audio_conflicts_with_audio_wav() {
        let err = parse(&["--audio", "--audio-wav", "/tmp/out.wav"]).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "{err:#}");
    }

    #[test]
    fn live_audio_profile_mode_parses_duration_and_requires_live_audio() -> Result<()> {
        let args = parse(&["--profile-live-audio", "0.25"])?;
        assert_eq!(args.live_audio_profile_secs, Some(0.25));
        assert!(args.audio_live);
        assert!(args.audio_wav.is_none());

        let err = parse(&["--profile-live-audio", "0.25", "--noaudio"]).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "{err:#}");
        Ok(())
    }

    #[test]
    fn benchmark_until_parses_and_defaults_to_null_audio() -> Result<()> {
        let args = parse(&["--benchmark-until", "85.4"])?;
        assert_eq!(args.benchmark_until, Some(85.4));
        assert!(!args.audio_live);
        assert!(args.audio_wav.is_none());
        validate_benchmark_args(&args)?;
        Ok(())
    }

    #[test]
    fn benchmark_until_preserves_explicit_live_audio() -> Result<()> {
        let args = parse(&["--benchmark-until", "85.4", "--audio"])?;
        assert_eq!(args.benchmark_until, Some(85.4));
        assert!(args.audio_live);
        validate_benchmark_args(&args)?;
        Ok(())
    }

    #[test]
    fn benchmark_until_rejects_window_scheduled_work() -> Result<()> {
        let args = parse(&["--benchmark-until", "85.4", "--press-after", "1.0", "ctrl"])?;
        let err = validate_benchmark_args(&args).unwrap_err();
        assert!(err.to_string().contains("scheduled input"), "{err:#}");

        let args = parse(&["--benchmark-until", "85.4", "--profile-live-audio", "0.1"])?;
        let err = validate_benchmark_args(&args).unwrap_err();
        assert!(err.to_string().contains("--profile-live-audio"), "{err:#}");

        let args = parse(&[
            "--benchmark-until",
            "85.4",
            "--screenshot-after",
            "85.4",
            "/tmp/x",
        ])?;
        let err = validate_benchmark_args(&args).unwrap_err();
        assert!(err.to_string().contains("--screenshot-after"), "{err:#}");
        Ok(())
    }

    #[test]
    fn gdb_mode_parses_and_defaults_to_null_audio() -> Result<()> {
        let args = parse(&["--gdb", ":2345"])?;
        assert_eq!(args.gdb.as_deref(), Some(":2345"));
        assert!(!args.audio_live);
        validate_gdb_args(&args)?;
        Ok(())
    }

    #[test]
    fn gdb_gui_parses_and_excludes_the_contending_modes() -> Result<()> {
        // The windowed stub keeps live audio: the machine stays paced
        // and audible like any interactive session.
        let args = parse(&["--gdb-gui", ":2345"])?;
        assert_eq!(args.gdb_gui.as_deref(), Some(":2345"));
        assert!(args.audio_live);
        validate_gdb_args(&args)?;
        validate_control_args(&args)?;

        let args = parse(&["--gdb-gui", ":2345", "--gdb", ":2346"])?;
        let err = validate_gdb_args(&args).unwrap_err();
        assert!(err.to_string().contains("cannot be combined"), "{err:#}");

        let args = parse(&["--gdb-gui", ":2345", "--benchmark-until", "5"])?;
        let err = validate_gdb_args(&args).unwrap_err();
        assert!(err.to_string().contains("--benchmark-until"), "{err:#}");

        // The two windowed forms share one interactive session; the
        // headless forms each own the machine and still exclude the other.
        let args = parse(&["--gdb-gui", ":2345", "--control-gui", ":7710"])?;
        validate_gdb_args(&args)?;
        validate_control_args(&args)?;
        let args = parse(&["--gdb-gui", ":2345", "--control", ":7710"])?;
        let err = validate_control_args(&args).unwrap_err();
        assert!(err.to_string().contains("--control cannot"), "{err:#}");
        let args = parse(&["--control-gui", ":7710", "--gdb", ":2345"])?;
        let err = validate_control_args(&args).unwrap_err();
        assert!(err.to_string().contains("--gdb"), "{err:#}");
        Ok(())
    }

    #[test]
    fn gdb_mode_rejects_window_scheduled_work() -> Result<()> {
        let args = parse(&["--gdb", ":2345", "--press-after", "1.0", "ctrl"])?;
        let err = validate_gdb_args(&args).unwrap_err();
        assert!(err.to_string().contains("scheduled input"), "{err:#}");

        let args = parse(&[
            "--gdb",
            ":2345",
            "--screenshot-after",
            "1.0",
            "/tmp/gdb.png",
        ])?;
        let err = validate_gdb_args(&args).unwrap_err();
        assert!(err.to_string().contains("--screenshot-after"), "{err:#}");
        Ok(())
    }

    /// An unsupported serving speed is refused where it is typed, naming
    /// the values that work, rather than surfacing later from config parsing.
    #[cfg(feature = "fluxbridge")]
    #[test]
    fn floppy_bridge_speed_flag_refuses_an_unsupported_percentage() {
        let err =
            parse(&["--floppy-replay-speed", "df0", "120"]).expect_err("120 is not a replay speed");
        let msg = format!("{err:#}");
        assert!(msg.contains("normal"), "unexpected error: {msg}");
    }

    /// A real drive can be asked for entirely from the command line, with no
    /// config file: the flags have to create the bay's table, not just fill
    /// one in.
    // The flags only exist in a build that can attach a physical drive.
    #[cfg(feature = "fluxbridge")]
    #[test]
    fn floppy_bridge_flags_configure_a_bay_with_no_config_file() -> Result<()> {
        let args = parse(&[
            "--floppy-bridge",
            "df1",
            "greaseweazle",
            "--floppy-bridge-port",
            "df1",
            "/dev/ttyACM0",
            "--floppy-bridge-cable",
            "df1",
            "b",
            "--floppy-bridge-writable",
            "df1",
            "--floppy-bridge-mode",
            "df1",
            "compatible",
            "--floppy-bridge-density",
            "df1",
            "dd",
            "--floppy-replay-speed",
            "df1",
            "fast",
        ])?;

        let raw = Config::load_raw(None, &args.overrides)?;
        let cfg = Config::try_from(raw)?;
        let bridge = cfg.floppy.bridges[1].as_ref().expect("df1 bridged");
        assert_eq!(
            bridge.driver,
            copperline::config::BridgeDriver::Greaseweazle
        );
        assert_eq!(bridge.port.as_deref(), Some("/dev/ttyACM0"));
        assert_eq!(bridge.cable, copperline::config::BridgeCable::DriveB);
        assert!(!bridge.write_protected);
        assert_eq!(bridge.mode, copperline::config::BridgeReadMode::Compatible);
        assert_eq!(bridge.density, copperline::config::BridgeDensity::Dd);
        assert_eq!(bridge.speed, 200);
        // Untouched bays stay as they were.
        assert!(cfg.floppy.bridges[0].is_none());
        Ok(())
    }

    /// The flag means "this bay is a real drive", so it displaces an image the
    /// config file put there rather than colliding with it -- a bay cannot
    /// hold both, and the command line wins.
    /// `load_config` must put `[paths]` in force *before* building the
    /// machine: the conversion resolves the implicit battery-RAM backing
    /// files through the paths in force, so adopting afterwards would site
    /// this run's NVRAM by the previous answer. Adopting after the
    /// conversion fails this.
    ///
    /// No lock is taken: this is the binary's own test process, and it is
    /// the only test in it that adopts.
    #[test]
    fn a_configs_paths_are_in_force_before_the_machine_is_built() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "copperline-paths-order-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            "[machine]\nprofile = \"A4000\"\n\n[paths]\nnvram = \"elsewhere\"\n",
        )?;
        let loaded = load_config(Some(&path), &ConfigOverrides::default(), true);
        let _ = std::fs::remove_file(&path);
        let (cfg, _) = loaded?;
        let battmem = cfg.battmem_path.expect("an A4000 fits an RP5C01");
        // A host with no per-user directory keeps the bare name, which is
        // the documented degradation and has no directory to check.
        if copperline::paths::config_dir().is_some() {
            assert!(
                battmem.parent().is_some_and(|p| p.ends_with("elsewhere")),
                "the config's [paths] was adopted too late: {battmem:?}"
            );
        }
        copperline::paths::adopt(Default::default());
        Ok(())
    }

    // The flags only exist in a build that can attach a physical drive.
    #[cfg(feature = "fluxbridge")]
    #[test]
    fn floppy_bridge_flag_replaces_a_configured_image() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "copperline-bridge-cli-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            "[floppy.df0]\npath = \"workbench.adf\"\nwrite_protected = true\n",
        )?;

        let args = parse(&["--floppy-bridge", "df0", "greaseweazle"])?;
        let raw = Config::load_raw(Some(&path), &args.overrides)?;
        let cfg = Config::try_from(raw)?;
        let _ = std::fs::remove_file(&path);

        assert!(cfg.floppy.bridges[0].is_some(), "the bay is a real drive");
        assert!(cfg.floppy.drives[0].is_none(), "and has no image left");
        Ok(())
    }

    #[test]
    fn frame_dump_options_parse() -> Result<()> {
        let args = parse(&[
            "--dump-frames",
            "/tmp/frontier-clouds",
            "--dump-start",
            "18.5",
            "--dump-count",
            "42",
        ])?;
        assert_eq!(
            args.frame_dump,
            Some(FrameDumpSpec {
                dir: PathBuf::from("/tmp/frontier-clouds"),
                start_secs: 18.5,
                count: 42,
            })
        );
        Ok(())
    }

    #[test]
    fn frame_dump_requires_count_and_directory() {
        let err = parse(&["--dump-frames", "/tmp/frontier-clouds"]).unwrap_err();
        assert!(
            err.to_string().contains("--dump-count"),
            "unexpected error: {err:#}"
        );

        let err = parse(&["--dump-count", "10"]).unwrap_err();
        assert!(
            err.to_string().contains("--dump-frames"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn insert_disk_after_parses_explicit_drive_and_path() -> Result<()> {
        let args = parse(&["--insert-disk-after", "10", "df0", "demo-disk.adf"])?;
        assert_eq!(
            args.disk_insert_after,
            vec![CliDiskInsert::Explicit(DiskInsertSpec {
                secs: 10.0,
                drive_idx: 0,
                path: PathBuf::from("demo-disk.adf"),
                write_protected: true,
            })]
        );
        Ok(())
    }

    #[test]
    fn floppy_drive_count_override_parses_with_alias() -> Result<()> {
        assert_eq!(
            parse(&["--floppy-drives", "2"])?.overrides.floppy_drives,
            Some(2)
        );
        assert_eq!(
            parse(&["--fdd-drives", "4"])?.overrides.floppy_drives,
            Some(4)
        );
        assert_eq!(
            parse(&["--floppy-drives", "0"])?.overrides.floppy_drives,
            Some(0)
        );
        let err = parse(&["--floppy-drives", "5"]).unwrap_err();
        assert!(err.to_string().contains("from 0 to 4"), "{err:#}");
        Ok(())
    }

    #[test]
    fn floppy_speed_override_parses_with_alias() -> Result<()> {
        assert_eq!(
            parse(&["--floppy-speed", "800"])?.overrides.floppy_speed,
            Some(800)
        );
        // 0 selects turbo.
        assert_eq!(
            parse(&["--fdd-speed", "0"])?.overrides.floppy_speed,
            Some(0)
        );
        let err = parse(&["--floppy-speed", "150"]).unwrap_err();
        assert!(err.to_string().contains("100, 200, 400, 800"), "{err:#}");
        Ok(())
    }

    #[test]
    fn defer_disk_insert_parses_configured_drive() -> Result<()> {
        let args = parse(&["--defer-disk-insert", "10", "df0:"])?;
        assert_eq!(
            args.disk_insert_after,
            vec![CliDiskInsert::Configured {
                secs: 10.0,
                drive_idx: 0,
            }]
        );
        Ok(())
    }

    #[test]
    fn deferred_configured_disk_insert_starts_drive_empty() -> Result<()> {
        let mut cfg = Config::default();
        cfg.floppy.drives[0] = Some(copperline::config::FloppyDriveConfig {
            path: PathBuf::from("demo-disk.adf"),
            write_protected: true,
        });

        let inserts = resolve_disk_insert_after(
            &mut cfg,
            vec![CliDiskInsert::Configured {
                secs: 10.0,
                drive_idx: 0,
            }],
        )?;

        assert!(cfg.floppy.drives[0].is_none());
        assert_eq!(
            inserts,
            vec![DiskInsertSpec {
                secs: 10.0,
                drive_idx: 0,
                path: PathBuf::from("demo-disk.adf"),
                write_protected: true,
            }]
        );
        Ok(())
    }

    #[test]
    fn scheduled_disk_insert_requires_connected_drive() {
        let mut cfg = Config::default();
        let err = resolve_disk_insert_after(
            &mut cfg,
            vec![CliDiskInsert::Explicit(DiskInsertSpec {
                secs: 10.0,
                drive_idx: 1,
                path: PathBuf::from("demo-disk.adf"),
                write_protected: true,
            })],
        )
        .unwrap_err();
        assert!(err.to_string().contains("connected drive"), "{err:#}");

        cfg.floppy_connected[1] = true;
        let inserts = resolve_disk_insert_after(
            &mut cfg,
            vec![CliDiskInsert::Explicit(DiskInsertSpec {
                secs: 10.0,
                drive_idx: 1,
                path: PathBuf::from("demo-disk.adf"),
                write_protected: true,
            })],
        )
        .unwrap();
        assert_eq!(inserts[0].drive_idx, 1);
    }

    #[test]
    fn press_after_accepts_named_keys_with_default_hold() -> Result<()> {
        let args = parse(&["--press-after", "1.5", "ctrl"])?;
        assert_eq!(
            args.press_after,
            vec![KeyPressSpec {
                secs: 1.5,
                rawkey: 0x63,
                hold_ms: DEFAULT_KEY_HOLD_MS,
            }]
        );
        Ok(())
    }

    #[test]
    fn key_after_accepts_named_modifier_and_hold_duration() -> Result<()> {
        let args = parse(&["--key-after", "2.0", "lami", "750"])?;
        assert_eq!(
            args.press_after,
            vec![KeyPressSpec {
                secs: 2.0,
                rawkey: 0x66,
                hold_ms: 750,
            }]
        );
        Ok(())
    }

    #[test]
    fn press_after_still_accepts_raw_numeric_keys() -> Result<()> {
        let args = parse(&["--press-after", "1.0", "0x04"])?;
        assert_eq!(args.press_after[0].rawkey, 0x04);
        Ok(())
    }

    #[test]
    fn machine_override_flags_parse_into_config_overrides() -> Result<()> {
        let args = parse(&[
            "--model",
            "A1200",
            "--cpu",
            "68030",
            "--cpu-clock",
            "50",
            "--fpu",
            "--chip",
            "2M",
            "--fast",
            "8M",
            "--slow",
            "512K",
            "--ram-init",
            "pattern:0x5555",
            "--floppy-drives",
            "3",
            "--chipset",
            "AGA",
            "--video",
            "NTSC",
            "--jit",
        ])?;
        assert_eq!(args.overrides.model.as_deref(), Some("A1200"));
        assert_eq!(args.overrides.cpu.as_deref(), Some("68030"));
        assert_eq!(args.overrides.cpu_clock_mhz, Some(50.0));
        assert_eq!(args.overrides.fpu, Some(true));
        assert_eq!(args.overrides.cpu_jit, Some(true));
        assert_eq!(args.overrides.chip.as_deref(), Some("2M"));
        assert_eq!(args.overrides.fast.as_deref(), Some("8M"));
        assert_eq!(args.overrides.slow.as_deref(), Some("512K"));
        assert_eq!(args.overrides.ram_init.as_deref(), Some("pattern:0x5555"));
        assert_eq!(args.overrides.floppy_drives, Some(3));
        assert_eq!(args.overrides.chipset.as_deref(), Some("AGA"));
        assert_eq!(args.overrides.video.as_deref(), Some("NTSC"));
        Ok(())
    }

    #[test]
    fn no_fpu_flag_sets_override_false_and_default_is_unset() -> Result<()> {
        assert_eq!(parse(&[])?.overrides.fpu, None);
        assert_eq!(parse(&["--no-fpu"])?.overrides.fpu, Some(false));
        Ok(())
    }

    #[test]
    fn cpu_clock_rejects_non_numeric() {
        let err = parse(&["--cpu-clock", "fast"]).unwrap_err();
        assert!(err.to_string().contains("--cpu-clock"), "{err:#}");
    }
}

#[cfg(test)]
mod netplay_cli_tests {
    use super::*;

    fn args(extra: &[&str]) -> Result<CliArgs> {
        let mut args = vec![
            "--netplay-bind",
            "127.0.0.1:19732",
            "--netplay-peer",
            "127.0.0.1:19733",
            "--netplay-player",
            "1",
            "--netplay-session",
            "0123456789abcdef0123456789abcdef",
        ];
        args.extend_from_slice(extra);
        parse_args_from(args.into_iter().map(str::to_string))
    }

    #[test]
    fn netplay_defaults_and_headless_local_inputs() -> Result<()> {
        let cli = args(&[
            "--noaudio",
            "--screenshot-after",
            "2",
            "/tmp/netplay.png",
            "--joy-after",
            "1",
            "red",
            "100",
            "1",
            "--press-after",
            "0",
            "space",
            "--insert-disk-after",
            "0",
            "df0",
            "game.adf",
        ])?;
        let options = cli.netplay.as_ref().unwrap().settings();
        assert_eq!(options.player, 0);
        assert_eq!(options.input_delay, 2);
        assert_eq!(options.rollback_frames, 8);
        assert!(!launcher_requested(&cli));
        assert_eq!(
            args(&["--netplay-delay", "0", "--netplay-rollback", "12"])?
                .netplay
                .unwrap()
                .settings()
                .input_delay,
            0
        );
        Ok(())
    }

    #[test]
    fn netplay_rejects_incomplete_invalid_and_unilateral_operations() {
        assert!(parse_args_from(["--netplay-delay".to_string(), "2".to_string()]).is_err());
        for extra in [
            vec!["--netplay-player", "0"],
            vec!["--netplay-player", "3"],
            vec!["--netplay-delay", "7"],
            vec!["--netplay-rollback", "0"],
            vec!["--netplay-rollback", "13"],
            vec!["--netplay-session", "bad"],
            vec!["--netplay-peer", "0.0.0.0:19732"],
            vec!["--load-state", "saved.clstate"],
            vec!["--run", "program"],
            vec!["--whdload", "game.lha"],
            vec!["--warp-boot"],
            vec!["--control", ":1234"],
            vec!["--audio-wav", "/tmp/netplay.wav"],
            vec!["--insert-disk-after", "1", "df0", "disk.adf"],
            vec!["--joy-after", "1", "red", "100", "2"],
            vec!["--mouse-after", "1", "3", "4"],
        ] {
            assert!(args(&extra).is_err(), "accepted {extra:?}");
        }
    }
}

#[cfg(all(test, feature = "netplay-internet"))]
mod internet_netplay_cli_tests {
    use super::*;

    #[test]
    fn host_and_join_negotiate_roles_and_reject_conflicting_cli_settings() -> Result<()> {
        let parse = |args: &[&str]| parse_args_from(args.iter().map(|arg| arg.to_string()));
        let host = parse(&[
            "--netplay-host",
            "/tmp/invitation.txt",
            "--netplay-delay",
            "4",
            "--netplay-relay-only",
        ])?;
        assert_eq!(
            host.netplay_invitation_out.as_deref(),
            Some(std::path::Path::new("/tmp/invitation.txt"))
        );
        assert!(!launcher_requested(&host));
        let Some(copperline::netplay::ConnectionOptions::Internet(options)) = host.netplay else {
            panic!("expected Internet options")
        };
        assert!(options.relay_only);
        assert_eq!(options.settings().player, 0);
        let code = options.invitation.encode()?;
        let join = parse(&["--netplay-join", &code])?;
        assert_eq!(join.netplay.as_ref().unwrap().settings().player, 1);
        assert_eq!(join.netplay.as_ref().unwrap().settings().input_delay, 4);
        for args in [
            vec![
                "--netplay-host",
                "/tmp/invitation.txt",
                "--netplay-join",
                &code,
            ],
            vec![
                "--netplay-host",
                "/tmp/invitation.txt",
                "--netplay-peer",
                "127.0.0.1:1",
            ],
            vec![
                "--netplay-host",
                "/tmp/invitation.txt",
                "--netplay-relay",
                "http://relay.test",
            ],
            vec!["--netplay-join", &code, "--netplay-delay", "2"],
            vec!["--netplay-join", &code, "--netplay-player", "2"],
            vec!["--netplay-join", &code, "--control", ":0"],
            vec!["--netplay-join", "bad"],
            vec!["--netplay-relay-only"],
        ] {
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
        Ok(())
    }
}
