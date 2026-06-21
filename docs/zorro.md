# Writing Zorro board plugins

Copperline's expansion bus models Zorro II/III autoconfig
(`src/zorro.rs`). Boards are *data-driven*: a board is
described by a `BoardSpec`, and additional boards are added from TOML
metadata files without writing any Rust. The built-in `[memory] fast` and
`z3` options are themselves just boards built from the same specs, and the
`[scsi]` option adds the A2091 SCSI controller (`src/a2091.rs`) as a
device-backed board (see the device-board notes below and the `[scsi]`
section of [](guide/configuration)).

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
type = "ram"            # board backing; only "ram" so far
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

For board types that need code (a new `BoardBacking` beyond RAM), the flow
is below; the A2091 (`BoardBacking::A2091`, `src/a2091.rs`) is the worked
example of a device-backed board:

1. Extend `BoardBacking` (`src/zorro.rs`) with the new backing and teach
   `ZorroChain`'s access paths (`region_at` callers in `src/bus.rs` and
   `src/cpu.rs`) how reads/writes reach it.
2. Provide a `BoardSpec` constructor, mirroring the existing ones:

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

3. Register it in `Config::build_zorro_chain` (`src/config.rs`) or accept
   it from the metadata loader (`load_board_metadata`, `src/zorro.rs`).
4. Add unit tests next to the existing ones in `src/zorro.rs`, which cover
   ROM nibble encoding, Zorro II/III base assignment, chain advance,
   shut-up, and power-on reset -- they are the best worked examples of the
   protocol.

Keep the hardware-first rule in mind: boards model autoconfig hardware
behaviour, and anything guest-visible (IDs, sizes, ROM bytes) should match
what a real board of that class would expose.
