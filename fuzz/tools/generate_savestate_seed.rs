//! Regenerate the valid seed for the save-state fuzz target.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = copperline::config::Config::default();
    let mut emulator = copperline::emulator::build_machine(
        &cfg,
        Box::new(copperline::audio::NullSink),
        false,
        true,
    )?;
    let bytes = emulator.save_state_bytes()?;
    let mut verifier = copperline::emulator::build_machine(
        &cfg,
        Box::new(copperline::audio::NullSink),
        false,
        true,
    )?;
    verifier.load_state_bytes(&bytes)?;
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/savestate/current.clstate");
    std::fs::create_dir_all(path.parent().expect("corpus path has a parent"))?;
    std::fs::write(&path, bytes)?;
    println!("wrote {}", path.display());
    Ok(())
}
