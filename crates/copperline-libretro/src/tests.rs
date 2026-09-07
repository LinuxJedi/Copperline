// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use copperline::bus::PortDevice;
use sha2::{Digest, Sha256};

#[derive(Default)]
struct Host {
    root: CString,
    model: bool,
    ntsc: bool,
    frames: u32,
    video_frames: u32,
    video: Vec<u32>,
    audio: Vec<i16>,
    geometry: Option<(u32, u32, usize)>,
    messages: Vec<String>,
    av_changes: usize,
}
thread_local! { static HOST: RefCell<Host> = RefCell::new(Host::default()); }

unsafe extern "C" fn environment(command: u32, data: *mut c_void) -> bool {
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        match command {
            9 | 31 => {
                unsafe {
                    *data.cast::<*const c_char>() = host.root.as_ptr();
                }
                true
            }
            10 => unsafe { *data.cast::<u32>() == 1 },
            15 => {
                let variable = unsafe { &mut *data.cast::<Variable>() };
                let key = unsafe { CStr::from_ptr(variable.key) };
                variable.value = match key.to_bytes() {
                    b"copperline_model" if host.model => c"A1200".as_ptr(),
                    b"copperline_model" => c"A500".as_ptr(),
                    b"copperline_video" if host.ntsc => c"NTSC".as_ptr(),
                    b"copperline_video" => c"PAL".as_ptr(),
                    b"copperline_rom" => c"AROS".as_ptr(),
                    _ => c"disabled".as_ptr(),
                };
                true
            }
            6 => {
                let message = unsafe { &*data.cast::<Message>() };
                host.messages.push(
                    unsafe { CStr::from_ptr(message.msg) }
                        .to_string_lossy()
                        .into_owned(),
                );
                true
            }
            32 | 37 => {
                host.av_changes += 1;
                true
            }
            11 | 13 | 16 | 18 | 35 => true,
            _ => false,
        }
    })
}
unsafe extern "C" fn video(data: *const c_void, width: u32, height: u32, pitch: usize) {
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        host.geometry = Some((width, height, pitch));
        host.video_frames += 1;
        host.video = unsafe {
            std::slice::from_raw_parts(data.cast::<u32>(), width as usize * height as usize)
        }
        .to_vec();
    });
}
unsafe extern "C" fn audio(data: *const i16, frames: usize) -> usize {
    HOST.with(|host| {
        host.borrow_mut()
            .audio
            .extend_from_slice(unsafe { std::slice::from_raw_parts(data, frames * 2) })
    });
    frames
}
unsafe extern "C" fn audio_single(left: i16, right: i16) {
    HOST.with(|host| host.borrow_mut().audio.extend([left, right]));
}

unsafe extern "C" fn audio_one_frame(data: *const i16, frames: usize) -> usize {
    if frames == 0 {
        return 0;
    }
    unsafe { audio(data, 1) }
}

#[test]
fn modifier_aliases_keep_the_correct_amiga_key_held() {
    let root = tempfile::tempdir().unwrap();
    let config = core::configuration("A500", "PAL", false, root.path()).unwrap();
    let mut core = Core::load(&config, None, root.path().into(), true).unwrap();
    // libretro RCTRL/LCTRL, LMETA/LSUPER, and RMETA/RSUPER respectively.
    for (aliases, raw) in [([305, 306], 0x63), ([310, 311], 0x66), ([309, 312], 0x67)] {
        for held in [vec![aliases[0]], aliases.to_vec(), vec![aliases[1]], vec![]] {
            core.controls.poll(&mut core.emu, |_, device, key| {
                i16::from(device == KEYBOARD && held.contains(&key))
            });
            let pressed: Vec<_> = core
                .controls
                .keys
                .iter()
                .enumerate()
                .filter_map(|(key, held)| held.then_some(key))
                .collect();
            assert_eq!(pressed, if held.is_empty() { vec![] } else { vec![raw] });
        }
    }
}

#[test]
fn partial_audio_batches_keep_their_unconsumed_samples() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), false, false);
    assert!(load(None));
    with_core(|core| {
        core.audio
            .borrow_mut()
            .extend([100, -100, 200, -200, 300, -300]);
        Ok(())
    })
    .unwrap();
    retro_set_audio_sample_batch(Some(audio_one_frame));
    retro_run();
    let saved = state();
    retro_run();
    retro_run();
    assert_eq!(
        HOST.with(|host| host.borrow().audio.clone()),
        [100, -100, 200, -200, 300, -300]
    );
    assert!(unsafe { retro_unserialize(saved.as_ptr().cast(), saved.len()) });
    HOST.with(|host| host.borrow_mut().audio.clear());
    retro_run();
    retro_run();
    assert_eq!(
        HOST.with(|host| host.borrow().audio.clone()),
        [200, -200, 300, -300]
    );
    retro_deinit();
}

#[test]
fn neutral_video_fields_retain_and_restore_the_aperture() {
    use copperline::video::{present_common as present, present_common::TvApertureFrame, FB_WIDTH};
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), false, false);
    assert!(load(None));
    with_core(|core| {
        core.presentation.resolve_tv_aperture(TvApertureFrame::Full);
        Ok(())
    })
    .unwrap();
    retro_run();
    with_core(|core| {
        assert_eq!(
            present::standard_tv_aperture_frame(
                core.emu.bus().frame_geometry(),
                copperline::video::deinterlace::OUT_HEIGHT,
                &core.emu.bus().frame_render_base(),
            ),
            TvApertureFrame::Neutral(present::TV_PAL_PRESENT_HEIGHT)
        );
        assert!(!core.presentation.uses_standard_aperture());
        assert_eq!(core.width, FB_WIDTH);
        Ok(())
    })
    .unwrap();
    let saved = state();
    retro_reset();
    with_core(|core| {
        assert!(core.presentation.uses_standard_aperture());
        Ok(())
    })
    .unwrap();
    assert!(unsafe { retro_unserialize(saved.as_ptr().cast(), saved.len()) });
    retro_run();
    with_core(|core| {
        assert!(!core.presentation.uses_standard_aperture());
        assert_eq!(core.width, FB_WIDTH);
        Ok(())
    })
    .unwrap();
    retro_deinit();
}

#[test]
fn both_models_fit_a_clock_seeded_in_emulated_time() {
    let root = tempfile::tempdir().unwrap();
    for model in ["A500", "A1200"] {
        let config = core::configuration(model, "PAL", false, root.path()).unwrap();
        let core = Core::load(&config, None, root.path().into(), true).unwrap();
        assert!(core.emu.bus().rtc_present());
        let clock = &core.emu.bus().rtc;
        assert_eq!(clock.current_unix(0.0), 946_684_800);
        assert_eq!(clock.current_unix(2.0), 946_684_802);
    }
}
unsafe extern "C" fn poll() {
    HOST.with(|host| host.borrow_mut().frames += 1);
}
unsafe extern "C" fn input(port: u32, device: u32, _: u32, id: u32) -> i16 {
    HOST.with(|host| scripted(host.borrow().frames, port, device, id))
}
fn scripted(frame: u32, port: u32, device: u32, id: u32) -> i16 {
    match (port, device, id) {
        (0, KEYBOARD, 97) => i16::from((25..30).contains(&frame)),
        (0, MOUSE, 0) if frame == 40 => 12,
        (0, MOUSE, 1) if frame == 40 => -7,
        (0, MOUSE, 2) => i16::from((45..50).contains(&frame)),
        (0, JOYPAD, 0) => i16::from((60..65).contains(&frame)),
        (0, JOYPAD, 7) => i16::from((70..75).contains(&frame)),
        _ => 0,
    }
}

fn setup(root: &Path, model: bool, ntsc: bool) {
    retro_deinit();
    HOST.with(|host| {
        *host.borrow_mut() = Host {
            root: CString::new(root.to_str().unwrap()).unwrap(),
            model,
            ntsc,
            ..Host::default()
        }
    });
    retro_set_environment(Some(environment));
    retro_init();
    retro_set_video_refresh(Some(video));
    retro_set_audio_sample_batch(Some(audio));
    retro_set_input_poll(Some(poll));
    retro_set_input_state(Some(input));
}
fn load(path: Option<&Path>) -> bool {
    let path = path.map(|path| CString::new(path.to_str().unwrap()).unwrap());
    let info = GameInfo {
        path: path.as_ref().map_or(std::ptr::null(), |path| path.as_ptr()),
        data: std::ptr::null(),
        size: 0,
        meta: std::ptr::null(),
    };
    unsafe {
        retro_load_game(if path.is_some() {
            &info
        } else {
            std::ptr::null()
        })
    }
}
fn state() -> Vec<u8> {
    let mut state = vec![0; retro_serialize_size()];
    assert!(unsafe { retro_serialize(state.as_mut_ptr().cast(), state.len()) });
    state
}
fn machine_hash() -> [u8; 32] {
    with_core(|core| Ok(Sha256::digest(core.emu.netplay_snapshot()?).into())).unwrap()
}

fn probe_adf() -> Vec<u8> {
    let mut bytes = vec![0; 901_120];
    bytes[..4].copy_from_slice(b"DOS\0");
    bytes[8..12].copy_from_slice(&880u32.to_be_bytes());
    let words = include_str!("../tests/probe.hex")
        .lines()
        .flat_map(|line| line.split('#').next().unwrap().split_whitespace());
    for (index, word) in words.enumerate() {
        bytes[12 + index * 2..14 + index * 2]
            .copy_from_slice(&u16::from_str_radix(word, 16).unwrap().to_be_bytes());
    }
    let mut sum = 0u64;
    for word in bytes[..1024].as_chunks::<4>().0 {
        sum += u64::from(u32::from_be_bytes(*word));
        sum = (sum & 0xffff_ffff) + (sum >> 32);
    }
    bytes[4..8].copy_from_slice(&(!(sum as u32)).to_be_bytes());
    bytes
}

#[test]
fn abi_matches_headless_execution_and_restored_audio_video() {
    for model in [false, true] {
        for ntsc in [false, true] {
            let root = tempfile::tempdir().unwrap();
            setup(root.path(), model, ntsc);
            let disk = root.path().join("probe.adf");
            std::fs::write(&disk, probe_adf()).unwrap();
            assert!(load(Some(&disk)));
            let config = core::configuration(
                if model { "A1200" } else { "A500" },
                if ntsc { "NTSC" } else { "PAL" },
                false,
                root.path(),
            )
            .unwrap();
            let mut reference =
                Core::load(&config, Some(&disk), root.path().into(), false).unwrap();
            const FRAMES: u32 = 1200;
            for frame in 1..=FRAMES {
                // Independent headless input injection, without the libretro
                // control mapper or callback dispatch.
                if frame == 25 || frame == 30 {
                    reference.emu.bus_mut().enqueue_key_event(0x20, frame == 25);
                }
                let input = &mut reference.emu.bus_mut().input;
                input.set_port_device(0, PortDevice::Mouse);
                if frame == 40 {
                    input.add_mouse_delta(0, 12, -7);
                }
                input.set_mouse_button(0, 0, (45..50).contains(&frame));
                input.set_joystick(
                    1,
                    false,
                    false,
                    false,
                    (70..75).contains(&frame),
                    (60..65).contains(&frame),
                    false,
                );
                reference.emu.step_video_frame().unwrap();
                reference.render();
                retro_run();
            }
            assert_eq!(
                with_core(|core| Ok(core.emu.bus().emulated_frames())).unwrap(),
                u64::from(FRAMES)
            );
            assert_eq!(
                machine_hash(),
                <[u8; 32]>::from(Sha256::digest(reference.emu.netplay_snapshot().unwrap()))
            );
            HOST.with(|host| {
                let host = host.borrow();
                assert_eq!(host.video, reference.pixels);
                assert_eq!(host.audio, *reference.audio.borrow());
                assert!(!host.video.is_empty());
                assert_eq!(host.video_frames, FRAMES);
                assert!(
                    host.audio.iter().any(|sample| *sample != 0),
                    "probe audio is silent"
                );
                assert!(
                    host.video
                        .iter()
                        .copied()
                        .collect::<std::collections::HashSet<_>>()
                        .len()
                        > 256,
                    "probe raster did not start"
                );
                assert!(host.audio.len() > 150_000);
                assert!(host.messages.is_empty(), "{:?}", host.messages);
                let (width, height, pitch) = host.geometry.unwrap();
                assert_eq!(pitch, width as usize * 4);
                assert_eq!(host.video.len(), width as usize * height as usize);
            });
            assert_eq!(retro_get_region(), u32::from(!ntsc));
            let saved = state();
            HOST.with(|host| host.borrow_mut().audio.clear());
            for _ in 0..12 {
                retro_run();
            }
            let hash = machine_hash();
            let (pixels, audio) =
                HOST.with(|host| (host.borrow().video.clone(), host.borrow().audio.clone()));
            assert!(unsafe { retro_unserialize(saved.as_ptr().cast(), saved.len()) });
            HOST.with(|host| {
                host.borrow_mut().audio.clear();
                host.borrow_mut().frames = FRAMES;
            });
            for _ in 0..12 {
                retro_run();
            }
            assert_eq!(machine_hash(), hash);
            HOST.with(|host| {
                assert_eq!(host.borrow().video, pixels);
                assert_eq!(host.borrow().audio, audio);
            });
            assert_eq!(retro_serialize_size(), saved.len());
            retro_deinit();
            assert_eq!(retro_serialize_size(), 0);
        }
    }
}

#[test]
fn playlists_persist_and_restore_inactive_disks_without_touching_sources() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("first.adf");
    let b = root.path().join("second.adf");
    std::fs::write(&a, vec![0; 901_120]).unwrap();
    std::fs::write(&b, vec![1; 901_120]).unwrap();
    let playlist = root.path().join("game.m3u");
    std::fs::write(
        &playlist,
        "\u{feff}#EXTM3U\r\nfirst.adf\r\n# comment\nsecond.adf\n",
    )
    .unwrap();
    setup(root.path(), false, false);
    assert!(load(Some(&playlist)));
    assert_eq!(unsafe { get_count() }, 2);
    let change_disk = |value| {
        with_core(|core| {
            core.emu.bus_mut().floppy.insert_memory_disk_image_bytes(
                0,
                vec![value; 901_120],
                PathBuf::from("test.adf"),
                false,
            )
        })
        .unwrap()
    };
    change_disk(2);
    assert!(unsafe { set_eject(true) });
    assert!(unsafe { set_index(1) });
    assert!(unsafe { set_eject(false) });
    change_disk(3);
    let saved = state();
    change_disk(4);
    assert!(unsafe { set_eject(true) });
    assert!(unsafe { set_index(0) });
    assert!(unsafe { set_eject(false) });
    change_disk(5);
    assert!(unsafe { retro_unserialize(saved.as_ptr().cast(), saved.len()) });
    assert_eq!(unsafe { get_index() }, 1);
    assert!(!unsafe { get_eject() });
    assert!(unsafe { set_eject(true) });
    assert!(unsafe { set_index(0) });
    assert!(unsafe { set_eject(false) });
    with_core(|core| {
        assert!(core
            .emu
            .bus()
            .floppy
            .export_disk_image(0)?
            .iter()
            .all(|byte| *byte == 2));
        Ok(())
    })
    .unwrap();
    retro_unload_game();
    assert!(load(Some(&playlist)));
    with_core(|core| {
        assert!(core.disks[0]
            .as_ref()
            .unwrap()
            .bytes
            .iter()
            .all(|byte| *byte == 2));
        assert!(core.disks[1]
            .as_ref()
            .unwrap()
            .bytes
            .iter()
            .all(|byte| *byte == 3));
        Ok(())
    })
    .unwrap();
    assert!(std::fs::read(a).unwrap().iter().all(|byte| *byte == 0));
    assert!(std::fs::read(b).unwrap().iter().all(|byte| *byte == 1));
    assert!(HOST.with(|host| host.borrow().messages.is_empty()));
    retro_deinit();
}

#[test]
fn pending_mouse_and_controller_choices_survive_states() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), false, false);
    retro_set_controller_port_device(1, MOUSE);
    assert!(load(None));
    with_core(|core| {
        core.controls.poll(&mut core.emu, |port, device, id| {
            if port == 1 && device == MOUSE && id == 0 {
                1000
            } else {
                0
            }
        });
        assert_eq!(core.controls.pending[1], [900, 0]);
        Ok(())
    })
    .unwrap();
    let saved = state();
    retro_set_controller_port_device(1, JOYPAD);
    retro_run();
    assert!(unsafe { retro_unserialize(saved.as_ptr().cast(), saved.len()) });
    with_core(|core| {
        assert_eq!(core.controls.devices[1], MOUSE);
        assert_eq!(core.controls.pending[1], [900, 0]);
        Ok(())
    })
    .unwrap();
    retro_run();
    with_core(|core| {
        assert_eq!(core.controls.pending[1], [800, 0]);
        Ok(())
    })
    .unwrap();
    retro_deinit();
}

#[test]
fn reset_reload_single_sample_audio_and_state_failure_leave_usable_core() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), false, false);
    retro_set_audio_sample_batch(None);
    retro_set_audio_sample(Some(audio_single));
    assert!(load(None));
    for _ in 0..3 {
        retro_run();
    }
    assert!(HOST.with(|host| !host.borrow().audio.is_empty()));
    let saved = state();
    let hash = machine_hash();
    assert!(!unsafe { retro_unserialize(saved.as_ptr().cast(), 16) });
    assert_eq!(hash, machine_hash());
    retro_reset();
    assert_eq!(
        with_core(|core| Ok(core.emu.bus().emulated_frames())).unwrap(),
        0
    );
    retro_run();
    retro_unload_game();
    assert!(load(None));
    retro_run();
    assert_eq!(
        with_core(|core| Ok(core.emu.bus().emulated_frames())).unwrap(),
        1
    );
    retro_deinit();
}

#[test]
fn nominal_refresh_tracks_interlace_and_programmable_totals() {
    use copperline::chipset::agnus::{Agnus, AgnusRevision, VideoStandard};
    let mut agnus =
        Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
    assert_eq!(agnus.nominal_frame_cck(), 313.0 * 227.0);
    agnus.set_lace(true);
    assert_eq!(agnus.nominal_frame_cck(), 312.5 * 227.0);
    agnus.set_lace(false);
    agnus.write_beamcon0(0);
    assert_eq!(agnus.nominal_frame_cck(), 263.0 * 227.5);
    agnus.write_beamcon0(1 << 11); // LOLDIS holds the current line length.
    assert_eq!(
        agnus.nominal_frame_cck(),
        263.0 * f64::from(agnus.current_line_cck())
    );
    agnus.write_beamcon0(0xa0);
    agnus.write_htotal(199);
    agnus.write_vtotal(199);
    agnus.set_lace(true);
    assert_eq!(agnus.nominal_frame_cck(), 40_000.0);
}

#[test]
fn disk_slots_no_disk_selection_and_write_protection() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("disk.adf");
    std::fs::write(&path, vec![0; 901_120]).unwrap();
    let config = core::configuration("A500", "PAL", false, root.path()).unwrap();
    let mut core = Core::load(&config, Some(&path), root.path().into(), true).unwrap();
    assert_eq!(
        core.emu.bus().floppy.disk_image_write_protected(0),
        Some(true)
    );
    core.set_ejected(true).unwrap();
    core.add().unwrap();
    core.replace(1, Some(&path)).unwrap();
    core.select(1).unwrap();
    core.replace(0, None).unwrap();
    assert_eq!(core.disks.len(), 1);
    assert_eq!(core.selected, 0);
    core.select(1).unwrap(); // libretro's out-of-list index means no disk.
    core.set_ejected(false).unwrap();
    assert!(!core.emu.bus().floppy.disk_inserted(0));
    core.persist().unwrap();
    assert!(!root.path().join("copperline").exists());
}

#[test]
fn frontend_is_notified_of_runtime_video_timing_changes() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), true, false);
    assert!(load(None));
    retro_run();
    let before = with_core(|core| Ok(core.av_info())).unwrap();
    let changes = HOST.with(|host| host.borrow().av_changes);
    with_core(|core| {
        core.emu.bus_mut().agnus.write_beamcon0(0);
        Ok(())
    })
    .unwrap();
    retro_run();
    assert_ne!(
        with_core(|core| Ok(core.av_info().timing)).unwrap(),
        before.timing
    );
    assert!(HOST.with(|host| host.borrow().av_changes > changes));
    retro_deinit();
}

#[test]
fn core_lifecycle_fits_a_one_megabyte_frontend_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let root = tempfile::tempdir().unwrap();
            setup(root.path(), true, false);
            assert!(load(None));
            retro_run();
            let saved = state();
            assert!(unsafe { retro_unserialize(saved.as_ptr().cast(), saved.len()) });
            retro_reset();
            retro_run();
            retro_deinit();
            assert!(HOST.with(|host| host.borrow().messages.is_empty()));
        })
        .unwrap()
        .join()
        .unwrap();
}
