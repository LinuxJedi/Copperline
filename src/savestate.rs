// SPDX-License-Identifier: GPL-3.0-or-later

//! Versioned save states: snapshot and restore the full emulated machine.
//!
//! A state captures everything the deterministic core needs to resume
//! exactly where it left off: the CPU core, the whole `Bus` (RAM, ROM,
//! chipset, CIAs, floppy images in memory, expansion boards, CD state),
//! and the machine-level timing carries. Host-side state is deliberately
//! excluded and survives the load instead: audio/serial sinks, debugger
//! instrumentation, and diagnostic trace files. File-backed hard-drive and
//! CD images are stored as paths and reopened on load, so their sector
//! contents are NOT part of the state -- a guest that wrote to a hard
//! drive after the snapshot will see those writes after restoring too.
//!
//! Save and load must happen at an emulated-frame boundary; mid-frame the
//! beam-event capture buffers and slice accounting are not in a resumable
//! state. The emulator wrappers (`Emulator::save_state`/`load_state`) are
//! called from the frame loop between frames, which satisfies this.
//!
//! File format: an 8-byte magic, a little-endian u32 format version, an
//! (uncompressed) bincode `MachineDescriptor` naming the machine the state was
//! produced on, then a zlib stream of bincode-encoded components in the fixed
//! order written by `M68kMachine::write_state`. The descriptor lets a load
//! detect that the state belongs to a different machine than the running
//! config and reconfigure the host to match it; the serialized components
//! already carry the actual hardware, so the machine itself always rebuilds
//! from the state regardless.
//!
//! `save`/`load` name a file; `save_to_writer`/`load_from_reader` are the
//! same format over any byte stream, for hosts without a filesystem (the
//! browser build keeps its states in a download or IndexedDB).

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::fs::File;
#[cfg(not(target_arch = "wasm32"))]
use std::io::BufWriter;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use crate::config::MachineDescriptor;
use crate::cpu::M68kMachine;

/// Deserialize one bincode value from a save-state stream, on the wire
/// format `bincode::deserialize_from` uses (fixed-width integers, trailing
/// bytes allowed), but through [`StateReader`] so that no allocation is
/// sized from the stream's own length prefixes.
pub(crate) fn deserialize_from_state<R: Read, T: serde::de::DeserializeOwned>(
    reader: R,
) -> bincode::Result<T> {
    use bincode::Options;
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .deserialize_from_custom(StateReader::new(reader))
}

/// How much a byte buffer grows per read while a length-prefixed string or
/// byte vector is being filled: the most a state can over-allocate past the
/// data it actually holds.
const STATE_FILL_CHUNK: usize = 1 << 20;

/// A bincode reader for untrusted state streams. bincode's own `IoReader`
/// sizes every string and byte-vector buffer from the stream's length
/// prefix before reading a byte of it, so a corrupt or hostile `.clstate`
/// naming a multi-gigabyte chip RAM takes the process down in the
/// allocator (capacity overflow, or an abort) instead of failing the load.
/// This reader fills such buffers in [`STATE_FILL_CHUNK`] steps, so a bogus
/// length runs into the end of the stream having allocated no more than one
/// chunk beyond the bytes that exist, and the load fails with an ordinary
/// error. Found by the `savestate` fuzz target. Reads that are not
/// length-prefixed pass straight through, so the reader never consumes
/// more of the underlying stream than the value being decoded.
///
/// The guarantee is deliberately "memory tracks bytes actually present in
/// the stream", not an absolute cap: state components arrive through a
/// zlib decoder, and a state's legitimate decompressed size is unbounded
/// by design (memory-backed disk images -- HDZ, directory mounts -- ride
/// in the payload), so any fixed limit would refuse real states. A
/// crafted stream that really supplies gigabytes therefore costs
/// gigabytes to load, the same deal as opening any large image the user
/// points the emulator at.
pub(crate) struct StateReader<R> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: Read> StateReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    fn fill(&mut self, length: usize) -> bincode::Result<()> {
        self.buf.clear();
        while self.buf.len() < length {
            let start = self.buf.len();
            let want = (length - start).min(STATE_FILL_CHUNK);
            self.buf.resize(start + want, 0);
            self.inner.read_exact(&mut self.buf[start..])?;
        }
        Ok(())
    }
}

impl<R: Read> Read for StateReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(out)
    }
}

impl<'a, R: Read> bincode::BincodeRead<'a> for StateReader<R> {
    fn forward_read_str<V>(&mut self, length: usize, visitor: V) -> bincode::Result<V::Value>
    where
        V: serde::de::Visitor<'a>,
    {
        self.fill(length)?;
        let string = std::str::from_utf8(&self.buf)
            .map_err(|e| Box::new(bincode::ErrorKind::InvalidUtf8Encoding(e)))?;
        visitor.visit_str(string)
    }

    fn get_byte_buffer(&mut self, length: usize) -> bincode::Result<Vec<u8>> {
        self.fill(length)?;
        Ok(std::mem::take(&mut self.buf))
    }

    fn forward_read_bytes<V>(&mut self, length: usize, visitor: V) -> bincode::Result<V::Value>
    where
        V: serde::de::Visitor<'a>,
    {
        self.fill(length)?;
        visitor.visit_bytes(&self.buf)
    }
}

const STATE_MAGIC: &[u8; 8] = b"CLSSTATE";

/// Save-state format version. The payload is bincode of the live state
/// structs, so ANY shape change to a serialized struct (Bus, the chipset
/// modules, CpuCore, floppy/expansion state, ...) -- fields added, removed,
/// reordered, or retyped -- makes old states unreadable: bump this whenever
/// that happens so stale files fail with a clear version message instead of
/// a confusing decode error.
// Version history:
//   1: initial format
//   2: keyboard MCU model replaced the Bus kbd_queue byte path
//   3: keyboard MCU clock-based handshake timing (state shape change)
//   4: PollStats.custom HashMap replaced by a flat Vec table
//   5: MachineDescriptor header (machine-shape guard rail)
//   6: Memory gained the A1000 WCS (wcs + wcs_write_protected)
//   7: Bus.a2091 Option replaced by Bus.devices Vec<BoardDevice>; the
//      BoardBacking::A2091 variant became BoardBacking::Device(slot)
//   8: BoardDevice gained Wasm and A2065 variants (enum layout change)
//   9: CpuCore.fpr retyped f64 -> FloatX80 (80-bit extended FPU registers)
//  10: CpuCache backing arrays became Vec (variable line count for the
//      68040's 4 KB caches vs the 020/030's 256 bytes)
//  11: CpuCore MMU registers collapsed (removed tc/urp/srp/mmusr duplicates;
//      mmu_sr retyped u16->u32) so the 040 MOVEC path and the page-table
//      walker share one register set
//  12: Paula audio channels gained deferred AUDxEN-disable state so a DMA
//      clear is observed at the current word boundary
//  13: 68060 support - CpuType::M68060 appended; CpuCore gained pcr, buscr,
//      emulate_unimplemented_060, and the Oep060Timing pairing/branch-cache
//      state; MmuFault gained a cause (transient, but part of CpuCore's
//      serde shape indirectly via new fields)
//  14: 68030 resumable bus-fault frames - CpuCore gained mmu_read_override
//      and mmu_write_suppress (the RTE DF-cleared completion protocol,
//      pending across one instruction boundary) and pending_fault_wdata
//      (the frame's data output buffer)
//  15: Bus gained the bitplane DDF sequencer flop state (ddf_seq_line_initial,
//      ddf_seq_line_start_regs, ddf_seq_writes) - the per-line flop walk that
//      replaces the value-range DDF window for FMODE=0 fetches
//  16: CapturedBitplaneRow gained fetch_origin_cck (the sequencer run origin
//      for rows whose fetch diverges from the register-derived window)
//  17: DisplaySpriteDmaState gained the two-slot sprite fetch fields
//      (data_words_fetched pointer progression, pending_data)
//  21: CpuCore gained the 68010 loop-mode state (loop_mode,
//      loop_body_word, loop_dbcc_word)
//  22: Paula AudChannel replaced by the HRM state-machine shape (state,
//      buffer/auddat holding registers, percnt, request latches)
//  23: FloppyDrive gained the step-pulse timestamps (last_step_cck and the
//      per-direction stamps for the mechanism's 40 us pulse floor)
//  24: Cia gained the delayed /IRQ pin state (irq_pin,
//      irq_pin_delay_eticks - the 8520 one-E-cycle interrupt delay)
//  25: Blitter gained the early-dropping DMACONR BBUSY flag (bbusy) and
//      Bus the one-cck INTREQ.BLIT raise delay (blit_irq_delay_cck)
//  26: Copper gained the deferred SKIP decision (skip_eval - the condition
//      sampled at the next instruction's first-word fetch) and the COPJMP
//      strobe tail state (CopperState::Jumping, COP_JMP1/COP_JMP2)
//  27: LineBlitState gained the USEB line-program state (use_a/use_b flags,
//      the live B pointer bpt) and LineBlitPhase the two extra USEB pixel
//      cycles (LB fetch, LBus bare bus cycle)
//  28: Denise and RenderRegisterSnapshot gained the hardware-true sprite
//      latch view (spr_hw_pos/ctl/data/datb/armed - CPU/Copper writes AND
//      sprite DMA fetches, last writer wins; the existing spr* fields
//      remain the CPU/Copper write shadow the render replay is calibrated
//      against)
//  29: Msm6242Rtc gained the deterministic clock seed (seed_unix, frozen -
//      [machine] rtc_time / rtc_frozen), so a resumed run keeps reading
//      the same guest-visible time
//  30: Paula gained per-channel POT scan/discharge state and InputState gained
//      analogue paddle resistances for the RC-based POTxDAT converter
//  31: InputState reshaped into per-port ControllerPort device state (device
//      kind, JOYxDAT counters, button/direction/pot lines, CD32 serial
//      shifter); the Bus cd32_pad_shifter/cd32_pad_fire_oldstate fields
//      moved into the port
//  32: SCSI target slots (Wd33c93, A4091) hold a ScsiTarget enum (disk or
//      CD-ROM drive) instead of a bare ScsiDisk; the CD-ROM drive carries
//      CD-DA playback state and the tray countdown of a pending disc swap
//  33: A2065 gained the latched init-block MODE word (DTX/DRX/LOOP gating
//      of the LANCE engines) and NetConfig the Nat variant (userspace NAT
//      backend)
//  34: the Z3660 RTG board was appended to the BoardDevice enum; a state
//      holding one cannot be read by a build without the variant, so the
//      shape change bumps the version
//  35: Memory gained the Ramsey-controlled motherboard fast RAM bank
//      (mb_ram, ending at $08000000) and MachineDescriptor its size
//      (mb_ram_bytes)
//  36: Memory gained the CPU-slot accelerator fast RAM bank (accel_ram,
//      starting at $08000000) and MachineDescriptor its size
//      (accel_ram_bytes)
//  37: the Bus rtc field became the Rtc chip enum - MSM6242 or the
//      A3000/A4000's RP5C01 ([machine] rtc_chip), the Ricoh part carrying
//      its mode/alarm/battery-RAM state and both sharing the seeded
//      ClockSource
//  38: DriveSounds voices reshaped for the measured clack model (thump/
//      body/ring/tick components, pending rebound clatter, step spacing
//      counter) and the rev-locked motor (hum partial phases, revolution
//      phase, cascaded rumble poles, per-drive pattern seed); the
//      read-gated hiss voice was removed outright
//  39: Rp5c01Rtc gained the battmem backing-file binding (battmem_path,
//      battmem_dirty - [machine] battmem), so a resumed run keeps
//      persisting battery RAM to the same file
//  40: the WASM plugin host moved from wasmtime 27 to the 36 LTS. A board
//      snapshot stores a linear-memory image replayed against a module
//      recompiled at load time, so the serialized shape is unchanged and
//      an older state would still deserialize - and then run against
//      different codegen. Bump so it is refused rather than resumed into
//      a silent divergence (see the wasmtime pin in Cargo.toml)
//  41: Paula records the guest /LED bit (led_filter_guest_on) apart from the
//      effective filter state, for the [audio] audio_filter override; the
//      override mode itself is a host preference and is not serialized
//  42: NetConfig gained the Bridge variant and its host interface identifier
//  43: Zorro BoardSpec gained explicit memory-space, chained-configuration,
//      and tagged device-window fields; BoardDevice gained Picasso2 and its
//      complete CL-GD5426/VRAM state
//  44: Picasso2 and its Cirrus core gained the II+ revision identity and
//      serializable vertical-blank interrupt latch
//  45: Bus gained the 020+ posted-write debt, chip-port turnaround and
//      read-return carry
//  46: the 020+ read-return carry became the shared CPU/chip-bus clock phase
//      (Bus::cpu_chip_clock_phase). The layout is unchanged, but the field
//      now feeds chip-access synchronisation, so a state written before the
//      change would resume with a stale phase
//  47: CdImage's serde shadow became a backend enum (plain image files vs
//      CHD) to carry the new CHD CD image support
//  48: WasmCaps gained the resolve capability (host-OS-resolver lookups for
//      plugin boards -- the bundled HostSocket board's default resolver),
//      changing the bincode layout of every serialized WASM board's
//      manifest (same class of change as 42's NetConfig::Bridge)
//  49: HardDriveImage records a real host disk as one, rather than as a file
//      at the device's path. Loading a 48 state that had one would reopen
//      the raw node as an ordinary file -- read-write whatever it was
//      attached as, and past the checks that refuse the host's own disk
//  50: HostDiskState gained a stable hardware fingerprint and defers raw-media
//      acquisition until the complete state has decoded. Wasmtime also moved
//      from 36.0.12 to the security-fixed 36.0.13; plugin snapshots replay
//      through that runtime, so states from the older codegen are refused.
//  51: Bus gained the cold-power-on RAM initialisation policy, so a state
//      restored and later power-cycled repeats its zero or seeded pseudo-random
//      pattern.
//  52: WasmBoardState gained the faulted flag, preserving permanent plugin
//      fault isolation across save-state restoration.
//  53: BoardDevice gained the IdeZorro variant (the lide.device-compatible
//      Zorro II IDE board, `[lide]`), appended at the end of the enum.
//  54: AtaBus's cylinder registers became per-device-slot pairs, so each
//      slot keeps its own post-reset signature instead of device-select
//      rewriting a shared pair.
//  55: BoardDevice gained the GraffityZ2 and GraffityZ3 variants (the Atéo
//      Concepts Graffity RTG boards, `[rtg] card`), appended at the end of
//      the enum.
//  56: BoardDevice gained the Toccata variant (the MacroSystem Toccata
//      AD1848 sound board, `[toccata]`), appended at the end of the enum.
//  57: Paula gained the MHI-board audio ring (mhi_audio, `MhiAudioRing`) and
//      BoardDevice gained the Mhi variant (the virtual MPEG audio decoder
//      board, `[mhi]`, feature-gated behind `mhi`), appended at the end of
//      the enum.
//  58: The MHI board's decoder snapshot changed shape: the minimp3
//      field-for-field `mp3dec_t` shadow became a Symphonia warmup history
//      (the raw bytes of the most recently decoded frames, re-decoded on
//      restore) when the decoder moved to pure Rust for MSVC ARM64 hosts
//      (issue #474).
//  59: Mhi gained the M4 bass/mid/treble filter bank (tone_filters,
//      `ToneFilterBank`) -- the param-latch DSP chain's genuine machine
//      state (biquad coefficients and filter memory), `[mhi]`, feature-
//      gated behind `mhi`.
//  60: Akiko's `command_active` widened from u8 to u32: it now counts the
//      drive microcontroller's command turnaround in emulated CCKs
//      (CMD_EXEC_DELAY_CCK) instead of counting register accesses.
//  61: PortDevice gained the GamepadMouse variant (a mouse a gamepad can
//      move as well as the host's own, `[input] port1`), appended at the
//      end of the enum.
//  62: CdImage's cue-sheet shadow records each FILE's format and sector
//      byte length (WAVE/MP3 audio tracks, reopened and re-indexed on
//      load) and its extents gained a storage tag (file bytes or an
//      unstored PREGAP/POSTGAP).
//  63: DiskDma gained `write_start_pending`, so a write armed against an
//      idle floppy mechanism re-latches its rotational start when cells
//      first arrive.
//  64: the bundled HostSocket WASM module moved from smoltcp 0.13 to 0.14.
//      Its TCP/IP stack lives as Rust values in the plugin's snapshotted
//      linear memory, whose internal layout is replayed against the current
//      module on load; reject old snapshots rather than interpret that memory
//      with the new dependency's layout.
//  65: floppy images record whether writable changes go to a host file or
//      remain in serialized memory for filesystem-free hosts such as WASM.
//  66: BoardDevice gained the CD32 FMV variant, including the cartridge ROM,
//      CL450/L64111 state, decoder warm-up histories, queued PCM/video, and
//      genlock presentation latch; Akiko also records READ DATA's end LSN.
//  67: CD32 FMV audio gained native-rate resampler state and BoardSpec records
//      whether an autoconfig board ignores ec_Shutup.
//  68: Akiko gained carried physical byte offsets for command/response packets
//      whose visible eight-bit indices cross a page boundary.
//  69: the Bus gained the Agnus vertical display window flop
//      (`diw_vertical_open`, set on a DIWSTRT.V match, reset on DIWSTOP.V).
//      It is history-dependent and snapshots can be taken mid-frame, so it
//      travels in the state instead of being reconstructed from registers.
//  70: Akiko gained the cold/warm spin-up gate on the first lead-in dump
//      (`toc_spin_up_cck`) and the dump-exclusive command hold
//      (`command_deferred_for_toc`), both calibrated against a real-CD32
//      boot video and the cd32-probe rows.
//  71: the Bus gained two pieces of beam-timed interrupt/sprite state.
//      `sprite_dma_replay` is the pre-display sprite-DMA replay cursor
//      (line, slot, render-event index and running DMACON): that replay is
//      now paced by the beam rather than batched at the display start, and a
//      snapshot can be taken part way through the pre-display window, so the
//      cursor has to travel with the state. `irq_latency_visible_at` replaces
//      the single IPL-pipe countdown with one delivery deadline per interrupt
//      source, in absolute colour clocks.
//  72: the floppy controller lost the DSKBYTR track-grid position tracking
//      and the WORDEQUAL latch (`last_dskbytr_pos`, `last_stream_sync_pos`,
//      `word_equal_latch`): DSKBYTR's byte and WORDEQUAL now come from
//      Paula's read shifter on the framing a WORDSYNC match resets, as on
//      hardware. Raw track images
//      (`FloppyTrackImage::RawMfm`) and cached revolutions (`TrackRev`)
//      gained the mastered cell-rate profile of IPF density models.
//  73: the bus gained the WinUAE-compatible uaelib trap (`uaelib`): its
//      result/doorbell latch image, the pending warp request, the debug
//      event queue, the resource registry and the idle accounting travel
//      with the state, so run-ahead and rewind restore them together with
//      the guest state that produced them.
//  74: the bus dropped the `diag_lowmem_blit` crash-context alarm from the
//      layout. It is host diagnostics (consumed only by
//      COPPERLINE_DIAG_CRASH) and the loader already cleared it, so a run
//      resumed from a state and the uninterrupted run it was taken from now
//      write byte-identical states again.
//  75: the bus gained the freezer cartridge (`cartridge`): its 1 MiB bank,
//      the custom/CIA register shadows kept for it and the pending level-7
//      freeze interrupt travel with the state, so a monitor session and
//      the snapshot it took of the interrupted program survive a resume.
//  76: the uaelib trap gained the fn-88 overlay display list (the rects
//      and text the guest asked to be drawn over the picture) and its
//      drop counter, guest state like the registry.
//  77: copperhf.device's asynchronous worker-thread I/O (M5): CopperhfBoard
//      gained a per-unit cached `total_sectors` (`unit_sectors`) alongside
//      the existing per-unit state; always serialized quiesced (no
//      in-flight requests), so the shape otherwise stays close to M4's.
//      (Was 71, then 75 on the copperhf-device branch; renumbered past
//      upstream's own 71-76 at each merge.)
//  78: Akiko's dump-exclusive command hold (`command_deferred_for_toc`)
//      became the drive's unparsed receive buffer (`tx_fifo`): the TX DMA
//      now drains the guest's command ring into it regardless of the
//      drive's state, and the drive parses commands out of it only
//      between dumps.
//  79: Expansion-board kind IDs are explicit and feature-independent. Builds
//      without an optional board reject its kind without renumbering others.
//  80: Hard-drive state distinguishes private netplay sector overlays from
//      full in-memory volumes and persistent image files.
pub const STATE_VERSION: u32 = 80;

/// Default state file name, timestamped like the screenshot/recorder names.
pub fn auto_filename() -> std::path::PathBuf {
    crate::paths::state_file()
}

/// Number of numbered quick-save slots. Ten, so they map onto the host
/// number-row keys `1`..`9`, `0`.
pub const SLOT_COUNT: usize = 10;

/// Resolve a numbered quick-save slot below an explicit state directory.
/// Keeping this separate from host-directory discovery lets frontends and
/// tests inject an isolated slot root without changing process-global path
/// state.
pub(crate) fn slot_path_in(dir: &Path, slot: usize) -> Option<std::path::PathBuf> {
    (1..=SLOT_COUNT)
        .contains(&slot)
        .then(|| dir.join(format!("slot{slot}.clstate")))
}

/// File backing quick-save slot `slot` (1-based, `1..=SLOT_COUNT`). `None`
/// when the host offers no directory to keep them in.
///
/// Slots normally live in the per-user state directory rather than beside a
/// config file or in the working directory: they are a host convenience, they
/// must be reachable however the emulator was launched, and a bare relative
/// path would scatter them across whatever directory happened to be current.
/// Portable mode deliberately roots them beside the executable instead. A
/// state carries its own [`MachineDescriptor`], so loading a slot saved from a
/// different machine is caught and reported rather than silently wrong.
pub fn slot_path(slot: usize) -> Option<std::path::PathBuf> {
    crate::paths::state_slot_dir().and_then(|dir| slot_path_in(&dir, slot))
}

/// Write the machine's emulated state to `path`, stamped with `descriptor`
/// (the shape of the machine that produced it). Call only between emulated
/// frames.
pub fn save(machine: &M68kMachine, descriptor: &MachineDescriptor, path: &Path) -> Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        replace_state_file(path, |file| {
            save_to_writer(machine, descriptor, BufWriter::new(file))
        })
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (machine, descriptor, path);
        bail!("file-backed save states are unavailable on wasm32; use save_to_writer")
    }
}

/// Publish only a complete state. The temporary file is on the destination
/// filesystem so replacement is atomic; dropping it cleans up every failure.
#[cfg(not(target_arch = "wasm32"))]
fn replace_state_file(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    crate::paths::ensure_parent(path)
        .with_context(|| format!("creating the directory for {}", path.display()))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".copperline-state-")
        .tempfile_in(parent)
        .with_context(|| format!("creating temporary save state beside {}", path.display()))?;
    write(temp.as_file_mut()).with_context(|| format!("writing save state {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .context("syncing completed save state")?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replacing save state {}", path.display()))?;
    Ok(())
}

/// `save` without a filesystem: write the same bytes a state file holds into
/// any sink. Hosts with nowhere to put a file -- the browser build, which
/// hands the blob to a download or IndexedDB -- go through here, so a state
/// produced in a browser and one produced by the desktop are the same format
/// and interchangeable.
pub fn save_to_writer<W: Write>(
    machine: &M68kMachine,
    descriptor: &MachineDescriptor,
    mut writer: W,
) -> Result<()> {
    writer.write_all(STATE_MAGIC)?;
    writer.write_all(&STATE_VERSION.to_le_bytes())?;
    // The descriptor sits uncompressed ahead of the zlib stream so it can be
    // read (and a mismatch detected) without decompressing the whole machine.
    bincode::serialize_into(&mut writer, descriptor)
        .map_err(|e| anyhow!("serializing machine descriptor: {e}"))?;
    let mut encoder = ZlibEncoder::new(writer, Compression::fast());
    machine.write_state(&mut encoder)?;
    encoder.finish().and_then(|mut w| w.flush())?;
    Ok(())
}

/// Restore the machine from a state written by `save`, returning the machine
/// descriptor the state was stamped with so the caller can compare it against
/// the running machine and reconfigure the host. The live machine is left
/// untouched if the file is unreadable, has the wrong magic/version, or any
/// referenced disk image cannot be reopened. Call only between emulated
/// frames, and re-anchor real-time pacing afterwards (`Emulator::load_state`
/// does both).
pub fn load(machine: &mut M68kMachine, path: &Path) -> Result<MachineDescriptor> {
    let file =
        File::open(path).with_context(|| format!("opening save state {}", path.display()))?;
    load_from_reader(machine, BufReader::new(file))
        .with_context(|| format!("loading save state {}", path.display()))
}

/// `load` without a filesystem: restore from the bytes of a state file held
/// anywhere (a browser download, an IndexedDB record, a network response).
/// The same guarantees hold -- the live machine is untouched unless the whole
/// state parses -- and the caller still owns re-anchoring host pacing.
pub fn load_from_reader<R: Read>(
    machine: &mut M68kMachine,
    mut reader: R,
) -> Result<MachineDescriptor> {
    let mut magic = [0u8; STATE_MAGIC.len()];
    reader
        .read_exact(&mut magic)
        .context("reading save state header")?;
    if &magic != STATE_MAGIC {
        bail!("not a Copperline save state");
    }
    let mut version_bytes = [0u8; 4];
    reader
        .read_exact(&mut version_bytes)
        .context("reading save state header")?;
    let version = u32::from_le_bytes(version_bytes);
    if version != STATE_VERSION {
        bail!(
            "save state is format version {version}; this build reads version {}",
            STATE_VERSION
        );
    }
    // Read the descriptor straight from the reader; bincode consumes exactly
    // its encoded bytes, leaving the reader positioned at the zlib stream.
    let descriptor: MachineDescriptor = deserialize_from_state(&mut reader)
        .map_err(|e| anyhow!("reading machine descriptor: {e}"))?;
    let mut decoder = ZlibDecoder::new(reader);
    machine.apply_state(&mut decoder)?;
    Ok(descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::NullSink;
    use crate::bus::Bus;
    use crate::chipset::paula::Paula;
    use crate::config::CpuModel;
    use crate::floppy::FloppyController;
    use crate::memory::{Memory, RamInit, CHIP_RAM_BASE, ROM_SIZE};
    use crate::serial::NullSerialSink;
    use crate::zorro::ZorroChain;

    /// Minimal machine: reset vectors into ROM, where a `bra.s` spins.
    fn test_machine() -> M68kMachine {
        let mut rom = vec![0u8; ROM_SIZE];
        rom[0..4].copy_from_slice(&0x0000_4000u32.to_be_bytes()); // SP
        rom[4..8].copy_from_slice(&0x00F8_0010u32.to_be_bytes()); // PC
        rom[0x10..0x12].copy_from_slice(&0x60FEu16.to_be_bytes()); // bra.s self
        let bus = Bus::new(
            Memory {
                chip_ram: vec![0u8; 512 * 1024],
                slow_ram: Vec::new(),
                mb_ram: Vec::new(),
                accel_ram: Vec::new(),
                rom,
                overlay: false,
                zorro: ZorroChain::default(),
                extended_rom: Vec::new(),
                extended_rom_base: 0,
                wcs: Vec::new(),
                wcs_write_protected: false,
            },
            Paula::new(Box::new(NullSerialSink), Box::new(NullSink)),
            FloppyController::default(),
        );
        crate::cpu::build(bus, CpuModel::M68000, false, 2, Default::default(), false).unwrap()
    }

    fn temp_state(name: &str) -> std::path::PathBuf {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "copperline-savestate-{}-{unique}-{name}.clstate",
            std::process::id()
        ))
    }

    /// Machine whose ROM bootstrap copies a busy workload into chip RAM and
    /// runs it there: a main loop that waits for blitter idle, programs a
    /// 32x16-word A->D copy blit and starts it, and counts iterations at
    /// $180; a Copper list with colour MOVEs and two WAITs; and a VERTB
    /// interrupt handler counting fields at $184. Together they keep the
    /// blitter pipeline, Copper comparator, interrupt latency pipe, and
    /// CPU chip-bus arbitration all active at any snapshot point.
    fn blitting_workload_machine() -> M68kMachine {
        // Chip-RAM image, assembled for base $2000.
        // $2000 handler: move.w #$0020,$9C(a5); addq.l #1,$184.w; rte
        let handler: [u16; 7] = [0x3B7C, 0x0020, 0x009C, 0x52B8, 0x0184, 0x4E73, 0x4E71];
        // $2010 entry:
        let entry: [u16; 42] = [
            0x4BF9, 0x00DF, 0xF000, // lea $DFF000,a5
            0x21FC, 0x0000, 0x2000, 0x006C, // move.l #$2000,$6C.w (level-3 autovector)
            0x41F8, 0x1000, // lea $1000.w,a0 (copper list)
            0x20FC, 0x0180, 0x0F00, // move.l #$01800F00,(a0)+  COLOR00 red
            0x20FC, 0x8107, 0xFFFE, // move.l #$8107FFFE,(a0)+  WAIT v=$81
            0x20FC, 0x0180, 0x000F, // move.l #$0180000F,(a0)+  COLOR00 blue
            0x20FC, 0xC107, 0xFFFE, // move.l #$C107FFFE,(a0)+  WAIT v=$C1
            0x20FC, 0x0180, 0x00F0, // move.l #$018000F0,(a0)+  COLOR00 green
            0x20FC, 0xFFFF, 0xFFFE, // move.l #$FFFFFFFE,(a0)+  end of list
            0x2B7C, 0x0000, 0x1000, 0x0080, // move.l #$1000,COP1LC
            0x3B7C, 0x0000, 0x0088, // move.w #0,COPJMP1
            0x3B7C, 0xC020, 0x009A, // move.w #$C020,INTENA (master+VERTB)
            0x3B7C, 0x82C0, 0x0096, // move.w #$82C0,DMACON (DMAEN|COPEN|BLTEN)
            0x46FC, 0x2000, // move.w #$2000,sr (supervisor, IPL 0)
        ];
        // $2064 loop:
        let mainloop: [u16; 37] = [
            0x302D, 0x0002, // wait_idle: move.w DMACONR(a5),d0
            0x0240, 0x4000, // andi.w #$4000,d0 (BBUSY)
            0x66F6, // bne.s wait_idle
            0x3B7C, 0x09F0, 0x0040, // move.w #$09F0,BLTCON0 (A->D copy)
            0x3B7C, 0x0000, 0x0042, // move.w #0,BLTCON1
            0x3B7C, 0xFFFF, 0x0044, // move.w #$FFFF,BLTAFWM
            0x3B7C, 0xFFFF, 0x0046, // move.w #$FFFF,BLTALWM
            0x2B7C, 0x0000, 0x8000, 0x0050, // move.l #$8000,BLTAPT
            0x2B7C, 0x0004, 0x0000, 0x0054, // move.l #$40000,BLTDPT
            0x3B7C, 0x0000, 0x0064, // move.w #0,BLTAMOD
            0x3B7C, 0x0000, 0x0066, // move.w #0,BLTDMOD
            0x3B7C, 0x0810, 0x0058, // move.w #$0810,BLTSIZE (32 rows x 16 words)
            0x52B8, 0x0180, // addq.l #1,$180.w (loop counter)
            0x60B6, // bra.s wait_idle
        ];

        // Everything lives in chip RAM: with the boot overlay off, the CPU
        // reads its reset vectors from address 0 there, so the program is
        // placed directly and no ROM bootstrap is involved.
        let mut chip_ram = vec![0u8; 512 * 1024];
        chip_ram[0..4].copy_from_slice(&0x0000_4000u32.to_be_bytes()); // reset SP
        chip_ram[4..8].copy_from_slice(&0x0000_2010u32.to_be_bytes()); // reset PC
        let mut poke = |base: usize, words: &[u16]| {
            for (i, w) in words.iter().enumerate() {
                chip_ram[base + 2 * i..base + 2 * i + 2].copy_from_slice(&w.to_be_bytes());
            }
        };
        poke(0x2000, &handler);
        poke(0x2010, &entry);
        poke(0x2064, &mainloop);
        // Blit source pattern at $8000 so the A->D copies move real data.
        for (i, byte) in chip_ram[0x8000..0x8400].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let bus = Bus::new(
            Memory {
                chip_ram,
                slow_ram: Vec::new(),
                mb_ram: Vec::new(),
                accel_ram: Vec::new(),
                rom: vec![0u8; ROM_SIZE],
                overlay: false,
                zorro: ZorroChain::default(),
                extended_rom: Vec::new(),
                extended_rom_base: 0,
                wcs: Vec::new(),
                wcs_write_protected: false,
            },
            Paula::new(Box::new(NullSerialSink), Box::new(NullSink)),
            FloppyController::default(),
        );
        crate::cpu::build(bus, CpuModel::M68000, false, 2, Default::default(), false).unwrap()
    }

    fn state_blob(machine: &M68kMachine) -> Vec<u8> {
        let mut blob = Vec::new();
        machine.write_state(&mut blob).unwrap();
        blob
    }

    /// A state saved while the machine is mid-workload (blitter busy, Copper
    /// waiting, interrupts flowing) must resume into a byte-identical
    /// timeline: continue the live machine and a restored copy by the same
    /// instruction count and compare the FULL serialized state of both.
    /// Guards the save/restore completeness of the scheduled-blitter
    /// micro-programs, the Copper WAIT/SKIP state, and the IRQ latency pipe
    /// (the "resumed demo freezes" class of bug).
    #[test]
    fn resumed_state_continues_byte_identically_under_active_workload() {
        let mut machine = blitting_workload_machine();

        // Run past boot into the steady blit loop, then to a frame boundary
        // (production saves happen there), then onto a colour clock where a
        // blit is actually in flight. `step_slice(n)` is a budget that ends
        // early on MMIO preempts (every BLTSIZE write), so loop on the
        // retired-instruction count like the production frame loop does.
        let mut retired = 0usize;
        while retired < 4000 {
            retired += machine.step_slice(4000 - retired).unwrap().instructions;
        }
        assert!(
            machine.bus().mem.chip_ram[0x180..0x184] != [0, 0, 0, 0],
            "workload loop counter must be advancing (program mis-assembled?): \
             pc={:06X} vertb_count={:02X?} cck={}",
            machine.pc(),
            &machine.bus().mem.chip_ram[0x184..0x188],
            machine.bus().emulated_cck(),
        );
        let start_frames = machine.bus().emulated_frames();
        while machine.bus().emulated_frames() == start_frames {
            machine.step_slice(16).unwrap();
        }
        while !machine.bus().blitter.busy {
            machine.step_slice(1).unwrap();
        }

        let saved = state_blob(&machine);
        let counter_at_save = machine.bus().mem.chip_ram[0x180..0x184].to_vec();
        let frames_at_save = machine.bus().emulated_frames();

        // Continue both timelines by the same instruction count, far enough
        // to cross at least two frame wraps: the state loader deliberately
        // clears the (serialized) mid-frame render-capture buffers
        // (`reset_transient_video_after_state_load`), and the wrap is where
        // both timelines rebuild them identically. Everything the chips and
        // CPU compute must match from the very first instruction; the wraps
        // only launder the render-capture bookkeeping.
        let continue_instructions = 40_000usize;
        let run = |m: &mut M68kMachine| {
            let mut retired = 0usize;
            while retired < continue_instructions {
                retired += m
                    .step_slice(continue_instructions - retired)
                    .unwrap()
                    .instructions;
            }
        };

        run(&mut machine);
        assert!(
            machine.bus().mem.chip_ram[0x180..0x184] != counter_at_save[..],
            "live workload stalled after the save point"
        );
        assert!(
            machine.bus().emulated_frames() >= frames_at_save + 2,
            "continuation must cross two frame wraps to launder capture state"
        );
        let live_after = state_blob(&machine);

        // Restore the snapshot into a fresh machine and continue identically.
        let mut restored = blitting_workload_machine();
        restored
            .apply_state(&mut std::io::Cursor::new(&saved))
            .unwrap();
        run(&mut restored);
        let restored_after = state_blob(&restored);

        // The restored timeline advanced past the save point...
        assert!(
            restored.bus().mem.chip_ram[0x180..0x184] != counter_at_save[..],
            "restored machine stopped executing the workload (the resume-freeze class)"
        );
        // ...and matches the live one exactly, in every serialized component.
        if live_after != restored_after {
            let first_diff = live_after
                .iter()
                .zip(restored_after.iter())
                .position(|(a, b)| a != b);
            let ram_diff = machine
                .bus()
                .mem
                .chip_ram
                .iter()
                .zip(restored.bus().mem.chip_ram.iter())
                .position(|(a, b)| a != b);
            panic!(
                "resumed timeline diverged from the live one: blob lengths {}/{}, \
                 first differing byte at {:?}; chip RAM first diff at {:X?}; \
                 live cck={} v={} h={} pc={:06X} counter={:02X?} vertb={:02X?}; \
                 restored cck={} v={} h={} pc={:06X} counter={:02X?} vertb={:02X?}",
                live_after.len(),
                restored_after.len(),
                first_diff,
                ram_diff,
                machine.bus().emulated_cck(),
                machine.bus().agnus.vpos,
                machine.bus().agnus.hpos,
                machine.pc(),
                &machine.bus().mem.chip_ram[0x180..0x184],
                &machine.bus().mem.chip_ram[0x184..0x188],
                restored.bus().emulated_cck(),
                restored.bus().agnus.vpos,
                restored.bus().agnus.hpos,
                restored.pc(),
                &restored.bus().mem.chip_ram[0x180..0x184],
                &restored.bus().mem.chip_ram[0x184..0x188],
            );
        }
    }

    /// The in-memory API writes the same format the file API reads, and
    /// round-trips a machine through a `Vec<u8>` with no filesystem in the
    /// way. This is the path the browser build takes, where `save`/`load`
    /// cannot work at all.
    #[test]
    fn writer_reader_round_trip_matches_the_file_format() {
        let mut machine = blitting_workload_machine();
        // Into the running workload, then to a frame boundary: where a
        // production save happens.
        machine.step_slice(20_000).unwrap();
        let start_frames = machine.bus().emulated_frames();
        while machine.bus().emulated_frames() == start_frames {
            machine.step_slice(16).unwrap();
        }
        let descriptor = MachineDescriptor::default();

        let mut blob = Vec::new();
        save_to_writer(&machine, &descriptor, &mut blob).unwrap();

        // A file written by `save` is byte-identical to the blob, so states
        // move between the desktop and the browser in either direction.
        let path = temp_state("writer-parity");
        save(&machine, &descriptor, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), blob);
        let _ = std::fs::remove_file(&path);

        let mut restored = test_machine();
        let loaded = load_from_reader(&mut restored, blob.as_slice()).unwrap();
        assert_eq!(loaded, descriptor);
        assert_eq!(restored.pc(), machine.pc());
        assert_eq!(
            restored.bus().emulated_cck(),
            machine.bus().emulated_cck(),
            "the restored timeline must resume where the save left off"
        );
        assert_eq!(restored.bus().mem.chip_ram, machine.bus().mem.chip_ram);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn failed_save_preserves_the_existing_state_and_removes_the_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slot1.clstate");
        let machine = test_machine();
        save(&machine, &MachineDescriptor::default(), &path).unwrap();
        let previous = std::fs::read(&path).unwrap();

        let failure = replace_state_file(&path, |file| {
            file.write_all(b"incomplete replacement")?;
            bail!("injected write failure")
        });
        assert!(failure.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        load(&mut test_machine(), &path).unwrap();

        let mut changed = test_machine();
        changed.bus_mut().mem.chip_ram[0x100] = 0x5a;
        save(&changed, &MachineDescriptor::default(), &path).unwrap();
        let mut restored = test_machine();
        load(&mut restored, &path).unwrap();
        assert_eq!(restored.bus().mem.chip_ram[0x100], 0x5a);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn failed_save_publication_removes_the_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("directory");
        std::fs::create_dir(&destination).unwrap();
        let failure = replace_state_file(&destination, |file| {
            file.write_all(b"complete state")?;
            Ok(())
        });
        assert!(failure.is_err());
        assert!(destination.is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn ram_initialisation_policy_survives_a_save_state() {
        let init = RamInit::Random { seed: 0x1234_5678 };
        let mut machine = test_machine();
        machine.bus_mut().set_ram_init(init);
        let descriptor = MachineDescriptor::default();
        let mut blob = Vec::new();
        save_to_writer(&machine, &descriptor, &mut blob).unwrap();

        let mut restored = test_machine();
        load_from_reader(&mut restored, blob.as_slice()).unwrap();
        restored.bus_mut().power_on_reset();

        let mut expected = vec![0; restored.bus().mem.chip_ram.len()];
        init.fill(&mut expected, CHIP_RAM_BASE);
        assert_eq!(restored.bus().mem.chip_ram, expected);
    }

    #[test]
    fn reader_rejects_a_blob_without_the_state_magic() {
        let mut machine = test_machine();
        let before_pc = machine.pc();
        let err = load_from_reader(&mut machine, b"NOTASTATEFILE".as_slice()).unwrap_err();
        assert!(format!("{err:#}").contains("not a Copperline save state"));
        assert_eq!(machine.pc(), before_pc);
    }

    #[test]
    fn rejects_files_without_the_state_magic() {
        let path = temp_state("magic");
        std::fs::write(&path, b"NOTASTATEFILE").unwrap();
        let mut machine = test_machine();
        let before_pc = machine.pc();
        let err = load(&mut machine, &path).unwrap_err();
        // The cause carries the diagnosis; the outer context names the file.
        let reported = format!("{err:#}");
        assert!(reported.contains("not a Copperline save state"));
        assert!(reported.contains(&path.display().to_string()));
        // A failed load leaves the live machine untouched.
        assert_eq!(machine.pc(), before_pc);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_other_format_versions() {
        let path = temp_state("version");
        let mut bytes = STATE_MAGIC.to_vec();
        bytes.extend_from_slice(&(STATE_VERSION + 1).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let mut machine = test_machine();
        let err = load(&mut machine, &path).unwrap_err();
        assert!(format!("{err:#}").contains("format version"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn state_reader_matches_bincode_wire_format_across_chunk_boundaries() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Sample {
            // A String is the type that actually reaches
            // `BincodeRead::get_byte_buffer` in bincode 1 (`Vec<u8>` goes
            // through serde's element-wise sequence path); longer than two
            // fill chunks, so the buffer is assembled from several reads.
            name: String,
            ram: Vec<u8>,
            words: Vec<u16>,
            tail: u32,
        }
        let sample = Sample {
            name: "A1200-"
                .chars()
                .cycle()
                .take(STATE_FILL_CHUNK * 2 + 777)
                .collect(),
            ram: (0..4096).map(|i| i as u8).collect(),
            words: vec![1, 2, 3],
            tail: 0xDEAD_BEEF,
        };
        let bytes = bincode::serialize(&sample).unwrap();
        assert!(
            bytes.len() > STATE_FILL_CHUNK * 2,
            "string must span chunks"
        );
        let back: Sample = deserialize_from_state(&bytes[..]).unwrap();
        assert_eq!(back, sample);
    }

    #[test]
    fn state_reader_refuses_length_prefixes_past_the_stream_without_allocating_them() {
        // A byte vector claiming u64::MAX bytes, then nothing. bincode's own
        // reader resizes its buffer to that length first (capacity
        // overflow); this reader runs into the end of the stream instead.
        let bogus = [0xFFu8; 8];
        let err = deserialize_from_state::<_, Vec<u8>>(&bogus[..]).unwrap_err();
        assert!(
            matches!(*err, bincode::ErrorKind::Io(_)),
            "expected an end-of-stream error, got {err}"
        );
        let err = deserialize_from_state::<_, String>(&bogus[..]).unwrap_err();
        assert!(matches!(*err, bincode::ErrorKind::Io(_)), "{err}");
        // A plausible but oversized length (a claimed 1 GiB chip RAM in a
        // 16-byte stream) is refused the same way, having allocated no more
        // than one chunk.
        let mut oversized = (1u64 << 30).to_le_bytes().to_vec();
        oversized.extend_from_slice(&[0u8; 8]);
        let err = deserialize_from_state::<_, Vec<u8>>(&oversized[..]).unwrap_err();
        assert!(matches!(*err, bincode::ErrorKind::Io(_)), "{err}");
    }

    #[test]
    fn truncated_payload_leaves_the_machine_untouched() {
        let save_path = temp_state("full");
        let truncated_path = temp_state("truncated");
        let mut machine = test_machine();
        machine.step_slice(500).unwrap();
        save(&machine, &MachineDescriptor::default(), &save_path).unwrap();
        let bytes = std::fs::read(&save_path).unwrap();
        std::fs::write(&truncated_path, &bytes[..bytes.len() / 2]).unwrap();

        machine.step_slice(500).unwrap();
        let before_pc = machine.pc();
        let before_secs = machine.bus().emulated_seconds();
        assert!(load(&mut machine, &truncated_path).is_err());
        assert_eq!(machine.pc(), before_pc);
        assert_eq!(machine.bus().emulated_seconds(), before_secs);

        // The intact file still loads after the failed attempt.
        load(&mut machine, &save_path).unwrap();
        let _ = std::fs::remove_file(&save_path);
        let _ = std::fs::remove_file(&truncated_path);
    }

    #[test]
    fn round_trips_the_machine_descriptor() {
        let path = temp_state("descriptor");
        let descriptor = MachineDescriptor {
            cpu: CpuModel::M68EC020,
            chip_ram_bytes: 2 * 1024 * 1024,
            fast_ram_bytes: 8 * 1024 * 1024,
            slow_ram_bytes: 0,
            mb_ram_bytes: 4 * 1024 * 1024,
            accel_ram_bytes: 32 * 1024 * 1024,
            chipset: crate::config::Chipset::Aga,
            video_standard: crate::chipset::agnus::VideoStandard::Ntsc,
            machine: Some(crate::config::MachineModel::A1200),
            rom: crate::config::RomId::of(b"a fake kickstart image"),
            extended_rom: Some(crate::config::RomId::of(b"a fake extended rom")),
        };
        let mut machine = test_machine();
        save(&machine, &descriptor, &path).unwrap();
        // The descriptor the load reports is the one the state was stamped
        // with, not the (default) shape of the machine being loaded into.
        let loaded = load(&mut machine, &path).unwrap();
        assert_eq!(loaded, descriptor);
        assert!(!MachineDescriptor::default().differences(&loaded).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cd_controller_travels_in_the_state() {
        // A state taken on a CD machine carries its CD controller, so loading
        // it into a machine that had none makes the CD drive appear. This is
        // what lets the status bar's CD controls (keyed on
        // `Bus::cd_drive_present`) show up after loading, e.g., a CD32 state
        // over an A500 session.
        let path = temp_state("cd-controller");
        let mut cd_machine = test_machine();
        cd_machine
            .bus_mut()
            .attach_akiko(crate::akiko::Akiko::new());
        assert!(cd_machine.bus().cd_drive_present());
        save(&cd_machine, &MachineDescriptor::default(), &path).unwrap();

        // A fresh machine with no CD controller gains one from the load.
        let mut plain_machine = test_machine();
        assert!(!plain_machine.bus().cd_drive_present());
        load(&mut plain_machine, &path).unwrap();
        assert!(plain_machine.bus().cd_drive_present());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cartridge_bank_and_shadows_travel_in_the_state() {
        use crate::cartridge::{Cartridge, CartridgeModel, HRTMON_CUSTOM_SHADOW};
        // A monitor session lives in the cartridge's own bank, so the bank,
        // the register shadows the host keeps for it and a freeze still
        // waiting for the CPU all resume with the guest.
        let path = temp_state("cartridge");
        let mut machine = test_machine();
        let mut image = vec![0u8; 0x60];
        image[4..8].copy_from_slice(b"HRT!");
        let mut cartridge = Cartridge::hrtmon(&image).unwrap();
        cartridge.bank_mut()[0x1234] = 0xAB; // the monitor's own variables
        cartridge.note_custom_write(0x180, 0x0F00);
        machine.bus_mut().attach_cartridge(cartridge);
        machine.bus_mut().cartridge_freeze(0).unwrap();
        save(&machine, &MachineDescriptor::default(), &path).unwrap();

        let mut plain_machine = test_machine();
        assert!(plain_machine.bus().cartridge.is_none());
        load(&mut plain_machine, &path).unwrap();
        let cartridge = plain_machine
            .bus()
            .cartridge
            .as_ref()
            .expect("the state fits the cartridge");
        assert_eq!(cartridge.model(), CartridgeModel::Hrtmon);
        assert_eq!(cartridge.bank()[0x1234], 0xAB);
        assert_eq!(
            &cartridge.bank()[HRTMON_CUSTOM_SHADOW + 0x180..HRTMON_CUSTOM_SHADOW + 0x182],
            &[0x0F, 0x00]
        );
        assert_eq!(&cartridge.custom_shadow()[0x180..0x182], &[0x0F, 0x00]);
        assert!(
            cartridge.nmi_pending(),
            "a freeze not yet taken is still waiting"
        );
        assert_eq!(cartridge.freezes(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
