# Writing Zorro board plugins

Copperline's expansion bus models Zorro II/III autoconfig
(`src/zorro.rs`). Boards are *data-driven*: a board is
described by a `BoardSpec`, and additional boards are added from TOML
metadata files without writing any Rust. The built-in `[memory] fast` and
`z3` options are themselves just boards built from the same specs, and the
`[scsi]` option can add the A2091 (Zorro II, `src/a2091.rs`) or A4091
(Zorro III, `src/a4091.rs`) SCSI controller as a device-backed board (see
the device-board notes below and the `[scsi]` section of
[](guide/configuration); the third `[scsi]` choice, the A3000's
motherboard SDMAC, is silicon at `$DD0000` rather than a Zorro board).
Both Zorro controllers have freely redistributable bundled autoboot ROMs;
the A2091 image is built from `a2091-rom/` and installed under
`share/copperline/a2091/`.

There are two board kinds:

- **RAM boards** (`type = "ram"`) -- an autoconfig identity over a slab of
  RAM, described entirely in TOML.
- **WASM plugin boards** (`type = "wasm"`) -- *functional* boards (registers,
  interrupts, DMA) whose behaviour is supplied by an external WebAssembly
  module, loaded at runtime. These let you add a working board without
  forking and recompiling Copperline. See
  [WASM plugin boards](#wasm-plugin-boards) below.

Functional boards (the A2091, the A4091, the A2065, the CDTV DMAC, the
Toccata, the MHI decoder board, and WASM plugins) all implement the `ZorroDevice` trait
(`src/zorro_device.rs`): the bus drives every board through that one
boundary for register access, ticking, interrupts, and DMA.

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
- Zorro III boards need a 32-bit CPU (68020 and later); configuring one on
  a 68000/68010/68EC020 machine is rejected at startup, since a 24-bit
  address bus cannot reach the Zorro III space.
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

(wasm-plugin-boards)=
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
# A NIC plugin may also request the shared host networking capability:
# net = "bridge"         # none / loopback / nat / bridge
# net_interface = "en0" # required for bridge
# resolve = true         # host-OS-resolver DNS lookups (resolve_start/resolve_poll)
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
| `config_get` / `resource_len` / `resource_read` | see below | always available |
| `dma_read` | `(addr i32, ptr i32, len i32)` | `dma`: Amiga `addr` -> plugin memory `ptr` |
| `dma_write` | `(addr i32, ptr i32, len i32)` | `dma`: plugin memory `ptr` -> Amiga `addr` |
| `net_send` | `(ptr i32, len i32)` | `net`: transmit the Ethernet frame at plugin memory `ptr` |
| `net_recv` | `(ptr i32, cap i32) -> i32` | `net`: copy the next inbound frame into `ptr` (truncated to `cap`), or 0 |
| `resolve_start` | `(name_ptr i32, name_len i32) -> i32` | `resolve`: start a host-OS-resolver lookup, returns a request id or -1 |
| `resolve_poll` | `(id i32, out_ptr i32) -> i32` | `resolve`: poll it -- -2 pending, -1 failed, or 0 with the address at `out_ptr` |

DMA transfers (`dma_read`/`dma_write`) are transactional and permitted only
during active host transactions (`read`, `write`, `tick`). Calling DMA
functions during module initialization (`init`) traps immediately and causes
plugin instantiation to fail. Calling DMA functions during passive interrupt
queries (`int2`, `int6`) traps immediately and transitions the board into the
faulted offline state. Within an active host callback, `dma_write` buffers
transfers into a host-side journal (bounded to 4,096 transfers and 16 MiB
cumulative size per callback to prevent host resource exhaustion); pending
writes are committed to Amiga memory only upon successful return. If the plugin
traps (e.g. out of fuel, panic, or unhandled exception), uncommitted writes are
rolled back, leaving Amiga memory untouched. `dma_read` provides read-your-writes
coherency by overlaying pending uncommitted writes, including across 32-bit
address wrap boundaries (`0xFFFF_FFFF` -> `0x0000_0000`).

### Fault isolation and lifecycle

When a plugin traps during runtime execution (`read`, `write`, `tick`, `int2`,
or `int6` -- e.g. from fuel exhaustion, unreachable code, out-of-bounds access,
or calling DMA outside active transactions):
- The board immediately enters a **faulted offline state**.
- Open host resources are cleaned up: sockets are closed, uncommitted DMA
  journals are discarded, and background resolve receiver handles are dropped
  (in-flight OS resolver threads terminate on their own and their responses are
  discarded).
- Subsequent host callbacks bypass the WASM module entirely:
  - Register reads return Open Bus (`0xFFFF_FFFF`).
  - Register writes and clock ticks are ignored (no-ops).
  - Interrupt lines (`int2`, `int6`) remain unasserted (low / `0`).
- The faulted state is preserved across save-state snapshots and restores.
- The board remains offline until a bus reset (`reset()`), including a warm
  keyboard reset, which re-instantiates a clean module instance with reset
  linear memory.

Interrupt lines are level-sensitive and polled, exactly like the in-tree
boards: a plugin holds `int2`/`int6` non-zero while the line is asserted, and
the bus applies the interrupt-delivery pipeline automatically -- the
plugin never pulses INTREQ.

`resolve_start`/`resolve_poll` ask Copperline's own process to resolve a
hostname via its OS resolver (`getaddrinfo`) on a short-lived background
thread, rather than the plugin having to speak DNS wire format itself over
its own `net` traffic -- the only way a plugin can get "whatever the host's
resolver is configured for" name resolution under a backend (like a direct
LAN bridge) with no virtual DNS forwarder of its own. `resolve_start` reads
the name from the plugin's own linear memory and returns a request id;
`resolve_poll` is a non-blocking poll of that id, writing the resolved IPv4
address (4 bytes, big-endian) into the plugin's own linear memory at
`out_ptr` on success (0). Like `net`, using it makes a board
non-deterministic -- see [](guide/configuration)'s `[hostsocket]` section for
the concrete example (its `resolver` key, which defaults to using this
capability under `net = "nat"`/`"bridge"`).

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
net = "nat"   # "bridge", "loopback", or "none" for isolation
# interface = "en0"  # required for "bridge"
```

(`--a2065-net BACKEND` is the matching per-run flag, and the launcher's
**I/O Ports** tab's **Networking** page has the same picker under its
**Ethernet:** heading. Bridged
mode adds a live host-adapter picker. `--list-net-interfaces` prints the stable
names accepted by `[a2065] interface` and `--a2065-interface`.)

Unlike the DMAC boards, the LANCE does not master the Amiga bus: its init
block, descriptor rings, and packet buffers live in the board's own 32 KiB RAM
(which the CPU reaches through the board window), so the board is self-contained
and owns its host network backend directly.

Host network backends live in `src/net/` behind the `NetBackend` trait. Three are
built in:

- **`nat`** -- userspace NAT (`src/net/nat/`, behind the default-on `net-nat`
  build feature): a slirp-style virtual gateway that NATs the guest's outbound
  IPv4 onto ordinary host sockets. No host privileges, drivers, or setup, and
  identical behavior on Linux, macOS, and Windows. The guest sees the QEMU/slirp
  segment -- configure its TCP/IP stack with:

  | Setting | Value |
  |---|---|
  | IP address | `10.0.2.15` (or use the built-in BOOTP/DHCP server) |
  | Netmask | `255.255.255.0` |
  | Gateway | `10.0.2.2` |
  | DNS server | `10.0.2.3` |

  DNS is answered through the host's own resolver; TCP and UDP to the gateway
  address reach the host's `127.0.0.1`, so guest software can talk to services
  on the host. Limitations: outbound only (no inbound connections or port
  forwards yet), IPv4 only, no IP fragmentation, and ICMP echo is answered
  locally by the gateway for any destination -- a ping "succeeding" proves the
  NAT is up, not that the target is reachable.
- **`loopback`** -- transmitted frames are queued straight back; useful for a
  self-contained demo, driver bring-up, and tests.
- **`bridge`** -- direct layer-2 attachment to one host adapter. The guest's
  frames go to the adapter unchanged (destination through payload, no FCS), and
  receive filters admit its station MAC plus broadcast/multicast. There is no
  protocol translation and no managed TAP device: LAN DHCP, ARP, IPv4, and IPv6
  work as they do for a physical station. LAN peers and the router can reach
  the guest; the host's own IP is not guaranteed to be reachable through the
  same adapter. Wi-Fi is explicitly best-effort because many access points do
  not accept the guest's additional source MAC.

  - **Windows:** install [Npcap](https://npcap.com/). Copperline loads it from
    the Windows system directory only when a bridge is selected; NAT-only
    installs do not depend on Npcap.
  - **macOS:** Copperline uses the system packet-capture interface. The account
    must be allowed to open `/dev/bpf` (commonly through the machine's
    `access_bpf` setup).
  - **Linux:** Copperline first tries AF_PACKET directly. Normal desktop users
    install the companion with `copperline --install-net-helper`, log out and
    back in once to activate membership in `copperline-net`, then use the
    per-user systemd socket. Only the root-owned helper binary receives
    `CAP_NET_RAW`; it opens/binds/filters a socket and passes the descriptor to
    Copperline, never pumps frames and never runs as root. `--net-helper-status`
    and `--uninstall-net-helper` inspect/remove it. Flatpak users install the
    published Linux helper companion on the host; the sandbox can access only
    `$XDG_RUNTIME_DIR/copperline-net-helper/control.sock`.

Bridge open failures are fatal to startup and save-state restoration; Copperline
never silently substitutes NAT or isolation. If an adapter disappears while
running, the worker logs link loss and disconnects the guest.

**Networking is non-deterministic.** Inbound frames arrive on the host's
schedule, not the emulated clock, so a fitted A2065 (or any `net`-capable WASM
plugin) breaks Copperline's byte-identical replay and save-state reproducibility
while traffic flows -- the emulator logs this when the board is attached. Save
states record only the chosen backend and bring up a fresh one on load
(in-flight frames are dropped; the guest's TCP retransmits).

## Audio: the Toccata sound board

`[toccata]` fits an in-tree MacroSystem Toccata (`src/toccata.rs`), a Zorro
II AD1848-based sound board with a mature, open-source AHI driver
(`toccata.audio`), so AHI-aware guest software gets 16-bit sound with no
Copperline-specific driver work:

```toml
[toccata]
enabled = true
```

No other options exist yet. Unlike the A2065/HostSocket boards above,
Toccata's guest interface is purely register-and-FIFO, not bus-mastering
DMA, and its output joins Copperline's own mixer as a named source (the
`toccata` stem in `--audio-stems`) rather than talking to a host device
directly -- see [](internals/toccata) for the register model and
[](internals/audio) for the mixer/stem-capture integration.

## Audio: the MHI decoder board

`[mhi]` fits an in-tree virtual MPEG-1/2/2.5 Layer III audio decoder board
(`src/mhi.rs`) that serves the Amiga MHI API through the ported
`mhi_copperline.library` (`guest/mhi/`), rather than modelling any real
physical hardware -- MHI-aware players such as AmigaAMP decode MP3 through
it exactly as they would through a real MHI decoder board or software MHI
driver:

```toml
[mhi]
enabled = true
```

No other options exist yet. Omit the section (or `enabled = false`) for no
board. Like Toccata, its guest interface is register-and-descriptor-queue,
not bus-mastering DMA, and its decoded output joins Copperline's own mixer
as a named source (the `mhi` stem in `--audio-stems`) rather than talking
to a host device directly -- see [](internals/mhi) for the register model
and [](internals/audio) for the mixer/stem-capture integration.

## Networking: the bundled HostSocket board

`[hostsocket]` fits the bundled HostSocket board: guest-facing
`bsdsocket.library` backed by a host-side smoltcp TCP/IP stack, so
socket-using applications run with no guest network stack to boot -- see the
[configuration guide](guide/configuration.md) for the user-facing knobs and
caveats. Where the A2065 answers "does this driver/stack work," HostSocket
answers "does this application use sockets correctly," and serves it as the
real, everyday `bsdsocket.library` for software running under Copperline.

Architecturally it is not a native board like the A2065 but a WASM plugin
board (previous section) whose module and guest autoboot ROM ship inside the
`copperline` binary:

- `crates/hostsocket-plugin/` is the plugin source; the committed artifact it
  builds (`assets/hostsocket/hostsocket_plugin.wasm`) is what a plain
  `cargo build` embeds, so building Copperline needs no wasm toolchain.
  Refresh it with `make` in that crate when the plugin changes -- the
  install step copies the wasm32 build output into `assets/`, same as the
  guest ROM Makefiles.
- `guest/hostsocket/` is the m68k stub -- LVO trampolines and the
  `rt_Init`-deferred library install, staged through a register-window RPC
  the same way the services board's hostfs handler works. Its committed ROM
  (`assets/hostsocket/hostsocket_rom.bin`) is served to the plugin as its
  `rom` resource and boots via the board's DiagArea on Kickstart 1.3-3.x
  and AROS.
- Config resolution (`src/hostsocket.rs`) expands `[hostsocket]` into an
  ordinary plugin-board entry whose module path is the sentinel
  `<bundled-hostsocket>`; the plugin host and save-state restore resolve the
  sentinel to the embedded bytes, so states taken with the board fitted load
  anywhere the same Copperline build runs. Because it is a plugin board, the
  whole TCP/IP stack lives in the module's linear memory and rides in save
  states like any other plugin state.

The board autoconfigs under the Copperline manufacturer ID with product 6
(see below). Its conformance record against the external bsdsocktest suite
is `crates/hostsocket-plugin/docs/bsdsocktest-status.md`.

(crypto-the-bundled-zz9k-board)=
## Crypto: the bundled zz9k board

`[zz9k]` fits the bundled ZZ9000 SDK crypto board: a register-compatible
subset of the MNT ZZ9000's "SDK v2" service platform (the CORE + MEMORY +
CRYPTO services) whose crypto runs host-side, so the SDK's unmodified
Amiga-side software -- its transport library, the `zz9k-*` tools, and the
accelerated AmiSSL build -- gets modern-speed hashing, AEAD, key exchange,
and signature verification from an emulated 68k. See the
[configuration guide](guide/configuration.md) for the user-facing knobs and
[the protocol contract](internals/zz9k.md) for the register/opcode surface
and the exact zz9000-sdk revision it tracks.

Like HostSocket it is a WASM plugin board whose module ships inside the
`copperline` binary: `crates/zz9k-plugin/` is the source, the committed
artifact is `assets/zz9k/zz9k_plugin.wasm` (refresh with `make` in the
crate), and config resolution (`src/zz9k.rs`) expands `[zz9k]` into a
plugin-board entry with the module-path sentinel `<bundled-zz9k>`. Unlike
HostSocket it is **pure compute** -- no DMA, no network, no host sockets --
so fitting it keeps the machine fully deterministic and replay-safe, and it
carries no autoboot ROM or guest driver of its own: the SDK software finds
the board via `FindConfigDev` and speaks to it directly.

It is also the one bundled board that does **not** autoconfig under the
Copperline manufacturer ID: it presents the ZZ9000's own identity
(manufacturer 0x6D6E, product 4 on Zorro III / 3 on Zorro II), because that
identity is what the SDK's board probe looks for. The RTG/USB/Ethernet
faces of the real ZZ9000 are absent -- their registers read zero and their
services report unsupported -- so installing the real board's P96
`zz9000.card` driver against it is unsupported (harmless, but no display).

## Graphics: RTG boards

Copperline fits at most one RTG board through `[rtg]`. Both are functional
device-backed boards whose guest drivers program real hardware interfaces;
there is no software-aware virtual framebuffer.

### Z3660

`[rtg] card = "z3660"` fits an in-tree RTG (retargetable graphics) board
(`src/z3660.rs`) modelled on the Z3660 accelerator's FPGA graphics core,
driven by the open-source Z3660.card Picasso96 driver in the guest (see
[the configuration guide](guide/configuration) for setup). Although
the physical board sits in the A3000/A4000 CPU slot, its RTG core
autoconfigs as an ordinary Zorro III board -- manufacturer `0x144B`,
product 1, the real board's identity rather than Copperline's own
manufacturer ID below -- with one 128 MB window. The driver finds it with
`FindConfigDev` and talks to a 32-bit register file in the first 2 KB of
the window; the rest is board RAM, with P96 VRAM from window offset
`+0x200000` and the GFXData blit-parameter mailbox the driver fills
before ringing a blit at `+0x3200000`. Only the first 64 MB is backed --
the driver never touches the window beyond ~52 MB, so the allocation
stays honest without carrying all 128 MB.

Like the A2065 it is a device-backed board, not a `[[zorro]]` metadata
board; the scanout and blitter model live in `z3660.rs` and the
presentation path is described in [](internals/video).

### Village Tronic Picasso II and II+

`[rtg] card = "picasso2"` fits the 1993 Zorro II card with its CL-GD5426;
`card = "picasso2plus"` selects the CL-GD5428-based Picasso II+. Both take 1
or 2 MB of VRAM (`[rtg] vram`). One physical board enumerates as two consecutive
autoconfig identities under Village Tronic manufacturer 2167 (`$0877`). The
original reports serial `$00020000`; the II+ reports `$00100000` on both
identities:

| product | size | autoconfig space | purpose |
| --- | ---: | --- | --- |
| 11 | 1 or 2 MB | Zorro II memory | linear VRAM aperture |
| 12 | 64 KB | Zorro II I/O | VGA registers and monitor switch |

Product 11 has `ERTF_CHAINEDCONFIG` set and is not added to the system free
memory list; product 12 follows it. Both windows route to one `Picasso2`
device. Copperline tags the VRAM mapping internally so the shared device can
distinguish it without changing the common Zorro device interface. Product 13,
the physical board's jumper-selected segmented mode, is intentionally not
implemented because Picasso96 does not support it.

The product-12 register window decodes as follows:

| offset | function |
| --- | --- |
| `$0000-$0FFF` | VGA I/O ports at the same numeric port address |
| `$1000-$1FFF` | the same ports with address + 1, for odd ISA byte lanes |
| `$2000-$7FFF` | unused, reads as open bus and drops writes |
| `$8xxx`, `$Axxx` | even-address write selects the RTG output |
| `$9xxx`, `$Bxxx` | even-address write selects native Amiga pass-through |

A 68k word write to `$3C4` therefore supplies the sequencer index in its high
byte and the `$3C5` data in its low byte. The linear memory aperture preserves
byte order exactly. Both Cirrus revisions interpret 15/16-bit pixels as
little-endian words and 24-bit pixels as B, G, R bytes, matching the `*PC` and
BGR formats advertised by the Picasso96 driver. CRTC part ID `$27` reads `$90`
on the CL-GD5426 and `$98` on the CL-GD5428.

Only the II+ drives an interrupt line. A write to register-window offset
`$1001` enables its INT2 output and `$1000` disables it. An enabled VGA
vertical interrupt latches at the CRTC-programmed retrace edge in emulated
time; writing CRTC `$11` with bit 4 clear acknowledges it. The original card
stores the board-enable bit for register compatibility but never asserts INT2.

### Ateo Concepts Graffity [Zorro II] and [Zorro III]

`[rtg] card = "graffityz2"`/`"graffityz3"` fit Graffity, a lesser-known board
that reuses Picasso II+'s CL-GD5428 core under Ateo Concepts' own registered
manufacturer ID 2092 (`$082C`). Both take 1 or 2 MB of VRAM (`[rtg] vram`);
see [](internals/graffity) for the chip-level detail. Graffity ships a
first-class Picasso96 board driver (`Graffity.card` in the classic Aminet
`Picasso96Install` package), so no CyberGraphX or custom driver is needed.

The Zorro II variant enumerates the same way Picasso II does -- a chained
VRAM aperture (product 34) and a register aperture (product 33), except the
register aperture is 128 KB rather than 64 KB, and its VGA ports sit directly
at the window offset (no odd-lane `+0x1000` mirror):

| product | size | autoconfig space | purpose |
| --- | ---: | --- | --- |
| 34 | 1 or 2 MB | Zorro II memory | linear VRAM aperture |
| 33 | 128 KB | Zorro II I/O | VGA registers and monitor switch |

The Zorro III variant is a single 16 MB window (product 33, no chained
identity) with three fixed sub-apertures instead of one shared register
window:

| offset | size | purpose |
| --- | ---: | --- |
| `+$400000` | 64 KB | monitor-switch strobe trap only; never reaches VGA registers |
| `+$800000` | 64 KB | VGA registers, same direct port addressing as the Zorro II variant |
| `+$C00000` | 1 or 2 MB | linear VRAM |

Both variants decode the monitor switch the same way Picasso II does (`$60`
selects RTG, `$40` selects native Amiga pass-through), but neither has a
board-level interrupt-enable latch: INT2 follows the CL-GD5428 core's own
vertical-blank state directly.

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
on the chip bus (see [](internals/timing)). The standalone
`ZorroChain::power_on_reset` API returns every board to the unconfigured state
and zeroes its RAM by default. A machine-level cold boot returns it to the same
unconfigured state but uses the selected `[memory] init` fill policy for the
RAM.

Device-backed boards (`BoardBacking` other than `Ram`) differ in three
ways:

- their configured window is looked up through
  `ZorroChain::device_region_at` and accesses route to the device model on
  the bus (the A2091's registers, boot ROM, and DMA strobes) rather than
  into board RAM;
- ordinary I/O boards do not claim the autoconfig memory-space flag
  (`ERFF_MEMSPACE`), while a device-backed framebuffer such as Picasso II
  product 11 can request memory-space placement explicitly;
- a board may carry a `diag_vec`, which sets `ERTF_DIAGVALID` in
  `er_Type` and emits `er_InitDiagVec` so Kickstart autoboots from the
  DiagArea inside the board window. The A2091 points it at `$2000`, where
  its boot ROM (and the scsi.device driver in it) appears.

On CDTV machines the DMAC occupies the config window first; the Zorro chain
follows once it is configured, matching real-machine autoconfig order.

On a CD32 with `fmv_rom`, the Commodore Full Motion Video cartridge instead
occupies the first Zorro II slot, as its module ROM expects: manufacturer 514,
product `$6A`, serial `$0028001E`, 1 MiB memory-space board with DiagArea vector
`$80` and the no-shut-up flag. Its hardware model and address map are documented in
[](internals/peripherals).

(the-copperline-manufacturer-id)=
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
| 5 | Copperline services board (host `[[filesys]]` mounts; `filesys.rs`) |
| 6 | HostSocket bsdsocket.library board (`[hostsocket]`; `hostsocket.rs`) |
| 7 | MHI virtual MPEG audio decoder board (`[mhi]`; `mhi.rs`) |

(The bundled [zz9k crypto board](#crypto-the-bundled-zz9k-board) is the one
exception: it autoconfigs under MNT's manufacturer ID 0x6D6E with the
ZZ9000's own product numbers, because the ZZ9000 SDK detects the board by
that identity.)

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
   to each call. Add a `BoardDevice` variant wrapping it to the declaration in
   `src/zorro_device/state.rs`, assigning a new, unused wire ID. Existing IDs
   are permanent, including those belonging to disabled features. Extend
   all of `BoardDevice`'s forwarding `match` arms (`read`, `write`,
   `peek_word`, `tick`, `int2_line`, `int6_line`, `take_activity`, `reset`,
   `kind`) for the new variant. `Bus` ticks every board at each timed-device
   boundary, then samples its IRQ lines. A board may return early internally
   when it has no work; the bus does not query board idle/deadline hooks.
2. Provide a `BoardSpec` constructor with `backing: BoardBacking::Device(slot)`,
   mirroring the existing ones -- note the full field set (a stale example
   here previously omitted three of them):

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
           memory_space: true,
           chained: false,
           window: 0,
           diag_vec: None,
       }
   }
   ```

3. Instantiate the device in `build_machine` (`src/emulator.rs`, *not*
   `src/main.rs`): assign it a slot, add its `BoardSpec` to the chain, and
   push the `BoardDevice` onto `Bus::devices` (the A2091 block, and the
   lide-compatible IDE board's block right after it, are worked templates).
   Give the new `BoardDevice` variant the next free kind ID in
   `zorro_device/state.rs` (IDs are never reused). A new board needs no
   save-state version change: boards travel in the `ZORR` chunk, whose
   payload names its fields, so older states simply lack the board. Bump
   that chunk's version in `savestate/chunk.rs` (with a migration) only if
   an existing board's serialized meaning changes in a way a
   `#[serde(default)]` cannot express (`docs/internals/savestate.md`,
   "Versioning").
4. Add unit tests next to the existing ones in `src/zorro.rs`, which cover
   ROM nibble encoding, Zorro II/III base assignment, chain advance,
   shut-up, and power-on reset -- they are the best worked examples of the
   protocol.

Keep the hardware-first rule in mind: boards model autoconfig hardware
behaviour, and anything guest-visible (IDs, sizes, ROM bytes) should match
what a real board of that class would expose.
