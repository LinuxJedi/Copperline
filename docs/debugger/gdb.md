# Remote GDB

Copperline can run as a headless GDB remote target:

```sh
./target/release/copperline --config copperline.example.toml --noaudio --gdb :2345
```

Port-only forms (`2345` or `:2345`) bind to `127.0.0.1`. Use an explicit
address such as `0.0.0.0:2345` only on a trusted network: the remote
protocol can read and write guest RAM and can resume the emulated machine.

Connect from GDB with the 68k architecture selected:

```gdb
(gdb) set architecture m68k
(gdb) target remote :2345
```

The target starts paused at reset. The stub implements the normal all-stop
remote packets for register access, RAM reads/writes, hardware-style PC
breakpoints, memory watchpoints, single-step, continue, Ctrl-C interrupt,
and GDB reverse execution (`reverse-step` / `reverse-continue`).

## CPU and Memory

GDB sees the core 68000 register set as `d0`-`d7`, `a0`-`a6`, `sp`, `ps`,
and `pc`. Register writes go through Copperline's CPU wrapper so SR stack
banking and interrupt state stay coherent.

Generic GDB memory packets are intentionally conservative:

- Reads use a side-effect-free CPU-visible RAM/ROM view, including the boot
  ROM overlay.
- Writes modify RAM-backed regions only: chip RAM, trapdoor slow RAM, and
  configured RAM expansion boards.
- Writes to ROM, overlay ROM, custom chips, CIA, RTC, IDE, SCSI, CD, and
  other device windows are ignored by `M` packets.

Use `monitor write-reg` for deliberate custom-chip writes.

## Custom Chips

Use GDB's `monitor` command for Amiga-specific state:

```gdb
(gdb) monitor status
(gdb) monitor beam
(gdb) monitor custom
(gdb) monitor reg DMACON
(gdb) monitor reg DFF100
(gdb) monitor write-reg COLOR00 00f
```

Custom-register inspection is side-effect-free. It reads Copperline's
internal Agnus/Denise/Paula/blitter latches rather than executing a real CPU
read from `$DFFxxx`, so it will not acknowledge interrupts, clear latches, or
advance collision/audio state. `write-reg` is different: it routes a word
write through the normal custom-register write path and therefore has real
hardware effects.

Register names match the debugger window (`DMACON`, `BPLCON0`, `COLOR00`,
`AUD0VOL`, and so on). Numeric offsets (`96`) and full custom addresses
(`DFF096`) are also accepted.

## Copper

The Copper list can be dumped from the live list pointer, the current Copper
PC, or an explicit chip-RAM address:

```gdb
(gdb) monitor copper
(gdb) monitor copper pc 20
(gdb) monitor copper 00c01000 80
```

Counts are hexadecimal, matching GDB's packet syntax and Copperline's other
debugger address inputs.

## Reverse Debugging

`--gdb` arms the same snapshot-ring reverse debugger used by the window and
headless reverse watchpoint. GDB commands map as follows:

| GDB command | Copperline operation |
|---|---|
| `reverse-step` | reconstruct the previous instruction boundary |
| `reverse-continue` | run backward to the previous GDB PC breakpoint |
| `monitor last-writer ADDR` | find the last instruction that changed the watched word |

Reverse history uses `COPPERLINE_DBG_RR_BUDGET_MB` and
`COPPERLINE_DBG_RR_INTERVAL`, with the same tradeoff as the other frontends:
more memory and more frequent snapshots make reverse operations faster.

For byte-identical replay, keep the usual determinism requirements from
[](reverse): set `COPPERLINE_RTC_FIXED_SECS` when guest RTC reads matter, and
avoid externally mutating hard-drive/CD images during a debug session.

## Monitor Commands

| Command | Effect |
|---|---|
| `help` | list monitor commands |
| `status` | CPU PC/SR, frame, beam, instruction position, reverse status |
| `beam` | beam/frame/colour-clock position |
| `custom` | compact custom-chip state dump |
| `stepover` | step over a BSR/JSR/TRAP call (single step otherwise) |
| `finish` | run until the current subroutine returns to its caller |
| `reg NAME\|OFFSET` | side-effect-free custom-register latch read |
| `write-reg NAME\|OFFSET VALUE` | real custom-register word write |
| `watch-reg NAME\|OFFSET` | stop on CPU or Copper writes to the custom register |
| `unwatch-reg NAME\|OFFSET` | remove one custom-register watch |
| `clear-reg-watches` | remove all custom-register watches |
| `copper [auto\|pc\|ADDR] [COUNT]` | disassemble Copper instructions |
| `last-writer ADDR` | reverse-search the last write to a word |
