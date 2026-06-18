# Configuration reference

Copperline is configured by a TOML file: `./copperline.toml` by default, or
any file passed with `--config`. Every field is optional; missing fields use
the defaults documented here. `copperline.example.toml` in the repository
root is a commented companion to this reference.

The configuration is validated up front and the emulator refuses to start
with a clear error message rather than guessing (unknown CPU or chipset
names, out-of-range sizes, missing disk images, and so on).

### Paths on Windows

This applies to every path field below (`rom`, disk images, hard-drive
files, the SCSI ROMs, and so on). In a TOML double-quoted string the
backslash is an escape character, so a Windows path written the obvious way
(`rom = "C:\Kickstarts\KICK31.ROM"`) is rejected: `\K` is not a valid escape.
Use any one of:

```toml
rom = 'C:\Kickstarts\KICK31.ROM'    # single quotes: a literal string, no escaping
rom = "C:\\Kickstarts\\KICK31.ROM"  # double quotes: backslashes doubled
rom = "C:/Kickstarts/KICK31.ROM"    # forward slashes also work on Windows
```

Single-quoted literal strings are the least error-prone. macOS and Linux paths
use forward slashes and need none of this.

## Command-line overrides

The most common machine knobs can be set on the command line without writing
a config file. These flags layer on top of the config file (or, when there is
none, the built-in defaults) and are validated by exactly the same parsers and
range checks as the equivalent TOML fields:

| Flag | Overrides | Accepts |
|---|---|---|
| `--model NAME` | `[machine] profile` | `A500`, `A500Plus`, `A600`, `A1200`, `CDTV`, `CD32` |
| `--chipset NAME` | `[chipset] revision` | `OCS`, `ECS`, `AGA` |
| `--cpu MODEL` | `[cpu] model` | `68000`, `68EC020`, `68020`, `68030`, `68040` |
| `--cpu-clock MHZ` | `[cpu] clock_mhz` | a number of MHz |
| `--fpu` / `--no-fpu` | `[cpu] fpu` | fit / omit a 68881/68882 |
| `--chip SIZE` | `[memory] chip` | `512K`, `1M`, `2M`, ... |
| `--fast SIZE` | `[memory] fast` | `0`, `1M`, `4M`, `8M`, ... |
| `--slow SIZE` | `[memory] slow` | `0`, up to `512K` |

For example, to boot a stock A1200 profile but with 8 MB of fast RAM and a
faster CPU, with no config file at all:

```sh
./target/release/copperline --model A1200 --fast 8M --cpu-clock 28 KICK31.ROM
```

A `--model` profile supplies the chipset, CPU, and memory defaults of a real
machine; the other flags then override individual values on top of it, just as
explicit `[cpu]`/`[chipset]`/`[memory]` sections override a `[machine]`
profile in a config file.

## Top level

```toml
rom = "KICK13.ROM"            # Kickstart image, exactly 512 KiB
extended_rom = "cd32ext.rom"  # optional: CDTV (256K at $F00000) or
                              # CD32 (512K at $E00000) extended ROM
```

The ROM path can be overridden by a positional CLI argument. Omit `rom`
entirely (and pass no ROM argument) to boot the bundled AROS open-source
Kickstart replacement, which ships with Copperline as the default boot ROM;
its main and extended halves are located next to the binary (under
`share/copperline/aros` for a Homebrew install) or set
`COPPERLINE_AROS_DIR`. You can also fit a different ROM at runtime from the
menu's **Load Kickstart ROM...** item, which hard-resets the machine. Machine
profiles that need an extended ROM (CDTV, CD32) will tell you if it is
missing.

## `[machine]` -- machine profiles

```toml
[machine]
profile = "A1200" # A500, A500Plus (A500+), A600, A1200, CDTV, CD32
rtc = true        # whether the $DC0000 battery RTC is fitted
```

A machine profile bundles the chipset, CPU, memory, gate array, and
peripheral defaults of a real machine. The key is `profile` (the deprecated
`model` alias still parses) so it never collides with `[cpu] model`. Explicit `[cpu]`, `[chipset]`, and
`[memory]` sections override individual profile defaults. Without a
`[machine]` section you get legacy A500-like defaults (OCS, 68000, 512K
chip RAM).

| Profile | Chipset | CPU | Chip RAM | Extras |
|---|---|---|---|---|
| `A500` | OCS | 68000 @ 7.09 MHz | 512K | -- |
| `A500Plus` | ECS (8372A Agnus, ECS Denise) | 68000 @ 7.09 MHz | 1M | -- |
| `A600` | ECS (8375 Agnus, ECS Denise) | 68000 @ 7.09 MHz | 1M | Gayle IDE, RTC |
| `A1200` | AGA (Alice/Lisa) | 68EC020 @ 14.18 MHz | 2M | Gayle IDE, RTC |
| `CDTV` | ECS | 68000 @ 7.09 MHz | 1M | DMAC CD controller, 256K extended ROM |
| `CD32` | AGA (Alice/Lisa) | 68EC020 @ 14.18 MHz | 2M | Akiko, CD32 pad, NVRAM, 512K extended ROM |

`rtc` exists because some machines shipped both ways: the base A600 had no
RTC while the A600HD did. The default keeps it fitted so the Workbench
clock works.

## `[emulation]`

```toml
[emulation]
power_on = true            # false = start powered off at the test screen
pacing_budget = "cycles"   # "cycles" (hardware-accurate) or "instructions"
```

The deterministic cycle-driven core is the only emulation timing. It is
paced to wall-clock for the interactive window and runs unthrottled for
headless captures; the emulated result is identical. (An older `speed` key
here is accepted but ignored -- "real" was the only timing model, so it
carried no information.)

- `power_on = false` starts the machine powered off showing a test screen
  until you click the status-bar power button -- useful for arming video
  capture first. The power button cold-boots (clears RAM).
- `pacing_budget` selects how real-time pacing budgets CPU work per frame:
  `"cycles"` (default) charges each instruction its actual 68000 cycle cost
  plus chip-bus waits, matching real hardware speed; `"instructions"` uses a
  flat `COPPERLINE_REAL_CPU_CPI` (default 4.0) cycles/instruction quota,
  which is cheaper but runs the CPU faster than hardware.
  `COPPERLINE_REAL_PACING_BUDGET` overrides this for one run. See
  [](../internals/timing) for the full rationale.

## `[cpu]`

```toml
[cpu]
model = "68000"     # 68000, 68EC020, 68020, 68030, 68040
clock_mhz = 14.0    # optional; defaults to the model's stock speed
# icache = false    # 020/030 instruction-cache model (on by default for them)
# dcache = false    # 030 data-cache model (on by default for the 030)
# fpu = true        # fit a 68881/68882 (68020/68030; needs the coprocessor
#                   # interface, so not valid on a 68000). The full 68040's
#                   # on-die FPU is enabled by default.
```

- `model`: the 68EC020 is a 68020 instruction set with a 24-bit external
  address bus. The 68060 is explicitly unsupported.
- `clock_mhz` defaults to the model's stock speed (68000 ~7.09, 020 ~14,
  030/040 ~25) and is modelled as a whole multiple of the colour clock
  (3.546895 MHz). Fast RAM and ROM run at the CPU clock; chip and slow RAM
  stay chip-bus bound, so overclocking speeds up only what a real
  accelerator would speed up.
- `icache`/`dcache` model the on-chip caches and default **on** for the
  silicon that has them (instruction cache on the 020/68EC020/030, data cache
  on the 030 only), matching real hardware where AmigaOS enables them via
  CACR. Set either to `false` to opt out. This is not cosmetic: 020/030 code
  that loops out of chip RAM otherwise contends with bitplane DMA on every
  instruction fetch and can run at roughly half speed, which is why an AGA
  demo's music or animation may pace correctly only with the cache modelled.
  The data cache caches expansion RAM/ROM only, since chip and slow RAM are
  DMA-visible and cache-inhibited as on real Amigas.

## `[memory]`

```toml
[memory]
chip = "512K"   # OCS max 512K; ECS/AGA max 2M
fast = "0"      # Zorro II fast RAM at $200000: 64K..8M board sizes
slow = "0"      # A500 trapdoor RAM at $C00000: up to 512K
z3   = "0"      # Zorro III RAM (needs a 32-bit CPU): 64K..1G, power of two
```

Sizes accept `K`/`KB`/`M`/`MB` (and `G`/`GB` for Zorro III) suffixes or
plain byte counts, and must be multiples of 4 KiB.

- **Chip RAM** is range-checked against the chipset: 512K on OCS, 2M on
  ECS/AGA (also bounded by the selected Agnus revision's address reach).
- **Fast RAM** is exposed as a Zorro II autoconfig board at `$200000`, so
  it must be a legal Zorro II board size: 64K, 128K, 256K, 512K, 1M, 2M,
  4M, or 8M.
- **Slow RAM** ($C00000 "ranger" RAM) is arbitrated on the chip bus through
  Agnus exactly like chip RAM -- it is slow in the authentic way.
- **Z3 RAM** requires a 68020/68030/68040 (a 24-bit bus cannot reach it);
  Kickstart assigns its base address, usually `$40000000`.

Additional expansion boards can be described with `[[zorro]]` metadata
files; see [](../zorro).

## `[chipset]`

```toml
[chipset]
revision = "OCS"   # OCS, ECS, or AGA preset
video = "PAL"      # PAL or NTSC
# agnus = "8372A"  # optional fine-grained override
# denise = "OCS"   # optional fine-grained override
```

`revision` is a preset; `agnus` and `denise` allow the mixed configurations
real machines shipped with (a late A500 with an ECS Agnus but OCS Denise,
for example):

- `agnus`: `OCS`/`8370`/`8371` (OCS), `8372`/`8372A` (ECS, 1M chip),
  `8375`/`8372B` (ECS, 2M chip), `8374`/`ALICE` (AGA).
- `denise`: `OCS`/`8362`, `ECS`/`8373`, `LISA`/`4203`.

The ECS preset picks an 8372A for up to 1M chip RAM and an 8375 above; the
A600 profile always uses the 8375 as the real machine did. The AGA preset
resolves to Alice and Lisa: 8 bitplanes, the 256-entry 25-bit palette with
BPLCON3 BANK/LOCT banking, HAM8, FMODE wide bitplane and sprite fetch
(DMA and manual sprites), SSCAN2/BSCAN2 scan doubling, BPLCON4, and
CLXCON2 (remaining gaps, such as true 35 ns SuperHires sprite output, are
recorded in [](../internals/chipset)).

## `[display]`

```toml
[display]
overscan = "tv"   # "tv" (default) or "full"
phosphor = 0.0    # CRT persistence fraction, 0.0 (off) to 0.95
```

The emulated framebuffer always carries the full overscan field Denise
produces. `"tv"` masks the deep-overscan margins in black like a CRT bezel,
presenting the standard PAL window plus a TV-style overscan margin (20
lo-res pixels per side, 8 lines above and below) that tracks the centred
picture; `"full"` shows everything, which is useful when debugging
display alignment. `COPPERLINE_OVERSCAN=full|tv`
overrides this for a single run.

`phosphor` blends each presented frame with a fraction of the previous
one, approximating the exponential decay of CRT phosphor. Software that
relies on the tube to fuse field-rate flicker -- alternate-field dither
transparency or flicker-dithered animation -- reads as intended with values
around `0.3`-`0.5`,
at the cost of a slight motion trail. Off by default so screenshots and
frame dumps stay frame-exact. `COPPERLINE_PHOSPHOR=0.4` overrides the
config for a single run.

Rendering completed frames uses a worker thread by default so emulation can
advance while the previous frame is painted. The worker is an implementation
detail of presentation: screenshots, frame dumps, and recordings wait for
the exact frame they save. `COPPERLINE_THREADED_RENDER=0` forces the old
synchronous render path for comparison.

## `[audio]`

```toml
[audio]
floppy_sounds = true        # synthesized drive sounds (not sampled)
floppy_sounds_volume = 100  # 0-100, relative to Paula's output
```

The drive sounds are generated from scratch: motor hum with spin-up/down,
head-step clicks for seeks and the empty-drive poll, and faint read/write
hiss during disk DMA.

## `[floppy.df0]` .. `[floppy.df3]`

```toml
[floppy.df0]
path = "demo.adf"            # single image, or:
# paths = ["disk1.adf", "disk2.adf"]   # swap playlist (shortcut cycles)
write_protected = true       # default true
# enabled = true             # implied by path/paths
```

Supported image formats: standard 901120-byte DD ADF, gzip-compressed
images (ADZ), DMS archives, UAE extended ADF, and read-only SCP flux
images. DMS, gzip, and SCP images are decoded at load time and always
treated as write-protected; set `write_protected = false` on a plain ADF to
allow write-through updates to the image file.

A `paths` playlist lets multi-disk software that only drives DF0: run
without a second drive: the first entry is the boot disk and the disk-swap
shortcut (`Cmd+D` on macOS, `Alt+D` on Linux/Windows) or the status-bar
swap button cycles to the next image, wrapping around.

## `[ide]` -- Gayle hard disks

```toml
[machine]
profile = "A600"             # IDE needs a Gayle machine (A600 or A1200)

[ide]
master = "AmigaSYS.hdf"      # raw flat HDF, read/write
# slave = "scratch.hdf"
```

Images are opened read/write. Both kinds of HDF work directly:

- a full disk image with its own Rigid Disk Block (RDSK/PART chain), and
- a bare partition hardfile (boot block starts with `DOS\x..`), which is
  wrapped in a synthesized RDB on the fly: one extra cylinder of
  16-surface x 32-sector geometry holding an RDSK and a bootable `DH0`
  PART block, with the image's own dostype. The image must be a multiple
  of 256 KiB so the partition is an exact cylinder count. Writes to the
  partition go back to the image file; writes to the synthesized RDB area
  (re-partitioning) live only for the session.

A path may also name a **host directory**: its tree is built into an
in-memory FFS volume at startup (volume name = directory name, files and
subdirectories included; entries whose names cannot exist on an Amiga
volume are skipped with a warning). The guest sees an ordinary bootable
FFS disk and may write to it, but the volume lives only in memory --
nothing is written back to the host directory, and changes are lost at
exit. Note that the stock A1200/A600 Kickstart `scsi.device` only probes
the IDE master; a slave drive needs a guest OS or driver that supports
two units (e.g. Kickstart 3.1.4).

The drive responds to ATA IDENTIFY with the Gayle byte order real hardware
uses, so both Kickstart 3.1 variants boot from it. An HDD activity LED
appears in the status bar on IDE machines.

## `[scsi]` -- A2091 SCSI controller

```toml
[scsi]
rom = "a2091-v6.6.rom"       # A590/A2091 boot ROM image (required)
# rom_odd = "a2091-odd.rom"  # for split even/odd EPROM dumps
unit0 = "workbench.hdf"      # SCSI IDs 0-6
unit1 = "data.hdf"
# unit2..unit6 = ...
```

The `[scsi]` section attaches a Commodore A2091 controller (Commodore
DMAC + WD33C93A) as a Zorro II autoconfig board. It is the preferred way
to mount hard disks: up to **seven drives** on one controller, on **any
machine model** (the board needs no Gayle), and with no dependence on the
Kickstart IDE driver -- the board's own boot ROM carries `scsi.device`
and autoboots on Kickstart 1.3 and newer, which also sidesteps the stock
A600/A1200 `scsi.device` only probing the IDE master. `[ide]` remains
available, and both can be used at once.

`rom` must point at an A590/A2091 boot ROM image (version 6.6 or later;
16K/32K, available from the same vendors and dump sets as Kickstart
ROMs). Dumps split into even/odd EPROM halves can be given as `rom`
(even, U13) plus `rom_odd` (odd, U12). The ROM is required because the
autoboot DiagArea and the scsi.device driver itself live in it; the
autoconfig identity comes from the board (Commodore, product 3, DiagArea
vector at `$2000`).

Each `unitN` accepts everything `[ide]` paths do: RDB images, bare
partition hardfiles (a synthesized RDB advertises a bootable `DHn`
partition, named after the SCSI ID), and host directories built into
in-memory FFS volumes. The HDD activity LED covers SCSI traffic too.

## `[cd]` -- CDTV and CD32

```toml
[machine]
profile = "CD32"

[cd]
image = "disc.cue"        # BIN/CUE cue sheet (MODE1/2048, MODE1/2352, AUDIO)
insert_delay = 0.0        # emulated seconds after power-on to insert
# nvram = "cd32-nvram.bin" # CD32 save-game EEPROM backing file (default)
```

The disc mounts on the machine's CD controller: Akiko on CD32, the DMAC on
CDTV. `insert_delay` inserts the disc some emulated seconds after power-on
with the proper media-change notification; some CDTV discs only boot when
inserted after the boot screen appears. CD32 NVRAM
persists to `cd32-nvram.bin` next to the working directory unless
overridden; without a path the EEPROM is session-only.

## `[[zorro]]` -- expansion boards

```toml
[[zorro]]
metadata = "boards/megaram.toml"
```

Each entry adds a Zorro board described by a TOML metadata file, configured
in file order after the built-in `[memory]` fast/z3 boards. See
[](../zorro) for the metadata format and how autoconfig assigns
addresses.
