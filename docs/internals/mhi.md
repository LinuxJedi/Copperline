# The MHI decoder board: mailbox register protocol

This chapter specifies the mailbox protocol shared by the host board
(`src/mhi.rs`) and `mhi_copperline.library` (`guest/mhi/`). Protocol changes
must follow the [versioning rules](#mhi-versioning).

Offsets are relative to the board's autoconfigured window. The protocol
can be implemented on another bus or emulator without changing register
semantics; see [porting](#porting-to-another-emulator).

MHI itself -- `MHIAllocDecoder`/`MHIQueueBuffer`/`MHIGetStatus`/etc., the
Amiga-side API AmigaAMP and other players call -- is not this board's wire
format. MHI is a **library API**, implemented by `mhi_copperline.library`
(the guest side of this project); this board never sees an `MHIP_*` or
`MHIQ_*` constant, a decoder handle, or a signal mask. See
[The MHI-API/board split](#the-mhi-api-board-split) for exactly what stays
guest-side and why.

## Zorro identity

- Zorro II slave, one 64 KiB register window, no autoboot ROM (`romtype`
  not present -- the board never appears in the Exec free-memory list and
  never autoboots).
- Manufacturer **5192** / `0x1448` (the Copperline manufacturer ID; see
  [](../zorro)'s [manufacturer ID table](../zorro.md#the-copperline-manufacturer-id)),
  product **7** -- the next free product number after HostSocket (6).
- The board is **not** a bus master at the guest-visible level: nothing in
  this protocol lets the guest program a live DMA pointer the board reads
  asynchronously. The guest only ever hands the board a static
  address+length pair per descriptor (see
  [Descriptor queue and doorbell](#descriptor-queue-and-doorbell)); when the board consumes it,
  it is Copperline's own host-side implementation detail that the copy
  happens through `DeviceHost`'s DMA accessors, exactly as the A2091
  SCSI controller's data phase does. Another emulator could just as
  validly implement "consume the descriptor" by memcpy'ing from its own
  guest RAM model directly -- the protocol only promises *what* gets read
  (a byte range of 24-bit Amiga address space) and *when* (in emulated
  time, at the decoded-audio rate -- see [Determinism and timing](#determinism-and-timing)),
  never *how*.

(register-map)=
## Register map

All registers are 16-bit and word-aligned; addresses are offsets within the
64 KiB window. `RO` = guest may only read; `WO` = guest may only write
(reads of a `WO` register return 0); `RW` = both. Offsets not listed are
**reserved**: they read as `0x0000` and silently discard writes, in every
protocol version, so a guest built against a newer spec than the board
implements degrades safely on old fields rather than reading garbage. See
[Access size and alignment](#access-size-and-alignment) for the rules
`move.b`/`move.w` accesses follow.

| Offset | Name | Width | Access | Reset value | Group |
|---|---|---|---|---|---|
| `0x00` | `VERSION` | word | RO | `0x0002` | Capability/version |
| `0x02` | `CAPS` | word | RO | board-fixed | Capability/version |
| `0x04` | `STATUS` | word | RO | `0x0000` (STOPPED) | Status/control |
| `0x06` | `CONTROL` | word | WO | -- | Status/control |
| `0x08` | `INTREQ` | word | RW | `0x0000` | Interrupts |
| `0x0A` | `INTENA` | word | RW | `0x0000` | Interrupts |
| `0x0C` | `QUEUE_DEPTH` | word | RO | board-fixed (`16`) | Descriptor queue |
| `0x0E` | `QUEUE_COUNT` | word | RO | `0x0000` | Descriptor queue |
| `0x10` | `DESC_ADDR_HI` | word | WO | -- | Descriptor queue |
| `0x12` | `DESC_ADDR_LO` | word | WO | -- | Descriptor queue |
| `0x14` | `DESC_LEN_HI` | word | WO | -- | Descriptor queue |
| `0x16` | `DESC_LEN_LO` | word | WO | -- | Descriptor queue |
| `0x18` | `DOORBELL` | word | WO | -- | Descriptor queue |
| `0x1A` | `COMPLETED_COUNT` | word | RO | `0x0000` | Completion/reclaim |
| `0x1C` | `PARAM_SELECT` | word | RW | `0x0000` | Param latches |
| `0x1E` | `PARAM_VALUE` | word | RW | per-param default | Param latches |
| `0x20`-`0xFFFF` | *reserved* | -- | -- | `0x0000` | -- |

The window is 64 KiB (the smallest legal Zorro II size) even though only
32 bytes contain registers. The remaining space is reserved for future
protocol versions
without moving the board to a bigger window, which would change its
autoconfig identity.

(capability-version-registers)=
### Capability/version registers

- **`VERSION`** (`0x00`, RO) -- the register-protocol version this board
  implements: `2`. The guest library accepts version 1 or newer when
  finding the board and checks `CAPS` for optional behavior. See
  [Versioning](#mhi-versioning).
- **`CAPS`** (`0x02`, RO) -- a bitmask of the MPEG formats and bitrate
  modes this board's decoder accepts. Bit layout:

  | Bit | Meaning |
  |---|---|
  | 0 | MPEG-1 supported |
  | 1 | MPEG-2 supported |
  | 2 | MPEG-2.5 supported |
  | 3 | Layer III supported |
  | 4 | CBR (constant bitrate) supported |
  | 5 | VBR accepted as input, including re-entry at an arbitrary (non-frame-aligned) byte offset -- see [Seek-entry hardening](#seek-entry-hardening) |
  | 6 | Param latches are applied to decoded PCM (see [Param latches](#param-latches)) |
  | 7-15 | reserved, read as 0 |

  Version 2 sets bits 0-6. Layer I/II are not implemented. Version 1
  sets bits 0-5 and stores parameter values without applying them to audio.
  The guest library checks bit 6 before advertising volume, panning, tone,
  prefactor, and crossmix controls through `MHIQuery`; it does not infer
  these capabilities from its own build version.

  Decoder identity strings (`MHIQ_DECODER_NAME`, `MHIQ_DECODER_VERSION`,
  `MHIQ_AUTHOR`, `MHIQ_CAPABILITIES`'s MIME-type string,
  `MHIQ_IS_HARDWARE`/`MHIQ_IS_68K`/`MHIQ_IS_PPC`) have **no register** --
  they identify the *guest library*, not the board, and are answered
  entirely from constants compiled into `mhi_copperline.library`. See
  [The MHI-API/board split](#the-mhi-api-board-split).

### Status and control

- **`STATUS`** (`0x04`, RO) -- the board's current transport state:

  | Value | State |
  |---|---|
  | `0` | `STOPPED` |
  | `1` | `PLAYING` |
  | `2` | `PAUSED` |
  | `3` | `OUT_OF_DATA` |

  These are the board's own state codes, not `MHIF_*` values (the official
  `libraries/mhi.h` defines `MHIF_PLAYING`=0, `MHIF_STOPPED`=1,
  `MHIF_OUT_OF_DATA`=2, `MHIF_PAUSED`=3 -- a different assignment and a
  different order from this register's `STOPPED`=0/`PLAYING`=1/`PAUSED`=2/
  `OUT_OF_DATA`=3, deliberately, so a guest that forgets to translate
  fails loudly instead of silently reporting the wrong status half the
  time). `mhi_copperline.library`'s `MHIGetStatus` maps this register's
  value to the matching `MHIF_*` constant. Reading `STATUS` has no side
  effect and may be polled freely at any time, from any context --
  `MHIGetStatus` is documented as callable at will, so nothing here may
  depend on how recently it was last read.

- **`CONTROL`** (`0x06`, WO) -- a one-shot command register; each write is
  interpreted as a command and takes effect immediately (there is no
  latency or acknowledgement -- by the time the `move.w` retires, `STATUS`
  already reflects the new state):

  | Value | Command | Effect |
  |---|---|---|
  | `0` | (no-op) | Ignored; reserved so an accidental zero write is inert |
  | `1` | `PLAY` | `STOPPED`/`PAUSED` &rarr; `PLAYING`. From `STOPPED`, playback (and bitstream consumption) starts at the head of the queue if non-empty, or the board immediately reports `OUT_OF_DATA` if the queue is empty. From `PAUSED`, resumes exactly where it left off. No-op from `PLAYING`/`OUT_OF_DATA`. |
  | `2` | `PAUSE` | `PLAYING` &rarr; `PAUSED`. Halts bitstream consumption and audio output; the queue is untouched and the decoder's cross-frame state is preserved, so `PLAY` resumes mid-stream with no audible gap or restart. No-op from any other state (in particular, `PAUSE` from `OUT_OF_DATA` or `STOPPED` does nothing -- MHI's own `MHIPause` is only meaningful while playing). |
  | `3` | `STOP` | Any state &rarr; `STOPPED`. **Discards every queued descriptor**, completed or not yet started (`QUEUE_COUNT` &rarr; 0), and resets `COMPLETED_COUNT` to 0. **Also resets the decoder's cross-frame state** (bit reservoir, MDCT/QMF overlap) -- a fresh transport session starts with a fresh decoder, not one still carrying the discarded stream's reservoir into whatever plays next. This matches `MHIStop`'s documented semantics exactly ("stop all decoding... all buffers in the queue are flushed") -- the guest library performs no separate flush step, and this is also exactly what a seeking player's `MHIStop` &rarr; reposition &rarr; `MHIQueueBuffer` sequence needs: nothing from the pre-seek stream can bleed into the post-seek decode (see [Seek-entry hardening](#seek-entry-hardening)). |

  Values above `3` are reserved and behave as the no-op.

### Interrupts

INT2, level-sensitive: the line is asserted whenever `(INTREQ & INTENA) !=
0`. Both registers share one bit layout:

| Bit | Meaning | Raised when |
|---|---|---|
| 0 | `BUFFER_DONE` | A descriptor finished playing out and was reclaimed (`COMPLETED_COUNT` advanced) |
| 1 | `OUT_OF_DATA` | `STATUS` transitioned into `OUT_OF_DATA` |
| 2 | `QUEUE_OVERFLOW` | A `DOORBELL` write was dropped because the queue was full (diagnostic; not part of MHI's own semantics, but useful to a guest that mis-tracks `QUEUE_COUNT`) |
| 3-15 | reserved | never set in this version |

- **`INTREQ`** (`0x08`, RW) -- the pending-interrupt bits. **Ack protocol
  is write-1-to-clear**: writing a word to `INTREQ` clears exactly the
  bits that are `1` in the value written and leaves the rest untouched;
  writing `0` to a bit never sets it (only the board itself sets bits, on
  the events above). This is a deliberate departure from Toccata's
  read-acknowledges-on-status-read pattern: Toccata's status register
  exists only on the interrupt-service path, so conflating "read status"
  with "ack" is harmless there. This board's `STATUS` is polled from
  contexts that have nothing to do with servicing INT2 (`MHIGetStatus`
  can be called at any time, per its own docs), so acknowledging
  interrupts as a side effect of an unrelated status read would risk
  losing a completion notification a client never meant to consume yet.
  Write-1-to-clear keeps the INT2 server's job mechanical and exactly
  right: read `INTREQ`, act on the set bits, write back the same value to
  ack precisely the bits handled, nothing else.
- **`INTENA`** (`0x0A`, RW) -- enable mask, same bit layout, reset to
  `0x0000` (fully masked) like every other Zorro board's interrupt enable
  on power-on/reset. The guest library must set the bits it wants before
  it can expect INT2 to fire.

(descriptor-queue-and-doorbell)=
### Descriptor queue and doorbell

The board holds a FIFO queue of up to `QUEUE_DEPTH` descriptors, each an
(Amiga address, length) pair identifying a buffer of encoded MPEG
bitstream in Amiga memory that the guest handed over via `MHIQueueBuffer`.
**16 descriptors deep** (`QUEUE_DEPTH` reads back the constant `16`) --
generous enough that AmigaAMP-style double/triple buffering never
back-pressures on the board even with small buffers, without pinning an
unreasonable amount of encoded audio in flight (at typical 32 KiB MP3
buffers, 16 deep is 512 KiB of staged bitstream, well within a stock
machine's chip+fast memory).

To enqueue a descriptor:

1. Write the Amiga source address, high word then low word (either order
   is accepted; both are independent latches -- see
   [Access size and alignment](#access-size-and-alignment)), to
   `DESC_ADDR_HI`/`DESC_ADDR_LO`.
2. Write the buffer length, high word then low word, to
   `DESC_LEN_HI`/`DESC_LEN_LO`. Length is a full 32-bit byte count (Zorro
   II's 24-bit address space bounds it further in practice, but the
   register pair itself does not truncate).
3. Write any value to `DOORBELL`. This is what actually commits the
   staged address+length as a new descriptor at the tail of the queue --
   steps 1-2 only load latches that `DOORBELL` reads at the moment it is
   written; nothing is queued until the doorbell write happens.

If the queue is full (`QUEUE_COUNT == QUEUE_DEPTH`) when `DOORBELL` is
written, the descriptor is **dropped** (the staged address/length are
left as-is, so the guest may simply retry once space frees) and
`INTREQ.QUEUE_OVERFLOW` is set. This mirrors `MHIQueueBuffer`'s own
contract: it returns `FALSE` when a buffer cannot be queued, and the
guest library is expected to poll room before calling it via
`QUEUE_COUNT < QUEUE_DEPTH`, exactly as `MHIGetEmpty` polls for reclaimed
buffers. `QUEUE_OVERFLOW` exists as a diagnostic for a guest bug (racing
the check), not as a code path production drivers should hit.

A **zero-length descriptor** (`DESC_LEN_HI:DESC_LEN_LO == 0` at the
`DOORBELL` write) is accepted, not dropped, but **completes immediately**:
it has no bytes to decode and no audio to play out, so it is never
appended to the queue -- `QUEUE_COUNT` does not move -- and instead
`COMPLETED_COUNT` advances and `INTREQ.BUFFER_DONE` is set on the spot.
Because it never touches `QUEUE_COUNT`, it specifically does **not** count
as the `0`-to-`1` transition [Out-of-data semantics](#out-of-data-semantics)
describes: a zero-length `DOORBELL` write while `STATUS == OUT_OF_DATA`
completes (and raises `BUFFER_DONE`) without moving `STATUS` back to
`PLAYING`.

- **`QUEUE_DEPTH`** (`0x0C`, RO) -- the constant `16`. Read once; it never
  changes at runtime, but is a register (not baked into the spec as a
  bare number) so a future board revision could legally offer a deeper
  queue and have a conforming guest library adapt without a rebuild.
- **`QUEUE_COUNT`** (`0x0E`, RO) -- the number of descriptors currently
  outstanding: enqueued but not yet fully consumed. Incremented by a
  successful `DOORBELL` write, decremented when a descriptor finishes
  playing out (the same instant `COMPLETED_COUNT` advances and
  `INTREQ.BUFFER_DONE` is set). Reset to `0` by `CONTROL=STOP` or a
  hardware reset.

### Completion and reclaim

The board does not hand back buffer pointers or a per-descriptor
ring index -- descriptors complete strictly in FIFO order (the guest
library already knows, in its own client-side queue mirroring
`MHIQueueBuffer`'s call order, which buffer is next), so a single
monotonic counter is sufficient and simpler than a ring of completion
records:

- **`COMPLETED_COUNT`** (`0x1A`, RO) -- a free-running counter,
  incremented by one each time a descriptor finishes playing out (see
  [Determinism and timing](#determinism-and-timing)), wrapping modulo
  65536. Reading it has no side effect. The guest library keeps its own
  local copy of the last-observed value and, on each `BUFFER_DONE`
  interrupt (or when polling `MHIGetEmpty` directly), computes the delta
  with wraparound-safe 16-bit subtraction (`(u16)(now - last)`) to learn
  how many buffers to pop from its own client-side queue and return via
  `MHIGetEmpty` -- the same idiom other in-tree boards use for
  free-running hardware counters. `CONTROL=STOP` resets it to `0`
  alongside `QUEUE_COUNT`; a guest observing a `STOP` (its own or another
  client's, if the board is ever shared -- it uses a single
  `MHIAllocDecoder` model) must resynchronize its local counter to `0`
  rather than compute a delta across the reset.

(param-latches)=
### Param latches

MHI's tone/volume/panning controls (`MHISetParam`) are modeled as a
two-register **select/value mailbox** rather than one register per
parameter -- MHI defines a long tail of them (`MHIP_BAND1`..`MHIP_BAND10`
for a 10-band EQ, on top of volume/panning/bass/mid/treble/crossmixing/
prefactor), and a fixed one-register-per-param layout would either waste
window space up front or need a protocol bump the day a client asks for
one more band. The mailbox pattern keeps the register count fixed
regardless of how many parameters the guest library ends up exposing.

- **`PARAM_SELECT`** (`0x1C`, RW) -- the board-defined parameter index to
  address. This is **not** an `MHIP_*` value (see
  [The MHI-API/board split](#the-mhi-api-board-split)); the guest library
  translates `MHISetParam`'s `MHIP_*` constant to the board's own index:

  | Index | Parameter | Range | Default |
  |---|---|---|---|
  | `0` | Volume | 0-100 | 100 |
  | `1` | Panning | 0-100 (50 = centre) | 50 |
  | `2` | Bass | 0-100 (50 = flat) | 50 |
  | `3` | Mid | 0-100 (50 = flat) | 50 |
  | `4` | Treble | 0-100 (50 = flat) | 50 |
  | `5` | Crossmixing | 0-100 (0 = none) | 0 |
  | `6` | Prefactor | 0-100 (50 = unity) | 50 |

  Indices `7`-`65535` are reserved (unimplemented in this version; a
  future version adding the 5/10-band EQ params would assign them here
  under a `VERSION` bump). Selecting a reserved index and then reading or
  writing `PARAM_VALUE` is well-defined but inert: reads return `0`,
  writes are latched but never consulted by anything.
- **`PARAM_VALUE`** (`0x1E`, RW) -- write: latches the given value against
  whichever index `PARAM_SELECT` currently holds (out-of-range values for
  a 0-100 parameter are clamped by the board, not rejected). Read:
  returns the currently latched value for that index, so the guest
  library can implement a param readback path (MHI itself has no
  `MHIGetParam`, but a latch that cannot be read back would make the
  guest library's own bookkeeping the only source of truth, which this
  avoids).

  In version 1, writes only store the value. Version 2 applies the latches
  to PCM and advertises that behavior through `CAPS` bit 6.

(m4-the-dsp-chain)=
#### DSP chain

Every latch is applied, every produced sample, in one fixed order --
order is audible, so it is as much a part of the contract as the latch
ranges themselves:

```text
decoded PCM -> prefactor -> bass -> mid -> treble -> volume -> pan -> crossmix -> (FIFO -> resampler)
```

This runs entirely in the causal native-rate producer, *before* the FIFO
a non-causal resampler pulls from (see
[Determinism and timing](#determinism-and-timing)) -- so a latch write's
effect lands at the decoded stream's own sample rate, and the resampler
never needs to know params exist. A latch change takes effect at the next
sample this chain produces; there is no ramping or click/zipper
suppression in this version (matching a cheap hardware decoder with no
smoothing of its own -- a later `VERSION` could add it without moving any
existing field). Filter state (see "Tone filters" below) is genuine
machine state and round-trips through savestates exactly like the
decoder's own reservoir does.

- **Volume** (index `0`) -- linear gain, `gain = value / 100.0`: `0` is
  exact digital silence, `100` (default) is unity (matches the range's
  own "100 = 0 dB" definition -- unity is the *maximum*, there is no
  boost). Applied as `sample * gain` to both channels identically.
- **Prefactor** (index `6`) -- linear gain, `gain = value / 50.0`: `50`
  (default) is unity, `100` is `2.0` (+6.02 dB headroom, matching MHI's
  own "50 = unity" definition needing boost room above it), `0` is
  silence. Same shape as volume, different curve -- prefactor is meant as
  a pre-EQ trim a client rides independently of the user-facing volume
  control.
- **Panning** (index `1`) -- a linear **balance** control, not a mono-
  source pan pot: the decoded stream is already stereo (or joint-stereo
  collapsed to two channels by the decoder), so "panning" here scales the
  two existing channels' gains rather than placing a single signal in a
  stereo field. Let `p = value / 100.0`:

  ```text
  gain_left  = min(1.0, 2.0 * (1.0 - p))
  gain_right = min(1.0, 2.0 * p)
  ```

  At `p = 0.5` (`value = 50`, default) both gains are exactly `1.0` --
  identity, chosen deliberately so the default param set is a no-op on
  decoded audio (every param's default reduces to unity/identity by this
  same reasoning). At `p = 0` (hard left) `gain_right = 0` (right channel
  silenced, left untouched); at `p = 1` (hard right), the mirror image.
  Between `0` and `0.5` only the right channel's gain moves (linearly `0`
  to `1`); between `0.5` and `1` only the left channel's gain moves
  (linearly `1` to `0`) -- a piecewise-linear crossfade, hence "linear":
  no constant-power (sine/cosine) law, no dB curve, exactly reproducible
  with one multiply and one `min` per channel.
- **Crossmixing** (index `5`) -- stereo-to-mono blend. Let
  `mix = value / 100.0` and `mono = (left + right) / 2.0`:

  ```text
  left'  = left  * (1.0 - mix) + mono * mix
  right' = right * (1.0 - mix) + mono * mix
  ```

  `0` (default) leaves both channels untouched; `100` collapses both
  channels to the identical mono sum (`left' == right'` exactly).
- **Tone filters: bass/mid/treble** (indices `2`/`3`/`4`) -- a fixed
  three-band filter bank, one 2nd-order IIR (biquad) section per band,
  run independently on each channel (so stereo image is preserved except
  for whatever a shelf/peak filter itself does to level). Each band's
  gain in dB is `(value - 50.0) / 50.0 * 12.0`: `50` (default) is exactly
  `0.0` dB, i.e. a unity-coefficient filter that is the identity
  transform in exact arithmetic (`0` and `100` are `-12` dB / `+12` dB,
  a conventional EQ range). Corner frequencies and filter types, computed
  against whatever the decoder's *current native sample rate* is (the
  same rate the resampler keys its cache on):

  | Band | Type | Corner | Notes |
  |---|---|---|---|
  | Bass | Low shelf | 200 Hz | RBJ Audio EQ Cookbook `lowShelf`, shelf slope `S = 1.0` |
  | Mid | Peaking (bell) | 1000 Hz | RBJ Cookbook `peakingEQ`, `Q = 1.0` (about one octave wide) |
  | Treble | High shelf | 4000 Hz | RBJ Cookbook `highShelf`, shelf slope `S = 1.0` |

  Corner frequencies are clamped to `min(corner_hz, sample_rate_hz *
  0.45)` before the coefficients are derived, so a low-sample-rate MPEG
  stream (MPEG-2.5's 8/11.025/12 kHz modes) never asks for a corner at or
  past Nyquist -- the filter degrades gracefully (a lower effective
  corner) rather than becoming numerically unstable. The three bands run
  in series, bass first, in the fixed order shown above; each is
  transparent (unity, no phase or magnitude change beyond floating-point
  rounding) at its own default value regardless of what the other two
  bands are doing, since each is an independent biquad section.

  Coefficient derivation (the RBJ Cookbook formulas, reproduced here so
  an independent implementation matches bit-for-bit modulo its own
  floating-point unit's `sin`/`cos`/`sqrt`): with `A = 10^(dBgain/40)`,
  `w0 = 2*pi*f0/Fs`, `cs = cos(w0)`, `sn = sin(w0)`,

  - Peaking (mid): `alpha = sn / (2*Q)`; `b0=1+alpha*A, b1=-2*cs,
    b2=1-alpha*A, a0=1+alpha/A, a1=-2*cs, a2=1-alpha/A`.
  - Low shelf (bass): `alpha = sn/2 * sqrt((A + 1/A)*(1/S - 1) + 2)`;
    `b0=A*((A+1)-(A-1)*cs+2*sqrt(A)*alpha), b1=2*A*((A-1)-(A+1)*cs),
    b2=A*((A+1)-(A-1)*cs-2*sqrt(A)*alpha), a0=(A+1)+(A-1)*cs+2*sqrt(A)*alpha,
    a1=-2*((A-1)+(A+1)*cs), a2=(A+1)+(A-1)*cs-2*sqrt(A)*alpha`.
  - High shelf (treble): mirrors the low shelf with every `cs` sign
    flipped in the `(A-1)`/`(A+1)` grouping: `b0=A*((A+1)+(A-1)*cs+
    2*sqrt(A)*alpha), b1=-2*A*((A-1)+(A+1)*cs),
    b2=A*((A+1)+(A-1)*cs-2*sqrt(A)*alpha), a0=(A+1)-(A-1)*cs+2*sqrt(A)*alpha,
    a1=2*((A-1)-(A+1)*cs), a2=(A+1)-(A-1)*cs-2*sqrt(A)*alpha`.

  Every `b`/`a` coefficient above is then normalized by dividing through
  by `a0` (the classic Direct Form II transposed structure Copperline's
  own `AnalogLedFilter`/`BiquadLowPass` -- `src/chipset/paula.rs` -- already
  uses for the LED filter, reused here rather than inventing a second
  filter-processing convention in the same codebase).

(access-size-and-alignment)=
## Access size and alignment

The window behaves like a 16-bit peripheral: every register above is a
plain word (16-bit) register at an even offset, and the board only ever
decodes even addresses.

- **Word access** (`move.w`) is the primary and recommended access size,
  and is what the guest library uses exclusively.
- **Byte access** (`move.b`) is honored for compatibility: the high byte
  of a register is at its listed offset, the low byte at offset+1
  (big-endian, matching 68k byte order), and a byte write only changes
  that half of the register -- there is no special latch-on-low-byte
  behaviour the way the 32-bit descriptor fields latch on a separate
  register (see below). A `WO` register's byte-write value is simply
  discarded like any other write to it.
- **Longword access** (`move.l`) is **not supported** and its behaviour
  is undefined at the protocol level -- the bus is 16 bits wide, so a
  32-bit access does not atomically span two registers the way it would
  on a genuinely 32-bit-decoded peripheral. The guest library must never
  issue one; two `move.w`s are always used instead, including for the
  32-bit `DESC_ADDR_*`/`DESC_LEN_*` pairs (see
  [Descriptor queue and doorbell](#descriptor-queue-and-doorbell)), which are
  deliberately specified as two independent word registers rather than
  one 32-bit register precisely so this never comes up.
- Reads of a **`WO`** register return `0`; writes to an **`RO`** register
  are silently discarded. Neither is an error condition -- there is no
  fault or diagnostic bit for it, matching the rest of Copperline's
  Zorro-board conventions (e.g. Toccata's undecoded-port behaviour, see
  [](toccata.md)).
- **Reserved offsets** (`0x20` and above) read as `0x0000` and discard
  writes, in every protocol version -- see the note at the top of
  [Register map](#register-map).
- No register access has a read side effect in this protocol (contrast
  Toccata, where reading its status register acknowledges pending
  interrupt bits): every ack is the explicit `INTREQ` write-1-to-clear
  described above, and every other register is freely, repeatedly
  pollable.

(out-of-data-semantics)=
## Out-of-data semantics

`STATUS` transitions to `OUT_OF_DATA` (`3`) exactly when playback drains
the queue: the board is in `PLAYING`, the last outstanding descriptor
finishes playing out, and `QUEUE_COUNT` reaches `0` with nothing new
enqueued in the same instant. That transition raises **both**
`INTREQ.BUFFER_DONE` (for the descriptor that just completed) and
`INTREQ.OUT_OF_DATA` together -- a guest servicing only `BUFFER_DONE` and
checking `STATUS` afterward, or one servicing both bits, both see a
consistent picture.

While `OUT_OF_DATA`, the board is not stopped -- it matches MHI's own
description of the state ("run out of data but still waiting for more"):
decoder cross-frame state is preserved exactly as `PAUSE` preserves it,
and audio output is silence in the meantime (no repeat-last-sample
holdover the way Toccata's FIFO underrun behaves -- MPEG frames are not
individually meaningful to hold on to). The moment a `DOORBELL` write
successfully enqueues a new descriptor while `STATUS == OUT_OF_DATA`
(`QUEUE_COUNT` goes from `0` to `1`), the board resumes playback
automatically and `STATUS` returns to `PLAYING` with no `CONTROL=PLAY`
required -- a guest need not notice `OUT_OF_DATA` at all if it always
keeps the queue fed; it exists for the client that briefly runs dry (the
common case an MHI-aware application like AmigaAMP polls for, e.g. to
know a track has finished once no more data is coming).

`CONTROL=STOP` from `OUT_OF_DATA` behaves exactly as from any other
state: transition to `STOPPED` (a no-op on the already-empty queue).

**Undecodable bitstream content** (bytes that are not a valid Layer III
frame at all, or that carry a sync-valid-looking header but fail to decode
-- corrupt encodes, or a hostile/buggy guest handing the board arbitrary
bytes) is skipped exactly as any real decoder resyncs across junk: the
board hunts forward for the next decodable frame, consuming and completing
descriptors as their bytes are skipped past, the same as if those bytes
had decoded into audio. That resync work is budgeted per tick rather than
run to completion in one step, so a descriptor consisting entirely of
undecodable bytes does not stall the emulation; it still completes and
reaches `OUT_OF_DATA` once the queue genuinely empties out -- the resync
merely spreads across a handful of ticks instead of resolving within one.
(Skipped bytes are not paced at the decoded audio's sample rate the way
played-out bytes are -- there is no audio to pace them by -- so an
all-garbage descriptor drains in far less emulated time than the same
bytes of genuine audio would take to play out.) Nothing about the
guest-visible contract changes -- `COMPLETED_COUNT`/`QUEUE_COUNT`/`INTREQ`
still only ever advance in whole-descriptor, whole-frame steps -- only the
emulated wall-clock-adjacent pacing of how many ticks that takes.

(seek-entry-hardening)=
### Seek-entry hardening

MHI itself has no seek call -- the ABI is exactly `MHIAllocDecoder`/
`MHIFreeDecoder`/`MHIQueueBuffer`/`MHIGetEmpty`/`MHIGetStatus`/`MHIPlay`/
`MHIStop`/`MHIPause`/`MHIQuery`/`MHISetParam`, nothing more. Seeking is
entirely the player's own responsibility: it calls `MHIStop`, repositions
its own file read to wherever it wants to resume, and `MHIQueueBuffer`s
buffers starting at that new position. From the board's side, a seek is
indistinguishable from any other `STOP` followed by fresh descriptors --
there is no seek-specific register or command, and none is needed.

The board must handle both parts of a seek:

- **`STOP` resets decoder state, not just the queue** (see the `CONTROL`
  table above) -- otherwise the discarded pre-seek stream's bit reservoir
  or MDCT/QMF overlap would audibly bleed into the first frame or two
  decoded after the seek. `src/mhi.rs`'s
  `stop_resets_decoder_state_so_a_reseek_matches_a_fresh_decode` proves
  this against a real encoded fixture: decode a real prefix, `STOP`,
  requeue the remainder as a seek would, and the result must be
  byte-for-byte identical to decoding that same remainder on a board that
  never saw the prefix.
- **Bitstream re-entry at an arbitrary byte offset resyncs cleanly.** A
  player's file-position seek lands wherever the underlying file format
  puts it -- routinely not aligned to an MPEG frame boundary, and
  routinely just past metadata (an ID3v2 tag between tracks in a
  concatenated file, a Xing/LAME info frame, silence-trimming padding).
  The board's existing resync machinery (see "Undecodable bitstream
  content" above) already handles this with no seek-specific code: bytes
  that are not a valid frame sync are skipped exactly like any other
  undecodable content, bounded by the same per-tick resync budget. This
  holds for VBR content too -- a VBR stream's frames vary in size but
  each still carries its own valid sync header, so the same byte-level
  resync search finds the next real frame regardless of bitrate mode; a
  frame decoded starting immediately after a blind seek may legitimately
  sound imperfect for a frame or two (the bit reservoir it would have
  inherited from now-discarded prior frames is empty, matching what any
  real decoder does when handed a stream it has never seen the start of),
  but decoding itself is always correct and resumes clean steady-state
  output once caught up.
  `mid_frame_entry_resyncs_to_the_next_real_frame` and
  `seek_entry_past_an_id3v2_tag_resyncs_correctly` in `src/mhi.rs` cover
  these cases directly.

**An incomplete trailing frame** (the queued bytes end mid-frame -- too few
bytes for the decoder to tell whether they are even a valid sync, let alone
decode them) is different from undecodable content: it is not junk to skip,
it is a real frame waiting on the rest of its bytes, which a subsequent
`DOORBELL` may yet supply (the guest's next descriptor can complete a frame
split across a buffer boundary, and this is expected to happen routinely --
see "Determinism and timing" below). The board therefore holds those bytes
and does not touch `QUEUE_COUNT`/`STATUS` while it waits. If no further
`DOORBELL` ever completes the frame, though, an implementation must not
wait forever: `QUEUE_COUNT`/`STATUS` need to recover in bounded time so a
guest polling for completion is not wedged by a stream that simply ended
mid-frame. The exact bound is an implementation choice, not part of this
protocol's guest-visible contract -- Copperline's is documented in its own
implementation notes below.

(determinism-and-timing)=
## Determinism and timing

The board consumes a descriptor's bitstream at the **decoded audio's own
emulated-time rate**, the same principle as every other in-tree audio
device (see [](audio.md)'s determinism section and [](toccata.md)'s "mixer
cadence" for the worked example): a decoded MPEG frame (1152 PCM samples
at the stream's sample rate) is not considered "played out" -- and its
bytes are not considered consumed from the descriptor, and
`COMPLETED_COUNT`/`INTREQ` do not advance -- until that many emulated
sample-clock ticks have elapsed, exactly as if the samples were being
produced for playback in real time. A descriptor's completion event and
any INT2 assertion it causes are therefore also emulated-time events, not
host-wall-clock ones: they fire the tick a frame's worth of emulated
sample time has elapsed, identically whether the host machine runs in
real time, is throttled, or is warped as fast as the host CPU allows.
This is what makes a scripted scenario against this board reproducible
byte-for-byte and makes `--audio-wav`/stem captures of its output
deterministic across runs, the same guarantee every other Copperline
audio path already gives.

(the-mhi-api-board-split)=
## The MHI-API/board split

This board's registers are deliberately **innocent of MHI's own
numbering** -- no `MHIF_*`, `MHIP_*`, or `MHIQ_*` constant appears in this
protocol, and the split is intentional, not an oversight:

| Concern | Lives in |
|---|---|
| Decoder identity strings (`MHIQ_DECODER_NAME`/`_VERSION`, `MHIQ_AUTHOR`, the `MHIQ_CAPABILITIES` MIME-type string) | Guest library (`guest/mhi/`) -- compile-time constants; they describe the library, not the board |
| `MHIQ_IS_HARDWARE`/`_IS_68K`/`_IS_PPC` | Guest library -- static answers (this is a real register-mailbox device the library talks to over the Zorro bus, so `MHIQ_IS_HARDWARE` answers true; it runs no 68k/PPC code of its own, so both processor queries answer false) |
| MPEG version/layer/bitrate-mode support (`MHIQ_MPEG1`/`_MPEG2`/`_MPEG25`, `MHIQ_LAYER3`, `MHIQ_VARIABLE_BITRATE`) | `CAPS` register (`0x02`) -- genuinely board-reported, since a future board revision's decoder could differ |
| `MHIQ_JOINT_STEREO` | Guest library -- fixed `MHIF_SUPPORTED`; decoding joint-stereo Layer III is inherent to any conforming decoder, not a distinct board capability worth its own `CAPS` bit |
| Tone/volume/output query flags (`MHIQ_VOLUME_CONTROL`, `MHIQ_PANNING_CONTROL`, `MHIQ_BASS_CONTROL`, `MHIQ_TREBLE_CONTROL`, `MHIQ_MID_CONTROL`, `MHIQ_PREFACTOR_CONTROL`, `MHIQ_CROSSMIXING`, `MHIQ_5_BAND_EQ`, `MHIQ_10_BAND_EQ`) | Guest library -- keyed off `CAPS` bit 6 for the seven params this board's [param latch](#param-latches) table defines (indices `0`-`6`: volume, panning, bass, mid, treble, crossmixing, prefactor): `MHIF_UNSUPPORTED` against a version-1 board (bit 6 clear -- the latches exist and round-trip, but nothing applies them to decoded PCM, so answering `MHIF_SUPPORTED` would tell a client its `MHISetParam` calls are audible when they are not), `MHIF_SUPPORTED` when bit 6 is set (bit 6 set). One guest library binary answers correctly either way -- see `CAPS`'s own bit-6 note above. The 5/10-band EQ stays `MHIF_UNSUPPORTED` regardless of bit 6, until a later `VERSION` adds `MHIP_MIDBASS`/`MHIP_MIDHIGH`/`MHIP_BAND1`-`MHIP_BAND10` equivalents at reserved indices `7`+ |
| Decoder handle, client task pointer, signal mask (`MHIAllocDecoder`/`MHIFreeDecoder`) | Guest library only -- entirely a host-side (Amiga-side) bookkeeping concept; the board has no notion of "a handle" and serves exactly one client at a time |
| Transport (`MHIPlay`/`MHIStop`/`MHIPause`), status (`MHIGetStatus`), queueing (`MHIQueueBuffer`/`MHIGetEmpty`), params (`MHISetParam`) | Guest library translates 1:1 to/from this board's `CONTROL`/`STATUS`/descriptor-queue/`PARAM_*` registers |

Keeping MHI's own vocabulary entirely out of the wire protocol is what
lets this spec describe a board that could serve *any* MHI-shaped guest
front-end (or, in principle, a non-MHI player that just wants a hardware
MPEG decoder) without the register file encoding one particular API
version's constants -- and it is what makes the split in
[Porting to another emulator](#porting-to-another-emulator) below
possible without also porting MHI-specific glue.

(mhi-versioning)=
## Versioning

`VERSION` (`0x00`) is the register-protocol version; the current value is
**2**. Changes to offsets, widths, access rules, bit meanings, or documented
semantics require a version bump. Preserve existing register meanings and
use reserved offsets for additions.

The guest library requires at least version 1. It accepts newer versions
and checks `CAPS` for optional features. A newer board must therefore
remain compatible with the register operations older drivers use.

Version 2 applies parameter latches 0-6 to decoded PCM and sets `CAPS` bit 6.
Version 1 stores and reads back the same values but leaves the audio unchanged.
Register locations and widths are identical in both versions.

(porting-to-another-emulator)=
## Porting to another emulator

Everything above is expressed purely in terms of the autoconfigured
window's own offsets and the Amiga's 24-bit address space -- nothing
references Copperline's internal types, its `ZorroDevice`/`DeviceHost`
Rust traits, or its savestate format. An unrelated emulator wanting to
support the same guest library and the same MHI test assets needs only
to:

1. Autoconfig a Zorro II board at manufacturer `0x1448`, product `7`,
   64 KiB, no autoboot ROM.
2. Implement the register map above over its own bus/register dispatch.
3. On a successful `DOORBELL` write, copy `DESC_LEN_HI:DESC_LEN_LO` bytes
   from `DESC_ADDR_HI:DESC_ADDR_LO` in the emulated Amiga's address space
   into wherever its own MPEG decoder wants the bytes -- by whatever
   internal mechanism that emulator already uses to read guest memory
   from a device model (a literal DMA engine, a direct memory-array
   read, anything at all; this spec does not constrain it).
4. Pace descriptor consumption and `COMPLETED_COUNT`/`INTREQ` updates to
   the decoded audio's own emulated-time rate, per
   [Determinism and timing](#determinism-and-timing), so that scripted
   scenarios and captures built against one implementation reproduce on
   the other.

Copperline's own implementation notes -- the Symphonia-based decoder
choice, how `push_source("mhi", ...)` joins the mixer, `BoardDevice`
wiring, and savestate serialization of in-flight decoder/queue state --
are Copperline-internal and out of scope for this document; they belong
in `src/mhi.rs`'s own doc comments and this page's future host-board
implementation notes once WP3 lands, not in the protocol spec itself.

## Copperline implementation notes

This section summarizes `src/mhi.rs` (`[mhi]`, feature-gated behind the
default-on `mhi` build feature); it does not change any of the protocol
content above.

- **Decoder**:
  [Symphonia](https://github.com/pdeljanov/Symphonia)'s pure-Rust
  MPEG audio decoder (`MpaDecoder`, MPL-2.0), with only its Layer III
  feature enabled to match `CAPS`. It requires no C decoder build.
  `MpaDecoder` is
  packet-based, so `src/mhi.rs` carries its own packetizer that cuts the
  doorbell-fed byte queue into whole frames using the same ISO 11172-3
  header/length arithmetic Symphonia's parser applies; junk bytes, fake
  syncs, free-format frames (bitrate index 0, outside `CAPS`), and
  non-Layer-III frames are consumed as resync junk. Everything
  register-visible (pacing, completion counts, interrupts) derives from
  integer header parsing and decode success/failure, so emulated-machine
  behaviour is reproducible byte-for-byte across platforms; the decoded
  PCM itself is deterministic per platform, but a few of Symphonia's
  precomputed tables call `powf`, whose last-ulp rounding may differ
  between libm implementations, so `--audio-wav` captures of MHI audio
  are guaranteed identical run-to-run on one platform rather than across
  operating systems.
- **Mixer cadence**: reuses Toccata's causal-producer/non-causal-resampler
  split ([](toccata.md)'s "Mixer cadence and resampling") -- a causal
  producer decodes and evaluates queue/interrupt state at the board's own
  paced rate into a plain FIFO of raw frames, and a separate non-causal
  `Resampler` (`src/audio/resample.rs`, per-rate cached) pulls from that
  FIFO to the mixer's fixed rate, so the resampler's lookahead can never
  reorder when a descriptor completes or an interrupt raises.
- **Savestates**: Symphonia keeps its cross-frame decoder state private.
  `DecoderSnapshot` therefore stores a bounded history of encoded frames;
  restoring a board replays them into a fresh decoder and discards the
  warmup output. Queued descriptor bytes and the unplayed sample tail
  are saved too. The in-process unit test
  `savestate_round_trip_reproduces_an_uninterrupted_runs_output` verifies
  exact output after restoration. The separate-process integration test
  `mhi_m2_savestate_resume_matches_the_uninterrupted_tail` in `tests/mhi.rs`
  uses an alignment window and a small error tolerance. It records a
  remaining difference around guest input-buffer refills when restoring
  mid-decode; that case is not established as bit-exact.
- **Save-state layout**: decoder warmup history, tone-filter coefficients,
  and filter memory are serialized machine state, in the `ZORR` chunk.
  A new field needs `#[serde(default)]`; a change of meaning bumps that
  chunk's version with a migration (`docs/internals/savestate.md`,
  "Versioning"), independently of the board's register-protocol version.
- **DSP chain implementation**: `Biquad` is Direct Form II transposed,
  reusing `src/chipset/paula.rs`'s `AnalogLedFilter`/`BiquadLowPass`
  structure and `process` shape rather than a second filter convention in
  the same codebase (see "Tone filters: bass/mid/treble" above for the RBJ
  Cookbook coefficient formulas). `ToneFilterBank` recomputes coefficients
  only when the latch values or the native sample rate actually change
  (`retune_if_stale`), preserving each biquad's own `z1`/`z2` memory across
  a recompute so a latch write never introduces a discontinuity beyond
  what the new coefficients themselves imply. `STOP` (`cmd_stop`) and
  `RESET` both clear that filter memory (`ToneFilterBank::clear_state`/a
  fresh `ToneFilterBank`), matching the existing resampler-history-clear
  reasoning in both places -- a stopped or reset stream's filter ringing
  must not bleed into whatever plays next.
- **Launcher**: the machine-configuration launcher's **I/O Ports** tab
  (Audio page) has a plain fit/don't-fit toggle for the board (same as
  Toccata, see [](toccata.md)'s "What's out of scope" section); host-side
  audio capture/backend options stay command-line/config-file only.
- **Large-descriptor DMA copy**: `DESC_LEN` genuinely does not truncate --
  `Mhi` copies a descriptor's full length into its bitstream buffer in
  bounded chunks (`MAX_DESCRIPTOR_BYTES`, 1 MiB) so a single oversized
  `DOORBELL` write cannot force one huge host-side allocation, but every
  byte still lands regardless of how many chunks that takes.
- **Incomplete-trailing-frame reclaim** ("An incomplete trailing frame"
  above): `Mhi` gives a stalled trailing frame up to `MAX_STALL_TICKS`
  (1/10 s of emulated Paula-clock time -- comfortably longer than any real
  `DOORBELL` round-trip, short enough not to visibly stall playback) to be
  completed by a subsequent doorbell before discarding the leftover bytes
  and reclaiming every descriptor they belong to, the same "skip and
  complete" treatment undecodable content gets. Reset on `RESET`/`STOP`
  and whenever a frame decodes or the bitstream reaches empty, so it never
  carries stale state across sessions.
- **Golden CI fixtures**: `tests/data/mhi/golden_tone_cbr64_mono.mp3` (a
  tiny locally-synthesized CBR fixture, `ffmpeg` sine source encoded with
  `lame`) and `vbr_sweep.mp3` (same synthesis, LAME `-V4` VBR) are
  committed -- unlike `test-assets/mhi/` (gitignored, fetched from Aminet),
  these are Copperline's own generated output, not third-party binaries,
  so the "ROMs and disk images are local assets and are never committed"
  rule does not apply. `src/mhi.rs`'s `golden_tone_decodes_to_a_stable_pcm_capture`
  decodes the CBR fixture through the real board/decoder path and compares
  against a committed golden PCM capture
  (`golden_tone_cbr64_mono.pcm`, within a small per-sample tolerance --
  Symphonia's own precomputed tables call `powf`, whose last-ulp rounding
  differs across platforms/optimization levels, confirmed in practice
  across CI's Linux/Windows/macOS legs) on every plain `cargo test` -- unlike
  every `#[ignore]`d integration test in `tests/mhi.rs`, this needs no
  fetched `test-assets/`, so it catches decoder-dependency drift or
  resampler/pacing regressions immediately rather than only when a
  developer happens to have local assets staged. The same two fixtures
  back the seek-entry tests (`stop_resets_decoder_state_so_a_reseek_
  matches_a_fresh_decode`, `mid_frame_entry_resyncs_to_the_next_real_frame`,
  `seek_entry_into_vbr_content_decodes_cleanly_after_a_settle_window`) --
  real encoded streams carry genuine cross-frame reservoir state that a
  hand-authored `mp3_frame` (all-zero body, no reservoir use) cannot, so
  the "STOP doesn't leak decoder state across a seek" claim is provable
  only against real content. To regenerate after an intentional
  decode-path change, see `golden_tone_decodes_to_a_stable_pcm_capture`'s
  own doc comment.
