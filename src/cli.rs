// SPDX-License-Identifier: GPL-3.0-or-later

//! Command-line argument parsing for the Copperline binary: the flag
//! grammar, `--script` file expansion, and the parsed `CliArgs` shape.

use anyhow::{anyhow, bail, Result};
use std::path::PathBuf;

use copperline::config::ConfigOverrides;
use copperline::video::window::{
    parse_amiga_key, DiskInsertSpec, FrameDumpSpec, KeyPressSpec, DEFAULT_KEY_HOLD_MS,
};
use copperline::video::HOST_SHORTCUT_MODIFIER_LABEL;

#[derive(Debug)]
pub struct CliArgs {
    pub netplay: Option<copperline::netplay::Options>,
    pub config_path: Option<PathBuf>,
    pub rom_path: Option<PathBuf>,
    /// `--whdload GAME`: stage a WHDLoad package (.lha archive or
    /// directory) and boot straight into it (src/whdload.rs).
    pub whdload: Option<PathBuf>,
    /// `--run PROG`: warp launch -- stage a minimal boot volume around an
    /// ordinary Amiga executable on the host and boot straight into it
    /// (src/runprog.rs).
    pub run: Option<PathBuf>,
    /// `--run-args STRING`: extra guest command-line arguments appended to
    /// the `--run` program's invocation.
    pub run_args: Option<String>,
    /// `--run-stack BYTES`: issue `Stack BYTES` before the run program.
    pub run_stack: Option<u32>,
    /// `--run-detach`: start the run program asynchronously and close its CLI.
    pub run_detach: bool,
    /// `--warp-boot`: warp boot -- run unpaced from power-on until the
    /// boot storage has been idle for `[emulation] warp_boot_idle`
    /// emulated seconds, then resume real-time pacing (src/warpboot.rs).
    pub warp_boot: bool,
    /// `--warp-until SECS`: warp boot until an absolute emulated
    /// timestamp instead of the storage-idle heuristic.
    pub warp_until: Option<f64>,
    /// `--screenshot-after SECS PATH`: save the framebuffer after SECS
    /// emulated seconds. Repeatable, like the scheduled-input flags: every
    /// occurrence is captured, and the run ends once the last one has
    /// fired.
    pub screenshot_after: Vec<(f32, PathBuf)>,
    /// `--save-state-after SECS PATH`: write a save state of the whole
    /// machine after SECS emulated seconds, then keep running (combine
    /// with --screenshot-after/--dump-frames to bound the run).
    /// Repeatable, like `--screenshot-after`.
    pub save_state_after: Vec<(f32, PathBuf)>,
    /// `--load-state PATH`: restore a save state before entering the
    /// event loop, resuming from its emulated timeline.
    pub load_state: Option<PathBuf>,
    pub load_uss: Option<PathBuf>,
    /// `--benchmark-until SECS`: run frames directly, without opening a
    /// window, until the absolute emulated-time target is reached.
    pub benchmark_until: Option<f32>,
    /// `--gdb ADDR`: run a headless GDB remote-protocol server on ADDR,
    /// `:PORT`, or `PORT`, pausing at reset until the debugger resumes.
    /// Held as the listen address so the struct has one shape whether or
    /// not the `gdb` feature is compiled in; rejected at validation when
    /// it is not, like the control flags.
    pub gdb: Option<String>,
    /// `--gdb-gui ADDR`: attach a GDB remote-protocol server to the
    /// normal windowed session instead of owning the machine, the way
    /// `--control-gui` attaches the control server.
    pub gdb_gui: Option<String>,
    pub gdb_dialect: Option<String>,
    /// `--control ADDR`: run the headless Copperline Control Protocol
    /// server (JSON-RPC over loopback TCP), pausing at reset until a
    /// client resumes. `--control-token`/`--control-info` refine it.
    /// Kept as the raw listen address so the CLI parses without the
    /// `control` feature; a build without it rejects the flags in
    /// validation, and `main` assembles the server config at dispatch.
    pub control: Option<String>,
    /// `--control-gui ADDR`: attach a control server to the normal
    /// windowed session instead of owning the machine.
    pub control_gui: Option<String>,
    /// `--control-token TOKEN` / `--control-info PATH` for either mode.
    pub control_token: Option<String>,
    pub control_info: Option<PathBuf>,
    /// Dump consecutive rendered frames after an emulated-time delay. This
    /// is intended for debugging flicker and frame-to-frame palette
    /// changes that a single screenshot cannot show.
    pub frame_dump: Option<FrameDumpSpec>,
    /// `--waveform PATH` (+ `--wave-trigger/--wave-duration/--wave-signals`):
    /// arm a trigger-based VCD logic-analyser capture of internal chipset
    /// signals for GTKWave (see docs/debugger/waveform.md).
    pub waveform: Option<copperline::waveform::WaveOptions>,
    /// Scripted key presses to inject after the window opens. Useful
    /// for headless testing of menus and modifier chords.
    pub press_after: Vec<KeyPressSpec>,
    /// `--click-after SECS BUTTON DURATION_MS [PORT]`: at SECS seconds
    /// after the window opens, press the named mouse button
    /// (left/right/middle), hold for DURATION_MS, then release. The
    /// optional trailing PORT (1 or 2) names the controller port,
    /// defaulting to 1; the tuple carries it 0-based. Useful for headless
    /// testing of the mouse-button-driven wait prompts.
    pub click_after: Vec<(f32, MouseButtonKind, u32, u8)>,
    /// `--joy-after SECS BUTTON DURATION_MS [PORT]`: at SECS emulated
    /// seconds, press a joystick / CD32-pad control (up/down/left/right/
    /// red|fire/blue/green/yellow/play/rwd/ffw), hold for DURATION_MS,
    /// then release. PORT defaults to 2 (carried 0-based). Useful for
    /// headless testing of joystick-driven titles, especially CD32 games
    /// whose pad otherwise needs a calibrated physical gamepad.
    pub joy_after: Vec<(f32, JoyButtonKind, u32, u8)>,
    /// `--mouse-after SECS DX DY [PORT]`: at SECS emulated seconds, apply
    /// a relative mouse motion of (DX, DY) counter steps. PORT defaults
    /// to 1 (carried 0-based). Emitted by the input recorder one event
    /// per frame of recorded movement.
    pub mouse_after: Vec<(f32, i32, i32, u8)>,
    /// `--mouse-to-after SECS X Y [PORT]`: at SECS emulated seconds,
    /// servo the guest pointer to presented-pixel (X, Y) -- the same
    /// coordinates a screenshot is measured in -- by watching sprite 0
    /// and correcting relative motion until it lands. PORT defaults to 1
    /// (carried 0-based). See `src/pointer.rs` for why absolute pointer
    /// positioning has to be closed-loop.
    pub mouse_to_after: Vec<(f32, i32, i32, u8)>,
    /// `--pot-after SECS X Y [PORT]`: at SECS emulated seconds, set an
    /// analogue controller's stick/paddle position (each axis 0-255, the
    /// count POTxDAT latches). PORT defaults to 2 (carried 0-based).
    pub pot_after: Vec<(f32, u8, u8, u8)>,
    /// `--record-input PATH`: record every input event that reaches the
    /// emulated machine for the whole run and write the scripted-input
    /// file to PATH on exit (the windowed toggle is the host shortcut
    /// modifier plus Shift+R).
    pub record_input: Option<PathBuf>,
    /// Scripted floppy image insertion. This supports both explicit
    /// paths and deferring a disk image already configured in the TOML.
    pub disk_insert_after: Vec<CliDiskInsert>,
    /// Scripted CD swaps: (SECS, image path) pairs from --insert-cd-after,
    /// landing in whichever CD drive the machine has (CDTV, CD32, or a
    /// SCSI CD-ROM unit).
    pub cd_insert_after: Vec<(f32, PathBuf)>,
    /// `--freeze-after SECS`: press the freezer cartridge's button at SECS
    /// emulated seconds (`[cartridge] model`), one-shot each.
    pub freeze_after: Vec<f32>,
    /// Real-time stereo audio output through cpal. Enabled by default;
    /// `--noaudio` disables it, and `--audio-wav` selects WAV output.
    pub audio_live: bool,
    /// Whether `--audio` was passed explicitly. When set, live audio is forced
    /// on regardless of `[audio] output_enabled`; otherwise that config key (the
    /// GUI "Disabled" option) can turn default-on audio off.
    pub audio_live_forced: bool,
    /// `--audio-wav PATH`: dump the mixed stereo output to a WAV file
    /// (32-bit float, 44100 Hz). No live output. Useful for headless
    /// verification of the audio path.
    pub audio_wav: Option<PathBuf>,
    /// `--audio-stems DIR`: write per-granularity stem WAVs (selected by
    /// `--audio-stems-mode`) into DIR instead of a single mixed file. No
    /// live output; mutually exclusive with `--audio-wav`/`--audio`.
    pub audio_stems: Option<PathBuf>,
    /// `--audio-stems-mode LIST`: comma-separated granularities to write
    /// under `--audio-stems` (`master`, `source`, `channel`, combinable).
    pub audio_stems_mode: Option<Vec<copperline::audio::mux::StemGranularity>>,
    /// `--profile-live-audio SECS`: run a no-window Paula-to-cpal
    /// profile workload for SECS seconds. Use COPPERLINE_AUDIO_PROFILE=1
    /// to emit the live-audio counters while it runs.
    pub live_audio_profile_secs: Option<f32>,
    /// `--calibrate-gamepad`: run the interactive gamepad calibration and
    /// exit, without starting the emulator.
    pub calibrate_gamepad: bool,
    /// `--list-midi`: print the host MIDI endpoints and exit.
    pub list_midi: bool,
    /// `--list-audio-devices`: print the host audio output devices and exit.
    pub list_audio_devices: bool,
    /// `--list-net-interfaces`: print adapters usable for bridging and exit.
    pub list_net_interfaces: bool,
    pub list_disks: bool,
    /// The privileged half of the host-disk opener, and everything the
    /// unprivileged half wrote for it. What the words mean is private to those
    /// two halves and differs by host -- Windows names a process and a file to
    /// answer in, Linux a socket to answer down -- so they are kept as written
    /// and handed to the backend to read. Set only by this same program, which
    /// is why it is absent from `--help`.
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    pub host_disk_broker: Option<Vec<String>>,
    /// Linux companion helper setup action: install, uninstall, or status.
    pub net_helper_action: Option<String>,
    /// `--sampler-list-audio-inputs`: print the host audio input devices (for
    /// `--sampler-audio-input`) and exit.
    pub list_sampler_inputs: bool,
    /// Command-line machine overrides (`--model`, `--chipset`, `--cpu`,
    /// `--fpu`/`--no-fpu`, `--cpu-clock`, `--chip`, `--fast`, `--slow`,
    /// `--ram-init`, `--floppy-drives`).
    /// Applied on top of the config file (or the built-in defaults) before
    /// validation.
    pub overrides: ConfigOverrides,
    /// `--factory`: start from Copperline's own defaults, ignoring a
    /// configuration saved with Save default. An explicit `--config` still
    /// wins -- this says "not the saved default", not "no configuration".
    pub factory: bool,
}

use copperline::video::window::{JoyButtonKind, MouseButtonKind};

#[derive(Debug, Clone, PartialEq)]
pub enum CliDiskInsert {
    Explicit(DiskInsertSpec),
    Configured { secs: f32, drive_idx: usize },
}

pub fn parse_args() -> Result<CliArgs> {
    parse_args_from(std::env::args().skip(1))
}

/// Scripted-input directives accepted inside a `--script` file. These are
/// the flag names (without the leading dashes) whose effects accumulate;
/// anything else in a script is an error so a typo cannot silently change
/// emulator configuration.
const SCRIPT_DIRECTIVES: [&str; 12] = [
    "press-after",
    "key-after",
    "hold-key-after",
    "click-after",
    "joy-after",
    "mouse-after",
    "mouse-to-after",
    "pot-after",
    "insert-disk-after",
    "defer-disk-insert",
    "insert-cd-after",
    "freeze-after",
];

/// Split one script line into tokens: whitespace-separated, with
/// double-quoted tokens allowed to carry spaces (for disk-image paths).
fn tokenize_script_line(line: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut tok = String::new();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some(c) => tok.push(c),
                    None => return Err(anyhow!("unterminated quote in script line {line:?}")),
                }
            }
            tokens.push(tok);
        } else {
            let mut tok = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                tok.push(c);
                chars.next();
            }
            tokens.push(tok);
        }
    }
    Ok(tokens)
}

/// Expand every `--script FILE` argument in place: each non-empty,
/// non-`#` line of the file is a scripted-input directive in the flag
/// syntax without the leading dashes (`key-after 14.0 ctrl 500`), and
/// expands to the equivalent flags for the main parser. Scripts cannot
/// include other scripts.
fn expand_script_files(args: Vec<String>) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg != "--script" {
            out.push(arg);
            continue;
        }
        let path = iter
            .next()
            .ok_or_else(|| anyhow!("--script requires a path"))?;
        let text =
            std::fs::read_to_string(&path).map_err(|e| anyhow!("reading script {path}: {e}"))?;
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let tokens = tokenize_script_line(line)?;
            let Some((directive, rest)) = tokens.split_first() else {
                continue;
            };
            if !SCRIPT_DIRECTIVES.contains(&directive.as_str()) {
                return Err(anyhow!(
                    "{path}:{}: {directive:?} is not a scripted-input directive \
                     (allowed: {})",
                    lineno + 1,
                    SCRIPT_DIRECTIVES.join(", ")
                ));
            }
            out.push(format!("--{directive}"));
            out.extend(rest.iter().cloned());
        }
    }
    Ok(out)
}

/// Parse the next CLI argument as `T`: the common shape behind most of
/// this parser's `--flag VALUE` options, which otherwise each repeat the
/// same "missing argument" / "not a valid value" error-handling pair.
/// `missing` names what the whole flag needs when no argument follows;
/// `invalid` explains what shape this particular value must have.
fn next_arg<T: std::str::FromStr>(
    args: &mut impl Iterator<Item = String>,
    missing: &str,
    invalid: &str,
) -> Result<T> {
    args.next()
        .ok_or_else(|| anyhow!("{missing}"))?
        .parse()
        .map_err(|_| anyhow!("{invalid}"))
}

/// Consume the optional trailing PORT token (exactly "1" or "2") a
/// scripted-input flag accepts after its fixed arguments, returning the
/// 0-based port index; anything else leaves the token for the main loop
/// and yields the flag's traditional default port. (A positional ROM/disk
/// path literally named "1" or "2" therefore cannot directly follow one
/// of these flags; name it "./1" instead.)
fn take_port_token(
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    default_port: u8,
) -> u8 {
    match args.peek().map(String::as_str) {
        Some("1") => {
            args.next();
            0
        }
        Some("2") => {
            args.next();
            1
        }
        _ => default_port - 1,
    }
}

pub fn parse_args_from<I>(args: I) -> Result<CliArgs>
where
    I: IntoIterator<Item = String>,
{
    let args = expand_script_files(args.into_iter().collect())?;
    let mut netplay_bind = None;
    let mut netplay_peer = None;
    let mut netplay_player = None;
    let mut netplay_session = None;
    let mut netplay_delay = None;
    let mut netplay_rollback = None;
    let mut config_path: Option<PathBuf> = None;
    let mut rom_path: Option<PathBuf> = None;
    let mut whdload: Option<PathBuf> = None;
    let mut run: Option<PathBuf> = None;
    let mut warp_boot = false;
    let mut warp_until: Option<f64> = None;
    let mut run_args: Option<String> = None;
    let mut run_stack: Option<u32> = None;
    let mut run_detach = false;
    let mut screenshot_after: Vec<(f32, PathBuf)> = Vec::new();
    let mut save_state_after: Vec<(f32, PathBuf)> = Vec::new();
    let mut load_state: Option<PathBuf> = None;
    let mut load_uss = None;
    let mut benchmark_until: Option<f32> = None;
    let mut gdb: Option<String> = None;
    let mut gdb_gui: Option<String> = None;
    let mut gdb_dialect = None;
    let mut control_listen: Option<String> = None;
    let mut control_gui_listen: Option<String> = None;
    let mut control_token: Option<String> = None;
    let mut control_info: Option<PathBuf> = None;
    let mut dump_dir: Option<PathBuf> = None;
    let mut dump_start_secs: f32 = 0.0;
    let mut dump_count: Option<u32> = None;
    let mut press_after: Vec<KeyPressSpec> = Vec::new();
    let mut click_after: Vec<(f32, MouseButtonKind, u32, u8)> = Vec::new();
    let mut joy_after: Vec<(f32, JoyButtonKind, u32, u8)> = Vec::new();
    let mut mouse_after: Vec<(f32, i32, i32, u8)> = Vec::new();
    let mut mouse_to_after: Vec<(f32, i32, i32, u8)> = Vec::new();
    let mut pot_after: Vec<(f32, u8, u8, u8)> = Vec::new();
    let mut record_input: Option<PathBuf> = None;
    let mut wave_path: Option<PathBuf> = None;
    let mut wave_trigger: Option<copperline::waveform::Trigger> = None;
    let mut wave_duration: Option<copperline::waveform::WaveDuration> = None;
    let mut wave_signals: Option<copperline::waveform::SignalSet> = None;
    let mut disk_insert_after: Vec<CliDiskInsert> = Vec::new();
    let mut cd_insert_after: Vec<(f32, PathBuf)> = Vec::new();
    let mut freeze_after: Vec<f32> = Vec::new();
    let mut factory = false;
    let mut audio_live = true;
    let mut explicit_audio_live = false;
    let mut explicit_noaudio = false;
    let mut audio_wav: Option<PathBuf> = None;
    let mut audio_stems: Option<PathBuf> = None;
    let mut audio_stems_mode: Option<Vec<copperline::audio::mux::StemGranularity>> = None;
    let mut live_audio_profile_secs: Option<f32> = None;
    let mut calibrate_gamepad = false;
    let mut list_midi = false;
    let mut list_audio_devices = false;
    let mut list_net_interfaces = false;
    let mut list_disks = false;
    // Only the hosts with a privileged half of their own ever fill this in;
    // macOS has `authopen` and needs none.
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(unused_mut))]
    let mut host_disk_broker: Option<Vec<String>> = None;
    let mut net_helper_action: Option<String> = None;
    let mut list_sampler_inputs = false;
    let mut overrides = ConfigOverrides::default();
    let mut args = args.into_iter().peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--calibrate-gamepad" => {
                calibrate_gamepad = true;
            }
            "--list-midi" => {
                list_midi = true;
            }
            "--list-audio-devices" => {
                list_audio_devices = true;
            }
            "--list-net-interfaces" => {
                list_net_interfaces = true;
            }
            "--list-disks" => {
                list_disks = true;
            }
            // Copperline talking to itself across a privilege boundary, not an
            // interface: the unprivileged half writes this command line, so
            // everything after the flag belongs to the backend that will read
            // it, and opening those disks is the whole of what this process
            // does -- nothing else can follow.
            #[cfg(any(windows, target_os = "linux"))]
            copperline::blockdev::BROKER_FLAG => {
                let rest: Vec<String> = args.by_ref().collect();
                if rest.is_empty() {
                    return Err(anyhow!(
                        "{} is used by Copperline itself",
                        copperline::blockdev::BROKER_FLAG
                    ));
                }
                host_disk_broker = Some(rest);
            }
            "--host-disk" | "--host-disk-read-only" => {
                let read_only = a == "--host-disk-read-only";
                let usage = format!("{a} requires DEVICE (and optionally an attachment point)");
                let device = args.next().ok_or_else(|| anyhow!(usage))?;
                // The attachment point is optional and only ever one of a
                // known set, so a following token that is not one is the next
                // flag rather than a mistake.
                let takes_attach = args.peek().is_some_and(|next| {
                    copperline::config::HostDiskAttach::all()
                        .iter()
                        .any(|a| a.token().eq_ignore_ascii_case(next))
                });
                let attach = takes_attach.then(|| args.next().expect("peeked"));
                overrides.host_disks.push(copperline::config::HostDiskArg {
                    device,
                    attach,
                    read_only,
                });
            }
            "--install-net-helper" => {
                if net_helper_action.is_some() {
                    return Err(anyhow!("only one network-helper action may be requested"));
                }
                net_helper_action = Some("install".to_string());
            }
            "--uninstall-net-helper" => {
                if net_helper_action.is_some() {
                    return Err(anyhow!("only one network-helper action may be requested"));
                }
                net_helper_action = Some("uninstall".to_string());
            }
            "--net-helper-status" => {
                if net_helper_action.is_some() {
                    return Err(anyhow!("only one network-helper action may be requested"));
                }
                net_helper_action = Some("status".to_string());
            }
            "--sampler-list-audio-inputs" => {
                list_sampler_inputs = true;
            }
            "--config" | "-c" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--config requires a path"))?;
                config_path = Some(PathBuf::from(v));
            }
            "--whdload" => {
                let v = args.next().ok_or_else(|| {
                    anyhow!("--whdload requires a game package (.lha archive or directory)")
                })?;
                whdload = Some(PathBuf::from(v));
            }
            "--run" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--run requires the path to an Amiga executable"))?;
                run = Some(PathBuf::from(v));
            }
            "--run-args" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--run-args requires an argument string"))?;
                run_args = Some(v);
            }
            "--run-stack" => {
                let bytes: u64 = next_arg(
                    &mut args,
                    "--run-stack requires BYTES",
                    "--run-stack BYTES must be an integer",
                )?;
                run_stack = Some(
                    copperline::runprog::validate_stack_size(bytes)
                        .map_err(|e| anyhow!("--run-stack: {e}"))?,
                );
            }
            "--run-detach" => run_detach = true,
            "--warp-boot" => {
                warp_boot = true;
            }
            "--warp-until" => {
                let secs: f64 = next_arg(
                    &mut args,
                    "--warp-until requires SECS",
                    "--warp-until SECS must be a number",
                )?;
                if secs <= 0.0 || !secs.is_finite() {
                    bail!("--warp-until SECS must be a positive number of emulated seconds");
                }
                warp_until = Some(secs);
            }
            "--model" => {
                overrides.model = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--model requires a name (A500/A600/A1200/...)"))?,
                );
            }
            "--chipset" => {
                overrides.chipset = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--chipset requires OCS/ECS/AGA"))?,
                );
            }
            "--video" => {
                overrides.video = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--video requires PAL or NTSC"))?,
                );
            }
            "--cpu" => {
                overrides.cpu = Some(args.next().ok_or_else(|| {
                    anyhow!("--cpu requires a model (68000/68EC020/68020/68030/68040/68060)")
                })?);
            }
            "--fpu" => {
                overrides.fpu = Some(true);
            }
            "--no-fpu" => {
                overrides.fpu = Some(false);
            }
            "--full-screen" => {
                overrides.full_screen = Some(true);
            }
            "--windowed" => {
                overrides.full_screen = Some(false);
            }
            "--show-status-bar" => {
                overrides.status_bar = Some(true);
            }
            "--mt32-control-rom" => {
                overrides.mt32_control_rom = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--mt32-control-rom requires a path"))?,
                );
            }
            "--mt32-pcm-rom" => {
                overrides.mt32_pcm_rom = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--mt32-pcm-rom requires a path"))?,
                );
            }
            "--mt32-panel" => {
                overrides.mt32_panel = Some(true);
            }
            "--menu-scale" => {
                overrides.menu_scale = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--menu-scale requires 1x or 2x"))?,
                );
            }
            "--hide-status-bar" => {
                overrides.status_bar = Some(false);
            }
            "--perf-overlay" => {
                overrides.perf_overlay = Some(true);
            }
            "--cpu-clock" => {
                let mhz: f64 = next_arg(
                    &mut args,
                    "--cpu-clock requires MHZ",
                    "--cpu-clock MHZ must be a number",
                )?;
                overrides.cpu_clock_mhz = Some(mhz);
            }
            "--jit" => {
                overrides.cpu_jit = Some(true);
            }
            "--no-jit" => {
                overrides.cpu_jit = Some(false);
            }
            "--chip" => {
                overrides.chip = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--chip requires a size (e.g. 512K, 1M, 2M)"))?,
                );
            }
            "--fast" => {
                overrides.fast = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--fast requires a size (e.g. 0, 4M, 8M)"))?,
                );
            }
            "--slow" => {
                overrides.slow = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--slow requires a size (e.g. 0, 512K)"))?,
                );
            }
            "--ram-init" => {
                overrides.ram_init = Some(args.next().ok_or_else(|| {
                    anyhow!("--ram-init requires zero, random[:SEED], pattern:WORD, or 0xWORD")
                })?);
            }
            "--motherboard" => {
                overrides.motherboard =
                    Some(args.next().ok_or_else(|| {
                        anyhow!("--motherboard requires a size (e.g. 0, 4M, 16M)")
                    })?);
            }
            "--accelerator" => {
                overrides.accelerator =
                    Some(args.next().ok_or_else(|| {
                        anyhow!("--accelerator requires a size (e.g. 0, 32M, 128M)")
                    })?);
            }
            "--floppy-drives" | "--fdd-drives" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--floppy-drives requires COUNT (0-4)"))?;
                overrides.floppy_drives = Some(parse_floppy_drive_count(&value)?);
            }
            // Absent from a build without the feature, so an unknown-argument
            // error names it rather than the flag quietly doing nothing.
            #[cfg(feature = "fluxbridge")]
            "--floppy-bridge" => {
                // The names this build accepts come from the library's driver
                // table, so the usage line cannot advertise a driver that is
                // not compiled in.
                let usage = format!(
                    "--floppy-bridge requires DFN INTERFACE ({})",
                    copperline::config::supported_bridge_drivers().join(", ")
                );
                let drive_s = args.next().ok_or_else(|| anyhow!(usage.clone()))?;
                let idx = parse_floppy_drive_idx(&drive_s, "--floppy-bridge")?;
                let interface = args.next().ok_or_else(|| anyhow!(usage))?;
                overrides.floppy_bridge[idx] = Some(interface);
            }
            #[cfg(feature = "fluxbridge")]
            "--floppy-bridge-port" => {
                const USAGE: &str = "--floppy-bridge-port requires DFN PORT";
                let drive_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let idx = parse_floppy_drive_idx(&drive_s, "--floppy-bridge-port")?;
                overrides.floppy_bridge_port[idx] =
                    Some(args.next().ok_or_else(|| anyhow!(USAGE))?);
            }
            #[cfg(feature = "fluxbridge")]
            "--floppy-bridge-cable" => {
                const USAGE: &str = "--floppy-bridge-cable requires DFN CABLE \
                                     (a or b for a PC cable, 0-3 for Shugart)";
                let drive_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let idx = parse_floppy_drive_idx(&drive_s, "--floppy-bridge-cable")?;
                overrides.floppy_bridge_cable[idx] =
                    Some(args.next().ok_or_else(|| anyhow!(USAGE))?);
            }
            #[cfg(feature = "fluxbridge")]
            "--floppy-bridge-mode" => {
                const USAGE: &str = "--floppy-bridge-mode requires DFN MODE \
                                     (normal, compatible, or stalling)";
                let drive_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let idx = parse_floppy_drive_idx(&drive_s, "--floppy-bridge-mode")?;
                overrides.floppy_bridge_mode[idx] =
                    Some(args.next().ok_or_else(|| anyhow!(USAGE))?);
            }
            #[cfg(feature = "fluxbridge")]
            "--floppy-bridge-density" => {
                const USAGE: &str = "--floppy-bridge-density requires DFN DENSITY \
                                     (auto, dd, or hd)";
                let drive_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let idx = parse_floppy_drive_idx(&drive_s, "--floppy-bridge-density")?;
                overrides.floppy_bridge_density[idx] =
                    Some(args.next().ok_or_else(|| anyhow!(USAGE))?);
            }
            #[cfg(feature = "fluxbridge")]
            "--floppy-replay-speed" | "--floppy-bridge-speed" => {
                const USAGE: &str = "--floppy-replay-speed requires DFN SPEED (normal, or fast)";
                let drive_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let percent_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let idx = parse_floppy_drive_idx(&drive_s, "--floppy-replay-speed")?;
                overrides.floppy_bridge_speed[idx] = Some(parse_floppy_bridge_speed(&percent_s)?);
            }
            #[cfg(feature = "fluxbridge")]
            "--floppy-bridge-writable" => {
                const USAGE: &str = "--floppy-bridge-writable requires DFN";
                let drive_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let idx = parse_floppy_drive_idx(&drive_s, "--floppy-bridge-writable")?;
                overrides.floppy_bridge_writable[idx] = true;
            }
            "--floppy-speed" | "--fdd-speed" => {
                let value = args.next().ok_or_else(|| {
                    anyhow!("--floppy-speed requires PERCENT (100, 200, 400, 800, or 0 for turbo)")
                })?;
                overrides.floppy_speed = Some(parse_floppy_speed(&value)?);
            }
            "--rtc-time" => {
                overrides.rtc_time = Some(args.next().ok_or_else(|| {
                    anyhow!("--rtc-time requires Unix seconds or \"YYYY-MM-DD HH:MM[:SS]\"")
                })?);
            }
            "--rtc-frozen" => {
                overrides.rtc_frozen = Some(true);
            }
            "--joystick" => {
                overrides.joystick = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--joystick requires a mode (gamepad/keyboard)"))?,
                );
            }
            "--port1" => {
                overrides.port1 = Some(args.next().ok_or_else(|| {
                    anyhow!(
                        "--port1 requires a device \
                         (mouse/gamepad-mouse/joystick/cd32/analogue/none)"
                    )
                })?);
            }
            "--port2" => {
                overrides.port2 = Some(args.next().ok_or_else(|| {
                    anyhow!("--port2 requires a device (mouse/joystick/cd32/analogue/none)")
                })?);
            }
            "--autofire" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--autofire requires a rate in Hz (0 = off)"))?;
                overrides.autofire_hz = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| anyhow!("--autofire rate must be a whole number of Hz"))?,
                );
            }
            "--netplay-bind" => {
                netplay_bind = Some(next_arg(
                    &mut args,
                    "--netplay-bind requires IP:PORT",
                    "invalid netplay bind address",
                )?)
            }
            "--netplay-peer" => {
                netplay_peer = Some(next_arg(
                    &mut args,
                    "--netplay-peer requires IP:PORT",
                    "invalid netplay peer address",
                )?)
            }
            "--netplay-player" => {
                netplay_player = Some(next_arg::<u8>(
                    &mut args,
                    "--netplay-player requires 1 or 2",
                    "invalid netplay player",
                )?)
            }
            "--netplay-delay" => {
                netplay_delay = Some(next_arg::<u8>(
                    &mut args,
                    "--netplay-delay requires 0..6 frames",
                    "invalid netplay delay",
                )?)
            }
            "--netplay-rollback" => {
                netplay_rollback = Some(next_arg::<u8>(
                    &mut args,
                    "--netplay-rollback requires 1..12 frames",
                    "invalid netplay rollback window",
                )?)
            }
            "--netplay-session" => {
                netplay_session =
                    Some(args.next().ok_or_else(|| {
                        anyhow!("--netplay-session requires 32 hexadecimal digits")
                    })?)
            }
            "--run-ahead" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--run-ahead requires a frame count (0 = off)"))?;
                overrides.run_ahead_frames =
                    Some(value.parse::<u8>().map_err(|_| {
                        anyhow!("--run-ahead must be a whole number of frames (0..4)")
                    })?);
            }
            "--serial" => {
                overrides.serial = Some(args.next().ok_or_else(|| {
                    anyhow!("--serial requires a mode (off/stdout/midi/tcp/tcp-connect/pty/modem)")
                })?);
            }
            "--serial-connect" => {
                overrides.serial_connect =
                    Some(args.next().ok_or_else(|| {
                        anyhow!("--serial-connect requires an address (host:port)")
                    })?);
            }
            "--serial-session" => {
                overrides.serial_session =
                    Some(args.next().ok_or_else(|| {
                        anyhow!("--serial-session requires a scripted session file")
                    })?);
            }
            "--a2065-net" => {
                overrides.a2065_net = Some(args.next().ok_or_else(|| {
                    anyhow!("--a2065-net requires a backend (none/loopback/nat/bridge)")
                })?);
            }
            "--a2065-interface" => {
                overrides.a2065_interface = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--a2065-interface requires an adapter name"))?,
                );
            }
            "--hostsocket-net" => {
                overrides.hostsocket_net = Some(args.next().ok_or_else(|| {
                    anyhow!("--hostsocket-net requires a backend (none/loopback/nat/bridge/host)")
                })?);
            }
            "--hostsocket-interface" => {
                overrides.hostsocket_interface =
                    Some(args.next().ok_or_else(|| {
                        anyhow!("--hostsocket-interface requires an adapter name")
                    })?);
            }
            "--midi-out" => {
                overrides.midi_out = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--midi-out requires a device name"))?,
                );
            }
            "--midi-in" => {
                overrides.midi_in = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--midi-in requires a device name"))?,
                );
            }
            "--parallel" => {
                overrides.parallel = Some(args.next().ok_or_else(|| {
                    anyhow!("--parallel requires a device (none/printer/sampler)")
                })?);
            }
            "--sampler-audio-input" => {
                overrides.sampler_input = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--sampler-audio-input requires a device name"))?,
                );
            }
            "--sampler-input-gain" => {
                let gain: f32 = next_arg(
                    &mut args,
                    "--sampler-input-gain requires a value in dB (e.g. 0, 6, -6)",
                    "--sampler-input-gain must be a number",
                )?;
                overrides.sampler_gain = Some(gain);
            }
            "--audio-device" => {
                overrides.audio_device = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--audio-device requires a device name"))?,
                );
            }
            "--audio-channel-mode" => {
                overrides.audio_channel_mode = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--audio-channel-mode requires stereo or mono"))?,
                );
            }
            "--audio-filter" => {
                overrides.audio_filter = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--audio-filter requires auto, on, or off"))?,
                );
            }
            "--audio-stereo-separation" => {
                let v = args.next().ok_or_else(|| {
                    anyhow!("--audio-stereo-separation requires a percent (0-100)")
                })?;
                overrides.audio_stereo_separation =
                    Some(v.parse::<u16>().map_err(|_| {
                        anyhow!("--audio-stereo-separation must be a number 0-100")
                    })?);
            }
            "--mouse-sensitivity" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--mouse-sensitivity requires a value (0-100)"))?;
                overrides.mouse_sensitivity = Some(
                    v.parse::<u16>()
                        .map_err(|_| anyhow!("--mouse-sensitivity must be a number 0-100"))?,
                );
            }
            "--mouse-capture" => {
                let v = args.next().ok_or_else(|| {
                    anyhow!("--mouse-capture requires a mode (click, auto, or manual)")
                })?;
                overrides.mouse_capture = Some(v);
            }
            "--click-after" => {
                const USAGE: &str = "--click-after requires SECS BUTTON DURATION_MS";
                let secs: f32 = next_arg(&mut args, USAGE, "--click-after SECS must be a number")?;
                let button_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let button = match button_s.as_str() {
                    "left" | "lmb" | "l" => MouseButtonKind::Left,
                    "right" | "rmb" | "r" => MouseButtonKind::Right,
                    "middle" | "mmb" | "m" => MouseButtonKind::Middle,
                    _ => return Err(anyhow!("--click-after BUTTON must be left/right/middle")),
                };
                let dur_ms: u32 = next_arg(
                    &mut args,
                    USAGE,
                    "--click-after DURATION_MS must be a number",
                )?;
                let port = take_port_token(&mut args, 1);
                click_after.push((secs, button, dur_ms, port));
            }
            "--joy-after" => {
                const USAGE: &str = "--joy-after requires SECS BUTTON DURATION_MS";
                let secs: f32 = next_arg(&mut args, USAGE, "--joy-after SECS must be a number")?;
                let button_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let button = JoyButtonKind::parse(&button_s).ok_or_else(|| {
                    anyhow!(
                        "--joy-after BUTTON must be up/down/left/right/red/blue/green/yellow/play/rwd/ffw"
                    )
                })?;
                let dur_ms: u32 =
                    next_arg(&mut args, USAGE, "--joy-after DURATION_MS must be a number")?;
                let port = take_port_token(&mut args, 2);
                joy_after.push((secs, button, dur_ms, port));
            }
            "--mouse-to-after" => {
                const USAGE: &str = "--mouse-to-after requires SECS X Y";
                let secs: f32 =
                    next_arg(&mut args, USAGE, "--mouse-to-after SECS must be a number")?;
                let x: i32 = next_arg(&mut args, USAGE, "--mouse-to-after X must be an integer")?;
                let y: i32 = next_arg(&mut args, USAGE, "--mouse-to-after Y must be an integer")?;
                let port = take_port_token(&mut args, 1);
                mouse_to_after.push((secs, x, y, port));
            }
            "--mouse-after" => {
                const USAGE: &str = "--mouse-after requires SECS DX DY";
                let secs: f32 = next_arg(&mut args, USAGE, "--mouse-after SECS must be a number")?;
                let dx: i32 = next_arg(&mut args, USAGE, "--mouse-after DX must be an integer")?;
                let dy: i32 = next_arg(&mut args, USAGE, "--mouse-after DY must be an integer")?;
                let port = take_port_token(&mut args, 1);
                mouse_after.push((secs, dx, dy, port));
            }
            "--pot-after" => {
                const USAGE: &str = "--pot-after requires SECS X Y";
                let secs: f32 = next_arg(&mut args, USAGE, "--pot-after SECS must be a number")?;
                let x: u8 = next_arg(&mut args, USAGE, "--pot-after X must be a number 0-255")?;
                let y: u8 = next_arg(&mut args, USAGE, "--pot-after Y must be a number 0-255")?;
                let port = take_port_token(&mut args, 2);
                pot_after.push((secs, x, y, port));
            }
            "--record-input" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--record-input requires a path"))?;
                record_input = Some(PathBuf::from(v));
            }
            "--insert-disk-after" => {
                const USAGE: &str = "--insert-disk-after requires SECS DFN PATH";
                let secs: f32 = next_arg(
                    &mut args,
                    USAGE,
                    "--insert-disk-after SECS must be a number",
                )?;
                let drive_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let drive_idx = parse_floppy_drive_idx(&drive_s, "--insert-disk-after")?;
                let path = args.next().ok_or_else(|| anyhow!(USAGE))?;
                disk_insert_after.push(CliDiskInsert::Explicit(DiskInsertSpec {
                    secs,
                    drive_idx,
                    path: PathBuf::from(path),
                    write_protected: true,
                }));
            }
            "--defer-disk-insert" => {
                const USAGE: &str = "--defer-disk-insert requires SECS DFN";
                let secs: f32 = next_arg(
                    &mut args,
                    USAGE,
                    "--defer-disk-insert SECS must be a number",
                )?;
                let drive_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let drive_idx = parse_floppy_drive_idx(&drive_s, "--defer-disk-insert")?;
                disk_insert_after.push(CliDiskInsert::Configured { secs, drive_idx });
            }
            "--insert-cd-after" => {
                const USAGE: &str = "--insert-cd-after requires SECS PATH";
                let secs: f32 =
                    next_arg(&mut args, USAGE, "--insert-cd-after SECS must be a number")?;
                let path = args.next().ok_or_else(|| anyhow!(USAGE))?;
                cd_insert_after.push((secs, PathBuf::from(path)));
            }
            "--freeze-after" => {
                let secs: f32 = next_arg(
                    &mut args,
                    "--freeze-after requires SECS",
                    "--freeze-after SECS must be a number",
                )?;
                freeze_after.push(secs);
            }
            "--cartridge" => {
                overrides.cartridge = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--cartridge requires a model (none/hrtmon)"))?,
                );
            }
            "--press-after" => {
                const USAGE: &str = "--press-after requires SECS KEY";
                let secs: f32 = next_arg(&mut args, USAGE, "--press-after SECS must be a number")?;
                let key_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let rawkey = parse_amiga_key(&key_s)
                    .ok_or_else(|| anyhow!("--press-after KEY: unknown key {:?}", key_s))?;
                press_after.push(KeyPressSpec {
                    secs,
                    rawkey,
                    hold_ms: DEFAULT_KEY_HOLD_MS,
                });
            }
            "--key-after" | "--hold-key-after" => {
                const USAGE: &str = "--key-after requires SECS KEY DURATION_MS";
                let secs: f32 = next_arg(&mut args, USAGE, "--key-after SECS must be a number")?;
                let key_s = args.next().ok_or_else(|| anyhow!(USAGE))?;
                let rawkey = parse_amiga_key(&key_s)
                    .ok_or_else(|| anyhow!("--key-after KEY: unknown key {:?}", key_s))?;
                let hold_ms: u32 =
                    next_arg(&mut args, USAGE, "--key-after DURATION_MS must be a number")?;
                press_after.push(KeyPressSpec {
                    secs,
                    rawkey,
                    hold_ms,
                });
            }
            "--screenshot-after" => {
                const USAGE: &str = "--screenshot-after requires SECS PATH";
                let secs: f32 =
                    next_arg(&mut args, USAGE, "--screenshot-after SECS must be a number")?;
                let path = args.next().ok_or_else(|| anyhow!(USAGE))?;
                screenshot_after.push((secs, PathBuf::from(path)));
            }
            "--save-state-after" => {
                const USAGE: &str = "--save-state-after requires SECS PATH";
                let secs: f32 =
                    next_arg(&mut args, USAGE, "--save-state-after SECS must be a number")?;
                let path = args.next().ok_or_else(|| anyhow!(USAGE))?;
                save_state_after.push((secs, PathBuf::from(path)));
            }
            "--load-uss" => {
                load_uss = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("--load-uss requires a path"))?,
                ));
            }
            "--load-state" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--load-state requires a path"))?;
                load_state = Some(PathBuf::from(v));
            }
            "--benchmark-until" | "--bench-until" => {
                let secs: f32 = next_arg(
                    &mut args,
                    "--benchmark-until requires SECS",
                    "--benchmark-until SECS must be a number",
                )?;
                if secs <= 0.0 {
                    return Err(anyhow!("--benchmark-until SECS must be greater than zero"));
                }
                benchmark_until = Some(secs);
            }
            "--gdb" | "--gdb-listen" => {
                let listen = args
                    .next()
                    .ok_or_else(|| anyhow!("--gdb requires ADDR, :PORT, or PORT"))?;
                gdb = Some(listen);
            }
            "--gdb-dialect" => {
                let dialect = args
                    .next()
                    .ok_or_else(|| anyhow!("--gdb-dialect requires standard or bartman"))?;
                if !matches!(dialect.as_str(), "standard" | "bartman") {
                    return Err(anyhow!("--gdb-dialect requires standard or bartman"));
                }
                gdb_dialect = Some(dialect);
            }
            "--gdb-gui" => {
                let listen = args
                    .next()
                    .ok_or_else(|| anyhow!("--gdb-gui requires ADDR, :PORT, or PORT"))?;
                gdb_gui = Some(listen);
            }
            "--control" => {
                let listen = args
                    .next()
                    .ok_or_else(|| anyhow!("--control requires ADDR, :PORT, or PORT"))?;
                control_listen = Some(listen);
            }
            "--control-gui" => {
                let listen = args
                    .next()
                    .ok_or_else(|| anyhow!("--control-gui requires ADDR, :PORT, or PORT"))?;
                control_gui_listen = Some(listen);
            }
            "--control-token" => {
                let token = args
                    .next()
                    .ok_or_else(|| anyhow!("--control-token requires a token string"))?;
                control_token = Some(token);
            }
            "--control-info" => {
                let path = args
                    .next()
                    .ok_or_else(|| anyhow!("--control-info requires a file path"))?;
                control_info = Some(PathBuf::from(path));
            }
            "--dump-frames" => {
                let path = args
                    .next()
                    .ok_or_else(|| anyhow!("--dump-frames requires a directory"))?;
                dump_dir = Some(PathBuf::from(path));
            }
            "--waveform" => {
                let path = args
                    .next()
                    .ok_or_else(|| anyhow!("--waveform requires a VCD output path"))?;
                wave_path = Some(PathBuf::from(path));
            }
            "--wave-trigger" => {
                const USAGE: &str =
                    "--wave-trigger SPEC: now, pc=ADDR, beam=VPOS[:HPOS], reg=OFF, or time=SECS";
                let spec = args.next().ok_or_else(|| anyhow!(USAGE))?;
                wave_trigger = Some(
                    copperline::waveform::parse_trigger(&spec)
                        .ok_or_else(|| anyhow!("bad trigger {spec:?}; {USAGE}"))?,
                );
            }
            "--wave-duration" => {
                const USAGE: &str =
                    "--wave-duration SPEC: Ncck (bare N is cck), Nf/Nframes, Nms, or Ns";
                let spec = args.next().ok_or_else(|| anyhow!(USAGE))?;
                wave_duration = Some(
                    copperline::waveform::parse_duration(&spec)
                        .ok_or_else(|| anyhow!("bad duration {spec:?}; {USAGE}"))?,
                );
            }
            "--wave-signals" => {
                const USAGE: &str = "--wave-signals LIST: comma list of \
                     beam, bus, cpu, copper, blitter, regs, irq, audio, or all";
                let spec = args.next().ok_or_else(|| anyhow!(USAGE))?;
                wave_signals = Some(
                    copperline::waveform::parse_signals(&spec)
                        .ok_or_else(|| anyhow!("bad signal list {spec:?}; {USAGE}"))?,
                );
            }
            "--dump-start" => {
                dump_start_secs = next_arg(
                    &mut args,
                    "--dump-start requires SECS",
                    "--dump-start SECS must be a number",
                )?;
            }
            "--dump-count" => {
                let count: u32 = next_arg(
                    &mut args,
                    "--dump-count requires COUNT",
                    "--dump-count COUNT must be a positive integer",
                )?;
                if count == 0 {
                    return Err(anyhow!("--dump-count COUNT must be greater than zero"));
                }
                dump_count = Some(count);
            }
            "--audio" => {
                audio_live = true;
                explicit_audio_live = true;
            }
            "--factory" => factory = true,
            "--noaudio" | "--no-audio" => {
                audio_live = false;
                explicit_noaudio = true;
            }
            "--audio-wav" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--audio-wav requires a path"))?;
                audio_wav = Some(PathBuf::from(v));
                audio_live = false;
            }
            "--audio-stems" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--audio-stems requires a directory"))?;
                audio_stems = Some(PathBuf::from(v));
                audio_live = false;
            }
            "--audio-stems-mode" => {
                let v = args.next().ok_or_else(|| {
                    anyhow!("--audio-stems-mode requires a list, e.g. \"master,source\"")
                })?;
                audio_stems_mode = Some(
                    copperline::audio::mux::StemGranularity::parse_list(&v)
                        .map_err(|e| anyhow!("--audio-stems-mode: {e}"))?,
                );
            }
            "--profile-live-audio" => {
                let secs: f32 = next_arg(
                    &mut args,
                    "--profile-live-audio requires SECS",
                    "--profile-live-audio SECS must be a number",
                )?;
                if secs <= 0.0 {
                    return Err(anyhow!(
                        "--profile-live-audio SECS must be greater than zero"
                    ));
                }
                live_audio_profile_secs = Some(secs);
                audio_live = true;
                explicit_audio_live = true;
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("copperline {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other if other.starts_with("--") => {
                return Err(anyhow!("unknown option {:?} (see --help)", other));
            }
            _ => {
                if rom_path.is_some() {
                    return Err(anyhow!("more than one ROM path given"));
                }
                rom_path = Some(PathBuf::from(a));
            }
        }
    }
    if explicit_audio_live && audio_wav.is_some() {
        return Err(anyhow!("--audio and --audio-wav are mutually exclusive"));
    }
    if audio_stems.is_some() && audio_wav.is_some() {
        return Err(anyhow!(
            "--audio-stems and --audio-wav are mutually exclusive"
        ));
    }
    if explicit_audio_live && audio_stems.is_some() {
        return Err(anyhow!("--audio and --audio-stems are mutually exclusive"));
    }
    if audio_stems.is_none() && audio_stems_mode.is_some() {
        return Err(anyhow!("--audio-stems-mode requires --audio-stems DIR"));
    }
    if live_audio_profile_secs.is_some() && explicit_noaudio {
        return Err(anyhow!(
            "--profile-live-audio and --noaudio are mutually exclusive"
        ));
    }
    if (benchmark_until.is_some() || gdb.is_some()) && !explicit_audio_live && audio_wav.is_none() {
        audio_live = false;
    }
    let frame_dump = match (dump_dir, dump_count) {
        (Some(dir), Some(count)) => Some(FrameDumpSpec {
            dir,
            start_secs: dump_start_secs,
            count,
        }),
        (Some(_), None) => return Err(anyhow!("--dump-frames requires --dump-count COUNT")),
        (None, Some(_)) => return Err(anyhow!("--dump-count requires --dump-frames DIR")),
        (None, None) => {
            if dump_start_secs != 0.0 {
                return Err(anyhow!("--dump-start requires --dump-frames DIR"));
            }
            None
        }
    };
    let waveform = match wave_path {
        Some(path) => {
            let mut opts = copperline::waveform::WaveOptions::new(path);
            if let Some(trigger) = wave_trigger {
                opts.trigger = trigger;
            }
            if let Some(duration) = wave_duration {
                opts.duration = duration;
            }
            if let Some(signals) = wave_signals {
                opts.signals = signals;
            }
            Some(opts)
        }
        None => {
            if wave_trigger.is_some() || wave_duration.is_some() || wave_signals.is_some() {
                return Err(anyhow!(
                    "--wave-trigger/--wave-duration/--wave-signals require --waveform PATH"
                ));
            }
            None
        }
    };
    if control_listen.is_some() && control_gui_listen.is_some() {
        return Err(anyhow!("--control and --control-gui cannot be combined"));
    }
    if control_listen.is_none()
        && control_gui_listen.is_none()
        && (control_token.is_some() || control_info.is_some())
    {
        return Err(anyhow!(
            "--control-token/--control-info require --control or --control-gui"
        ));
    }
    if overrides.a2065_interface.is_some()
        && overrides
            .a2065_net
            .as_deref()
            .is_some_and(|net| !matches!(net.to_ascii_lowercase().as_str(), "bridge" | "bridged"))
    {
        return Err(anyhow!(
            "--a2065-interface conflicts with an explicit non-bridge --a2065-net"
        ));
    }
    if overrides.hostsocket_interface.is_some()
        && overrides
            .hostsocket_net
            .as_deref()
            .is_some_and(|net| !matches!(net.to_ascii_lowercase().as_str(), "bridge" | "bridged"))
    {
        return Err(anyhow!(
            "--hostsocket-interface conflicts with an explicit non-bridge --hostsocket-net"
        ));
    }
    let netplay = if netplay_bind.is_some()
        || netplay_peer.is_some()
        || netplay_player.is_some()
        || netplay_session.is_some()
        || netplay_delay.is_some()
        || netplay_rollback.is_some()
    {
        let usage = "netplay requires --netplay-bind IP:PORT, --netplay-peer IP:PORT, --netplay-player 1|2 and --netplay-session HEX";
        let player = netplay_player.ok_or_else(|| anyhow!(usage))?;
        if !(1..=2).contains(&player) {
            bail!("--netplay-player must be 1 or 2");
        }
        let code: String = netplay_session.ok_or_else(|| anyhow!(usage))?;
        let session = copperline::netplay::parse_session_id(&code)?;
        let options = copperline::netplay::Options {
            bind: netplay_bind.ok_or_else(|| anyhow!(usage))?,
            peer: netplay_peer.ok_or_else(|| anyhow!(usage))?,
            player: usize::from(player - 1),
            session,
            input_delay: netplay_delay.unwrap_or(2),
            rollback_frames: netplay_rollback.unwrap_or(8),
        };
        options.validate()?;
        if run.is_some()
            || whdload.is_some()
            || load_state.is_some()
            || load_uss.is_some()
            || benchmark_until.is_some()
            || gdb.is_some()
            || gdb_gui.is_some()
            || control_listen.is_some()
            || control_gui_listen.is_some()
            || waveform.is_some()
            || record_input.is_some()
            || audio_wav.is_some()
            || audio_stems.is_some()
            || warp_boot
            || warp_until.is_some()
            || !save_state_after.is_empty()
            || !cd_insert_after.is_empty()
            || !freeze_after.is_empty()
            || !click_after.is_empty()
            || !mouse_after.is_empty()
            || !mouse_to_after.is_empty()
            || !pot_after.is_empty()
            || disk_insert_after.iter().any(|d| match d {
                CliDiskInsert::Explicit(s) => s.secs != 0.0,
                CliDiskInsert::Configured { secs, .. } => *secs != 0.0,
            })
            || joy_after.iter().any(|j| usize::from(j.3) != options.player)
        {
            bail!("netplay supports cold boot, disks inserted at time 0, local-port --joy-after and keyboard input; state loads, media changes, scripted mouse/analogue input, debugging, warp and recording are unavailable");
        }
        Some(options)
    } else {
        None
    };
    Ok(CliArgs {
        netplay,
        config_path,
        rom_path,
        whdload,
        run,
        warp_boot,
        warp_until,
        run_args,
        run_stack,
        run_detach,
        screenshot_after,
        save_state_after,
        load_state,
        load_uss,
        benchmark_until,
        gdb,
        gdb_gui,
        gdb_dialect,
        control: control_listen,
        control_gui: control_gui_listen,
        control_token,
        control_info,
        frame_dump,
        waveform,
        press_after,
        click_after,
        joy_after,
        mouse_after,
        mouse_to_after,
        pot_after,
        record_input,
        disk_insert_after,
        cd_insert_after,
        freeze_after,
        audio_live,
        audio_live_forced: explicit_audio_live,
        audio_wav,
        audio_stems,
        audio_stems_mode,
        live_audio_profile_secs,
        calibrate_gamepad,
        list_midi,
        list_audio_devices,
        list_net_interfaces,
        list_disks,
        host_disk_broker,
        net_helper_action,
        list_sampler_inputs,
        overrides,
        factory,
    })
}

fn print_help() {
    let shortcut = HOST_SHORTCUT_MODIFIER_LABEL;
    // The MIDI endpoint options only do anything in a `midi`-feature build, so
    // list them only there. `--serial` itself is always shown: off/stdout work
    // in every build, and it names midi as a mode. The MT-32 and Coppersynth
    // ride with them when their features are in, both reached through
    // `--midi-out`.
    #[cfg(all(feature = "midi", feature = "mt32", feature = "coppersynth"))]
    let midi = "--midi-out NAME                host MIDI destination, or mt32/coppersynth (implies --serial midi)\n  \
                --midi-in NAME                 host MIDI source, or mt32 (implies --serial midi)\n  \
                --list-midi                    list host MIDI endpoints and exit\n  \
                --mt32-control-rom PATH        control ROM for the emulated MT-32\n  \
                --mt32-pcm-rom PATH            PCM ROM for the emulated MT-32\n  \
                --mt32-panel                   show the MT-32's front panel under the display\n  ";
    #[cfg(all(feature = "midi", feature = "mt32", not(feature = "coppersynth")))]
    let midi = "--midi-out NAME                host MIDI destination, or mt32 (implies --serial midi)\n  \
                --midi-in NAME                 host MIDI source, or mt32 (implies --serial midi)\n  \
                --list-midi                    list host MIDI endpoints and exit\n  \
                --mt32-control-rom PATH        control ROM for the emulated MT-32\n  \
                --mt32-pcm-rom PATH            PCM ROM for the emulated MT-32\n  \
                --mt32-panel                   show the MT-32's front panel under the display\n  ";
    #[cfg(all(feature = "midi", not(feature = "mt32"), feature = "coppersynth"))]
    let midi =
        "--midi-out NAME                host MIDI destination, or coppersynth (implies --serial midi)\n  \
                --midi-in NAME                 host MIDI source (implies --serial midi)\n  \
                --list-midi                    list host MIDI endpoints and exit\n  ";
    #[cfg(all(feature = "midi", not(feature = "mt32"), not(feature = "coppersynth")))]
    let midi = "--midi-out NAME                host MIDI destination (implies --serial midi)\n  \
                --midi-in NAME                 host MIDI source (implies --serial midi)\n  \
                --list-midi                    list host MIDI endpoints and exit\n  ";
    #[cfg(not(feature = "midi"))]
    let midi = "";
    // A build without the feature cannot attach a physical drive at all, so
    // the flags are not listed and not accepted.
    #[cfg(feature = "fluxbridge")]
    let floppy_bridge = {
        let floppy_bridge_names = copperline::config::supported_bridge_drivers().join(", ");
        format!(
            "--floppy-bridge DFN NAME       drive a physical floppy drive on DFN over NAME:\n  \
         \x20                            {floppy_bridge_names}\n  \
         --floppy-bridge-port DFN PORT  serial port of that interface (default: auto-detect)\n  \
         --floppy-bridge-cable DFN SEL  drive select on the cable: a/b (IBM PC) or 0-3 (Shugart)\n  \
         --floppy-bridge-mode DFN MODE  how tracks are captured: normal, compatible, stalling\n  \
         --floppy-bridge-density DFN D  force a density: auto, dd, or hd\n  \
         --floppy-replay-speed DFN SPEED  replay captured tracks at fast (default) or normal\n  \
         --floppy-bridge-writable DFN   let the guest write to the physical disk (which is\n  \
         \x20                            write-protected unless asked otherwise)\n  "
        )
    };
    #[cfg(not(feature = "fluxbridge"))]
    let floppy_bridge = String::new();
    eprintln!(
        "copperline - Amiga emulator\n\
         \n\
         Usage: copperline [--config FILE] [--screenshot-after SECS PATH] [ROM]\n\
         \n\
         Options:\n  \
         -c, --config FILE              load configuration from FILE (default: ./copperline.toml,\n  \
         \x20                            then the configuration saved with Save default)\n  \
         --factory                      ignore the saved default and start from Copperline's own\n  \
         \x20                            settings\n  \
         --whdload GAME                 boot a WHDLoad game package: an .lha archive or a\n  \
         \x20                            directory holding a .slave (see docs/guide/whdload.md)\n  \
         --run PROG                     warp launch: boot straight into an Amiga executable on\n  \
         \x20                            the host, unthrottled until the OS loads it\n  \
         \x20                            (see docs/guide/run.md)\n  \
         --run-args STRING              extra guest command-line arguments for --run\n  \
         --run-stack BYTES              issue AmigaDOS Stack BYTES before --run\n  \
         --run-detach                   start --run asynchronously and close the boot CLI\n  \
         --warp-boot                    warp the boot: unthrottled until the boot storage has\n  \
         \x20                            been idle for [emulation] warp_boot_idle seconds\n  \
         --warp-until SECS              warp the boot until an absolute emulated time\n  \
         --model NAME                   machine profile: A1000, A500, A500OCS, A500Plus, A600,\n  \
         \x20                              A1200, A3000, A4000, CDTV, CD32\n  \
         --chipset NAME                 chipset preset: OCS, ECS, or AGA\n  \
         --video STANDARD               video timing: PAL or NTSC\n  \
         --cpu MODEL                    CPU: 68000, 68010, 68EC020, 68020, 68030, 68040, or 68060\n  \
         --cpu-clock MHZ                CPU clock in MHz (default: the model's stock speed)\n  \
         --fpu / --no-fpu               fit / omit a 68881/68882 (68040/68060 on-die)\n  \
         --jit / --no-jit               fast batch/trace-JIT CPU execution (not cycle-exact,\n  \
         \x20                            like an accelerator card; default: off)\n  \
         --cartridge MODEL              freezer cartridge: none (default) or hrtmon (the\n  \
         \x20                            bundled HRTMon monitor, entered with --freeze-after,\n  \
         \x20                            the Freeze menu item, or cartridge.freeze)\n  \
         --chip SIZE                    chip RAM size, e.g. 512K, 1M, 2M\n  \
         --fast SIZE                    Zorro II fast RAM size, e.g. 0, 1M, 4M, 8M\n  \
         --slow SIZE                    trapdoor slow RAM at $C00000, e.g. 0, 512K\n  \
         --ram-init MODE                cold-start RAM contents: zero (default), random[:SEED],\n  \
         \x20                            pattern:WORD, or 0xWORD (uninitialised-read testing)\n  \
         --motherboard SIZE             Ramsey motherboard fast RAM (A3000/A4000), e.g. 0, 4M,\n  \
         \x20                            16M; the A4000 extends to 64M\n  \
         --accelerator SIZE             CPU-slot accelerator fast RAM at $08000000 (32-bit\n  \
         \x20                            CPUs), e.g. 0, 32M, 128M\n  \
         --floppy-drives COUNT          wired floppy drives, 0-4 (DF0 plus externals)\n  \
         --floppy-speed PERCENT         drive speed: 100, 200, 400, 800, or 0 (turbo)\n  \
         {floppy_bridge}--host-disk DEVICE [ATTACH]    give the machine one of the host's own disks\n  \
         \x20                            (--list-disks names them); ATTACH is ide-master\n  \
         \x20                            (default), ide-slave, or scsi0..scsi6\n  \
         --host-disk-read-only DEVICE [ATTACH]\n  \
         \x20                            the same, but the guest cannot write to the disk\n  \
         --rtc-time TIME                seed the battery clock (implies fitting one) with\n  \
         \x20                            Unix seconds or \"YYYY-MM-DD HH:MM[:SS]\"; it then\n  \
         \x20                            ticks in emulated time, so runs are deterministic\n  \
         --rtc-frozen                   stop the seeded clock at --rtc-time exactly\n  \
         --joystick MODE                initial joystick input: gamepad or keyboard\n  \
         \x20                            (gamepad lets the keyboard pass through to the Amiga)\n  \
         --mouse-sensitivity N          host mouse sensitivity 0-100 (50 default = 1:1)\n  \
         --mouse-capture MODE           when to grab the host mouse: click (default), auto, manual\n  \
         --port1 DEVICE                 controller in port 1: mouse (default), joystick,\n  \
         \x20                            cd32, analogue, or none\n  \
         --port2 DEVICE                 controller in port 2 (default: joystick;\n  \
         \x20                            cd32 on the CD32 profile)\n  \
         --autofire HZ                  pulse a held fire button at HZ (0 = off, the default)\n  \
         --netplay-bind IP:PORT         local UDP endpoint for two-player rollback netplay\n  \
         --netplay-peer IP:PORT         remote player's UDP endpoint\n  \
         --netplay-player 1|2           controller port owned by this player\n  \
         --netplay-session HEX          shared 32-digit hexadecimal session ID\n  \
         --netplay-delay FRAMES         local input delay, 0..6 (default 2)\n  \
         --netplay-rollback FRAMES      prediction limit, 1..12 (default 8)\n  \
         --run-ahead FRAMES             run-ahead input-latency reduction, 0..4 frames\n  \
         \x20                            (0 = off, the default; windowed sessions only)\n  \
         \x20                            (--model/--cpu/etc. override the config file or defaults)\n  \
         --screenshot-after SECS PATH   save a PNG to PATH after SECS emulated seconds, then exit\n  \
         --save-state-after SECS PATH   write a save state to PATH after SECS emulated seconds,\n  \
         \x20                            then keep running\n  \
         --load-uss PATH                import a WinUAE state; requires its matching Kickstart\n  \
         --load-state PATH              restore a save state before starting, resuming from\n  \
         \x20                            its emulated timeline\n  \
         --benchmark-until SECS         run frames with no window until absolute emulated\n  \
         \x20                            time SECS, report counters, then exit\n  \
         --gdb ADDR                     run a headless GDB remote server on ADDR,\n  \
         \x20                            :PORT, or PORT; port-only forms bind 127.0.0.1\n  \
         --gdb-dialect DIALECT         standard (default) or bartman\n  \
         --gdb-gui ADDR                 attach the GDB remote server to the normal window\n  \
         --control ADDR                 run the headless JSON-RPC control server on ADDR\n  \
         \x20                            (port 0 picks a free port; see docs/debugger/control.md)\n  \
         --control-gui ADDR             attach the control server to the normal window\n  \
         --control-token TOKEN          pin the control auth token (default: generated;\n  \
         \x20                            visible in ps -- prefer --control-info)\n  \
         --control-info PATH            write the control endpoint and token to PATH as JSON\n  \
         --dump-frames DIR              dump consecutive PNG frames into DIR, then exit\n  \
         --dump-start SECS              start frame dumping after SECS seconds (default: 0)\n  \
         --dump-count COUNT             number of frames to dump with --dump-frames\n  \
         --waveform PATH                arm a VCD logic-analyser capture of chipset signals\n  \
         \x20                            for GTKWave (see docs/debugger/waveform.md)\n  \
         --wave-trigger SPEC            capture trigger: now (default), pc=ADDR,\n  \
         \x20                            beam=VPOS[:HPOS], reg=OFF, or time=SECS\n  \
         --wave-duration SPEC           capture length: Ncck (bare N is cck), Nf,\n  \
         \x20                            Nms, or Ns (default: 1 frame)\n  \
         --wave-signals LIST            comma list of beam, bus, cpu, copper, blitter,\n  \
         \x20                            regs, irq, audio (default: all)\n  \
         --press-after SECS KEY         press/release Amiga KEY after SECS; KEY may be\n  \
         \x20                            decimal, 0x.., or a name like ctrl/lalt/lami/f1\n  \
         --key-after SECS KEY MS        press KEY after SECS, hold for MS milliseconds,\n  \
         \x20                            then release; may be passed multiple times\n  \
         --click-after SECS BTN MS [PORT]\n  \
         \x20                            press mouse BTN (left/right/middle) at SECS,\n  \
         \x20                            release MS ms later, on PORT (default 1)\n  \
         --joy-after SECS BTN MS [PORT] press joystick/CD32-pad BTN (up/down/left/right/\n  \
         \x20                            red|fire/blue/green/yellow/play/rwd/ffw) at SECS,\n  \
         \x20                            release MS ms later, on PORT (default 2)\n  \
         --mouse-after SECS DX DY [PORT]\n  \
         \x20                            apply a relative mouse motion at SECS on PORT\n  \
         \x20                            (default 1)\n  \
         --mouse-to-after SECS X Y [PORT]\n  \
         \x20                            from SECS, move the pointer to screen pixel\n  \
         \x20                            (X, Y) by watching sprite 0, on PORT (default 1)\n  \
         --pot-after SECS X Y [PORT]    set an analogue controller position (0-255 per\n  \
         \x20                            axis) at SECS on PORT (default 2)\n  \
         --record-input PATH            record all machine-bound input for the whole run\n  \
         \x20                            and write the script to PATH on exit\n  \
         --script FILE                  run scripted-input directives from FILE (the flag\n  \
         \x20                            syntax without the dashes; # comments allowed);\n  \
         \x20                            {shortcut}+Shift+R records a live session into this format\n  \
         --insert-disk-after SECS DFN PATH\n  \
         \x20                            insert PATH into DFN after SECS seconds\n  \
         --defer-disk-insert SECS DFN   start with configured DFN empty, then insert\n  \
         \x20                            its configured disk image after SECS seconds\n  \
         --insert-cd-after SECS PATH    swap the CD image (cue/iso/nrg/chd) in the machine's CD\n  \
         \x20                            drive (CDTV, CD32, or a SCSI CD-ROM unit) after\n  \
         \x20                            SECS seconds\n  \
         --freeze-after SECS            press the freezer cartridge's button (--cartridge\n  \
         \x20                            hrtmon) after SECS seconds\n  \
         --audio                        enable real-time stereo audio output via cpal (default)\n  \
         --noaudio                      disable real-time audio output\n  \
         --audio-device NAME            host audio output device (substring match)\n  \
         --audio-channel-mode MODE      output channels: stereo (default) or mono\n  \
         --audio-filter MODE            Paula filter: auto (default), on, or off\n  \
         --audio-stereo-separation PCT  stereo width 0-100 (100 default, 0 = mono)\n  \
         --list-audio-devices           list host audio output devices and exit\n  \
         --list-net-interfaces          list adapters usable for bridged Ethernet and exit\n  \
         --list-disks                   list the host disks that can be given to a machine,\n  \
         \x20                            and exit\n  \
         --install-net-helper           install Linux bridge helper (CAP_NET_RAW only)\n  \
         --uninstall-net-helper         remove the Linux bridge helper\n  \
         --net-helper-status            report Linux bridge-helper status\n  \
         --audio-wav PATH               dump mixed stereo audio to a 32-bit float WAV file\n  \
         \x20                            instead of live output\n  \
         --audio-stems DIR              write per-granularity stem WAVs into DIR instead of\n  \
         \x20                            live output (needs --audio-stems-mode or a\n  \
         \x20                            [audio] stem_granularity default)\n  \
         --audio-stems-mode LIST        comma-separated stem granularities to write:\n  \
         \x20                            \"master\", \"source\", \"channel\" (combinable)\n  \
         --profile-live-audio SECS      run a no-window Paula-to-cpal profile workload;\n  \
         \x20                            combine with COPPERLINE_AUDIO_PROFILE=1 for counters\n  \
         --full-screen / --windowed     open fullscreen / windowed at start (default: windowed)\n  \
         --show-status-bar / --hide-status-bar  status bar at start (default: shown)\n  \
         --perf-overlay                 show the performance overlay at start\n  \
         \x20                            (Cmd/Alt+P toggles it live)\n  \
         --menu-scale SIZE              size of the pop-up menu: 1x (default) or 2x\n  \
         --mt32-control-rom PATH        MT-32 control ROM (with --mt32-pcm-rom,\n  \
         \x20                            makes \"mt32\" selectable as the MIDI output)\n  \
         --mt32-pcm-rom PATH            MT-32 PCM ROM\n  \
         --mt32-panel                   show the MT-32 front panel\n  \
         --serial MODE                  Paula serial port: off, stdout, midi, tcp,\n  \
         \x20                            tcp-connect, pty, or modem\n  \
         --serial-connect HOST:PORT     dial a remote TCP service (a telnet BBS) with the\n  \
         \x20                            serial port (implies --serial tcp-connect)\n  \
         --serial-session FILE          modem mode: replay a scripted session from FILE\n  \
         \x20                            instead of dialing out over TCP (implies\n  \
         \x20                            --serial modem); see docs/guide/modem.md\n  \
         --a2065-net BACKEND            fit an A2065 Ethernet board: none, loopback, nat,\n  \
         \x20                            or bridge (direct attachment to a host adapter)\n  \
         --a2065-interface NAME         bridge adapter; implies --a2065-net bridge\n  \
         --hostsocket-net BACKEND       fit the HostSocket bsdsocket.library board: none,\n  \
         \x20                            loopback, nat, bridge, or host (direct passthrough\n  \
         \x20                            to real host OS sockets, bypassing the emulated\n  \
         \x20                            stack entirely)\n  \
         --hostsocket-interface NAME    bridge adapter; implies --hostsocket-net bridge\n  \
         --parallel DEVICE              parallel port: none, printer, or sampler\n  \
         --sampler-audio-input NAME     sampler host capture device (implies --parallel sampler)\n  \
         --sampler-input-gain DB        sampler input gain in dB (implies --parallel sampler)\n  \
         --sampler-list-audio-inputs    list host audio input devices and exit\n  \
         {midi}--calibrate-gamepad            interactively bind a USB gamepad to the port-2\n  \
         \x20                            joystick, save the calibration, then exit\n  \
         -h, --help                     show this help and exit\n  \
         -V, --version                  print the version and exit\n\
         \n\
         Window keys:\n  \
         {shortcut}+S save framebuffer to copperline-screenshot-<unix-ts>.png in cwd\n  \
         {shortcut}+D swap to the next disk in a drive's configured playlist\n  \
         {shortcut}+G capture/release host mouse; clicking the display also captures\n  \
         {shortcut}+Q quit\n\
         \n\
         Status bar: every connected floppy drive gets load (multi-select to\n\
         queue a swap playlist), swap, and eject buttons; CDTV/CD32 machines\n\
         add CD load and eject; plus screenshot, volume, pause, power, reboot.\n\
         \n\
         If ROM is given on the command line it overrides the rom path from\n\
         the config. If no config file exists, built-in defaults are used:\n  \
         CPU: 68000   chip RAM: 512K   slow RAM: 512K   fast RAM: 0   chipset: ECS\n  \
         ROM: bundled AROS"
    );
}

fn parse_floppy_drive_idx(s: &str, option: &str) -> Result<usize> {
    let drive = s.trim().to_ascii_lowercase();
    let drive = drive.strip_suffix(':').unwrap_or(&drive);
    let number = drive.strip_prefix("df").unwrap_or(drive);
    let idx: usize = number
        .parse()
        .map_err(|_| anyhow!("{option} drive must be df0, df1, df2, or df3"))?;
    if idx >= 4 {
        return Err(anyhow!("{option} drive must be df0, df1, df2, or df3"));
    }
    Ok(idx)
}

fn parse_floppy_speed(s: &str) -> Result<u16> {
    const MSG: &str = "--floppy-speed PERCENT must be 100, 200, 400, 800, or 0 (turbo)";
    let speed: u16 = s.trim().parse().map_err(|_| anyhow!(MSG))?;
    if speed != copperline::floppy::SPEED_TURBO
        && !copperline::floppy::SUPPORTED_SPEED_PERCENTS.contains(&speed)
    {
        return Err(anyhow!(MSG));
    }
    Ok(speed)
}

#[cfg(feature = "fluxbridge")]
fn parse_floppy_bridge_speed(s: &str) -> Result<u16> {
    const MSG: &str = "--floppy-replay-speed SPEED must be \"normal\" (real speed) or \
                       \"fast\" (double); a track's first read always streams at the \
                       platter's own pace";
    match s.trim().to_ascii_lowercase().as_str() {
        "normal" | "100" => Ok(100),
        "fast" | "200" => Ok(200),
        _ => Err(anyhow!(MSG)),
    }
}

fn parse_floppy_drive_count(s: &str) -> Result<u8> {
    let count: u8 = s
        .parse()
        .map_err(|_| anyhow!("--floppy-drives COUNT must be an integer from 0 to 4"))?;
    if count > 4 {
        return Err(anyhow!(
            "--floppy-drives COUNT must be an integer from 0 to 4"
        ));
    }
    Ok(count)
}
