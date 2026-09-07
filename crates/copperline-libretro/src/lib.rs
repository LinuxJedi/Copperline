// SPDX-License-Identifier: GPL-3.0-or-later

//! Copperline libretro core. The frontend calls the API on its emulation
//! thread, supplies valid ABI pointers/buffers, and keeps callbacks alive
//! until deinit. No Rust panic crosses the C boundary. Reentrant callbacks
//! cannot borrow a running machine and return an unavailable result.

pub mod abi;
mod core;
mod input;
mod media;

use abi::*;
use anyhow::{ensure, Context, Result};
use core::{Core, STATE_CAPACITY};
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Default)]
struct Callbacks {
    environment: Option<Environment>,
    video: Option<Video>,
    audio: Option<Audio>,
    batch: Option<AudioBatch>,
    poll: Option<Poll>,
    input: Option<Input>,
}

#[derive(Default)]
struct Runtime {
    core: Option<Core>,
    devices: [Option<u32>; 2],
    av: Option<AvInfo>,
}

thread_local! {
    static CALLBACKS: Cell<Callbacks> = Cell::new(Callbacks::default());
    static RUNTIME: RefCell<Runtime> = RefCell::new(Runtime::default());
}

fn env<T>(command: u32, value: &mut T) -> bool {
    CALLBACKS
        .get()
        .environment
        .is_some_and(|callback| unsafe { callback(command, std::ptr::from_mut(value).cast()) })
}

fn report(error: impl std::fmt::Display) {
    let text = format!("Copperline: {error}");
    eprintln!("{text}");
    if let Ok(text) = CString::new(text) {
        env(
            6,
            &mut Message {
                msg: text.as_ptr(),
                frames: 300,
            },
        );
    }
}

fn boundary<T: Default>(action: impl FnOnce() -> Result<T>) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            report(format!("{error:#}"));
            T::default()
        }
        Err(_) => {
            report("core operation panicked");
            T::default()
        }
    }
}

fn with_core<T>(action: impl FnOnce(&mut Core) -> Result<T>) -> Result<T> {
    RUNTIME.with(|runtime| {
        let mut runtime = runtime.try_borrow_mut().context("core is busy")?;
        action(runtime.core.as_mut().context("no content is loaded")?)
    })
}

fn variable(key: &'static CStr, default: &str) -> String {
    let mut variable = Variable {
        key: key.as_ptr(),
        value: std::ptr::null(),
    };
    if env(15, &mut variable) && !variable.value.is_null() {
        unsafe { CStr::from_ptr(variable.value) }
            .to_string_lossy()
            .into_owned()
    } else {
        default.to_owned()
    }
}

fn directory(command: u32) -> Option<PathBuf> {
    let mut path: *const c_char = std::ptr::null();
    if env(command, &mut path) && !path.is_null() {
        let path = unsafe { CStr::from_ptr(path) }.to_string_lossy();
        if !path.is_empty() {
            return Some(PathBuf::from(path.as_ref()));
        }
    }
    None
}

static OPTIONS: [Variable; 5] = [
    Variable {
        key: c"copperline_model".as_ptr(),
        value: c"Machine (restart required); A500|A1200".as_ptr(),
    },
    Variable {
        key: c"copperline_video".as_ptr(),
        value: c"Video standard (restart required); PAL|NTSC".as_ptr(),
    },
    Variable {
        key: c"copperline_rom".as_ptr(),
        value: c"Boot ROM (restart required); AROS|Kickstart".as_ptr(),
    },
    Variable {
        key: c"copperline_write_protect".as_ptr(),
        value: c"Floppy write protection (restart required); disabled|enabled".as_ptr(),
    },
    Variable {
        key: std::ptr::null(),
        value: std::ptr::null(),
    },
];
static DEVICES: [ControllerDescription; 4] = [
    ControllerDescription {
        desc: c"Automatic (mouse / joystick)".as_ptr(),
        id: AUTO,
    },
    ControllerDescription {
        desc: c"Amiga joystick".as_ptr(),
        id: JOYPAD,
    },
    ControllerDescription {
        desc: c"Amiga mouse".as_ptr(),
        id: MOUSE,
    },
    ControllerDescription {
        desc: c"Disconnected".as_ptr(),
        id: NONE,
    },
];
static PORTS: [ControllerInfo; 3] = [
    ControllerInfo {
        types: DEVICES.as_ptr(),
        num_types: 4,
    },
    ControllerInfo {
        types: DEVICES.as_ptr(),
        num_types: 4,
    },
    ControllerInfo {
        types: std::ptr::null(),
        num_types: 0,
    },
];

const fn input_descriptors() -> [InputDescriptor; 13] {
    let mut descriptors = [const {
        InputDescriptor {
            port: 0,
            device: 0,
            index: 0,
            id: 0,
            description: std::ptr::null(),
        }
    }; 13];
    let names = [c"Up", c"Down", c"Left", c"Right", c"Fire", c"Second fire"];
    let ids = [4, 5, 6, 7, 0, 8];
    let mut index = 0;
    while index < 12 {
        descriptors[index] = InputDescriptor {
            port: (index / 6) as u32,
            device: JOYPAD,
            index: 0,
            id: ids[index % 6],
            description: names[index % 6].as_ptr(),
        };
        index += 1;
    }
    descriptors
}
static INPUTS: [InputDescriptor; 13] = input_descriptors();

fn register_environment() {
    if let Some(callback) = CALLBACKS.get().environment {
        // The ABI accepts const descriptor tables through void*. The frontend
        // may read these tables but must not modify them.
        unsafe {
            callback(16, OPTIONS.as_ptr().cast_mut().cast());
            callback(35, PORTS.as_ptr().cast_mut().cast());
            callback(11, INPUTS.as_ptr().cast_mut().cast());
            callback(13, std::ptr::from_ref(&DISKS).cast_mut().cast());
        }
        env(18, &mut true);
    }
}

macro_rules! setter {
    ($name:ident, $field:ident, $type:ty) => {
        #[no_mangle]
        pub extern "C" fn $name(callback: Option<$type>) {
            let mut callbacks = CALLBACKS.get();
            callbacks.$field = callback;
            CALLBACKS.set(callbacks);
        }
    };
}
setter!(retro_set_video_refresh, video, Video);
setter!(retro_set_audio_sample, audio, Audio);
setter!(retro_set_audio_sample_batch, batch, AudioBatch);
setter!(retro_set_input_poll, poll, Poll);
setter!(retro_set_input_state, input, Input);

#[no_mangle]
pub extern "C" fn retro_set_environment(callback: Option<Environment>) {
    boundary(|| {
        let mut callbacks = CALLBACKS.get();
        callbacks.environment = callback;
        CALLBACKS.set(callbacks);
        register_environment();
        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn retro_api_version() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn retro_init() {
    boundary(|| {
        unload()?;
        register_environment();
        Ok(())
    });
}

fn unload() -> Result<()> {
    RUNTIME.with(|runtime| {
        let mut runtime = runtime.try_borrow_mut().context("core is busy")?;
        let result = runtime.core.as_mut().map_or(Ok(()), Core::persist);
        *runtime = Runtime::default();
        result
    })
}

#[no_mangle]
pub extern "C" fn retro_unload_game() {
    boundary(unload);
}

#[no_mangle]
pub extern "C" fn retro_deinit() {
    boundary(unload);
}

/// # Safety
/// `info` must point to writable storage for one libretro system-info struct.
#[no_mangle]
pub unsafe extern "C" fn retro_get_system_info(info: *mut SystemInfo) {
    if let Some(info) = unsafe { info.as_mut() } {
        *info = SystemInfo {
            library_name: c"Copperline".as_ptr(),
            library_version: c"0.1.0".as_ptr(),
            valid_extensions: c"adf|m3u".as_ptr(),
            need_fullpath: true,
            block_extract: false,
        };
    }
}

/// # Safety
/// `info` must point to writable storage for one libretro AV-info struct.
#[no_mangle]
pub unsafe extern "C" fn retro_get_system_av_info(info: *mut AvInfo) {
    boundary(|| {
        if let Some(info) = unsafe { info.as_mut() } {
            *info = with_core(|core| Ok(core.av_info()))?;
        }
        Ok(())
    });
}

unsafe fn content_path(info: *const GameInfo) -> Result<Option<PathBuf>> {
    let Some(info) = (unsafe { info.as_ref() }) else {
        return Ok(None);
    };
    ensure!(!info.path.is_null(), "content needs a filesystem path");
    let path = unsafe { CStr::from_ptr(info.path) }.to_str()?;
    ensure!(!path.is_empty(), "content path is empty");
    Ok(Some(path.into()))
}

/// # Safety
/// `info` is NULL for no-content boot or points to a valid libretro game-info
/// struct whose path is a NUL-terminated UTF-8 filesystem path.
#[no_mangle]
pub unsafe extern "C" fn retro_load_game(info: *const GameInfo) -> bool {
    boundary(|| {
        let path = unsafe { content_path(info) }?;
        let system = directory(9).unwrap_or_else(|| PathBuf::from("."));
        let save = directory(31)
            .or_else(|| {
                path.as_ref()
                    .and_then(|path| path.parent().map(Path::to_path_buf))
            })
            .unwrap_or_else(|| system.clone());
        let model = variable(c"copperline_model", "A500");
        let video = variable(c"copperline_video", "PAL");
        let rom = variable(c"copperline_rom", "AROS");
        let protected = variable(c"copperline_write_protect", "disabled") == "enabled";
        let config = core::configuration(&model, &video, rom == "Kickstart", &system)?;
        ensure!(env(10, &mut 1u32), "frontend must support XRGB8888 video");
        let mut core = Core::load(&config, path.as_deref(), save, protected)?;
        RUNTIME.with(|runtime| -> Result<()> {
            let mut runtime = runtime.try_borrow_mut().context("core is busy")?;
            ensure!(
                runtime.core.is_none(),
                "unload existing content before loading another game"
            );
            for (port, device) in runtime.devices.iter().enumerate() {
                if let Some(device) = device {
                    core.controls.devices[port] = *device;
                }
            }
            runtime.av = Some(core.av_info());
            runtime.core = Some(core);
            Ok(())
        })?;
        Ok(true)
    })
}

#[no_mangle]
pub extern "C" fn retro_run() {
    boundary(|| {
        let callbacks = CALLBACKS.get();
        if let Some(poll) = callbacks.poll {
            unsafe {
                poll();
            }
        }
        // Input callbacks execute before borrowing the machine. A frontend
        // may query disk state from a callback without aliasing the emulator.
        let mut keyboard = [0i16; 323];
        let mut mice = [[0i16; 7]; 2];
        let mut pads = [[0i16; 9]; 2];
        if let Some(input) = callbacks.input {
            for (key, held) in keyboard.iter_mut().enumerate() {
                *held = unsafe { input(0, KEYBOARD, 0, key as u32) };
            }
            for port in 0..2 {
                for (id, held) in mice[port].iter_mut().enumerate() {
                    *held = unsafe { input(port as u32, MOUSE, 0, id as u32) };
                }
                for (id, held) in pads[port].iter_mut().enumerate() {
                    *held = unsafe { input(port as u32, JOYPAD, 0, id as u32) };
                }
            }
        }
        let (av, pixels, audio) = with_core(|core| {
            core.controls
                .poll(&mut core.emu, |port, device, id| match device {
                    KEYBOARD => keyboard[id as usize],
                    MOUSE => mice[port as usize][id as usize],
                    _ => pads[port as usize][id as usize],
                });
            core.advance()?;
            Ok((
                core.av_info(),
                std::mem::take(&mut core.pixels),
                std::mem::take(&mut *core.audio.borrow_mut()),
            ))
        })?;
        let previous = RUNTIME.with(|runtime| runtime.borrow_mut().av.replace(av));
        if previous.is_some_and(|previous| previous.timing != av.timing) {
            env(32, &mut { av });
        } else if previous.is_some_and(|previous| previous.geometry != av.geometry) {
            env(37, &mut { av.geometry });
        }
        if let Some(video) = callbacks.video {
            if !pixels.is_empty() {
                unsafe {
                    video(
                        pixels.as_ptr().cast(),
                        av.geometry.base_width,
                        av.geometry.base_height,
                        av.geometry.base_width as usize * 4,
                    );
                }
            }
        }
        let mut consumed = audio.len() / 2;
        if let Some(batch) = callbacks.batch {
            if !audio.is_empty() {
                consumed = unsafe { batch(audio.as_ptr(), audio.len() / 2) }.min(consumed);
            }
        } else if let Some(sample) = callbacks.audio {
            for pair in audio.as_chunks::<2>().0 {
                unsafe {
                    sample(pair[0], pair[1]);
                }
            }
        }
        with_core(|core| {
            core.pixels = pixels;
            // Preserve a partial batch for the next callback. A stalled
            // frontend gets at most one second of backlog, as with a bounded
            // live audio device, instead of growing the session indefinitely.
            let pending = &audio[consumed * 2..];
            let keep = pending
                .len()
                .min(2 * copperline::audio::MIX_SAMPLE_RATE as usize);
            core.audio
                .borrow_mut()
                .extend_from_slice(&pending[pending.len() - keep..]);
            Ok(())
        })?;
        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn retro_reset() {
    boundary(|| {
        with_core(|core| {
            core.emu.power_on_reset()?;
            core.audio.borrow_mut().clear();
            core.controls.keys.fill(false);
            core.controls.pending = [[0; 2]; 2];
            core.presentation.reset();
            core.pixels.clear();
            Ok(())
        })
    });
}

#[no_mangle]
pub extern "C" fn retro_set_controller_port_device(port: u32, device: u32) {
    boundary(|| {
        ensure!(
            port < 2 && [AUTO, NONE, JOYPAD, MOUSE].contains(&device),
            "unsupported controller"
        );
        RUNTIME.with(|runtime| -> Result<()> {
            let mut runtime = runtime.try_borrow_mut().context("core is busy")?;
            runtime.devices[port as usize] = Some(device);
            if let Some(core) = runtime.core.as_mut() {
                core.controls.devices[port as usize] = device;
                core.controls.pending[port as usize] = [0; 2];
            }
            Ok(())
        })
    });
}

#[no_mangle]
pub extern "C" fn retro_serialize_size() -> usize {
    RUNTIME.with(|runtime| {
        runtime
            .try_borrow()
            .ok()
            .filter(|r| r.core.is_some())
            .map_or(0, |_| STATE_CAPACITY)
    })
}

/// # Safety
/// `data` points to a writable buffer of `size` bytes owned by the frontend.
#[no_mangle]
pub unsafe extern "C" fn retro_serialize(data: *mut c_void, size: usize) -> bool {
    boundary(|| {
        ensure!(
            !data.is_null() && size >= STATE_CAPACITY,
            "save-state buffer is too small"
        );
        with_core(|core| {
            core.serialize(unsafe { std::slice::from_raw_parts_mut(data.cast(), STATE_CAPACITY) })
        })?;
        Ok(true)
    })
}

/// # Safety
/// `data` points to a readable buffer of `size` bytes owned by the frontend.
#[no_mangle]
pub unsafe extern "C" fn retro_unserialize(data: *const c_void, size: usize) -> bool {
    boundary(|| {
        ensure!(
            !data.is_null() && size <= STATE_CAPACITY,
            "invalid save-state buffer"
        );
        with_core(|core| {
            core.unserialize(unsafe { std::slice::from_raw_parts(data.cast(), size) })
        })?;
        Ok(true)
    })
}

#[no_mangle]
pub extern "C" fn retro_get_region() -> u32 {
    boundary(|| {
        with_core(|core| {
            Ok(u32::from(matches!(
                core.emu.bus().agnus.video_standard(),
                copperline::chipset::agnus::VideoStandard::Pal
            )))
        })
    })
}

#[no_mangle]
pub extern "C" fn retro_get_memory_data(_id: u32) -> *mut c_void {
    std::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn retro_get_memory_size(_id: u32) -> usize {
    0
}
#[no_mangle]
pub extern "C" fn retro_cheat_reset() {}
#[no_mangle]
pub extern "C" fn retro_cheat_set(_index: u32, _enabled: bool, _code: *const c_char) {}
#[no_mangle]
pub extern "C" fn retro_load_game_special(
    _kind: u32,
    _info: *const GameInfo,
    _count: usize,
) -> bool {
    false
}

unsafe extern "C" fn set_eject(ejected: bool) -> bool {
    boundary(|| {
        with_core(|core| core.set_ejected(ejected))?;
        Ok(true)
    })
}
unsafe extern "C" fn get_eject() -> bool {
    boundary(|| with_core(|core| Ok(core.ejected)))
}
unsafe extern "C" fn get_index() -> u32 {
    boundary(|| with_core(|core| Ok(core.selected as u32)))
}
unsafe extern "C" fn set_index(index: u32) -> bool {
    boundary(|| {
        with_core(|core| core.select(index as usize))?;
        Ok(true)
    })
}
unsafe extern "C" fn get_count() -> u32 {
    boundary(|| with_core(|core| Ok(core.disks.len() as u32)))
}
unsafe extern "C" fn replace_disk(index: u32, info: *const GameInfo) -> bool {
    boundary(|| {
        let path = unsafe { content_path(info) }?;
        with_core(|core| core.replace(index as usize, path.as_deref()))?;
        Ok(true)
    })
}
unsafe extern "C" fn add_disk() -> bool {
    boundary(|| {
        with_core(Core::add)?;
        Ok(true)
    })
}
static DISKS: DiskControl = DiskControl {
    set_eject_state: set_eject,
    get_eject_state: get_eject,
    get_image_index: get_index,
    set_image_index: set_index,
    get_num_images: get_count,
    replace_image_index: replace_disk,
    add_image_index: add_disk,
};

#[cfg(test)]
mod tests;
