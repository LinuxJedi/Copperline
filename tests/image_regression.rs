use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[allow(dead_code)]
#[path = "../src/envcfg.rs"]
mod envcfg;

static EMULATOR_TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
struct Image {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

struct FrameDumpReport {
    frames: Vec<Image>,
    output: String,
    elapsed: Duration,
}

impl Image {
    fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let decoder = png::Decoder::new(File::open(path)?);
        let mut reader = decoder.read_info()?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf)?;
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        Ok(Self {
            width: info.width,
            height: info.height,
            rgba: buf[..info.buffer_size()].to_vec(),
        })
    }

    fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let idx = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[idx],
            self.rgba[idx + 1],
            self.rgba[idx + 2],
            self.rgba[idx + 3],
        ]
    }

    fn count_color(&self, color: [u8; 4]) -> usize {
        self.count_color_in(0, 0, self.width, self.height, color)
    }

    fn count_color_in(&self, x0: u32, y0: u32, x1: u32, y1: u32, color: [u8; 4]) -> usize {
        let x1 = x1.min(self.width);
        let y1 = y1.min(self.height);
        let mut count = 0;
        for y in y0.min(self.height)..y1 {
            for x in x0.min(self.width)..x1 {
                if self.pixel(x, y) == color {
                    count += 1;
                }
            }
        }
        count
    }

    fn distinct_color_count(&self) -> usize {
        let mut colors = HashSet::new();
        for pixel in self.rgba.chunks_exact(4) {
            colors.insert([pixel[0], pixel[1], pixel[2], pixel[3]]);
        }
        colors.len()
    }

    fn non_background_bounds(&self, background: [u8; 4]) -> Option<(u32, u32, u32, u32)> {
        let mut min_x = self.width;
        let mut min_y = self.height;
        let mut max_x = 0;
        let mut max_y = 0;
        let mut found = false;
        for y in 0..self.height {
            for x in 0..self.width {
                if self.pixel(x, y) != background {
                    found = true;
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
        }
        found.then_some((min_x, min_y, max_x, max_y))
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Where the integration tests look for ROM/ADF/HDF assets, which are never
/// committed (see `tests/README.md`). Override with `COPPERLINE_TEST_ASSETS`;
/// otherwise a `test-assets/` directory under the repo root is used when it
/// exists, falling back to the repo root itself so existing local checkouts
/// keep working during the transition.
fn asset_dir() -> PathBuf {
    if let Some(dir) = envcfg::var_os("COPPERLINE_TEST_ASSETS") {
        return PathBuf::from(dir);
    }
    let dir = repo_root().join("test-assets");
    if dir.is_dir() {
        return dir;
    }
    repo_root()
}

/// Absolutise a `--config <path>` argument against the repo root. The
/// emulator runs with its working directory set to `asset_dir()` (so a
/// config's relative `rom`/disk paths resolve against the asset directory),
/// while the example configs themselves still live in the repo.
fn resolve_arg_paths(args: &[&str]) -> Vec<String> {
    let root = repo_root();
    let mut out = Vec::with_capacity(args.len());
    let mut config_next = false;
    for &arg in args {
        if config_next {
            let path = Path::new(arg);
            if path.is_absolute() {
                out.push(arg.to_string());
            } else {
                out.push(root.join(arg).to_string_lossy().into_owned());
            }
            config_next = false;
        } else {
            out.push(arg.to_string());
            config_next = arg == "--config";
        }
    }
    out
}

fn lock_emulator_tests() -> std::sync::MutexGuard<'static, ()> {
    EMULATOR_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn have_required_files(paths: &[&str]) -> bool {
    // Assets live in the asset directory; the example configs live in the
    // repo. A required file counts as present if it is in either place.
    let dirs = [asset_dir(), repo_root()];
    let missing: Vec<_> = paths
        .iter()
        .filter(|path| !dirs.iter().any(|dir| dir.join(path).exists()))
        .copied()
        .collect();
    if !missing.is_empty() {
        eprintln!("skipping image regression; missing files: {missing:?}");
        return false;
    }
    true
}

fn write_temp_config(name: &str, contents: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join(format!(
        "copperline-{name}-{}-{}.toml",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, contents.trim_start())?;
    Ok(path)
}

fn screenshot_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "copperline-{name}-{}-{}.png",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ))
}

fn run_screenshot(
    name: &str,
    seconds: &str,
    extra_args: &[&str],
) -> Result<Image, Box<dyn std::error::Error>> {
    let path = screenshot_path(name);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_copperline"));
    cmd.current_dir(asset_dir())
        .env("RUST_LOG", "copperline=warn")
        // Baselines carry the full overscan field; keep them independent of
        // the presentation default ([display] overscan = "tv").
        .env("COPPERLINE_OVERSCAN", "full")
        .arg("--noaudio")
        .args(resolve_arg_paths(extra_args))
        .arg("--screenshot-after")
        .arg(seconds)
        .arg(&path);

    let output = cmd.output()?;
    if !output.status.success() {
        panic!(
            "copperline exited with {}\nstdout tail:\n{}\nstderr tail:\n{}",
            output.status,
            tail_text(&output.stdout),
            tail_text(&output.stderr)
        );
    }
    Image::load(&path)
}

fn frame_dump_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "copperline-{name}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ))
}

fn run_frame_dump_report(
    name: &str,
    seconds: &str,
    count: usize,
    extra_args: &[&str],
    live_audio: bool,
    rust_log: &str,
    envs: &[(&str, &str)],
) -> Result<FrameDumpReport, Box<dyn std::error::Error>> {
    let dir = frame_dump_dir(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_copperline"));
    cmd.current_dir(asset_dir())
        .env("RUST_LOG", rust_log)
        // Baselines carry the full overscan field; keep them independent of
        // the presentation default ([display] overscan = "tv").
        .env("COPPERLINE_OVERSCAN", "full")
        .args(resolve_arg_paths(extra_args))
        .arg("--dump-start")
        .arg(seconds)
        .arg("--dump-count")
        .arg(count.to_string())
        .arg("--dump-frames")
        .arg(&dir);
    if live_audio {
        cmd.arg("--audio");
    } else {
        cmd.arg("--noaudio");
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }

    let started = Instant::now();
    let output = cmd.output()?;
    let elapsed = started.elapsed();
    let combined_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        panic!(
            "copperline exited with {}\nstdout tail:\n{}\nstderr tail:\n{}",
            output.status,
            tail_text(&output.stdout),
            tail_text(&output.stderr)
        );
    }

    let frames = (0..count)
        .map(|idx| Image::load(&dir.join(format!("frame-{idx:06}.png"))))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FrameDumpReport {
        frames,
        output: combined_output,
        elapsed,
    })
}

fn tail_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let start = text.len().saturating_sub(4096);
    text[start..].to_string()
}

fn assert_color_count(img: &Image, color: [u8; 4], min: usize, label: &str) {
    let count = img.count_color(color);
    assert!(
        count >= min,
        "{label}: expected at least {min} pixels of {color:?}, got {count}"
    );
}

fn assert_region_color_count(
    img: &Image,
    rect: (u32, u32, u32, u32),
    color: [u8; 4],
    min: usize,
    label: &str,
) {
    let count = img.count_color_in(rect.0, rect.1, rect.2, rect.3, color);
    assert!(
        count >= min,
        "{label}: expected at least {min} pixels of {color:?} in {rect:?}, got {count}"
    );
}

fn assert_region_color_count_at_most(
    img: &Image,
    rect: (u32, u32, u32, u32),
    color: [u8; 4],
    max: usize,
    label: &str,
) {
    let count = img.count_color_in(rect.0, rect.1, rect.2, rect.3, color);
    assert!(
        count <= max,
        "{label}: expected at most {max} pixels of {color:?} in {rect:?}, got {count}"
    );
}

fn assert_region_non_color_count(
    img: &Image,
    rect: (u32, u32, u32, u32),
    color: [u8; 4],
    min: usize,
    label: &str,
) {
    let x1 = rect.2.min(img.width);
    let y1 = rect.3.min(img.height);
    let mut count = 0usize;
    for y in rect.1.min(img.height)..y1 {
        for x in rect.0.min(img.width)..x1 {
            if img.pixel(x, y) != color {
                count += 1;
            }
        }
    }
    assert!(
        count >= min,
        "{label}: expected at least {min} pixels different from {color:?} in {rect:?}, got {count}"
    );
}

fn assert_distinct_color_count(img: &Image, min: usize, label: &str) {
    let count = img.distinct_color_count();
    assert!(
        count >= min,
        "{label}: expected at least {min} distinct colors, got {count}"
    );
}

fn assert_distinct_color_count_at_most(img: &Image, max: usize, label: &str) {
    let count = img.distinct_color_count();
    assert!(
        count <= max,
        "{label}: expected at most {max} distinct colors, got {count}"
    );
}

fn isolated_vertical_rectangle_columns(prev: &Image, img: &Image, next: &Image) -> usize {
    assert_eq!((prev.width, prev.height), (img.width, img.height));
    assert_eq!((next.width, next.height), (img.width, img.height));

    let min_isolated_height = img.height / 4;
    let mut current_run = 0usize;
    let mut widest_run = 0usize;
    for x in 0..img.width {
        let mut isolated = 0u32;
        for y in 0..img.height {
            let before = prev.pixel(x, y);
            let here = img.pixel(x, y);
            let after = next.pixel(x, y);
            if here != before && here != after && before == after {
                isolated += 1;
            }
        }
        if isolated >= min_isolated_height {
            current_run += 1;
            widest_run = widest_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    widest_run
}

fn assert_no_isolated_vertical_rectangle_frames(frames: &[Image], label: &str) {
    for (idx, frame) in frames.iter().enumerate() {
        assert_eq!(
            (frame.width, frame.height),
            (716, 537),
            "{label} frame {idx}"
        );
        // A genuine noise frame has tens of thousands of distinct colours.
        // The bound leaves room for the presentation's bilinear vertical
        // resampling, which blends scanline pairs into new in-between
        // colours (a clean HAM frame measures ~1.3k after the filter).
        assert_distinct_color_count_at_most(
            frame,
            2_048,
            &format!("{label} frame {idx} random-noise color count"),
        );
    }
    for idx in 1..frames.len().saturating_sub(1) {
        let columns =
            isolated_vertical_rectangle_columns(&frames[idx - 1], &frames[idx], &frames[idx + 1]);
        assert!(
            columns < 8,
            "{label} frame {idx}: isolated vertical rectangle spans {columns} columns"
        );
    }
}

fn assert_noaudio_ham_performance(
    report: &FrameDumpReport,
    seconds: &str,
    count: usize,
    label: &str,
) {
    assert!(
        report
            .output
            .contains("collisions calls=0 pixels=0 full_line_scans=0"),
        "{label}: expected render path to avoid live collision replay scans\n{}",
        tail_string(&report.output)
    );
    assert!(
        !report.output.contains("cpal underrun"),
        "{label}: no-audio capture should not report CPAL underruns\n{}",
        tail_string(&report.output)
    );
    if !cfg!(debug_assertions) {
        let dump_start = seconds
            .parse::<f64>()
            .expect("test dump start should be a numeric literal");
        let emulated_budget = dump_start + (count as f64 / 50.0) + 1.0;
        // COPPERLINE_PERF_BUDGET_SCALE relaxes the wall-clock budget on
        // hosts that cannot sustain full speed (e.g. thermally throttled
        // laptops after long build sessions), so the pixel-content
        // assertions still run. Defaults to 1.0 (full strictness).
        let scale = envcfg::var("COPPERLINE_PERF_BUDGET_SCALE")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| *v >= 1.0)
            .unwrap_or(1.0);
        let wall_budget = emulated_budget * 1.10 * scale;
        let wall = report.elapsed.as_secs_f64();
        assert!(
            wall <= wall_budget,
            "{label}: release no-audio capture took {wall:.3}s, budget {wall_budget:.3}s"
        );
    }
}

fn assert_no_live_audio_underrun_bursts(report: &FrameDumpReport, label: &str) {
    assert!(
        !report.output.contains("cpal underrun"),
        "{label}: live-audio capture reported CPAL underruns\n{}",
        tail_string(&report.output)
    );
}

fn assert_bpu7_latched_plane_ham_content(img: &Image, label: &str) {
    assert_region_non_color_count(
        img,
        (76, 77, 628, 470),
        [0, 0, 0, 255],
        25_000,
        &format!("{label} latched high-plane HAM playfield"),
    );
    assert_distinct_color_count(img, 80, label);
}

fn tail_string(text: &str) -> String {
    let start = text.len().saturating_sub(4096);
    text[start..].to_string()
}

const INSIDE_MACHINE_FILES: &[&str] = &["kickstart205.rom", "DESiRE-InsideTheMachine.adf"];

const INSIDE_MACHINE_CONFIG: &str = r#"
rom = "kickstart205.rom"

[emulation]
speed = "real"

[cpu]
model = "68000"

[memory]
chip = "512K"
slow = "512K"
fast = "0"

[chipset]
revision = "OCS"
video = "PAL"

[floppy.df0]
path = "DESiRE-InsideTheMachine.adf"
write_protected = true
"#;

const DBLPAL_CONFIG: &str = r#"
rom = "KICK31.ROM"

[emulation]
speed = "real"

[machine]
model = "A1200"

[memory]
chip = "2M"
fast = "0"

[chipset]
revision = "AGA"
video = "PAL"

[floppy.df0]
path = "wb31-dblpal.adf"
write_protected = true
"#;

const KICKSTART_205_CONFIG: &str = r#"
rom = "kickstart205.rom"

[emulation]
speed = "real"

[cpu]
model = "68000"

[memory]
chip = "1M"
fast = "0"

[chipset]
revision = "ECS"
video = "PAL"
"#;

/// The DblPAL:LowRes Workbench boot (FMODE SSCAN2/BSCAN2 scan doubling on
/// a programmable 31 kHz scan) presents its full scan on the fixed 716x537
/// output: the desktop reaches the lower half of the picture (the old
/// fixed-canvas presentation cropped everything below the top 285 beam
/// lines) and the time-linear horizontal stretch carries content past the
/// unstretched 45% line width. Structural checks only: the 31 kHz
/// horizontal layout model still has a documented placement residual
/// (docs/internals/video.md), so exact pixel positions are not pinned yet.
#[test]
#[ignore = "runs the emulator and requires local Kickstart ROM assets"]
fn dblpal_boot_presents_full_programmable_scan() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = lock_emulator_tests();
    if !have_required_files(&["wb31-dblpal.adf", "KICK31.ROM"]) {
        return Ok(());
    }

    let cfg_path = write_temp_config("dblpal", DBLPAL_CONFIG)?;
    let cfg_arg = cfg_path.to_string_lossy().into_owned();
    let img = run_screenshot("dblpal", "150.0", &["--config", cfg_arg.as_str()])?;
    let _ = std::fs::remove_file(cfg_path);
    assert_eq!((img.width, img.height), (716, 537));

    // Workbench 3.1 grey desktop fills most of the picture.
    let grey = [170, 170, 170, 255];
    assert_color_count(&img, grey, 150_000, "DblPAL Workbench grey desktop");

    // The desktop's blue window title bar sits in the top quarter and the
    // screen's bottom border line in the bottom tenth: the full 552-line
    // scan is presented, not the top half.
    let blue = img.count_color_in(0, 0, img.width, img.height / 4, [102, 136, 187, 255]);
    assert!(
        blue > 300,
        "expected the window title bar's blue in the top quarter, got {blue} pixels"
    );
    let lower_grey = img.count_color_in(0, img.height / 2, img.width, img.height, grey);
    assert!(
        lower_grey > 80_000,
        "expected the desktop to extend into the lower half, got {lower_grey} grey pixels"
    );

    // Horizontal stretch: content (non-black, non-void) extends past
    // x = 430 (~60% of the width); unstretched it ended near 45%.
    let right_content = img.count_color_in(430, 0, 540, img.height, grey);
    assert!(
        right_content > 5_000,
        "expected stretched desktop content past x=430, got {right_content} pixels"
    );

    Ok(())
}

#[test]
#[ignore = "runs the emulator and requires local Kickstart ROM assets"]
fn kickstart_boot_screen_has_expected_structure() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = lock_emulator_tests();
    if !have_required_files(&["kickstart205.rom"]) {
        return Ok(());
    }

    let cfg_path = write_temp_config("kickstart205", KICKSTART_205_CONFIG)?;
    let cfg_arg = cfg_path.to_string_lossy().into_owned();
    let img = run_screenshot("kickstart", "8.0", &["--config", cfg_arg.as_str()])?;
    let _ = std::fs::remove_file(cfg_path);
    assert_eq!((img.width, img.height), (716, 537));

    let bg = [68, 17, 68, 255];
    assert_color_count(&img, bg, 250_000, "Kickstart purple background");
    assert_color_count(
        &img,
        [238, 170, 136, 255],
        5_000,
        "Kickstart disk/hand foreground",
    );
    assert_region_color_count(
        &img,
        (498, 220, 668, 290),
        [238, 170, 136, 255],
        2_000,
        "Kickstart disk top",
    );
    assert_region_color_count_at_most(
        &img,
        (0, 110, 102, 290),
        [238, 170, 136, 255],
        0,
        "Kickstart left-edge disk debris",
    );
    assert_region_color_count_at_most(
        &img,
        (668, 220, 716, 400),
        [238, 170, 136, 255],
        0,
        "Kickstart detached right-edge disk debris",
    );

    // Exact-color counts sit below the raw stripe areas: the presentation's
    // bilinear vertical resampling blends thin diagonal stripe edges into
    // neighbouring rows, so only interior pixels keep the exact colour.
    for (label, color, min) in [
        ("red check stripe", [255, 0, 0, 255], 70),
        ("yellow check stripe", [255, 255, 0, 255], 140),
        ("green check stripe", [0, 255, 0, 255], 70),
        ("cyan check stripe", [0, 255, 136, 255], 40),
        ("blue check stripe", [0, 51, 255, 255], 20),
    ] {
        assert_region_color_count(&img, (122, 115, 327, 300), color, min, label);
    }

    let bounds = img
        .non_background_bounds(bg)
        .expect("Kickstart should draw visible foreground");
    assert!(
        bounds.0 <= 112 && bounds.1 <= 150 && bounds.2 >= 647 && bounds.3 >= 375,
        "unexpected Kickstart foreground bounds: {bounds:?}"
    );
    Ok(())
}

#[test]
#[ignore = "runs the emulator and requires a local Kickstart 1.3 ROM asset"]
fn reset_dsksync_boot_regression_reaches_boot_display() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = lock_emulator_tests();
    if !have_required_files(&["KICK13.ROM"]) {
        return Ok(());
    }

    let cfg_path = std::env::temp_dir().join(format!(
        "copperline-kick13-reset-dsksync-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &cfg_path,
        r#"
rom = "KICK13.ROM"

[emulation]
speed = "real"

[cpu]
model = "68000"

[memory]
chip = "512K"
fast = "0"

[chipset]
revision = "OCS"
"#,
    )?;
    let cfg_arg = cfg_path.to_string_lossy().into_owned();
    let img = run_screenshot(
        "kick13-reset-dsksync",
        "20.0",
        &["--config", cfg_arg.as_str()],
    )?;
    let _ = std::fs::remove_file(cfg_path);

    let pixels = (img.width * img.height) as usize;
    assert_color_count(&img, [255, 255, 255, 255], pixels / 4, "white field");
    assert_color_count(&img, [0, 0, 0, 255], 5_000, "black insert-disk outline");
    assert_color_count(&img, [187, 187, 187, 255], pixels / 400, "gray disk detail");
    Ok(())
}

#[test]
#[ignore = "runs long OCS HAM captures and requires local Kickstart/disk assets"]
fn ocs_bpu7_ham_captures_avoid_isolated_vertical_rectangle_frames(
) -> Result<(), Box<dyn std::error::Error>> {
    let _guard = lock_emulator_tests();
    if !have_required_files(INSIDE_MACHINE_FILES) {
        return Ok(());
    }
    let cfg_path = write_temp_config("ocs-bpu7-ham", INSIDE_MACHINE_CONFIG)?;
    let cfg_arg = cfg_path.to_string_lossy().into_owned();

    let ham_118 = run_frame_dump_report(
        "ocs-bpu7-ham-118",
        "118.0",
        8,
        &["--config", cfg_arg.as_str()],
        false,
        "info",
        &[],
    )?;
    assert_no_isolated_vertical_rectangle_frames(&ham_118.frames, "118s OCS BPU=7 HAM");
    assert_noaudio_ham_performance(&ham_118, "118.0", 8, "118s OCS BPU=7 HAM");

    let ham_126 = run_frame_dump_report(
        "ocs-bpu7-ham-126",
        "126.0",
        8,
        &["--config", cfg_arg.as_str()],
        false,
        "info",
        &[],
    )?;
    assert_no_isolated_vertical_rectangle_frames(&ham_126.frames, "126s OCS BPU=7 HAM");
    assert_bpu7_latched_plane_ham_content(&ham_126.frames[0], "126s OCS BPU=7 HAM");
    assert_noaudio_ham_performance(&ham_126, "126.0", 8, "126s OCS BPU=7 HAM");
    let _ = std::fs::remove_file(cfg_path);
    Ok(())
}

#[test]
#[ignore = "runs long OCS HAM capture with live CPAL audio; set COPPERLINE_LIVE_AUDIO_ACCEPTANCE=1"]
fn ocs_bpu7_ham_live_audio_capture_has_no_cpal_underrun_bursts(
) -> Result<(), Box<dyn std::error::Error>> {
    let _guard = lock_emulator_tests();
    if envcfg::var_os("COPPERLINE_LIVE_AUDIO_ACCEPTANCE").is_none() {
        eprintln!(
            "skipping live-audio acceptance; set COPPERLINE_LIVE_AUDIO_ACCEPTANCE=1 to run it"
        );
        return Ok(());
    }
    if !have_required_files(INSIDE_MACHINE_FILES) {
        return Ok(());
    }
    let cfg_path = write_temp_config("ocs-bpu7-ham-live-audio", INSIDE_MACHINE_CONFIG)?;
    let cfg_arg = cfg_path.to_string_lossy().into_owned();

    let ham_118 = run_frame_dump_report(
        "ocs-bpu7-ham-live-audio-118",
        "118.0",
        8,
        &["--config", cfg_arg.as_str()],
        true,
        "info",
        &[
            ("COPPERLINE_AUDIO_PROFILE", "1"),
            ("COPPERLINE_REAL_PACING_PROFILE", "1"),
        ],
    )?;
    assert_no_isolated_vertical_rectangle_frames(&ham_118.frames, "118s live-audio OCS BPU=7 HAM");
    assert_no_live_audio_underrun_bursts(&ham_118, "118s live-audio OCS BPU=7 HAM");
    let _ = std::fs::remove_file(cfg_path);
    Ok(())
}

#[test]
#[ignore = "runs the emulator and requires local DiagROM assets"]
fn diagrom_menu_preserves_left_margin_text_columns() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = lock_emulator_tests();
    if !have_required_files(&["diagrom.rom"]) {
        return Ok(());
    }

    let img = run_screenshot("diagrom-left-margin", "6.0", &[])?;
    assert_eq!((img.width, img.height), (716, 537));

    assert_region_color_count(
        &img,
        (70, 103, 80, 116),
        [255, 255, 0, 255],
        12,
        "DiagROM left-margin text column",
    );
    Ok(())
}
