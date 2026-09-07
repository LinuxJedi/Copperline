//! Fuzz the save-state loader. `.clstate` files are untrusted input (a
//! downloaded state, a shared speedrun snapshot), and the loader parses the
//! chunk container (framing, block runs, MessagePack payloads, migrations)
//! into a full machine before anything else validates it.

#![no_main]

use libfuzzer_sys::fuzz_target;
use std::cell::RefCell;

thread_local! {
    // One deterministic AROS-bootable machine per fuzzing thread; every
    // iteration restores into it from scratch. The emulator is !Send (the
    // audio sink holds host handles), so it lives in thread-local storage.
    static EMULATOR: RefCell<Option<copperline::emulator::Emulator>> =
        const { RefCell::new(None) };
}

fuzz_target!(|data: &[u8]| {
    EMULATOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let loaded = {
            let emulator = slot.get_or_insert_with(|| {
                let cfg = copperline::config::Config::default();
                copperline::emulator::build_machine(
                    &cfg,
                    Box::new(copperline::audio::NullSink),
                    false,
                    true,
                )
                .expect("the factory configuration boots without external assets")
            });
            // Errors are fine; panics, hangs, and over-allocation are not.
            emulator.load_state_bytes(data).is_ok()
        };
        // A successful load replaces the machine -- possibly with a
        // different shape (RAM sizes, chipset, CPU), which power_on_reset
        // would keep. Drop it so the next iteration rebuilds the factory
        // machine and inherits nothing from this input.
        if loaded {
            *slot = None;
        }
    });
});
