Copperline - macOS disk image
==============================

Copperline is a cycle-driven Amiga emulator (OCS/ECS/AGA).

Installing
----------
Drag Copperline.app onto the Applications shortcut in this window, then launch
it from Applications or Launchpad. The app is a universal binary and runs
natively on both Apple Silicon and Intel Macs.

First launch (unsigned app)
---------------------------
This build is not code-signed or notarized, so on first launch macOS Gatekeeper
will refuse to open it ("Copperline cannot be opened because the developer
cannot be verified", or "is damaged" on Apple Silicon). This is expected for an
unsigned download. To run it anyway, right-click (or Control-click)
Copperline.app and choose Open, then confirm Open in the dialog. macOS
remembers the choice, so subsequent launches open normally.

If right-click Open still refuses, clear the download quarantine from a terminal:

    xattr -dr com.apple.quarantine /Applications/Copperline.app

Boot ROM
--------
With no ROM of your own, Copperline boots the bundled AROS open-source
Kickstart replacement, stored inside the app at
Copperline.app/Contents/Resources/aros. AROS is freely redistributable; see the
LICENSE next to the ROM. To use a real Kickstart instead, point a config file
at it, or load it at runtime from the menu (Load Kickstart ROM...).

Configuration
-------------
copperline.example.toml is a starting point. Copy it, edit the paths to your
own Kickstart ROM and disk/hard-disk images, and launch from a terminal with:

    /Applications/Copperline.app/Contents/MacOS/copperline --config your-config.toml

Run that binary with --help for the full command-line surface.

Copperline is licensed under GPL-3.0-or-later; see LICENSE.txt.
