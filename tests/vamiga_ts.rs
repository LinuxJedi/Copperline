use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[allow(dead_code)]
#[path = "../src/envcfg.rs"]
mod envcfg;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct VAmigaTsCase {
    name: String,
    rel_path: PathBuf,
    adf_path: PathBuf,
}

#[derive(Debug)]
struct VAmigaReference {
    executable: PathBuf,
    setup: String,
}

#[test]
#[ignore = "requires COPPERLINE_VAMIGATS_DIR plus a local Kickstart 1.3 ROM"]
fn run_vamiga_ts_adf_screenshots() -> TestResult {
    let Some(root) = env_path("COPPERLINE_VAMIGATS_DIR") else {
        eprintln!("skipping vAmigaTS run; set COPPERLINE_VAMIGATS_DIR to a vAmigaTS checkout");
        return Ok(());
    };
    let root = root.canonicalize().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "canonicalizing COPPERLINE_VAMIGATS_DIR {}: {e}",
                root.display()
            ),
        )
    })?;
    let Some(kick13) = kickstart_13_path() else {
        eprintln!(
            "skipping vAmigaTS run; set COPPERLINE_VAMIGATS_KICK13 or provide {} or /tmp/kick13.rom",
            repo_root().join("KICK13.ROM").display()
        );
        return Ok(());
    };
    let kick13 = kick13.canonicalize().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("canonicalizing Kickstart 1.3 ROM {}: {e}", kick13.display()),
        )
    })?;

    let mut cases = discover_adf_cases(&root)?;
    let filter = envcfg::var("COPPERLINE_VAMIGATS_FILTER");
    if let Some(filter) = filter.as_deref() {
        cases.retain(|case| case.name.contains(filter));
    }
    if let Some(limit) = parse_optional_usize("COPPERLINE_VAMIGATS_LIMIT")? {
        cases.truncate(limit);
    }
    if cases.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no vAmigaTS .adf tests selected under {}{}",
                root.display(),
                filter
                    .as_deref()
                    .map(|f| format!(" with filter {f:?}"))
                    .unwrap_or_default()
            ),
        )
        .into());
    }

    let seconds = envcfg::var("COPPERLINE_VAMIGATS_SECONDS")
        .map(|s| s.parse::<f32>())
        .transpose()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
        .unwrap_or(9.0);
    let out_root = env_path("COPPERLINE_VAMIGATS_OUT")
        .unwrap_or_else(|| unique_temp_dir("copperline-vamigats"));
    fs::create_dir_all(&out_root)?;
    let out_root = out_root.canonicalize()?;
    let baseline_root = env_path("COPPERLINE_VAMIGATS_BASELINE");
    let vamiga_reference =
        env_path("COPPERLINE_VAMIGATS_VAMIGA").map(|executable| VAmigaReference {
            executable,
            setup: envcfg::var("COPPERLINE_VAMIGATS_VAMIGA_SETUP")
                .unwrap_or_else(|| "A500_OCS_1MB".to_string()),
        });

    eprintln!(
        "running {} vAmigaTS case(s) from {} for {seconds:.1}s each; output {}",
        cases.len(),
        root.display(),
        out_root.display()
    );
    for case in cases {
        run_case(
            env!("CARGO_BIN_EXE_copperline"),
            &kick13,
            &out_root,
            baseline_root.as_deref(),
            vamiga_reference.as_ref(),
            seconds,
            &case,
        )?;
    }
    Ok(())
}

fn run_case(
    emulator: &str,
    kick13: &Path,
    out_root: &Path,
    baseline_root: Option<&Path>,
    vamiga_reference: Option<&VAmigaReference>,
    seconds: f32,
    case: &VAmigaTsCase,
) -> TestResult {
    let mut cfg_path = out_root.join(&case.rel_path);
    cfg_path.set_extension("toml");
    let mut png_path = out_root.join(&case.rel_path);
    png_path.set_extension("png");

    if let Some(parent) = cfg_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&cfg_path, copperline_config(kick13, &case.adf_path))?;

    let output = Command::new(emulator)
        .current_dir(repo_root())
        .env("RUST_LOG", "copperline=warn")
        .arg("--noaudio")
        .arg("--config")
        .arg(&cfg_path)
        .arg("--screenshot-after")
        .arg(format!("{seconds:.3}"))
        .arg(&png_path)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{} exited with {}\nstdout tail:\n{}\nstderr tail:\n{}",
            case.name,
            output.status,
            tail_text(&output.stdout),
            tail_text(&output.stderr)
        ))
        .into());
    }

    // Copperline presents at the fixed multisync geometry (716x537) rather than
    // the old 640x480 framebuffer; a screenshot that is any other size means the
    // presentation path changed shape.
    assert_png_dimensions(&png_path, 716, 537)?;
    if let Some(baseline_root) = baseline_root {
        let mut expected = baseline_root.join(&case.rel_path);
        expected.set_extension("png");
        compare_png_bytes(&expected, &png_path, &case.name)?;
    }
    if let Some(vamiga_reference) = vamiga_reference {
        run_vamiga_reference(vamiga_reference, kick13, out_root, seconds, case)?;
    }
    Ok(())
}

fn run_vamiga_reference(
    reference: &VAmigaReference,
    kick13: &Path,
    out_root: &Path,
    seconds: f32,
    case: &VAmigaTsCase,
) -> TestResult {
    let stem = vamiga_temp_stem(case);
    let tmp_dir = std::env::temp_dir();
    let tmp_adf = tmp_dir.join(format!("{stem}.adf"));
    let tmp_kick = tmp_dir.join(format!("{stem}-kick13.rom"));
    let tmp_raw = tmp_dir.join(format!("{stem}.raw"));
    let mut script_path = out_root.join(&case.rel_path);
    script_path.set_extension("vamiga.retrosh");
    let mut raw_path = out_root.join(&case.rel_path);
    raw_path.set_extension("vamiga.raw");

    let result: TestResult = (|| {
        fs::copy(&case.adf_path, &tmp_adf)?;
        fs::copy(kick13, &tmp_kick)?;
        if let Some(parent) = script_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &script_path,
            vamiga_retroshell_script(&reference.setup, &tmp_kick, &tmp_adf, seconds, &stem),
        )?;

        let output = Command::new(&reference.executable)
            .arg(&script_path)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "{} vAmiga reference exited with {}\nstdout tail:\n{}\nstderr tail:\n{}",
                case.name,
                output.status,
                tail_text(&output.stdout),
                tail_text(&output.stderr)
            ))
            .into());
        }

        let raw = fs::read(&tmp_raw).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "{}: reading vAmiga raw output {}: {e}",
                    case.name,
                    tmp_raw.display()
                ),
            )
        })?;
        if raw.len() != 716 * 285 * 3 {
            return Err(io::Error::other(format!(
                "{}: vAmiga raw output has {} bytes, expected 716x285 RGB = {}",
                case.name,
                raw.len(),
                716 * 285 * 3
            ))
            .into());
        }
        fs::write(&raw_path, raw)?;
        Ok(())
    })();

    let _ = fs::remove_file(tmp_adf);
    let _ = fs::remove_file(tmp_kick);
    let _ = fs::remove_file(tmp_raw);
    result
}

fn copperline_config(kick13: &Path, adf: &Path) -> String {
    format!(
        r#"rom = {}

[emulation]
speed = "turbo"

[cpu]
model = "68000"
fpu = false

[memory]
chip = "512K"
fast = "0"
slow = "512K"

[chipset]
revision = "OCS"
video = "PAL"

[floppy.df0]
path = {}
write_protected = true
"#,
        toml_string(&kick13.to_string_lossy()),
        toml_string(&adf.to_string_lossy())
    )
}

fn discover_adf_cases(root: &Path) -> TestResult<Vec<VAmigaTsCase>> {
    let mut cases = Vec::new();
    collect_adf_cases(root, root, &mut cases)?;
    cases.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(cases)
}

fn collect_adf_cases(root: &Path, dir: &Path, cases: &mut Vec<VAmigaTsCase>) -> TestResult {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            if entry.file_name() != ".git" {
                collect_adf_cases(root, &path, cases)?;
            }
            continue;
        }
        if !file_type.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("adf") {
            continue;
        }
        let rel_path = path.strip_prefix(root)?.to_path_buf();
        let name = rel_path
            .with_extension("")
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<String>>()
            .join("/");
        cases.push(VAmigaTsCase {
            name,
            rel_path,
            adf_path: path,
        });
    }
    Ok(())
}

fn vamiga_retroshell_script(
    setup: &str,
    kick13: &Path,
    adf: &Path,
    seconds: f32,
    screenshot_stem: &str,
) -> String {
    format!(
        "# Regression reference script generated by Copperline\n\
         regression setup {setup} {}\n\
         regression run {}\n\
         wait {seconds:.3} seconds\n\
         screenshot save {screenshot_stem}\n",
        kick13.display(),
        adf.display()
    )
}

fn assert_png_dimensions(path: &Path, expected_width: u32, expected_height: u32) -> TestResult {
    let decoder = png::Decoder::new(File::open(path)?);
    let reader = decoder.read_info()?;
    let info = reader.info();
    assert_eq!(
        (info.width, info.height),
        (expected_width, expected_height),
        "{}",
        path.display()
    );
    Ok(())
}

fn compare_png_bytes(expected: &Path, actual: &Path, name: &str) -> TestResult {
    let expected_bytes = fs::read(expected).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "{name}: reading baseline PNG {} failed: {e}",
                expected.display()
            ),
        )
    })?;
    let actual_bytes = fs::read(actual)?;
    if expected_bytes != actual_bytes {
        return Err(io::Error::other(format!(
            "{name}: screenshot differs from baseline {}\nactual: {}",
            expected.display(),
            actual.display()
        ))
        .into());
    }
    Ok(())
}

fn kickstart_13_path() -> Option<PathBuf> {
    env_path("COPPERLINE_VAMIGATS_KICK13")
        .or_else(|| existing_path(repo_root().join("KICK13.ROM")))
        .or_else(|| existing_path(PathBuf::from("/tmp/kick13.rom")))
}

fn existing_path(path: PathBuf) -> Option<PathBuf> {
    path.exists().then_some(path)
}

fn env_path(name: &str) -> Option<PathBuf> {
    envcfg::var_os(name)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

fn parse_optional_usize(name: &str) -> TestResult<Option<usize>> {
    envcfg::var(name)
        .map(|s| {
            s.parse::<usize>()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e).into())
        })
        .transpose()
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()))
}

fn vamiga_temp_stem(case: &VAmigaTsCase) -> String {
    let mut hasher = DefaultHasher::new();
    case.rel_path.hash(&mut hasher);
    let file_stem = case
        .adf_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("case");
    format!(
        "copperline-vamigats-{}-{:016x}-{}",
        std::process::id(),
        hasher.finish(),
        shell_word_stem(file_stem)
    )
}

fn shell_word_stem(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn tail_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let start = text.len().saturating_sub(4096);
    text[start..].to_string()
}

fn toml_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[test]
fn toml_string_escapes_paths() {
    assert_eq!(
        toml_string(r#"C:\roms\kick "1.3".rom"#),
        r#""C:\\roms\\kick \"1.3\".rom""#
    );
}

#[test]
fn discover_adf_cases_finds_nested_tests_in_sorted_order() -> TestResult {
    let root = unique_temp_dir("copperline-vamigats-discovery-test");
    let first = root.join("Agnus/Blitter/bbusy/bbusy0");
    let second = root.join("Paula/Registers/ADKCON/adkcon1");
    fs::create_dir_all(&first)?;
    fs::create_dir_all(&second)?;
    fs::write(second.join("adkcon1.adf"), [])?;
    fs::write(first.join("bbusy0.adf"), [])?;
    fs::write(first.join("bbusy0.txt"), [])?;

    let cases = discover_adf_cases(&root)?;
    let _ = fs::remove_dir_all(&root);

    assert_eq!(
        cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Agnus/Blitter/bbusy/bbusy0/bbusy0",
            "Paula/Registers/ADKCON/adkcon1/adkcon1"
        ]
    );
    Ok(())
}

#[test]
fn vamiga_retroshell_script_uses_temp_paths_and_setup() {
    let script = vamiga_retroshell_script(
        "A500_OCS_1MB",
        Path::new("/tmp/kick13.rom"),
        Path::new("/tmp/bbusy0.adf"),
        9.0,
        "bbusy0",
    );

    assert!(script.contains("regression setup A500_OCS_1MB /tmp/kick13.rom"));
    assert!(script.contains("regression run /tmp/bbusy0.adf"));
    assert!(script.contains("wait 9.000 seconds"));
    assert!(script.contains("screenshot save bbusy0"));
}

#[test]
fn vamiga_temp_stem_keeps_shell_word_characters() {
    let case = VAmigaTsCase {
        name: "Agnus/Blitter/test case/test case".to_string(),
        rel_path: PathBuf::from("Agnus/Blitter/test case/test case.adf"),
        adf_path: PathBuf::from("/suite/Agnus/Blitter/test case/test case.adf"),
    };

    let stem = vamiga_temp_stem(&case);
    assert!(stem.starts_with(&format!("copperline-vamigats-{}-", std::process::id())));
    assert!(stem.ends_with("-test_case"));
    assert!(stem
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));
}
