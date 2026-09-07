![Copperline](assets/brand/copperline-logo.png)

Website: [copperline.dev](https://copperline.dev/) |
Online demo: [copperline.dev/try](https://copperline.dev/try/) |
Chat: [Discord](https://discord.gg/HDTjt3tYAC) |
Support: [Patreon](https://www.patreon.com/cw/Copperline)

Copperline is a cycle-driven Commodore Amiga emulator (OCS, ECS, and AGA) written in Rust. It models the Amiga custom chipset, 680x0 CPU, memory subsystems, and common expansion hardware on a unified clock timeline.

It boots out of the box with the bundled open-source AROS Kickstart replacement, and supports official Kickstart ROMs (1.3 through 3.1), DiagROM, and standard disk, hardfile, and CD media.

## Features

- **Chipset and machine models**: OCS, ECS, and AGA support with profiles from the A500 through the A4000, plus CDTV and CD32.
- **Configurable CPU and memory**: Motorola 68000 through 68060, optional FPU and MMU support, and configurable Chip, Fast, and Slow RAM.
- **Storage and media**: Floppy disk images (ADF, ADZ, DMS, IPF, SCP), physical floppy drives via Greaseweazle (FluxBridge), IDE (Gayle/A4000 and expansion boards), SCSI (A2091, A3000, A4091), virtual hard disks (`copperhf.device`), CD-ROM, and host directory mounts.
- **Audio and video**: 4-channel Paula audio, RTG graphics cards (Picasso II/II+, Z3660), host MIDI in/out bridging, and built-in Roland MT-32 and General MIDI synthesis.
- **Expansion and networking**: Zorro II/III autoconfig, A2065 Ethernet, host-backed bsdsocket.library, and sandboxed WebAssembly expansion plugins.
- **Debugging**: CPU and chipset debugger, reverse stepping, live Kickstart/AROS symbols, frame and instruction profiling, VCD waveforms, and source debugging through GDB or DAP. Both the native VS Code extension and the [Copperline fork of Bartman's extension](docs/debugger/vscode-bartman.md) are supported.
- **Automation and replay**: [Save states](docs/guide/ui.md#save-states), [WinUAE state import](docs/guide/winuae-state.md), headless input scripts and captures, and a JSON-RPC control protocol. `copperline-ctl` also provides DAP (`--dap`) and MCP (`--mcp`) servers.
- **Freezer cartridge**: Action Replay-style cartridge support with bundled HRTMon (`--cartridge hrtmon`), allowing running software to be frozen into the monitor via the menu, a hotkey, headless `--freeze-after`, or the control protocol.
- **Direct launching**: Boot directly into WHDLoad game packages (`--whdload`) or host-built Amiga executables (`--run`, including bare Kickstart 1.3), with a WinUAE-compatible `uaelib` trap allowing guest code to control warp speed, log debug messages, and register debug resources.
- **WebAssembly build**: Run directly in modern web browsers at [copperline.dev/try](https://copperline.dev/try/).

## Installation

### macOS (Homebrew)

```sh
brew tap copperlinehq/copperline https://github.com/CopperlineHQ/Copperline
brew install copperline
```

To build from the latest development commit instead:

```sh
brew install --HEAD copperline
```

Pre-built disk images (`.dmg`) are also available on the [releases page](https://github.com/CopperlineHQ/Copperline/releases).

### Linux (Flatpak or AppImage)

```sh
flatpak install flathub dev.copperline.Copperline
flatpak run dev.copperline.Copperline
```

Standalone AppImage binaries are also available on the [releases page](https://github.com/CopperlineHQ/Copperline/releases).

*Note:* On Linux, presentation requires a Vulkan driver. Most modern GPUs support Vulkan natively. For virtual machines or older hardware, install the Mesa software Vulkan driver (`mesa-vulkan-drivers` on Debian/Ubuntu/Fedora, or `vulkan-swrast` on Arch). The Flatpak package bundles software Vulkan automatically.

### Building from source

```sh
cargo build --release
```

Run the resulting `target/release/copperline` binary. `--release` is a Cargo
build option; unoptimized debug builds are too slow for real-time emulation.

Dependencies:

- Rust 1.95+ (tested on stable)
- Windows ARM64 source builds: LLVM clang on `PATH` for Internet netplay's crypto backend. The Windows packaging script also finds Visual Studio's bundled Clang tools.
- Fedora build dependencies: `sudo dnf install alsa-lib-devel systemd-devel gcc`
- Debian/Ubuntu build dependencies: `sudo apt install libasound2-dev libsystemd-dev gcc`

## Quick Start

Run Copperline from the terminal:

```sh
./target/release/copperline
```

Running with no arguments and no `./copperline.toml` opens the interactive configuration launcher (defaulting to an A500 profile with the bundled AROS ROM).

To boot directly into a Kickstart ROM, configuration file, or floppy image:

```sh
./target/release/copperline path/to/kickstart.rom
./target/release/copperline --config path/to/copperline.toml
./target/release/copperline --model A1200 --fast 8M KICK31.ROM --insert-disk-after 0 df0 game.adf
```

### Essential keyboard shortcuts

- **Quit**: `Cmd+Q` (macOS) / `Alt+Q` (Linux/Windows)
- **Debugger**: `Cmd+B` (macOS) / `Alt+B` (Linux/Windows)
- **Screenshot**: `Cmd+S` (macOS) / `Alt+S` (Linux/Windows)
- **Mouse capture**: `Cmd+G` (macOS) / `Alt+G` (Linux/Windows)
- **Joystick toggle**: `Cmd+J` (macOS) / `Alt+J` (Linux/Windows) switches between physical gamepad and keyboard arrows.

See the [UI guide](docs/guide/ui.md) for full interface details.

## Configuration

Copperline uses TOML configuration files. You can copy `copperline.example.toml` to `copperline.toml` to customize machine settings:

```toml
rom = "kickstart31.rom"

[cpu]
model = "68020"

[memory]
chip = "2M"
fast = "8M"

[chipset]
revision = "AGA"
video = "PAL"

[floppy.df0]
path = "game.adf"
```

See the [configuration guide](docs/guide/configuration.md) for the complete reference of all configuration options, machine profiles, and storage settings.

## Documentation

The manual is published at [copperline.dev/docs](https://copperline.dev/docs/) and available under `docs/`:

- [Getting Started](docs/guide/getting-started.md) - Installation and setup
- [Configuration](docs/guide/configuration.md) - Configuration file reference and CLI options
- [User Interface](docs/guide/ui.md) - Windows, menus, and controls
- [WHDLoad Support](docs/guide/whdload.md) - Direct WHDLoad package loading
- [Direct Executable Launching](docs/guide/run.md) - Running cross-compiled Amiga binaries
- [Floppy Hardware Bridge](docs/guide/fluxbridge.md) - Real floppy drives via Greaseweazle
- [Rollback Netplay](docs/guide/netplay.md) - two-player sessions with mice, joysticks or CD32 pads through desktop GUI/CLI (direct IP or encrypted Internet invitations with automatic NAT traversal and relay fallback), or browser WebRTC with QR invitations, host ROM/disk/configuration transfer, synchronized browser disk swaps and relay fallback, plus input prediction, rollback and matching-machine checks
- [Headless Mode](docs/guide/headless.md) - Scripted runs and screenshot/frame dumps
- [Debugging](docs/debugger/window.md) - In-window, headless, and GDB debugging
- [VS Code](docs/debugger/vscode.md) - Setup and illustrated source debugging; [Bartman with Copperline](docs/debugger/vscode-bartman.md) covers fork installation and visual profiling
- [Debug Adapter Protocol](docs/debugger/dap.md) - Launch, attach, and protocol reference for VS Code, nvim-dap, and other IDEs
- [Control Protocol](docs/debugger/control.md) - JSON-RPC control interface (`copperline-ctl`)
- [Internals](docs/internals/architecture.md) - Architecture, chipset, and timing models

To build the documentation locally:

```sh
npm install -g mystmd
cd docs && myst build --html
```

## Testing

Run the unit test suite:

```sh
cargo test
```

Integration tests requiring local ROM and disk assets can be run with:

```sh
cargo test --release -- --ignored
```

See [`tests/README.md`](tests/README.md) for details on asset paths and test configuration.

## Community and Support

- Chat with developers and users on [Discord](https://discord.gg/HDTjt3tYAC).
- Report bugs and request features via [GitHub Issues](https://github.com/CopperlineHQ/Copperline/issues).
- Contributing guidelines are detailed in [CONTRIBUTING.md](CONTRIBUTING.md).
- Support ongoing development on [Patreon](https://www.patreon.com/cw/Copperline) (see [FUNDING.md](FUNDING.md)).

## Credits

See [CREDITS.md](CREDITS.md) for contributor and third-party software credits.

- [AROS Research Operating System](https://www.aros.org/) (bundled boot ROM)
- [DiagROM](https://www.diagrom.com/) by John "Chucky" Hertell
- [HRTMon](https://github.com/wepl/hrtmon) by Alain Malek, Bert Jahn and contributors (bundled freezer-cartridge monitor, GPL-2.0-or-later)
- [m68k](https://crates.io/crates/m68k) CPU core

## License

Copperline is free software released under the GNU General Public License version 3 or later. See [LICENSE](LICENSE) for details.

## Trademarks

Amiga and Commodore are trademarks of their respective owners. Copperline is an independent, unofficial project and is not affiliated with or endorsed by any trademark holder.
