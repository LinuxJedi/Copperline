// SPDX-License-Identifier: GPL-3.0-or-later

//! The libretro v1 ABI subset used by this core. Field order and constants
//! follow https://github.com/libretro/libretro-common/blob/master/include/libretro.h.

use std::ffi::{c_char, c_void};

pub type Environment = unsafe extern "C" fn(u32, *mut c_void) -> bool;
pub type Video = unsafe extern "C" fn(*const c_void, u32, u32, usize);
pub type Audio = unsafe extern "C" fn(i16, i16);
pub type AudioBatch = unsafe extern "C" fn(*const i16, usize) -> usize;
pub type Poll = unsafe extern "C" fn();
pub type Input = unsafe extern "C" fn(u32, u32, u32, u32) -> i16;

pub const NONE: u32 = 0;
pub const JOYPAD: u32 = 1;
pub const MOUSE: u32 = 2;
pub const KEYBOARD: u32 = 3;
pub const AUTO: u32 = JOYPAD | (1 << 8);

#[repr(C)]
pub struct GameInfo {
    pub path: *const c_char,
    pub data: *const c_void,
    pub size: usize,
    pub meta: *const c_char,
}

#[repr(C)]
pub struct SystemInfo {
    pub library_name: *const c_char,
    pub library_version: *const c_char,
    pub valid_extensions: *const c_char,
    pub need_fullpath: bool,
    pub block_extract: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub base_width: u32,
    pub base_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub aspect_ratio: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    pub fps: f64,
    pub sample_rate: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AvInfo {
    pub geometry: Geometry,
    pub timing: Timing,
}

#[repr(C)]
pub struct Variable {
    pub key: *const c_char,
    pub value: *const c_char,
}

#[repr(C)]
pub struct Message {
    pub msg: *const c_char,
    pub frames: u32,
}

#[repr(C)]
pub struct ControllerDescription {
    pub desc: *const c_char,
    pub id: u32,
}

#[repr(C)]
pub struct ControllerInfo {
    pub types: *const ControllerDescription,
    pub num_types: u32,
}

#[repr(C)]
pub struct InputDescriptor {
    pub port: u32,
    pub device: u32,
    pub index: u32,
    pub id: u32,
    pub description: *const c_char,
}

// These descriptors are immutable process-lifetime tables pointing only at
// other immutable static tables and NUL-terminated literals.
unsafe impl Sync for Variable {}
unsafe impl Sync for ControllerDescription {}
unsafe impl Sync for ControllerInfo {}
unsafe impl Sync for InputDescriptor {}

#[repr(C)]
pub struct DiskControl {
    pub set_eject_state: unsafe extern "C" fn(bool) -> bool,
    pub get_eject_state: unsafe extern "C" fn() -> bool,
    pub get_image_index: unsafe extern "C" fn() -> u32,
    pub set_image_index: unsafe extern "C" fn(u32) -> bool,
    pub get_num_images: unsafe extern "C" fn() -> u32,
    pub replace_image_index: unsafe extern "C" fn(u32, *const GameInfo) -> bool,
    pub add_image_index: unsafe extern "C" fn() -> bool,
}
