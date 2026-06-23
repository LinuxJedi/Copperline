use std::path::Path;
use std::process::Command;

fn main() {
    let package_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());

    let git_dir = git_output(["rev-parse", "--git-dir"]);
    let git_common_dir = git_output(["rev-parse", "--git-common-dir"]);
    if let Some(git_dir) = git_dir.as_deref() {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
    }
    if let (Some(git_dir), Some(git_common_dir)) = (git_dir.as_deref(), git_common_dir.as_deref()) {
        if let Some(head_ref) = current_head_ref(git_dir) {
            println!("cargo:rerun-if-changed={git_dir}/{head_ref}");
            println!("cargo:rerun-if-changed={git_common_dir}/{head_ref}");
        }
    }
    if let Some(git_common_dir) = git_common_dir.as_deref() {
        println!("cargo:rerun-if-changed={git_common_dir}/packed-refs");
        println!("cargo:rerun-if-changed={git_common_dir}/refs/tags");
    }

    let display_version = if exact_tagged_head() {
        package_version
    } else if let Some(short_hash) = git_output(["rev-parse", "--short=8", "HEAD"]) {
        format!("{package_version}+g{short_hash}")
    } else {
        package_version
    };

    println!("cargo:rustc-env=COPPERLINE_DISPLAY_VERSION={display_version}");
}

fn current_head_ref(git_dir: &str) -> Option<String> {
    let head = std::fs::read_to_string(Path::new(git_dir).join("HEAD")).ok()?;
    head.strip_prefix("ref: ").map(|s| s.trim().to_string())
}

fn exact_tagged_head() -> bool {
    git_output(["describe", "--tags", "--exact-match", "HEAD"]).is_some()
}

fn git_output<const N: usize>(args: [&str; N]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}
