Copperline - portable Windows build
====================================

Copperline is a cycle-driven Amiga emulator (OCS/ECS/AGA).

Running
-------
This is a portable build: no installation is required. Unzip it anywhere and
run copperline.exe. It needs no administrator rights and no Visual C++
Redistributable (the C runtime is linked statically into the executable).

On first launch Windows SmartScreen may show "Windows protected your PC"
because the executable is not code-signed. Click "More info", then
"Run anyway" to start it.

Boot ROM
--------
With no ROM of your own, Copperline boots the bundled AROS open-source
Kickstart replacement, found in the aros\ folder next to the executable. AROS
is freely redistributable; see aros\LICENSE. To use a real Kickstart instead,
point a config file at it, or load it at runtime from the menu
(Load Kickstart ROM...).

Configuration
-------------
copperline.example.toml is a starting point. Copy it, edit the paths to your
own Kickstart ROM and disk/hard-disk images, and launch with:

    copperline.exe --config your-config.toml

Run "copperline.exe --help" for the full command-line surface.

Copperline is licensed under GPL-3.0-or-later; see LICENSE.txt.
