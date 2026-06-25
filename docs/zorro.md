# Writing Zorro board plugins

Copperline's expansion bus models Zorro II/III autoconfig
(`src/zorro.rs`). Boards are *data-driven*: a board is
described by a `BoardSpec`, and additional boards are added from TOML
metadata files without writing any Rust. The built-in `[memory] fast` and
`z3` options are themselves just boards built from the same specs, and the
`[scsi]` option adds the A2091 SCSI controller (`src/a2091.rs`) as a
device-backed board (see the device-board notes below and the `[scsi]`
section of [](guide/configuration)).

There are two board kinds:

- **RAM boards** (`type = "ram"`) -- an autoconfig identity over a slab of
  RAM, described entirely in TOML.
- **WASM plugin boards** (`type = "wasm"`) -- *functional* boards (registers,
  interrupts, DMA) whose behaviour is supplied by an external WebAssembly
  module, loaded at runtime. These let you add a working board without
  forking and recompiling Copperline. See
  [WASM plugin boards](#wasm-plugin-boards) below.

Functional boards (the A2091, the CDTV DMAC, and WASM plugins) all implement
the `ZorroDevice` trait (`src/zorro_device.rs`): the bus drives every board
through that one boundary for register access, ticking, interrupts, and DMA.

## Describing a board in TOML

Reference a board metadata file from the main configuration:

```toml
# copperline.toml
[[zorro]]
metadata = "boards/megaram.toml"
```

Multiple `[[zorro]]` entries are allowed; boards join the autoconfig chain
in file order, after the built-in fast/z3 RAM boards.

The metadata file:

```toml
# boards/megaram.toml
name = "MegaRAM"        # human-readable, appears in logs
zorro = 3               # 2 or 3
type = "ram"            # "ram" or "wasm"
size = "64M"
manufacturer = 0x07DB   # 16-bit autoconfig manufacturer ID
product = 0x20          # 8-bit product code, unique per manufacturer
serial = 0              # optional, defaults to 0
memlist = true          # optional; defaults true for type = "ram"
```

Field notes:

- `zorro = 2` boards must be a legal Zorro II size: 64K, 128K, 256K, 512K,
  1M, 2M, 4M, or 8M. `zorro = 3` boards may be any power of two from 64K to
  1G. Sizes accept `K`/`KB`/`M`/`MB`/`G`/`GB` suffixes or plain bytes.
- Zorro III boards need a 32-bit CPU (68020/68030/68040); configuring one
  on a 68000/68EC020 machine is rejected at startup, since a 24-bit address
  bus cannot reach the Zorro III space.
- `memlist` sets the autoconfig `ERTF_MEMLIST` flag, which asks Kickstart to
  link the board's space into the Exec free-memory list. Leave it `true`
  for RAM boards; a future I/O-style board would set it `false`.
- `manufacturer`/`product`/`serial` are what the guest OS sees in the
  expansion database. `0x07DB` is the conventional "hacker"/"prototype" ID
  for homemade boards. Copperline's own built-in boards instead use its
  registered manufacturer ID (5192 / `0x1448`, dec0de Consulting); see
  [The Copperline manufacturer ID](#the-copperline-manufacturer-id) below.

The spec is validated on load (`BoardSpec::validate`,
`src/zorro.rs`): bad sizes, unknown `zorro` versions, and unknown backing
types are reported with the metadata file's path.

## WASM plugin boards

A `type = "wasm"` board is a *functional* board whose behaviour comes from an
external WebAssembly module (`src/wasmboard.rs`), so you can ship a working
board -- registers, interrupts, DMA -- as a `.wasm` file plus a TOML manifest,
with no changes to Copperline.

```toml
# boards/example.toml
name = "Example Board"
zorro = 2
type = "wasm"
size = "64K"            # the board's autoconfig window size
manufacturer = 0x1448
product = 0x10
wasm = "example.wasm"   # module path, relative to this metadata file
dma  = true             # capabilities, all default false:
int2 = true             #   dma  -> the dma_read/dma_write host imports
int6 = false            #   int2 -> may assert INT2 (PORTS)
                        #   int6 -> may assert INT6 (EXTER)
```

WASM is chosen because a module's entire mutable state lives in its linear
memory -- a flat byte array that Copperline's save states snapshot and restore
exactly like Amiga RAM, preserving deterministic replay. The engine is run with
NaN canonicalization and without SIMD or threads for determinism; a plugin's
persistent state must live in linear memory (WebAssembly globals are not
captured). A save state stores the module's path and replays its memory image
on load, so the `.wasm` file must remain where the manifest points.

### Module ABI

The host calls these exports (all optional except `memory`):

| Export | Signature | Purpose |
|--------|-----------|---------|
| `memory` | (linear memory) | required; the board's state lives here |
| `init` | `() -> ()` | called once after instantiation |
| `read` | `(off i32, size i32) -> i32` | register read at a window offset |
| `write` | `(off i32, size i32, value i32)` | register write |
| `tick` | `(cck i32)` | advance by `cck` colour clocks |
| `int2` | `() -> i32` | INT2 (PORTS) line state, non-zero = asserted |
| `int6` | `() -> i32` | INT6 (EXTER) line state |

The plugin may import these host functions from module `env` (gated by the
manifest capabilities; importing one that was not granted fails to load):

| Import | Signature | Capability |
|--------|-----------|------------|
| `log` | `(ptr i32, len i32)` | always available |
| `dma_read` | `(addr i32, ptr i32, len i32)` | `dma`: Amiga `addr` -> plugin memory `ptr` |
| `dma_write` | `(addr i32, ptr i32, len i32)` | `dma`: plugin memory `ptr` -> Amiga `addr` |

Interrupt lines are level-sensitive and polled, exactly like the in-tree
boards: a plugin holds `int2`/`int6` non-zero while the line is asserted, and
the bus applies the 68000 interrupt-recognition latency automatically -- the
plugin never pulses INTREQ.

Plugins can be written in any language that targets `wasm32` (Rust, C, Zig,
...). An inert example module and its manifest can be generated with the
ignored test `emit_example_plugin_wasm` (see `src/wasmboard.rs`).

### Plugin settings, files, and the config panel

A plugin can take settings and files. The manifest declares defaults in a
`[config]` table and a schema in `[[option]]` entries:

```toml
[config]                 # defaults
mode = "bridged"
mtu = 1500
[[option]]               # schema (drives the launcher's config panel)
key = "mode"
label = "Mode"
type = "enum"            # string | bool | int | file | enum
choices = ["bridged", "nat"]
[[option]]
key = "rom"
label = "Boot ROM"
type = "file"            # the host loads the file and exposes it as a resource
```

At runtime the module reads a setting via the `config_get` host import, and a
file-typed option's bytes via `resource_len` / `resource_read` (keyed by the
option's `key`). For an autoboot ROM, the plugin copies the `rom` resource into
its linear memory at `init` and serves those bytes from `read()`, with `diag_vec`
set in the manifest -- just like the in-tree A2091.

The user overrides settings per board in the main config, layered over the
manifest defaults:

```toml
[[zorro]]
metadata = "boards/nic.toml"
config = { mode = "nat", rom = "boot.rom" }
```

The machine-configuration launcher renders the `[[option]]` schema as an
editable field per option (enum/int steppers, a bool toggle, a file picker, and
a text box for strings), writing changes back as these per-board overrides.

## Networking: the A2065 Ethernet board

Copperline includes an in-tree Commodore A2065 Ethernet board (`src/a2065.rs`),
an Am7990 LANCE NIC the AmigaOS SANA-II `a2065.device` drives. Fit it from the
config:

```toml
[a2065]
net = "loopback"   # host network backend; "none" for an isolated NIC
```

Unlike the DMAC boards, the LANCE does not master the Amiga bus: its init
block, descriptor rings, and packet buffers live in the board's own 32 KiB RAM
(which the CPU reaches through the board window), so the board is self-contained
and owns its host network backend directly.

Host network backends live in `src/net.rs` behind the `NetBackend` trait. Built
in today is **loopback** (transmitted frames are queued straight back -- useful
for a self-contained demo and for tests); a userspace NAT (libslirp/smoltcp) and
a host TAP bridge are planned and will slot in behind `make_backend` under build
features. TAP will require host privileges and interface setup; NAT will not.

**Networking is non-deterministic.** Inbound frames arrive on the host's
schedule, not the emulated clock, so a fitted A2065 (or any `net`-capable WASM
plugin) breaks Copperline's byte-identical replay and save-state reproducibility
while traffic flows -- the emulator logs this when the board is attached. Save
states record only the chosen backend and bring up a fresh one on load
(in-flight frames are dropped; the guest's TCP retransmits).

## How autoconfig works in Copperline

Everything below happens automatically; it is documented so you can debug a
board that the guest OS does not pick up, and so the model is clear when
adding new backing types.

At reset every board is unconfigured and the first board in the chain
appears in the autoconfig window at `$E80000`-`$E8FFFF`
(`AUTOCONFIG_BASE`/`AUTOCONFIG_SIZE`, `src/zorro.rs:22`). Kickstart's
expansion library then walks the chain:

1. **Discovery.** The board exposes a 16-byte autoconfig ROM,
   nibble-encoded at even addresses of the window. Byte 0 (`er_Type`:
   Zorro generation, memlist flag, size code) is presented as-is; all other
   bytes are presented inverted, per the hardware convention. The ROM
   carries the product, manufacturer, and serial from the spec, plus the
   size code (`zorro_ii_size_code` / `zorro_iii_size_bits`).
2. **Base assignment.** For a Zorro II board, Kickstart writes the base
   address high byte to `$E80048`; for Zorro III it writes a word of the
   base's high 16 bits to `$E80044`. The write configures the board at
   that base and maps its space.
3. **Chain advance.** The configured board disappears from the config
   window and the next unconfigured board appears. Kickstart can also write
   `$E8004C` to "shut up" a board it cannot place, removing it without
   mapping.

Successful configuration is logged:

```
zorro II board "fast RAM" autoconfigured at 0x00200000
zorro II board "Copperline" autoconfigured at 0x00E90000
```

Once configured, accesses inside a board's window are routed by
`ZorroChain::region_at` into the board's backing storage. RAM-backed board
space is external-bus memory: it runs at the CPU clock and does not contend
on the chip bus (see [](internals/timing)). A power-on reset
(`ZorroChain::power_on_reset`) returns every board to the unconfigured
state and clears its RAM, exactly like real hardware losing its config on
reset.

Device-backed boards (`BoardBacking` other than `Ram`) differ in three
ways:

- their configured window is looked up through
  `ZorroChain::device_region_at` and accesses route to the device model on
  the bus (the A2091's registers, boot ROM, and DMA strobes) rather than
  into board RAM;
- they do not claim the autoconfig memory-space flag (`er_Flags`
  `ERFF_MEMSPACE` stays clear, as on real I/O boards);
- a board may carry a `diag_vec`, which sets `ERTF_DIAGVALID` in
  `er_Type` and emits `er_InitDiagVec` so Kickstart autoboots from the
  DiagArea inside the board window. The A2091 points it at `$2000`, where
  its boot ROM (and the scsi.device driver in it) appears.

On CDTV machines the DMAC occupies the config window first; the Zorro chain
follows once it is configured, matching real-machine autoconfig order.

## The Copperline manufacturer ID

Copperline's built-in virtual boards autoconfig under manufacturer ID
**5192** (`0x1448`) -- the registered ID of dec0de Consulting, which also
makes the real ROMulus flash-ROM board. The product numbers under it are:

| Product | Board |
| ------- | ----- |
| 1 | ROMulus (physical hardware; not emulated) |
| 2 | Copperline identification board |
| 3 | Built-in fast RAM (`[memory] fast`) |
| 4 | Built-in Zorro III RAM (`[memory] z3`) |

The **identification board** (`BoardSpec::copperline_id`) is always added to
the chain (unless disabled, below) so guest software can detect that it is
running under Copperline rather than on real hardware or another emulator --
for example [identify.library](https://github.com/shred/identify) calling
`FindConfigDev(5192, 2)`. It is the smallest legal Zorro II board (64K), is
kept out of the Exec free-memory list, and never autoboots, so it sits
inertly on the chain without changing the machine's usable memory map. Its
autoconfig serial number carries the running Copperline version packed as
`major << 16 | minor << 8 | patch`, so a tool can report the exact version
and not just the emulator name.

The board is added last, after the RAM and `[[zorro]]` boards, so those keep
the base addresses they would get without it. Set `identify = false` in the
configuration to drop it entirely (for a chain with no emulator-identifying
board); see the `identify` option in [](guide/configuration).

## Adding a board in Rust

Most functional boards should be WASM plugins (above). Add an *in-tree* board
in Rust only when it needs host integration or performance that a plugin
cannot give (the A2091 SCSI controller, `src/a2091.rs`, is the worked example).
In-tree functional boards implement the `ZorroDevice` trait
(`src/zorro_device.rs`) and are stored as a `BoardDevice` enum variant in
`Bus::devices`; the chain maps each board's window to a
`BoardBacking::Device(slot)` index into that vector.

1. Implement `ZorroDevice` for the board (register `read`/`write`, `tick`,
   `int2_line`/`int6_line`, `reset`); DMA goes through the `DeviceHost` passed
   to each call. Add a `BoardDevice` variant wrapping it (`src/zorro_device.rs`).
2. Provide a `BoardSpec` constructor with `backing: BoardBacking::Device(slot)`,
   mirroring the existing ones:

   ```rust
   pub fn fast_ram(size_bytes: usize) -> Self {
       Self {
           name: "fast RAM".into(),
           version: ZorroVersion::II,
           manufacturer: COPPERLINE_MANUFACTURER_ID,
           product: PRODUCT_FAST_RAM,
           serial: 0,
           size_bytes,
           backing: BoardBacking::Ram,
           memlist: true,
           diag_vec: None,
       }
   }
   ```

3. Instantiate the device in `build_machine` (`src/main.rs`): assign it a
   slot, add its `BoardSpec` to the chain, and push the `BoardDevice` onto
   `Bus::devices` (the A2091 block is the template). Bump
   `savestate::STATE_VERSION` if the serialized layout changes.
4. Add unit tests next to the existing ones in `src/zorro.rs`, which cover
   ROM nibble encoding, Zorro II/III base assignment, chain advance,
   shut-up, and power-on reset -- they are the best worked examples of the
   protocol.

Keep the hardware-first rule in mind: boards model autoconfig hardware
behaviour, and anything guest-visible (IDs, sizes, ROM bytes) should match
what a real board of that class would expose.
