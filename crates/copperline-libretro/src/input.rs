// SPDX-License-Identifier: GPL-3.0-or-later

use crate::abi;
use copperline::bus::PortDevice;
use copperline::emulator::Emulator;

#[derive(Clone)]
pub struct Controls {
    pub keys: [bool; 128],
    pub pending: [[i32; 2]; 2],
    pub devices: [u32; 2],
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            keys: [false; 128],
            pending: [[0; 2]; 2],
            devices: [abi::AUTO; 2],
        }
    }
}

impl Controls {
    pub fn poll(&mut self, emu: &mut Emulator, input: impl Fn(u32, u32, u32) -> i16) {
        let mut keys = [false; 128];
        for key in 0..=322 {
            if let Some(raw) = raw_key(key) {
                keys[raw as usize] |= input(0, abi::KEYBOARD, key) != 0;
            }
        }
        for (raw, (&held, &previous)) in keys.iter().zip(&self.keys).enumerate() {
            if held != previous {
                emu.bus_mut().enqueue_key_event(raw as u8, held);
            }
        }
        self.keys = keys;
        for port in 0..2 {
            let device = match self.devices[port] {
                abi::AUTO if port == 0 => abi::JOYPAD,
                abi::AUTO => abi::MOUSE,
                device => device,
            };
            let state = &mut emu.bus_mut().input;
            // The first RetroPad drives the Amiga's usual game port (port 2).
            // Automatic player 2 uses the first physical mouse on Amiga port 1.
            let amiga_port = 1 - port;
            match device {
                abi::MOUSE => {
                    state.set_port_device(amiga_port, PortDevice::Mouse);
                    let mouse = if self.devices[port] == abi::AUTO {
                        0
                    } else {
                        port as u32
                    };
                    for axis in 0..2 {
                        self.pending[port][axis] = self.pending[port][axis]
                            .saturating_add(i32::from(input(mouse, abi::MOUSE, axis as u32)));
                    }
                    let [x, y] = self.pending[port].map(|value| value.clamp(-100, 100));
                    self.pending[port][0] -= x;
                    self.pending[port][1] -= y;
                    state.add_mouse_delta(amiga_port, x, y);
                    for (button, id) in [2, 3, 6].into_iter().enumerate() {
                        state.set_mouse_button(
                            amiga_port,
                            button as u8,
                            input(mouse, abi::MOUSE, id) != 0,
                        );
                    }
                }
                abi::JOYPAD => {
                    let held =
                        [4, 5, 6, 7, 0, 8].map(|id| input(port as u32, abi::JOYPAD, id) != 0);
                    state.set_joystick(
                        amiga_port, held[0], held[1], held[2], held[3], held[4], held[5],
                    );
                }
                _ => state.set_port_device(amiga_port, PortDevice::None),
            }
        }
    }
}

/// libretro keysyms to Amiga raw scan codes; modifier aliases are combined
/// before transitions are emitted (both host Ctrl keys drive one Amiga key).
pub fn raw_key(key: u32) -> Option<u8> {
    Some(match key {
        97..=122 => [
            0x20, 0x35, 0x33, 0x22, 0x12, 0x23, 0x24, 0x25, 0x17, 0x26, 0x27, 0x28, 0x37, 0x36,
            0x18, 0x19, 0x10, 0x13, 0x21, 0x14, 0x16, 0x34, 0x11, 0x32, 0x15, 0x31,
        ][(key - 97) as usize],
        49..=57 => (key - 48) as u8,
        48 => 0x0a,
        96 => 0x00,
        45 => 0x0b,
        61 => 0x0c,
        92 => 0x0d,
        91 => 0x1a,
        93 => 0x1b,
        59 => 0x29,
        39 => 0x2a,
        44 => 0x38,
        46 => 0x39,
        47 => 0x3a,
        32 => 0x40,
        8 => 0x41,
        9 => 0x42,
        13 => 0x44,
        27 => 0x45,
        127 => 0x46,
        273 => 0x4c,
        274 => 0x4d,
        275 => 0x4e,
        276 => 0x4f,
        282..=291 => (key - 282) as u8 + 0x50,
        303 => 0x61,
        304 => 0x60,
        305 | 306 => 0x63,
        307 => 0x65,
        308 => 0x64,
        309 | 311 => 0x67,
        310 | 312 => 0x66,
        301 => 0x62,
        315 | 277 => 0x5f,
        256..=265 => {
            [0x0f, 0x1d, 0x1e, 0x1f, 0x2d, 0x2e, 0x2f, 0x3d, 0x3e, 0x3f][(key - 256) as usize]
        }
        266 => 0x3c,
        267 => 0x5c,
        268 => 0x5d,
        269 => 0x4a,
        270 => 0x5e,
        271 => 0x43,
        _ => return None,
    })
}
