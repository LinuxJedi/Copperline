// SPDX-License-Identifier: GPL-3.0-or-later

//! Removable media: floppy and CD insertion/ejection, dropped files.

use super::*;

impl App {
    /// Pick one or more disk images for a drive. The selection replaces
    /// the drive's swap playlist; the first image is inserted right away
    /// and the rest are queued for the swap button / shortcut.
    pub(super) fn load_drive_disks_from_dialog(&mut self, drive_idx: usize) {
        if let Some(session) = &self.netplay {
            if !session.can_change_disk() || self.netplay_disk_picker.is_some() {
                self.show_osd("Only the host can change disks; wait for the session to be ready");
                return;
            }
            // Keep servicing the peer while the native picker is open.
            self.netplay_disk_picker = Some((
                drive_idx,
                Box::pin(
                    rfd::AsyncFileDialog::new()
                        .set_title(format!("Netplay: load DF{drive_idx} disk image(s)"))
                        .add_filter("Amiga disk images", crate::floppy::IMAGE_EXTENSIONS)
                        .pick_files(),
                ),
            ));
            return;
        }
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title(format!("Load DF{drive_idx} disk image(s)"))
            .add_filter("Amiga disk images", crate::floppy::IMAGE_EXTENSIONS)
            .pick_files();

        // The modal file dialog blocks this (the main/emulation) thread, so
        // wall-clock time advanced while emulated time stood still. Re-baseline
        // the pacing anchor whether or not a file was chosen, otherwise the
        // pacer would fast-forward to catch up and corrupt pacing for the
        // freshly inserted disk. insert_disk_image -> bus floppy
        // insert_disk_image already asserts the disk-change/eject signal.
        if let Some(paths) = picked {
            self.insert_disk_playlist(drive_idx, paths);
        }
        self.finish_host_io_pause();
    }

    /// Replace a drive's swap playlist with `paths` and insert the first
    /// image, with the standard OSD. Shared by the load dialog and window
    /// drops.
    pub(super) fn insert_disk_playlist(&mut self, drive_idx: usize, paths: Vec<PathBuf>) {
        let Some(path) = paths.first().cloned() else {
            return;
        };
        let count = paths.len();
        if self.netplay.is_none() {
            self.disk_playlists[drive_idx] = paths.clone();
            self.disk_playlist_index[drive_idx] = 0;
        }
        let name = display_file_name(&path);
        if self.insert_disk_image(drive_idx, path, self.disk_write_protected[drive_idx]) {
            self.disk_playlists[drive_idx] = paths;
            self.disk_playlist_index[drive_idx] = 0;
            if count > 1 {
                self.show_osd(format!("DF{drive_idx}: {name} (1/{count})"));
            } else {
                self.show_osd(format!("DF{drive_idx}: {name}"));
            }
        } else {
            self.show_osd(format!("DF{drive_idx}: load failed (see log)"));
        }
    }

    /// Advance the disk-swap playlist of the first drive that has more
    /// than one image queued (the disk-swap shortcut). With no multi-disk
    /// drive, just shows a notice.
    pub(super) fn cycle_disk(&mut self) {
        let Some(drive) =
            (0..self.disk_playlists.len()).find(|&idx| self.disk_playlists[idx].len() > 1)
        else {
            self.show_osd("No alternate disk configured");
            return;
        };
        self.swap_drive_disk(drive);
    }

    /// Insert the next disk in a drive's swap playlist, wrapping around,
    /// and flash the new filename on screen.
    pub(super) fn swap_drive_disk(&mut self, drive_idx: usize) {
        let count = self.disk_playlists[drive_idx].len();
        if count < 2 {
            self.show_osd(format!("DF{drive_idx}: no other disk queued"));
            return;
        }
        let next = (self.disk_playlist_index[drive_idx] + 1) % count;
        let path = self.disk_playlists[drive_idx][next].clone();
        let write_protected = self.disk_write_protected[drive_idx];
        if self.netplay.is_none() {
            self.disk_playlist_index[drive_idx] = next;
        }
        let name = display_file_name(&path);
        if self.insert_disk_image(drive_idx, path, write_protected) {
            self.disk_playlist_index[drive_idx] = next;
            self.show_osd(format!("DF{drive_idx}: {name} ({}/{count})", next + 1));
        } else {
            self.show_osd(format!("DF{drive_idx}: swap failed (see log)"));
        }
    }

    pub(super) fn eject_drive_disk(&mut self, drive_idx: usize) {
        if let Some(session) = &mut self.netplay {
            if let Err(error) = session.change_disk(&self.emu, drive_idx, Vec::new(), false) {
                self.show_osd(format!("DF{drive_idx}: {error}"));
            }
            return;
        }
        if !self.emu.bus().floppy.disk_inserted(drive_idx) {
            self.show_osd(format!("DF{drive_idx}: no disk"));
            return;
        }
        match self.emu.bus_mut().floppy.eject_disk_image(drive_idx) {
            Ok(()) => {
                info!("floppy.df{drive_idx} ejected");
                self.show_osd(format!("DF{drive_idx}: ejected"));
                self.request_redraw();
            }
            Err(e) => warn!("floppy.df{drive_idx} eject failed: {e:#}"),
        }
    }

    /// Pick a CD image and mount it with the media-change notification,
    /// ejecting any current disc first.
    pub(super) fn load_cd_from_dialog(&mut self) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Load CD image")
            .add_filter("CD images", &["cue", "iso", "nrg", "chd"])
            .pick_file();

        // Re-baseline pacing after the modal dialog, as for floppies.
        if let Some(path) = picked {
            self.insert_cd_image_from_path(&path);
        }
        self.finish_host_io_pause();
    }

    /// Mount a CD image with the media-change notification, ejecting any
    /// current disc first. Shared by the load dialog, window drops, and
    /// scheduled `--insert-cd-after` events.
    pub(super) fn insert_cd_image_from_path(&mut self, path: &std::path::Path) {
        match crate::cdrom::CdImage::load(path) {
            Ok(image) => {
                info!("cd image: {} ({})", path.display(), image.describe());
                self.emu.bus_mut().cd_insert_disc(image, path);
                self.show_osd(format!("CD: {}", display_file_name(path)));
                self.request_redraw();
            }
            Err(e) => {
                warn!("cd image load failed ({}): {e:#}", path.display());
                self.show_osd("CD: load failed (see log)");
            }
        }
    }

    pub(super) fn eject_cd(&mut self) {
        if !self.emu.bus().cd_disc_inserted() {
            self.show_osd("CD: no disc");
            return;
        }
        self.emu.bus_mut().cd_eject_disc();
        self.show_osd("CD: ejected");
        self.request_redraw();
    }

    /// Route files dropped on the window: floppy images to a drive
    /// (directly, or via the chooser panel when several drives could take
    /// them), a CD image (cue/iso/nrg/chd) to the CD drive, and everything else to
    /// an explanatory notice. winit reports drops with no cursor position,
    /// so the target drive can only be picked after the fact.
    pub(super) fn handle_dropped_files(&mut self, files: Vec<PathBuf>) {
        if self.netplay.is_some() {
            if !self.netplay.as_ref().is_some_and(|s| s.can_change_disk()) {
                self.show_osd("Only the host can change disks; wait for the session to be ready");
                return;
            }
            if files
                .iter()
                .any(|p| classify_dropped_media(p) != DroppedMediaKind::Floppy)
            {
                self.show_osd("During netplay, drop replacement floppy images only");
                return;
            }
            if let Some(drive) = (0..4).find(|&drive| self.emu.bus().floppy.drive_connected(drive))
            {
                self.insert_disk_playlist(drive, files);
            } else {
                self.show_osd("No floppy drive is connected");
            }
            return;
        }
        // The configuration screen runs on a placeholder machine: an insert
        // would target hardware the launcher is about to rebuild, and the
        // chooser would replace the launcher panel and its unsaved state.
        // A WHDLoad package is configuration rather than media, so it lands
        // in the setup's Game field exactly as the WHDLoad page's Browse
        // would.
        if matches!(self.ui.panel, Some(Panel::Launcher(_))) {
            let mut refused = false;
            for path in files {
                if classify_dropped_media(&path) == DroppedMediaKind::WhdloadGame {
                    let path = whdload_game_config_path(path);
                    let name = display_file_name(&path);
                    if let Some(state) = self.launcher_state_mut() {
                        state.edit_cancel();
                        state.setup.set_path(LauncherField::WhdloadGame, path);
                        state.status = Some(StatusMessage::ok(format!("WHDLoad game: {name}")));
                    }
                } else {
                    refused = true;
                }
            }
            if refused {
                self.show_osd("Close the machine screen to drop disks");
            }
            return;
        }
        let mut floppies: Vec<PathBuf> = Vec::new();
        let mut cd: Option<PathBuf> = None;
        let mut whdload: Option<PathBuf> = None;
        let mut notice: Option<&'static str> = None;
        for path in files {
            match classify_dropped_media(&path) {
                DroppedMediaKind::Floppy => floppies.push(path),
                // One disc tray; the first CD image wins.
                DroppedMediaKind::Cd => cd = cd.or(Some(path)),
                // One machine to reboot; the first game wins.
                DroppedMediaKind::WhdloadGame => whdload = whdload.or(Some(path)),
                DroppedMediaKind::HardDisk => {
                    notice = Some("Hard disks are configured in the machine screen");
                }
                DroppedMediaKind::Rom => {
                    notice = Some("Kickstart ROMs are configured in the machine screen");
                }
            }
        }
        let mut handled = false;
        if let Some(path) = whdload {
            self.boot_whdload_game(whdload_game_config_path(path));
            handled = true;
        }
        if let Some(path) = cd {
            if self.emu.bus().cd_drive_present() {
                self.insert_cd_image_from_path(&path);
            } else {
                self.show_osd("No CD drive on this machine");
            }
            handled = true;
        }
        if !floppies.is_empty() {
            let connected: Vec<usize> = (0..4)
                .filter(|&idx| self.emu.bus().floppy.drive_connected(idx))
                .collect();
            match connected.len() {
                0 => self.show_osd("No floppy drive connected"),
                1 => self.insert_disk_playlist(connected[0], floppies),
                _ => {
                    // The chooser takes the panel slot; an open menu or an
                    // informational panel (About, Shortcuts...) yields to it.
                    self.ui.menu_open = false;
                    if self.ui.panel.is_some() {
                        self.close_panel();
                    }
                    self.open_drop_chooser(floppies, connected);
                }
            }
            handled = true;
        }
        if !handled {
            if let Some(text) = notice {
                self.show_osd(text);
            }
        }
    }

    /// Reboot into a dropped WHDLoad package: stage it against the session's
    /// own configuration (explicit machine, ROM, and memory choices there
    /// still win over the WHDLoad derivation, exactly as on the command
    /// line) and swap the running machine for the staged one, as the
    /// configuration screen's Run does. The dropped game lands in
    /// `[whdload] game` on the remembered config, so a reopened
    /// configuration screen (and a save) carries it, while the derived
    /// machine and mounts stay out of it.
    pub(super) fn boot_whdload_game(&mut self, game: PathBuf) {
        let mut raw = self.machine_config.clone();
        raw.whdload.game = Some(game.to_string_lossy().into_owned());
        let name = display_file_name(&game);
        match self.stage_and_run(raw) {
            Ok(()) => self.show_osd(format!("WHDLoad: {name}")),
            Err(e) => {
                warn!("whdload boot failed ({}): {e:#}", game.display());
                self.show_osd(format!("WHDLoad failed: {}", short_status_error(&e)));
            }
        }
    }

    /// Open the modal drive chooser for dropped floppy images. Drive labels
    /// are snapshotted now; the panel is modal, so they cannot go stale
    /// under it.
    pub(super) fn open_drop_chooser(&mut self, disks: Vec<PathBuf>, connected: Vec<usize>) {
        let floppy = &self.emu.bus().floppy;
        let drives = connected
            .into_iter()
            .map(|drive| {
                let label = match floppy.inserted_disk_name(drive) {
                    Some(name) => format!("DF{drive}: {name}"),
                    None => format!("DF{drive} (empty)"),
                };
                ui::DropDriveEntry { drive, label }
            })
            .collect();
        let disk_label = display_file_name(&disks[0]);
        self.ui.panel = Some(Panel::DropChooser(ui::DropChooserState {
            disks,
            disk_label,
            drives,
        }));
        self.request_redraw();
    }

    /// Chooser click or digit key: insert the pending dropped disks into
    /// the picked drive and close the panel.
    pub(super) fn drop_chooser_route(&mut self, drive_idx: usize) {
        let state = match self.ui.panel.take() {
            Some(Panel::DropChooser(state)) => state,
            other => {
                self.ui.panel = other;
                return;
            }
        };
        self.insert_disk_playlist(drive_idx, state.disks);
        self.request_redraw();
    }

    pub(super) fn insert_disk_image(
        &mut self,
        drive_idx: usize,
        path: PathBuf,
        write_protected: bool,
    ) -> bool {
        if let Some(session) = &mut self.netplay {
            let result = (|| -> anyhow::Result<()> {
                anyhow::ensure!(
                    session.can_change_disk(),
                    "only the host can change disks; wait for the session to be ready"
                );
                use std::io::Read;
                let mut bytes = Vec::new();
                std::fs::File::open(&path)?
                    .take(16 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                anyhow::ensure!(!bytes.is_empty(), "the disk image is empty");
                session.change_disk(&self.emu, drive_idx, bytes, !write_protected)
            })();
            if let Err(error) = result {
                warn!("netplay disk change: {error:#}");
                self.show_osd(format!("DF{drive_idx}: {error}"));
                return false;
            }
            self.show_osd(format!(
                "Sharing replacement DF{drive_idx} with the other player..."
            ));
            return true;
        }
        self.suspend_live_audio_for_host_io();
        let result = match self.emu.bus_mut().floppy.insert_disk_image(
            drive_idx,
            path.clone(),
            write_protected,
        ) {
            Ok(()) => {
                self.last_fdd_track = None;
                info!("floppy.df{} inserted {}", drive_idx, path.display());
                if let Some(rec) = self.input_recorder.as_mut() {
                    rec.record_disk_insert(drive_idx, &path, self.emu.bus().emulated_seconds());
                }
                // Reverse-debug: mark the media change so replay across it warns
                // (the inserted image is host-file state, not in the log).
                self.emu
                    .tt_note_input(crate::inputsched::ReplayAction::DiskChange);
                self.request_redraw();
                true
            }
            Err(e) => {
                warn!(
                    "floppy.df{} insert failed ({}): {e:#}",
                    drive_idx,
                    path.display()
                );
                false
            }
        };
        self.finish_host_io_pause();
        result
    }
}
