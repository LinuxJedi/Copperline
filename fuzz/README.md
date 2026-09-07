# Fuzzing Copperline's untrusted-media parsers

Everything a session parses before any guest code runs is attacker-
reachable input: disk images, CD images, hardfiles, archives, and save
states can arrive from downloads and shared collections. The targets here
feed those parsers raw bytes under libFuzzer.

## Running

```sh
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run dms            # one target...
cargo +nightly fuzz run floppy_image   # ADF/extADF/DMS/SCP/IPF/gzip/zip
cargo +nightly fuzz run cd_image       # CUE/BIN, bare ISO, NRG, and CHD
cargo +nightly fuzz run hardfile_classification # HDF/RDSK/bare-volume classification
cargo +nightly fuzz run savestate      # .clstate chunked machine images
```

Crashes land in `fuzz/artifacts/<target>/`; reproduce one by passing the
file back to the same command. Each target treats parser errors as expected
and panics, hangs, and over-allocation as findings. File-backed targets use
unique temporary directories so parallel libFuzzer workers cannot share or
overwrite inputs.

The save-state target starts from
`corpus/savestate/current.clstate`, a valid state that gets mutations past
the magic, container version, descriptor chunk, and zlib wrapper into the
machine chunks. Whenever `savestate::STATE_VERSION` or a chunk version in
`src/savestate/chunk.rs` changes, regenerate and commit that seed from this
directory:

```sh
cargo +nightly run --example generate_savestate_seed
```

The fuzz crate follows the root Copperline package by path, so its `Cargo.lock`
is generated locally and ignored rather than committing a second copy of the
root dependency graph. The `ci.yml` fuzz job generates that lock and builds
every target (`cargo fuzz build`) so the harnesses cannot rot; it does not run
long campaigns. Sustained runs are a local or scheduled-job concern.
