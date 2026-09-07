// SPDX-License-Identifier: GPL-3.0-or-later

use crate::abi::{AvInfo, Geometry, Timing};
use crate::input::Controls;
use crate::media::{self, Disk, MAX_DISKS};
use anyhow::{ensure, Context, Result};
use copperline::audio::{AudioSink, MIX_SAMPLE_RATE};
use copperline::chipset::paula::PAULA_CLOCK_HZ;
use copperline::config::{
    machine_profile_defaults, parse_machine_model, parse_video_standard, Config, Overscan,
};
use copperline::emulator::{build_machine, Emulator};
use copperline::serial::NullSerialSink;
use copperline::video::deinterlace::Deinterlacer;
use copperline::video::{
    bitplane, present_common as present, FB_WIDTH, MAX_CANVAS_PIXELS, MAX_VISIBLE_LINES,
};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Fixed for the entire content session, including disk replacement. The
/// envelope bounds media to sixteen standard ADFs and machine state to the
/// remainder. Unused bytes are zeroed and compress well in frontend files.
pub const STATE_CAPACITY: usize = 64 * 1024 * 1024;
const MAGIC: &[u8; 8] = b"CLRETRO1";

pub struct BufferedAudio(pub Rc<RefCell<Vec<i16>>>);
impl AudioSink for BufferedAudio {
    fn push(&mut self, left: f32, right: f32) {
        self.0
            .borrow_mut()
            .extend([left, right].map(|sample| (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16));
    }
    fn flush(&mut self) {}
    fn reset_live_output_after_timeline_jump(&mut self) {
        self.0.borrow_mut().clear();
    }
}

pub struct Core {
    pub emu: Emulator,
    pub audio: Rc<RefCell<Vec<i16>>>,
    pub controls: Controls,
    pub disks: Vec<Option<Disk>>,
    pub selected: usize,
    pub ejected: bool,
    pub save_dir: PathBuf,
    pub write_protected: bool,
    pub pixels: Vec<u32>,
    pub width: usize,
    pub height: usize,
    fb: Vec<u32>,
    deinterlacer: Deinterlacer,
    machine_identity: [u8; 32],
}

pub fn configuration(model: &str, video: &str, kickstart: bool, system: &Path) -> Result<Config> {
    ensure!(
        matches!(model, "A500" | "A1200"),
        "unsupported machine model"
    );
    let mut config = machine_profile_defaults(parse_machine_model(model)?);
    config.video_standard = parse_video_standard(video)?;
    config.rtc_seed_unix = Some(946_684_800);
    config.floppy_connected = [true, false, false, false];
    if kickstart {
        let named = system.join(format!("kickstart-{}.rom", model.to_ascii_lowercase()));
        config.rom_path = if named.is_file() {
            named
        } else {
            system.join("kickstart.rom")
        };
    }
    Ok(config)
}

impl Core {
    pub fn load(
        config: &Config,
        content: Option<&Path>,
        save_dir: PathBuf,
        write_protected: bool,
    ) -> Result<Self> {
        let audio = Rc::new(RefCell::new(Vec::new()));
        let aros = config.rom_path == Path::new(copperline::config::BUNDLED_AROS_ROM);
        let mut emu = build_machine(config, Box::new(BufferedAudio(audio.clone())), false, aros)?;
        emu.bus_mut().paula.serial = Box::new(NullSerialSink);
        if aros {
            emu.reload_rom(
                include_bytes!("../../../assets/aros/aros-amiga-m68k-rom.bin").to_vec(),
                Some(include_bytes!("../../../assets/aros/aros-amiga-m68k-ext.bin").to_vec()),
            )?;
        }
        let machine_identity =
            Sha256::digest(format!("{:?}", emu.machine_descriptor()).as_bytes()).into();
        let disks = match content {
            Some(path) => media::playlist(path)?
                .iter()
                .map(|path| Disk::open(path, &save_dir).map(Some))
                .collect::<Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        let mut core = Self {
            emu,
            audio,
            controls: Controls::default(),
            disks,
            selected: 0,
            ejected: true,
            save_dir,
            write_protected,
            fb: vec![0; MAX_CANVAS_PIXELS],
            pixels: Vec::new(),
            width: present::TV_CAPTURED_WIDTH,
            height: present::TV_GLASS_PRESENT_ROWS,
            deinterlacer: Deinterlacer::with_settings(false, 0.0),
            machine_identity,
        };
        if !core.disks.is_empty() {
            core.set_ejected(false)?;
        }
        Ok(core)
    }

    pub fn av_info(&self) -> AvInfo {
        AvInfo {
            geometry: Geometry {
                base_width: self.width as u32,
                base_height: self.height as u32,
                max_width: (FB_WIDTH * 2) as u32,
                max_height: (MAX_VISIBLE_LINES * 2) as u32,
                aspect_ratio: 4.0 / 3.0,
            },
            timing: Timing {
                fps: f64::from(PAULA_CLOCK_HZ) / self.emu.bus().agnus.nominal_frame_cck(),
                sample_rate: f64::from(MIX_SAMPLE_RATE),
            },
        }
    }

    pub fn advance(&mut self) -> Result<()> {
        self.emu.step_video_frame()?;
        self.render();
        Ok(())
    }

    pub fn render(&mut self) {
        if !self.emu.bus().frame_render_available() {
            return;
        }
        bitplane::render(self.emu.bus_mut(), &mut self.fb);
        let bus = self.emu.bus();
        let geometry = bus.frame_geometry();
        let scale = bus.frame_canvas_scale();
        let base = bus.frame_render_base();
        let placement = present::post_process_rendered_field(
            &mut self.fb,
            geometry,
            scale,
            bus.frame_presentation_h_window(),
            bus.frame_presentation_v_window(),
            bus.frame_visible_start_vpos(),
            0,
            Overscan::Tv,
        );
        let lace = base.bplcon0 & 4 != 0;
        let double_rows = !geometry.programmable;
        let woven_rows = placement.rows * if lace || double_rows { 2 } else { 1 };
        let aperture = present::standard_tv_aperture_frame(geometry, woven_rows, &base);
        let mut latch = present::PresentationLatch::default();
        if let Some(rows) = latch.resolve_tv_aperture(aperture) {
            (self.height, self.width) = self.deinterlacer.present_field_region_into(
                &self.fb,
                placement.rows,
                FB_WIDTH * scale,
                lace,
                base.long_field,
                double_rows,
                present::TV_CAPTURED_SOURCE_X as i32,
                present::TV_PRESENT_SOURCE_Y as i32,
                rows,
                present::TV_CAPTURED_WIDTH,
                rows,
                &mut self.pixels,
            );
        } else {
            (self.height, self.width) = self.deinterlacer.present_field_into(
                &self.fb,
                placement.rows,
                FB_WIDTH * scale,
                lace,
                base.long_field,
                double_rows,
                &mut self.pixels,
            );
        }
        // Renderer stores RGBA bytes in little-endian u32s; libretro XRGB8888 is a
        // native integer 0x00RRGGBB, independent of host byte order.
        for pixel in &mut self.pixels {
            let [r, g, b, _] = pixel.to_le_bytes();
            *pixel = u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b);
        }
    }

    pub fn capture_disk(&mut self) -> Result<()> {
        if !self.ejected {
            if let Some(Some(disk)) = self.disks.get_mut(self.selected) {
                disk.bytes = self.emu.bus().floppy.export_disk_image(0)?;
            }
        }
        Ok(())
    }

    pub fn persist(&mut self) -> Result<()> {
        self.capture_disk()?;
        if !self.write_protected {
            for disk in self.disks.iter_mut().flatten() {
                disk.persist()?;
            }
        }
        Ok(())
    }

    pub fn set_ejected(&mut self, ejected: bool) -> Result<()> {
        if self.ejected == ejected {
            return Ok(());
        }
        if ejected {
            self.persist()?;
            self.emu.bus_mut().floppy.eject_disk_image(0)?;
        } else if let Some(Some(disk)) = self.disks.get(self.selected) {
            self.emu.bus_mut().floppy.insert_memory_disk_image_bytes(
                0,
                disk.bytes.clone(),
                disk.label.clone(),
                self.write_protected,
            )?;
        }
        self.ejected = ejected;
        Ok(())
    }

    pub fn select(&mut self, index: usize) -> Result<()> {
        ensure!(
            self.ejected && index <= self.disks.len(),
            "eject the disk before choosing another image"
        );
        self.selected = index;
        Ok(())
    }

    pub fn replace(&mut self, index: usize, path: Option<&Path>) -> Result<()> {
        ensure!(
            self.ejected && index < self.disks.len(),
            "eject the disk before replacing an image"
        );
        let replacement = path.map(|p| Disk::open(p, &self.save_dir)).transpose()?;
        self.persist()?;
        if let Some(disk) = replacement {
            self.disks[index] = Some(disk);
        } else {
            self.disks.remove(index);
            if self.selected > index {
                self.selected -= 1;
            }
        }
        Ok(())
    }

    pub fn add(&mut self) -> Result<()> {
        ensure!(
            self.ejected && self.disks.len() < MAX_DISKS,
            "eject the disk first; at most {MAX_DISKS} slots are supported"
        );
        self.disks.push(None);
        Ok(())
    }

    fn identity(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(self.machine_identity);
        hash.update([u8::from(self.write_protected)]);
        for disk in &self.disks {
            hash.update([u8::from(disk.is_some())]);
            if let Some(disk) = disk {
                hash.update(disk.source_hash);
            }
        }
        hash.finalize().into()
    }

    pub fn serialize(&mut self, out: &mut [u8]) -> Result<()> {
        ensure!(
            out.len() >= STATE_CAPACITY,
            "save-state buffer is too small"
        );
        self.capture_disk()?;
        let mut body = Vec::new();
        body.extend(self.controls.keys.map(u8::from));
        for port in self.controls.pending {
            for value in port {
                body.extend(value.to_le_bytes());
            }
        }
        for device in self.controls.devices {
            body.extend(device.to_le_bytes());
        }
        body.extend((self.selected as u32).to_le_bytes());
        body.push(u8::from(self.ejected));
        body.push(self.disks.len() as u8);
        for disk in &self.disks {
            put_bytes(&mut body, disk.as_ref().map_or(&[], |disk| &disk.bytes));
        }
        put_bytes(&mut body, &self.emu.save_state_bytes()?);
        let length = 8 + 32 + 4 + 32 + body.len();
        ensure!(
            length <= STATE_CAPACITY,
            "machine state exceeds this core's 64 MiB state capacity"
        );
        out[..STATE_CAPACITY].fill(0);
        out[..8].copy_from_slice(MAGIC);
        out[8..40].copy_from_slice(&self.identity());
        out[40..44].copy_from_slice(&(body.len() as u32).to_le_bytes());
        out[44..76].copy_from_slice(&Sha256::digest(&body));
        out[76..length].copy_from_slice(&body);
        Ok(())
    }

    pub fn unserialize(&mut self, data: &[u8]) -> Result<()> {
        ensure!(
            data.len() >= 76 && &data[..8] == MAGIC,
            "not a Copperline libretro state"
        );
        ensure!(
            data[8..40] == self.identity(),
            "state requires the same machine, ROM, playlist and write-protect option"
        );
        let length = u32::from_le_bytes(data[40..44].try_into()?) as usize;
        ensure!(length <= STATE_CAPACITY - 76, "state exceeds capacity");
        let body = data.get(76..76 + length).context("incomplete state")?;
        ensure!(
            Sha256::digest(body)[..] == data[44..76],
            "state checksum mismatch"
        );
        let mut reader = Reader(body);
        let mut controls = self.controls.clone();
        for key in &mut controls.keys {
            *key = reader.boolean()?;
        }
        for port in &mut controls.pending {
            for value in port {
                *value = i32::from_le_bytes(reader.take(4)?.try_into()?);
            }
        }
        for device in &mut controls.devices {
            *device = reader.number()?;
            ensure!(
                [
                    crate::abi::AUTO,
                    crate::abi::NONE,
                    crate::abi::JOYPAD,
                    crate::abi::MOUSE
                ]
                .contains(device),
                "invalid controller"
            );
        }
        let selected = reader.number()? as usize;
        let ejected = reader.boolean()?;
        ensure!(
            reader.take(1)?[0] as usize == self.disks.len() && selected <= self.disks.len(),
            "state playlist differs"
        );
        let mut disks = Vec::new();
        for disk in &self.disks {
            let bytes = reader.bytes()?;
            if let Some(disk) = disk {
                media::validate_adf(bytes)?;
                ensure!(
                    bytes.len() == disk.bytes.len(),
                    "state disk geometry differs"
                );
            } else {
                ensure!(bytes.is_empty(), "state disk slot differs");
            }
            disks.push(bytes);
        }
        let machine = reader.bytes()?;
        ensure!(reader.0.is_empty(), "unexpected state data");
        let description = copperline::savestate::read_descriptor(machine)?;
        ensure!(
            &description == self.emu.machine_descriptor(),
            "state machine differs"
        );
        self.emu.load_state_bytes(machine)?;
        self.emu.bus_mut().floppy.make_disk_images_memory_backed();
        self.audio.borrow_mut().clear();
        self.controls = controls;
        self.selected = selected;
        self.ejected = ejected;
        for (disk, bytes) in self.disks.iter_mut().zip(disks) {
            if let Some(disk) = disk {
                disk.bytes = bytes.to_vec();
            }
        }
        self.deinterlacer.reset_history();
        self.pixels.clear();
        Ok(())
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend((bytes.len() as u32).to_le_bytes());
    out.extend(bytes);
}
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let bytes = self.0.get(..length).context("incomplete state payload")?;
        self.0 = &self.0[length..];
        Ok(bytes)
    }
    fn number(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn boolean(&mut self) -> Result<bool> {
        let byte = self.take(1)?[0];
        ensure!(byte <= 1, "invalid boolean");
        Ok(byte == 1)
    }
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let length = self.number()? as usize;
        self.take(length)
    }
}
