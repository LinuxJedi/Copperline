#!/usr/bin/env python3
"""Exercise a Copperline shared library through libretro, without RetroArch.

Uses only the Python standard library. Both the ABI declarations and callback
dispatch live outside Rust so this also checks exported symbols and C layouts.
"""

import argparse
import ctypes as c
import hashlib
import json
from pathlib import Path
import struct
import tempfile
import zlib


class Game(c.Structure):
    _fields_ = [("path", c.c_char_p), ("data", c.c_void_p),
                ("size", c.c_size_t), ("meta", c.c_char_p)]


class System(c.Structure):
    _fields_ = [("name", c.c_char_p), ("version", c.c_char_p),
                ("extensions", c.c_char_p), ("fullpath", c.c_bool),
                ("block_extract", c.c_bool)]


class Geometry(c.Structure):
    _fields_ = [("width", c.c_uint), ("height", c.c_uint),
                ("max_width", c.c_uint), ("max_height", c.c_uint),
                ("aspect", c.c_float)]


class Timing(c.Structure):
    _fields_ = [("fps", c.c_double), ("sample_rate", c.c_double)]


class AV(c.Structure):
    _fields_ = [("geometry", Geometry), ("timing", Timing)]


class Variable(c.Structure):
    _fields_ = [("key", c.c_char_p), ("value", c.c_void_p)]


class Message(c.Structure):
    _fields_ = [("text", c.c_char_p), ("frames", c.c_uint)]


ENV = c.CFUNCTYPE(c.c_bool, c.c_uint, c.c_void_p)
VIDEO = c.CFUNCTYPE(None, c.c_void_p, c.c_uint, c.c_uint, c.c_size_t)
AUDIO = c.CFUNCTYPE(c.c_size_t, c.c_void_p, c.c_size_t)
POLL = c.CFUNCTYPE(None)
INPUT = c.CFUNCTYPE(c.c_int16, c.c_uint, c.c_uint, c.c_uint, c.c_uint)
SET_EJECT = c.CFUNCTYPE(c.c_bool, c.c_bool)
GET_EJECT = c.CFUNCTYPE(c.c_bool)
GET_UINT = c.CFUNCTYPE(c.c_uint)
SET_INDEX = c.CFUNCTYPE(c.c_bool, c.c_uint)
REPLACE = c.CFUNCTYPE(c.c_bool, c.c_uint, c.POINTER(Game))
ADD = c.CFUNCTYPE(c.c_bool)


class Disks(c.Structure):
    _fields_ = [("eject", SET_EJECT), ("is_ejected", GET_EJECT),
                ("index", GET_UINT), ("select", SET_INDEX),
                ("count", GET_UINT), ("replace", REPLACE), ("add", ADD)]


def png(path, pixels, width, height):
    """Encode native-endian XRGB8888 output as an RGB PNG."""
    rows = bytearray()
    values = (c.c_uint32 * (width * height)).from_buffer_copy(pixels)
    for y in range(height):
        rows.append(0)
        for value in values[y * width:(y + 1) * width]:
            rows.extend(((value >> 16) & 255, (value >> 8) & 255, value & 255))

    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data)))

    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"\x89PNG\r\n\x1a\n"
                     + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
                     + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


def probe_adf():
    source = Path(__file__).resolve().parents[1] / "crates/copperline-libretro/tests/probe.hex"
    words = [word for line in source.read_text().splitlines()
             for word in line.split("#", 1)[0].split()]
    disk = bytearray(901120)
    disk[:4] = b"DOS\0"
    struct.pack_into(">I", disk, 8, 880)
    for index, word in enumerate(words):
        struct.pack_into(">H", disk, 12 + index * 2, int(word, 16))
    checksum = 0
    for offset in range(0, 1024, 4):
        checksum += struct.unpack_from(">I", disk, offset)[0]
        checksum = (checksum & 0xffffffff) + (checksum >> 32)
    struct.pack_into(">I", disk, 4, checksum ^ 0xffffffff)
    return disk


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("library", type=Path)
    parser.add_argument("--frames", type=int, default=1200)
    parser.add_argument("--model", choices=["A500", "A1200"], default="A500")
    parser.add_argument("--video", choices=["PAL", "NTSC"], default="PAL")
    parser.add_argument("--screenshot", type=Path)
    args = parser.parse_args()
    lib = c.CDLL(str(args.library.resolve()))

    def bind(name, result, *params):
        function = getattr(lib, name)
        function.restype = result
        function.argtypes = params
        return function

    for name in ["retro_init", "retro_deinit", "retro_run", "retro_reset", "retro_unload_game"]:
        bind(name, None)
    bind("retro_api_version", c.c_uint)
    bind("retro_get_system_info", None, c.POINTER(System))
    bind("retro_get_system_av_info", None, c.POINTER(AV))
    bind("retro_load_game", c.c_bool, c.POINTER(Game))
    bind("retro_serialize_size", c.c_size_t)
    bind("retro_serialize", c.c_bool, c.c_void_p, c.c_size_t)
    bind("retro_unserialize", c.c_bool, c.c_void_p, c.c_size_t)
    bind("retro_get_region", c.c_uint)
    assert lib.retro_api_version() == 1
    system = System()
    lib.retro_get_system_info(c.byref(system))
    assert system.name == b"Copperline" and system.extensions == b"adf|m3u"
    assert system.fullpath

    with tempfile.TemporaryDirectory(prefix="copperline-libretro-") as temporary:
        root = Path(temporary)
        directory = c.create_string_buffer(str(root).encode())
        options = {key: c.create_string_buffer(value.encode()) for key, value in {
            b"copperline_model": args.model, b"copperline_video": args.video,
            b"copperline_rom": "AROS", b"copperline_write_protect": "disabled",
        }.items()}
        host = {"frame": 0, "video": b"", "audio": bytearray(), "errors": [], "disks": None}

        @ENV
        def environment(command, data):
            if command in (9, 31):
                c.cast(data, c.POINTER(c.c_void_p))[0] = c.addressof(directory)
            elif command == 10:
                return c.cast(data, c.POINTER(c.c_uint))[0] == 1
            elif command == 15:
                variable = c.cast(data, c.POINTER(Variable)).contents
                option = options.get(variable.key)
                if option is None:
                    return False
                variable.value = c.addressof(option)
            elif command == 13:
                host["disks"] = c.cast(data, c.POINTER(Disks)).contents
            elif command == 6:
                host["errors"].append(c.cast(data, c.POINTER(Message)).contents.text.decode())
            elif command not in (11, 16, 18, 32, 35, 37):
                return False
            return True

        @VIDEO
        def video(data, width, height, pitch):
            if pitch != width * 4 or width > 1432 or height > 1252:
                host["errors"].append("invalid framebuffer geometry")
                return
            host["video"] = c.string_at(data, pitch * height)
            host["width"], host["height"] = width, height

        @AUDIO
        def audio(data, frames):
            host["audio"].extend(c.string_at(data, frames * 4))
            return frames

        @POLL
        def poll():
            host["frame"] += 1

        @INPUT
        def input_state(port, device, index, control):
            if port == 0 and device == 2 and control == 0 and host["frame"] == 100:
                return 25
            return 0

        # Keep all decorated callback objects alive until deinit.
        for name, callback, signature in [
            ("retro_set_environment", environment, ENV),
            ("retro_set_video_refresh", video, VIDEO),
            ("retro_set_audio_sample_batch", audio, AUDIO),
            ("retro_set_input_poll", poll, POLL),
            ("retro_set_input_state", input_state, INPUT),
        ]:
            bind(name, None, signature)(callback)
        lib.retro_init()
        try:
            (root / "one.adf").write_bytes(probe_adf())
            (root / "two.adf").write_bytes(bytes([1]) * 901120)
            playlist = root / "game.m3u"
            playlist.write_text("one.adf\ntwo.adf\n")
            game = Game(str(playlist).encode(), None, 0, None)
            assert lib.retro_load_game(c.byref(game)), host["errors"]
            disks = host["disks"]
            assert disks.count() == 2 and not disks.is_ejected()
            assert disks.eject(True) and disks.select(1) and disks.eject(False)
            assert disks.index() == 1
            assert disks.eject(True) and disks.select(0) and disks.eject(False)
            av = AV()
            lib.retro_get_system_av_info(c.byref(av))
            assert 40 < av.timing.fps < 65 and av.timing.sample_rate == 44100
            assert lib.retro_get_region() == int(args.video == "PAL")
            for _ in range(args.frames):
                lib.retro_run()
            assert host["frame"] == args.frames and host["video"] and host["audio"]
            assert any(host["audio"]), "probe did not produce audio; allow enough frames to boot"
            assert len(set(c.c_uint32.from_buffer_copy(host["video"], i).value
                           for i in range(0, len(host["video"]), 4))) > 256, "probe raster did not start"
            if args.screenshot:
                png(args.screenshot, host["video"], host["width"], host["height"])
            capacity = lib.retro_serialize_size()
            saved = c.create_string_buffer(capacity)
            assert lib.retro_serialize(saved, capacity), host["errors"]
            payload_bytes = struct.unpack("<I", c.string_at(c.addressof(saved) + 40, 4))[0]

            def continuation():
                host["audio"].clear()
                for _ in range(20):
                    lib.retro_run()
                return (hashlib.sha256(host["video"]).hexdigest(),
                        hashlib.sha256(host["audio"]).hexdigest())

            expected = continuation()
            assert lib.retro_unserialize(saved, capacity), host["errors"]
            host["frame"] = args.frames
            assert continuation() == expected, "save/load changed audio or framebuffer"
            assert lib.retro_serialize_size() == capacity
            assert not host["errors"], host["errors"]
            lib.retro_reset()
            lib.retro_run()
            lib.retro_unload_game()
            assert lib.retro_serialize_size() == 0
            assert lib.retro_load_game(None), host["errors"]
            lib.retro_run()
            lib.retro_unload_game()
            assert not host["errors"], host["errors"]
            print(json.dumps({"model": args.model, "video": args.video, "frames": args.frames,
                              "fps": av.timing.fps, "state_payload_bytes": payload_bytes,
                              "framebuffer_sha256": expected[0], "audio_sha256": expected[1],
                              "result": "passed"}))
        finally:
            lib.retro_deinit()


if __name__ == "__main__":
    main()
