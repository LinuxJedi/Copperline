#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Run two local netplay peers and compare their confirmed headless captures."""

import argparse
import hashlib
import math
import os
from pathlib import Path
import re
import secrets
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/copperline"))
    parser.add_argument("--seconds", type=float, default=20.0)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--internet", action="store_true", help="use Internet invitations and public relays")
    parser.add_argument("--relay-only", action="store_true", help="disable direct IP paths (requires --internet)")
    parser.add_argument("machine_args", nargs=argparse.REMAINDER,
                        help="machine arguments after --, e.g. --config game.toml")
    args = parser.parse_args()
    if not math.isfinite(args.seconds) or args.seconds <= 0:
        parser.error("--seconds must be finite and positive")
    if args.relay_only and not args.internet:
        parser.error("--relay-only requires --internet")
    extra = args.machine_args
    if extra[:1] == ["--"]:
        extra = extra[1:]
    if any(arg.startswith("--netplay-") for arg in extra):
        parser.error("this check supplies its own netplay settings")
    output = (args.output_dir or Path(tempfile.mkdtemp(prefix="copperline-netplay-"))).resolve()
    output.mkdir(parents=True, exist_ok=True)
    ports = []
    if not args.internet:
        # Reserve distinct ports together, then release them immediately before launch.
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as first, \
                socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as second:
            first.bind(("127.0.0.1", 0))
            second.bind(("127.0.0.1", 0))
            ports = [first.getsockname()[1], second.getsockname()[1]]
    invitation = output / "invitation.txt"
    if args.internet and invitation.exists():
        parser.error("the output directory already contains an invitation; use a fresh directory")
    session = secrets.token_hex(16)
    processes = []
    logs = []
    try:
        for player in range(2):
            log = (output / f"player{player+1}.log").open("w")
            logs.append(log)
            command = [str(args.binary.resolve()), "--factory", "--model", "A500",
                       "--serial", "off", "--port1", "joystick", "--port2", "joystick"]
            # Only the host has game assets/configuration. The guest proves
            # setup transfer by starting with its bare local defaults.
            if player == 0:
                command += extra
            if args.internet:
                if player == 0:
                    command += ["--netplay-host", str(invitation)]
                else:
                    deadline = time.monotonic() + 30
                    while not invitation.exists():
                        if processes[0].poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError(f"host did not create an invitation; see {output}")
                        time.sleep(0.05)
                    # File creation precedes writing; wait for the completed code.
                    while not (code := invitation.read_text()):
                        if processes[0].poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError(f"host invitation remained empty; see {output}")
                        time.sleep(0.05)
                    command += ["--netplay-join", code]
                if args.relay_only:
                    command += ["--netplay-relay-only"]
            else:
                command += ["--netplay-bind", f"127.0.0.1:{ports[player]}",
                            "--netplay-peer", f"127.0.0.1:{ports[1-player]}",
                            "--netplay-player", str(player+1), "--netplay-session", session]
            command += ["--noaudio",
                        "--joy-after", str(args.seconds / 2), "red", "100", str(player+1),
                        "--screenshot-after", str(args.seconds), str(output / f"player{player+1}.png")]
            processes.append(subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT,
                                               env={**os.environ, "RUST_LOG": "warn,copperline::netplay=info"}))
        deadline = time.monotonic() + max(90, args.seconds * 10)
        for player, process in enumerate(processes, 1):
            result = process.wait(timeout=max(0.1, deadline - time.monotonic()))
            if result:
                raise RuntimeError(f"player {player} exited with {result}; see {output}")
    finally:
        for process in processes:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        for log in logs:
            log.close()
    hashes = []
    for player in (1, 2):
        text = (output / f"player{player}.log").read_text()
        if args.relay_only and "Internet route is relay" not in text:
            raise RuntimeError(f"player {player} did not select a relay; see {output}")
        status = re.search(r"netplay: finished frames=(\d+) confirmed=(\d+) checked=(\d+)", text)
        if not status or status[1] != status[2] or (args.seconds >= 2 and int(status[3]) == 0):
            raise RuntimeError(f"player {player} did not finish with confirmed input/checksums; see {output}")
        hashes.append(hashlib.sha256((output / f"player{player}.png").read_bytes()).hexdigest())
        print(f"Player {player}: {status[1]} frames, checked through {status[3]}")
    if hashes[0] != hashes[1]:
        raise RuntimeError(f"peer screenshots differ; see {output}")
    print(f"Matching PNG SHA-256: {hashes[0]}\nCaptures and logs: {output}")


if __name__ == "__main__":
    main()
