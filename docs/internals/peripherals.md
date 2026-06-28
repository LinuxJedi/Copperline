# Peripherals and expansion

## Zorro autoconfig (`zorro.rs`)

The `ZorroChain` implements the Zorro II/III autoconfig protocol --
nibble-encoded config ROMs in the `$E80000` window, base-address
assignment, shut-up, chain advance, and power-on reset. Boards are
described by data (`BoardSpec`) rather than a trait; the built-in fast and
Z3 RAM options and user `[[zorro]]` metadata boards all build the same
specs. The user-facing guide, including the metadata file format and the
autoconfig walk-through, is [](../zorro).

## Gayle IDE (`gayle.rs`)

A600/A1200 machines get the Gayle gate array: the ID register at
`$DE1000`, the IDE task file at `$DA0000` (byte registers on the odd word
half, 4-byte stride), and the IDE interrupt and status bits. Drives are
raw flat HDF images with an RDB inside, opened read/write; PIO transfers
complete synchronously within the access. One hardware subtlety worth
knowing: Gayle byte-swaps the IDE bus, so IDENTIFY data words are
low-byte-first while sector data passes through untouched -- Kickstart
3.1 expects exactly this. The absent-slave behaviour follows the
WinUAE-verified model so device scans terminate correctly. PCMCIA reports
an empty slot (the status/config registers exist so card.resource
behaves); credit-card device emulation is a non-goal.

## A2091 SCSI (`a2091.rs`, `scsi.rs`)

The `[scsi]` option attaches a Commodore A2091: a Zorro II device board
pairing the Commodore DMAC (rev 02 modeled) with a WD33C93A SBIC, plus
the board's autoboot ROM whose `scsi.device` drives them. The autoconfig
identity comes from the DMAC -- Commodore West Chester (514), product 3,
`ERTF_DIAGVALID` with `er_InitDiagVec` pointing at `$2000` -- while the
ROM supplies the DiagArea and the driver; the ROM image therefore is a
required configuration input (`rom`/`rom_odd`, split even/odd EPROM
dumps interleaved U13-first).

Board window layout: ISTR `$40`, CNTR `$42`, WTC `$80/$82`, ACR
`$84/$86` (low bit forced even), DAWR `$8E`, the WD33C93 SASR/auxiliary
status at `$90/$91` and data port at `$92/$93`, the ST_DMA/SP_DMA/CINT/
FLUSH strobes at `$E0/$E2/$E4/$E8` (read- or write-triggered), and the
boot ROM repeating from `$2000` to the end of the 64K window. Unpopulated
decode below the ROM reads as floating bus (`$FF`): the boot ROM's drive
probe ANDs the A590 XT-interface bytes at `$A1/$A3/$A5/$A7` and only
takes the SCSI-only path when they all read `$FF` -- zeros wedge it
polling a phantom XT drive.

The WD33C93A model covers both ways drivers run the bus, verified against
the real 7.0 boot ROM booting a Workbench install end-to-end:

- the **Select-and-Transfer** combination command (full transaction in
  one command, status byte landing in the Target LUN register, CSR
  `$16` then the `$85` disconnect interrupt), including the short-data
  pause (`$4B`, command phase `$46`) and resume that real targets force
  on MODE SENSE-style reads; and
- the **manual path** the 7.0 ROM uses: Select-with-ATN posting CSR
  `$11` then service-required `$88|phase`, identify message and CDB via
  Transfer Info (with the single-byte-transfer modifier), phase-qualified
  completions (`CSR_XFER_DONE | next phase`), message-in pausing with
  `$20` until Negate ACK releases the target to disconnect.

Data phases run through the DMAC handshake (a word per DMAC cycle into
chip, slow, or Zorro RAM with the 24-bit ACR auto-incrementing) or
through the PIO data register with DBR. Like the Gayle model, transfers
complete within the access; completion interrupts are delivered after a
short emulated delay, and INT2 is the level `CNTR_INTEN && ISTR &
(INTS|E_INT)` fed to Paula's PORTS latch each tick. DMAC bus-master
cycles are not yet arbitrated against the CPU (TODO in `a2091.rs`).

Both IDE and SCSI drives share the `harddrive.rs` sector backend: raw
HDF images, bare partition hardfiles wrapped in a synthesized RDB
(bootable `DHn` named after the unit), and host directories built into
in-memory FFS volumes by `dirfs.rs` (whose volume label defaults to the
directory name, or a `name` override configured on the drive). The
SCSI-2 target layer in
`scsi.rs` answers INQUIRY, MODE SENSE pages 3/4, READ CAPACITY,
READ/WRITE(6)/(10), REQUEST SENSE, and the no-op housekeeping commands,
with sense state kept per target.

## CDTV (`cdtv.rs`, `cdrom.rs`)

The CDTV model pairs the DMAC (which autoconfigs ahead of the Zorro chain,
as on the real machine -- the CDTV firmware requires the DMAC to be the
first configured board) with a Matshita drive speaking its fixed-length
command/response protocol: seek, read, play (LSN/MSF/track), status, SubQ,
and TOC queries, with responses delivered byte-by-byte with STEN pulses.
Data sectors DMA onto the system bus at the 24-bit ACR address -- chip,
slow, or Zorro board RAM, like the A2091's DMAC; Kickstart allocates the
CD buffers in fast RAM when a board is fitted -- paced at single speed and
raising the DMAC interrupt on completion. The 256 KiB extended ROM sits at
`$F00000`.

## CD32 Akiko (`akiko.rs`)

Akiko sits at `$B80000` with its `$C0CACAFE` ID: the chunky-to-planar
converter, the I2C lines to the 24C08 NVRAM EEPROM (persisted to the
`[cd] nvram` file), and the CD command/response rings talking to a Chinon
drive model (stop, pause, seek/play/read, LED, SubQ, status). Data sectors
stream as 2352-byte raw frames at 75 (or 150 at 2x) sectors/second; CD
audio mixes into the host output, and both light the blue CD LED. The
512 KiB extended ROM sits at `$E00000`, and the CD32 pad protocol drives
port 2.

`cdrom.rs` parses BIN/CUE cue sheets (single- or multi-file;
MODE1/2048, MODE1/2352, and AUDIO tracks) for both machines.

## RTC (`rtc.rs`)

An MSM6242-compatible register view at `$DC0000`, present on machines
configured with `rtc = true`. Reads reflect host time; guest writes only
affect the emulated latch/control state, never the host clock.

## Input (`gamepad.rs`, window input paths)

Host keyboard events translate to Amiga raw codes and feed a 6500/1
keyboard-MCU model (`chipset/keyboard.rs`) that clocks each event into
CIA-A bit by bit over the emulated KCLK/KDAT lines: 60 us bit cells,
the >= 85 us KDAT handshake after every byte, lost-sync recovery
(lone sync bits, $F9, retransmission), the $FD/$FE power-up stream,
the $78/KCLK-low reset protocol behind Ctrl+Amiga+Amiga, Caps Lock's
keyboard-owned LED toggle, a 10-event type-ahead buffer with $FA
overflow, and ghost suppression on the real A500 key matrix (the seven
qualifiers are on dedicated lines and never ghost). The protocol was
cross-checked against real-hardware-validated replacement keyboard
firmware. Mouse deltas
feed the JOY0DAT quadrature counters. Gamepads are read through raw
`gilrs` events against the per-UUID calibration described in
[](../guide/ui); on CD32 machines the pad output is serialized through
the CD32 pad protocol instead of the plain digital joystick lines.

The window layer has one host-source policy for the emulated port-2
joystick/CD32 pad: auto, keyboard, or gamepad. Auto is the default. It
polls the calibrated gamepad first and, when no usable pad state is
available, resolves the keyboard joystick state instead. Keyboard mode
skips gamepad polling for port-2 input; gamepad mode disables keyboard
joystick capture so the mapped keys take the normal Amiga keyboard path.
All three sources ultimately call the same `InputState::set_joystick_port2`
and `set_cd32_buttons_port2` helpers, so JOY1DAT, /FIR1, POT1Y/POTGOR, and
the CD32 serial bits remain hardware-derived.

Keyboard joystick emulation is deliberately a host input source, not a
guest-keyboard behaviour. When active, the winit key handler consumes the
mapped host keys before rawkey translation: cursor keys drive directions,
Right Ctrl/Right Alt drive fire, and the CD32 extras are C/X/D/S/Return/Z/A.
Each alias is tracked independently before resolving to a single joystick
state, so releasing one fire alias does not clear fire while another alias
is still held. Releases for keys already captured as joystick controls are
also swallowed if the source mode changes before key-up, preventing stray
Amiga rawkey releases.

## Audio output (`audio.rs`)

`AudioSink` abstracts the host boundary: a cpal live sink, a WAV-file sink
(`--audio-wav`), and a null sink (`--noaudio`). Paula renders in emulated
time; the live sink resamples and buffers against wall-clock. The
`CPAL_*` lead/prebuffer/stale-drop targets in `audio.rs` are fixed rather
than adaptive (currently a 131072-frame ring, a ~150 ms prebuffer equal to
the ~150 ms steady lead, and a ~300 ms stale-drop threshold at 44.1 kHz).
Playback starts only after the first audible frames have filled that
prebuffer, so silent boot/load periods do not queue seconds of zeros. If the
cpal callback later drains the queue completely, it stops playback, outputs
silence, and waits for the same prebuffer depth before restarting. While an
already-started queue is merely below target, the sink reports the missing
buffer depth as extra live-audio lead so the real-time pacer runs ahead and
restores the cushion without forcing a host-side silence gap first.

The live queue is host presentation state, not Paula state. A save-state or
reverse-debug timeline jump keeps the restored Paula/CD/floppy mixer state but
discards queued cpal frames from the abandoned timeline, then rebuilds the live
prebuffer from the restored emulated audio stream. Offline WAV capture is not
affected by any of this buffering policy.

Two profiling knobs cover the audio/pacing boundary, both emitting one
`info` line per second:

- `COPPERLINE_AUDIO_PROFILE=1` -- live-audio queue depth and the cpal
  callback counters (callbacks, callback frames, estimated device CCK,
  plus cumulative underrun/overrun/stale-frame totals). The cpal callback
  itself never logs; it only updates atomic counters under this flag.
- `COPPERLINE_REAL_PACING_PROFILE=1` -- the real-speed pacing line:
  retired instructions, raw `m68k` cycles, chip-bus wait CCK, device CCK,
  CPU chip-bus slots, host sleep count/time, and wall-time late
  count/time. Kept separate so CPU/device pacing can be measured without
  enabling the lower-level cpal counters.

Default live-audio warnings are emitted from the producer side at the same
one-second cadence, and only when an underrun, overrun, or stale-frame
counter is nonzero.

## Serial (`serial.rs`)

Paula's SERDAT transmit path lands on stdout through `StdoutSink` -- this
is how DiagROM's diagnostic stream and the `timing-test/` results are
captured in terminals and CI logs.

A `SerialSink` that can *produce* input (none of the built-in sinks do)
must override `has_pending_input` alongside `read_byte`/`read_word`:
Paula's per-tick UART step takes an idle fast path that skips the receiver
entirely while it reports false.
