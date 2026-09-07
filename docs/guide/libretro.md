# Libretro / RetroArch

Copperline's libretro core runs A500 and A1200 floppy software in RetroArch
and other libretro frontends. It includes the AROS boot ROM and uses the same
CPU, chipset, renderer and audio mixer as the desktop emulator.

This first version supports standard 880 KiB and 1760 KiB ADF files, M3U disk
playlists, PAL/NTSC, keyboard, mouse, two-button joysticks, and save states.
One floppy drive, DF0, is fitted. Extended ADF and compressed disk formats,
CD32/CD images, WHDLoad packages, hard drives and desktop configuration files
are outside this version's supported content formats.

## Build and install

From the repository root:

```sh
cargo build --manifest-path crates/copperline-libretro/Cargo.toml --release --locked
```

The output is under `crates/copperline-libretro/target/release/`:

| Platform | Build output | Installed core filename |
|---|---|---|
| Linux | `libcopperline_libretro.so` | `copperline_libretro.so` |
| macOS | `libcopperline_libretro.dylib` | `copperline_libretro.dylib` |
| Windows | `copperline_libretro.dll` | `copperline_libretro.dll` |

Copy the library to the frontend's cores directory using the installed name,
and copy `crates/copperline-libretro/copperline_libretro.info` to its core-info
directory. RetroArch shows these locations under **Settings → Directory**.
Load the Copperline core, then open an ADF or M3U through **Load Content**.
Starting the core without content boots AROS with an empty drive.

The repository's Libretro workflow builds packages for Linux, macOS and
Windows. Each package includes the core, its info file, the Copperline license,
and AROS licensing and acknowledgement files. These are workflow artifacts;
installation through RetroArch's Online Updater is not configured.

## Machine and ROM options

The core options select A500 or A1200, PAL or NTSC, AROS or Kickstart, and
floppy write protection. Close and reload content after changing an option;
the frontend's Reset command resets the current machine without applying new
options. The core does not read the desktop's saved defaults or `copperline.toml`.

AROS is embedded in the library and needs no external files. For software that
needs a Commodore ROM, select **Kickstart** and put your ROM in the frontend's
system directory. The core first looks for `kickstart-a500.rom` or
`kickstart-a1200.rom`, according to the selected machine, then `kickstart.rom`.
A missing or invalid selected ROM produces an error; it does not fall back to
AROS. Commodore ROMs are not included.

The host clock does not seed the emulated machine. A fitted RTC starts at
2000-01-01 00:00:00 UTC and advances with emulated time.

## Controls

With both frontend ports set to **Automatic**, the first RetroPad controls
the joystick on Amiga port 2, and the first host mouse controls Amiga port 1.
Keyboard input is always available; RetroArch's Game Focus mode passes keys
through without triggering its normal hotkeys.

| Frontend control | Amiga control |
|---|---|
| RetroPad D-pad | Joystick directions |
| RetroPad B (south button) | Fire |
| RetroPad A (east button) | Second fire |
| Mouse left / right / middle | Mouse buttons |
| Left / right Shift, Alt | Corresponding Amiga modifier |
| Either Ctrl | Amiga Ctrl |
| Left / right Super or Meta | Left / right Amiga |
| Insert or Help | Amiga Help |

Frontend port 1 corresponds to Amiga port 2; frontend port 2 corresponds to
Amiga port 1. Select **Amiga joystick** for both to use two gamepads. Select
**Amiga mouse** explicitly for both to use the frontend's first and second
mice; the frontend and its input driver must support multiple mice. A port
can also be disconnected. This version has no on-screen keyboard or
gamepad-to-mouse controls.

## Disk swapping and writes

An M3U playlist contains one ADF path per line, with paths relative to the
playlist's directory or absolute. Blank lines and lines beginning with `#`
are ignored. UTF-8 with an optional BOM and either Unix or Windows line
endings is accepted. A playlist can hold up to sixteen disks.

```text
Game Disk 1.adf
Game Disk 2.adf
Save Disk.adf
```

Use the frontend's **Disc Control** menu to eject, select an image, then
insert it. Every selection uses DF0. Adding and replacing playlist images
also requires an ejected drive.

Guest writes update memory during emulation. When a disk is ejected or
content is closed, changed ADFs are written under `copperline/` in the
frontend's save directory. Names include a digest of the original image to
separate different disks with the same filename. The original ADFs remain
unchanged. Loading the same content picks up these saved copies.

If the frontend supplies no save directory, the content's directory is used;
for a no-content session the system directory is used instead. A save error
is reported; a failed eject leaves the disk inserted so saving can be retried.
Unloading always releases the machine, even if writing its saved copies fails.
Close content normally to save changes: a crash or forced process termination
can lose writes since the last eject.

## Save states and presentation

Frontend save states include the machine, every disk in the playlist, the
selected image and eject state, controller selections, and pending mouse and
keyboard input. Restoring a state also restores disk contents. Those contents
become the persistent copies on the next eject or normal close.

States require the same machine, boot ROM, original playlist images and
write-protection option. They use a libretro-specific envelope and cannot be
opened directly as desktop `.clstate` files. The frontend is given a fixed
64 MiB capacity throughout each loaded session, including after disk changes;
unused space is zeroed and compresses well. A state that exceeds that bound
is rejected without changing the advertised capacity. Rewind and run-ahead
can therefore have significant memory and serialization costs. RetroArch
netplay, rewind and run-ahead are not validated features of this first version.

The frontend owns pacing, video output and audio output. Each `retro_run`
advances one hardware video field. Refresh information follows Agnus's actual
timing, including interlace and programmable totals, using Copperline's
emulated colour-clock frequency. It is not rounded to exactly 50 or 60 Hz.
The output uses XRGB8888 pixels, 4:3 display aspect and 44.1 kHz stereo audio.
Standard screens use the shared TV aperture; programmable scans use the
shared full-field presentation. Interlaced output uses line doubling, without
field history or phosphor effects. Frontend shaders can be applied normally.

## Development checks

```sh
cargo fmt --manifest-path crates/copperline-libretro/Cargo.toml --check
cargo clippy --manifest-path crates/copperline-libretro/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path crates/copperline-libretro/Cargo.toml --profile ci --locked
python3 tools/check-libretro.py crates/copperline-libretro/target/release/libcopperline_libretro.dylib --screenshot /tmp/libretro.png
```

Use the matching library suffix on Linux or Windows. The Rust test frontend
boots a 68000 probe that paints a raster pattern and plays a Paula tone. It
compares machine state, framebuffer and audio against direct headless
execution for both models and video standards, then repeats execution after
a state restore. It also checks disk persistence and inactive disk restoration.
The Python frontend loads the actual shared library, exercises its C ABI,
and verifies audio/video replay after save/load. It can write a PNG capture
without a display or audio device. API calls and callbacks run synchronously
on the frontend's emulation thread.
