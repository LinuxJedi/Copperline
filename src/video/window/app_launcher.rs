// SPDX-License-Identifier: GPL-3.0-or-later

//! Launcher/library glue: configuration screen, browsing, dialogs, jobs, machine build and run.

use super::*;

impl App {
    /// Open the machine-configuration screen, seeded from the running (or
    /// last-applied) machine so it reflects the current settings.
    pub fn open_launcher(&mut self) {
        self.ui.menu_open = false;
        let mut state = LauncherState::from_raw(&self.machine_config);
        if let Some(setup) = &self.netplay_setup {
            state.netplay = setup.clone();
        }
        // A machine set up with a real disk names it on the Storage page, and
        // naming it properly means knowing what is on it. Looking is otherwise
        // put off until the Host Disk page opens, so a launcher that never
        // goes there never touches the host's disks -- but a configuration
        // already naming one has spent that cost, and without this the same
        // disk reads by its bare device name here and by its volume there.
        if !state.setup.host_disks_attached().is_empty() {
            state.setup.refresh_host_disks();
        }
        self.ui.panel = Some(Panel::Launcher(Box::new(state)));
        // Every open starts on System, wherever the focus was standing
        // before -- on the status bar, or on the page this one replaced.
        // The first page is where the eye starts, so it is where the
        // first arrow key should find the marker.
        self.nav.park(self.nav_home());
        self.request_redraw();
    }

    pub(super) fn launcher_state(&self) -> Option<&LauncherState> {
        match self.ui.panel.as_ref() {
            Some(Panel::Launcher(state)) => Some(state.as_ref()),
            _ => None,
        }
    }

    pub(super) fn launcher_state_mut(&mut self) -> Option<&mut LauncherState> {
        match self.ui.panel.as_mut() {
            Some(Panel::Launcher(state)) => Some(state.as_mut()),
            _ => None,
        }
    }

    pub(super) fn set_launcher_status(&mut self, status: StatusMessage) {
        if let Some(state) = self.launcher_state_mut() {
            state.status = Some(status);
        }
    }

    /// Open a native file dialog for a configuration-screen path field, seeded
    /// at the field's current directory, and store the picked path.
    pub(super) fn launcher_browse(&mut self, field: LauncherField) {
        // Host FS mounts and the WHDLoad staging directories are a host
        // directory, not an image file, so they get a folder picker seeded
        // at the current directory itself.
        if field.is_filesys_dir_field() || field.is_whdload_dir_field() || field.is_paths_field() {
            self.launcher_browse_folder(field);
            return;
        }
        // The printer capture is a file we create/overwrite, not an existing
        // image to open, so it gets a save dialog seeded with a default name.
        if field == LauncherField::ParallelOutput {
            self.launcher_browse_save(field, "printer.txt");
            return;
        }
        // Beside whatever the field already holds; failing that, the
        // directory kept for that kind of media. A field with a value is
        // the better answer of the two -- somebody editing df1 after df0
        // wants the folder df0 came from, not a fixed one -- so the
        // configured directory is only ever the fallback.
        let start_dir = self
            .launcher_state()
            .and_then(|s| s.setup.path(field))
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .or_else(|| Self::media_start_dir(field));
        self.suspend_live_audio_for_host_io();
        // A hard-drive slot can be a raw image or a host directory (built
        // into an in-memory FFS/OFS volume at open time; see
        // `HardDriveImage`, shared by IdeDrive/ScsiDisk/lide's own open
        // call) -- so its dialog should let the user pick either, not just
        // a file, and should say so rather than inheriting the plain
        // "Select file" title every other field uses. `rfd` only offers a
        // combined file-or-folder picker on macOS; elsewhere this falls
        // back to the file-only dialog (and title) it already had,
        // unchanged.
        let hard_drive_slot = matches!(
            field,
            LauncherField::ScsiUnit0
                | LauncherField::ScsiUnit1
                | LauncherField::ScsiUnit2
                | LauncherField::ScsiUnit3
                | LauncherField::ScsiUnit4
                | LauncherField::ScsiUnit5
                | LauncherField::ScsiUnit6
                | LauncherField::IdeMaster
                | LauncherField::IdeSlave
                | LauncherField::LideDrive0
                | LauncherField::LideDrive1
                | LauncherField::LideDrive2
                | LauncherField::LideDrive3
                | LauncherField::CopperhfUnit0
                | LauncherField::CopperhfUnit1
                | LauncherField::CopperhfUnit2
                | LauncherField::CopperhfUnit3
                | LauncherField::CopperhfUnit4
                | LauncherField::CopperhfUnit5
                | LauncherField::CopperhfUnit6
        );
        let title = if hard_drive_slot && cfg!(target_os = "macos") {
            "Select file or folder"
        } else {
            "Select file"
        };
        let mut dialog = rfd::FileDialog::new().set_title(title);
        dialog = match field {
            LauncherField::Rom
            | LauncherField::ExtendedRom
            | LauncherField::FmvRom
            | LauncherField::ScsiRom
            | LauncherField::ScsiRomOdd
            | LauncherField::LideRom
            | LauncherField::LideRomBank2
            | LauncherField::Mt32ControlRom
            | LauncherField::Mt32PcmRom => {
                // Both cases spelled out: ROM dumps are as often shouted as
                // not, and some hosts match the filter case-sensitively.
                dialog.add_filter("ROM images", &["rom", "ROM", "bin", "BIN"])
            }
            LauncherField::Df0Image
            | LauncherField::Df1Image
            | LauncherField::Df2Image
            | LauncherField::Df3Image => {
                dialog.add_filter("Floppy images", crate::floppy::IMAGE_EXTENSIONS)
            }
            // Only formats CdImage::load takes: a cue sheet, a bare ISO,
            // an NRG, or a CHD (a raw .bin is a cue sheet's payload, not loadable
            // alone).
            LauncherField::CdImage => dialog.add_filter("CD images", &["cue", "iso", "nrg", "chd"]),
            // A WHDLoad package however it arrived: as distributed
            // (`.lha`), zipped, or as a bare `.slave` picked inside an
            // already-extracted one (stored as its directory, which is
            // what the stager mounts). Spelled in both cases like the ROM
            // filters, since the dialog matches exactly.
            LauncherField::WhdloadGame => dialog.add_filter(
                "WHDLoad packages",
                &[
                    "lha", "LHA", "lzh", "LZH", "zip", "ZIP", "slave", "Slave", "slav", "Slav",
                ],
            ),
            LauncherField::Cd32Nvram => dialog.add_filter("NVRAM images", &["bin", "nv", "sav"]),
            #[cfg(feature = "coppersynth")]
            LauncherField::CsynthSoundfont => {
                dialog.add_filter("SoundFonts", &["sf2", "SF2", "zip", "ZIP"])
            }
            // SCSI, IDE, and lide drive slots all take hard disks or CD
            // images (a cue/iso/nrg/chd attaches a CD-ROM drive at that slot,
            // over SCSI or ATAPI as appropriate).
            LauncherField::ScsiUnit0
            | LauncherField::ScsiUnit1
            | LauncherField::ScsiUnit2
            | LauncherField::ScsiUnit3
            | LauncherField::ScsiUnit4
            | LauncherField::ScsiUnit5
            | LauncherField::ScsiUnit6
            | LauncherField::IdeMaster
            | LauncherField::IdeSlave
            | LauncherField::LideDrive0
            | LauncherField::LideDrive1
            | LauncherField::LideDrive2
            | LauncherField::LideDrive3 => dialog
                .add_filter("Hard disk images", &["hdf", "hdz", "img", "bin"])
                .add_filter("CD images", &["cue", "iso", "nrg", "chd"]),
            // copperhf.device serves hard disks only -- no ATAPI/SCSI-CDROM
            // emulation behind it (`copperhf_drive_image` rejects a CD
            // extension) -- so its units get no CD filter, unlike the slots
            // above.
            LauncherField::CopperhfUnit0
            | LauncherField::CopperhfUnit1
            | LauncherField::CopperhfUnit2
            | LauncherField::CopperhfUnit3
            | LauncherField::CopperhfUnit4
            | LauncherField::CopperhfUnit5
            | LauncherField::CopperhfUnit6 => {
                dialog.add_filter("Hard disk images", &["hdf", "hdz", "img", "bin"])
            }
            _ => dialog.add_filter("Hard disk images", &["hdf", "hdz", "img", "bin"]),
        };
        if let Some(dir) = start_dir {
            dialog = dialog.set_directory(dir);
        }
        #[cfg(target_os = "macos")]
        let picked = if hard_drive_slot {
            dialog.pick_file_or_folder()
        } else {
            dialog.pick_file()
        };
        #[cfg(not(target_os = "macos"))]
        let picked = {
            let _ = hard_drive_slot;
            dialog.pick_file()
        };
        if let Some(mut path) = picked {
            if field == LauncherField::WhdloadGame {
                path = whdload_game_config_path(path);
            }
            if let Some(state) = self.launcher_state_mut() {
                // A pending volume-name edit (on this or another drive row)
                // would otherwise be left visually focused after the dialog.
                state.edit_cancel();
                state.setup.set_path(field, path);
                state.status = None;
            }
        }
        self.finish_host_io_pause();
    }

    /// The directory a dialog opens at when its field is empty, by what the
    /// field holds. The same grouping the filters use just below, so a field
    /// cannot be offered ROM filters and opened in the floppy folder.
    ///
    /// `None` throughout when the directory has not been made -- `paths` only
    /// answers for one that exists -- which is what keeps this opt-in.
    pub(super) fn media_start_dir(field: LauncherField) -> Option<std::path::PathBuf> {
        match field {
            LauncherField::Mt32ControlRom | LauncherField::Mt32PcmRom => {
                crate::paths::mt32_roms_dir().or_else(crate::paths::roms_dir)
            }
            LauncherField::Rom
            | LauncherField::ExtendedRom
            | LauncherField::ScsiRom
            | LauncherField::ScsiRomOdd => crate::paths::roms_dir(),
            LauncherField::Df0Image
            | LauncherField::Df1Image
            | LauncherField::Df2Image
            | LauncherField::Df3Image => crate::paths::floppies_dir(),
            LauncherField::CdImage => crate::paths::cds_dir(),
            // A SCSI unit takes either, and the hard-disk folder is the more
            // likely of the two; a CD there is the exception.
            LauncherField::ScsiUnit0
            | LauncherField::ScsiUnit1
            | LauncherField::ScsiUnit2
            | LauncherField::ScsiUnit3
            | LauncherField::ScsiUnit4
            | LauncherField::ScsiUnit5
            | LauncherField::ScsiUnit6
            | LauncherField::CopperhfUnit0
            | LauncherField::CopperhfUnit1
            | LauncherField::CopperhfUnit2
            | LauncherField::CopperhfUnit3
            | LauncherField::CopperhfUnit4
            | LauncherField::CopperhfUnit5
            | LauncherField::CopperhfUnit6 => crate::paths::harddrives_dir(),
            // The WHDLoad game folder and the NVRAM image have homes of their
            // own that the launcher already knows; nothing to add here.
            LauncherField::WhdloadGame | LauncherField::Cd32Nvram => None,
            _ => crate::paths::harddrives_dir(),
        }
    }

    /// Folder picker for a Host FS mount's directory field.
    pub(super) fn launcher_browse_folder(&mut self, field: LauncherField) {
        // A Paths row opens where it points now, resolved: its stored value
        // may be relative to the base, which is not somewhere a dialog can
        // be pointed at.
        let start_dir = self
            .launcher_state()
            .and_then(|s| {
                s.setup
                    .paths_resolved(field)
                    .or_else(|| s.setup.path(field).map(std::path::Path::to_path_buf))
            })
            .or_else(crate::paths::harddrives_dir);
        self.suspend_live_audio_for_host_io();
        let mut dialog = rfd::FileDialog::new().set_title("Select host directory");
        if let Some(dir) = start_dir {
            dialog = dialog.set_directory(dir);
        }
        let picked = dialog.pick_folder();
        if let Some(path) = picked {
            if let Some(state) = self.launcher_state_mut() {
                state.edit_cancel();
                state.setup.set_path(field, path);
                state.status = None;
            }
        }
        self.finish_host_io_pause();
    }

    /// Save-file picker for a path field that names a host file to create or
    /// overwrite (the printer capture), seeded with `default_name` so the dialog
    /// suggests a filename without the user typing one. An existing file can
    /// still be chosen.
    pub(super) fn launcher_browse_save(&mut self, field: LauncherField, default_name: &str) {
        let current = self
            .launcher_state()
            .and_then(|s| s.setup.path(field))
            .map(|p| p.to_path_buf());
        self.suspend_live_audio_for_host_io();
        let mut dialog = rfd::FileDialog::new().set_title("Choose output file");
        // Seed with the existing path's directory and name, else the default.
        match current.as_ref().and_then(|p| p.parent()) {
            Some(dir) if !dir.as_os_str().is_empty() => dialog = dialog.set_directory(dir),
            _ => {}
        }
        let name = current
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(default_name);
        dialog = dialog.set_file_name(name);
        let picked = dialog.save_file();
        if let Some(path) = picked {
            if let Some(state) = self.launcher_state_mut() {
                state.edit_cancel();
                state.setup.set_path(field, path);
                state.status = None;
            }
        }
        self.finish_host_io_pause();
    }

    /// Make a fresh disk image from what the Create Image page is showing.
    ///
    /// The file is chosen first: the save dialog is where a user cancels,
    /// and nothing is written until they have named somewhere to write it.
    /// Fetch a WHDLoad support archive, and point its row at what landed.
    ///
    /// On a worker for the same reason an image write is: the archives are
    /// a megabyte or two over somebody else's link, and a window that stops
    /// answering is read as a hung program.
    #[cfg(feature = "game-library")]
    pub(super) fn whdload_download(&mut self, field: crate::video::launcher::LauncherField) {
        use crate::gamelib::support::Archive;
        let archive = match field {
            crate::video::launcher::LauncherField::WhdloadWhdPackage => Archive::Whdload,
            crate::video::launcher::LauncherField::WhdloadSkickPackage => Archive::Skick,
            _ => return,
        };
        if self.whdload_job.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::gamelib::support::download(archive));
        });
        self.whdload_job = Some(WhdloadDownload { rx, field, archive });
        self.set_launcher_status(crate::video::launcher::StatusMessage::busy(format!(
            "Downloading {}...",
            archive.file_name()
        )));
    }

    /// Collect a finished download and point the row at it.
    #[cfg(feature = "game-library")]
    pub(super) fn poll_whdload_download(&mut self) {
        let Some(job) = &self.whdload_job else { return };
        let landed = match job.rx.try_recv() {
            Ok(landed) => landed,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(crate::gamelib::support::Error::Fetch("stopped".into()))
            }
        };
        let job = self.whdload_job.take().expect("checked above");
        let status = match landed {
            Ok(at) => {
                info!(
                    "whdload: fetched {} to {}",
                    job.archive.file_name(),
                    at.display()
                );
                let name = job.archive.file_name().to_string();
                if let Some(state) = self.launcher_state_mut() {
                    state.setup.set_path(job.field, at);
                }
                crate::video::launcher::StatusMessage::ok(format!("Downloaded {name}"))
            }
            Err(e) => {
                warn!("whdload: {} download failed: {e}", job.archive.file_name());
                crate::video::launcher::StatusMessage::err(e.to_string())
            }
        };
        self.set_launcher_status(status);
        self.request_redraw();
    }

    /// The one button: sign in when signed out, and out when in.
    #[cfg(feature = "game-library")]
    pub(super) fn openretro_login_or_out(&mut self) {
        let signed_in = self
            .launcher_state()
            .is_some_and(|state| state.openretro.is_some());
        if !signed_in {
            if let Some(state) = self.launcher_state_mut() {
                state.edit_commit();
                state.login = Some(crate::video::launcher::LoginDialog::default());
                state.status = None;
            }
            return;
        }
        // Signing out hands the token back rather than merely forgetting
        // it, so the session ends at the service too and not just here.
        // Whatever the dialog was still holding goes with it.
        self.login_close();
        self.login_job = None;
        if let Some(state) = self.launcher_state_mut() {
            if let Some(session) = state.openretro.take() {
                match std::sync::Arc::try_unwrap(session) {
                    // Handing the token back is a request, and a request
                    // to a service that is not answering must not be the
                    // launcher standing still. Nothing waits on it.
                    Ok(session) => {
                        std::thread::spawn(move || session.close());
                    }
                    // A scan still holds it. Dropping our handle is all
                    // that is left to do; the scan gives it back when it
                    // finishes with it.
                    Err(shared) => drop(shared),
                }
            }
        }
        // And a scan running on that session has nothing to sync with.
        self.stop_library_scan();
        self.set_launcher_status(StatusMessage::ok("Logged out of OpenRetro"));
        self.clear_status_in(STATUS_LINGER);
        self.request_redraw();
    }

    /// The dialog being typed into, if one is up.
    #[cfg(feature = "game-library")]
    pub(super) fn launcher_login_mut(
        &mut self,
    ) -> Option<&mut crate::video::launcher::LoginDialog> {
        self.launcher_state_mut()?.login.as_mut()
    }

    #[cfg(feature = "game-library")]
    pub(super) fn launcher_meta_mut(&mut self) -> Option<&mut crate::video::launcher::MetaDialog> {
        self.launcher_state_mut()?.meta.as_mut()
    }

    /// Choose a picture for the selected game, and put it in the cache
    /// under a name of its own.
    ///
    /// PNG only, because a PNG decoder is all Copperline carries. The file
    /// is decoded before it is kept, so what goes into the cache is
    /// something that will draw rather than something that merely has the
    /// right extension.
    #[cfg(feature = "game-library")]
    pub(super) fn meta_choose_art(&mut self) {
        let Some(picked) = rfd::FileDialog::new()
            .set_title("Choose cover art")
            .add_filter("PNG image", &["png"])
            .pick_file()
        else {
            return;
        };
        let config = crate::paths::library_root();
        let Some(state) = self.launcher_state_mut() else {
            return;
        };
        let Some(file) = state.meta.as_ref().map(|meta| meta.file.clone()) else {
            return;
        };
        let png = match std::fs::read(&picked) {
            Ok(png) => png,
            Err(e) => {
                log::warn!("game library: cannot read {}: {e}", picked.display());
                state.status = Some(StatusMessage::err("Could not read that file"));
                return;
            }
        };
        // Brought to the same bound the fetched covers have, so the cache
        // holds one kind of thing and a photograph of a box does not sit
        // in there at several megabytes.
        let Some(png) = crate::gamelib::cover::normalise(&png) else {
            state.status = Some(StatusMessage::err("Cover art must be a PNG image"));
            return;
        };
        // Named after the package rather than after the bytes, so choosing
        // a different picture replaces the old one instead of leaving it
        // in the cache with nothing pointing at it.
        let key = crate::gamelib::Database::art_key(&file);
        let cache = state.setup.library_cache(&config);
        let at =
            crate::gamelib::cover::cover_file(&crate::gamelib::scan::covers_path(&cache), &key);
        if let Err(e) = crate::paths::ensure_parent(&at).and_then(|()| std::fs::write(&at, &png)) {
            log::warn!("game library: cannot write {}: {e}", at.display());
            state.status = Some(StatusMessage::err("Could not save that image"));
            return;
        }
        if let Some(meta) = &mut state.meta {
            meta.art = Some(key.clone());
        }
        // Drop whatever was held under that name, so the new picture is
        // read rather than the one it replaced.
        state.library.covers.forget_one(&key);
        state.status = None;
        self.request_redraw();
    }

    /// Commit the editor and write the store.
    #[cfg(feature = "game-library")]
    pub(super) fn meta_save(&mut self) {
        let config = crate::paths::library_root();
        if let Some(state) = self.launcher_state_mut() {
            state.commit_meta_editor();
            state.save_library_database(&config);
            // The list is rebuilt so a changed name sorts where it belongs.
            state.library.games = Default::default();
            state.refresh_library(&config);
            state.status = Some(StatusMessage::ok("Metadata saved"));
        }
        self.clear_status_in(STATUS_LINGER);
        self.request_redraw();
    }

    /// Put the dialog away, wiping what was typed on the way out rather
    /// than leaving it for the allocator to hand on.
    #[cfg(feature = "game-library")]
    pub(super) fn login_close(&mut self) {
        if let Some(state) = self.launcher_state_mut() {
            if let Some(login) = &mut state.login {
                login.pass.clear();
            }
            state.login = None;
        }
    }

    /// Trade what was typed for a token, on a worker.
    #[cfg(feature = "game-library")]
    pub(super) fn login_submit(&mut self) {
        if self.login_job.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let Some(state) = self.launcher_state_mut() else {
            return;
        };
        let Some(login) = &mut state.login else {
            return;
        };
        if login.user.trim().is_empty() || login.pass.is_empty() {
            state.status = Some(StatusMessage::err("Enter a username and a password"));
            return;
        }
        login.sending = true;
        let user = login.user.trim().to_string();
        // Moved to the worker rather than copied: `Secret` is not `Clone`
        // for exactly this reason, and taking it leaves the dialog holding
        // an empty one.
        let pass = std::mem::take(&mut login.pass);
        state.status = Some(StatusMessage::busy("Logging in to OpenRetro..."));
        std::thread::spawn(move || {
            let opened = crate::gamelib::openretro::Session::open(
                &user,
                &pass,
                crate::gamelib::openretro::DEVICE_ID,
            );
            drop(pass);
            let _ = tx.send(opened);
        });
        self.login_job = Some(LoginJob { rx });
    }

    /// Collect a finished sign-in.
    #[cfg(feature = "game-library")]
    pub(super) fn poll_login(&mut self) {
        let Some(job) = &self.login_job else { return };
        let landed = match job.rx.try_recv() {
            Ok(landed) => landed,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(
                crate::gamelib::openretro::Error::Offline("the request stopped".into()),
            ),
        };
        self.login_job = None;
        let status = match landed {
            Ok(session) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.openretro = Some(std::sync::Arc::new(session));
                }
                StatusMessage::ok("Logged in to OpenRetro")
            }
            Err(e) => {
                // The status bar gets the one thing to do something
                // about; the log gets the whole of it.
                log::warn!("openretro: sign-in failed: {e}");
                StatusMessage::err(match e {
                    crate::gamelib::openretro::Error::Unauthorized => "Wrong username or password",
                    _ => "Could not reach OpenRetro",
                })
            }
        };
        self.login_close();
        self.set_launcher_status(status);
        self.clear_status_in(STATUS_LINGER);
        self.request_redraw();
    }

    /// Re-read the game folder. Cheap: it lists files and reads back what
    /// the last scan resolved, and asks the service nothing.
    #[cfg(feature = "game-library")]
    pub(super) fn library_refresh(&mut self) {
        let config = crate::paths::library_root();
        let Some(state) = self.launcher_state_mut() else {
            return;
        };
        let (found, fresh) = state.rescan_library(&config);
        state.status = Some(match (found, fresh) {
            (0, _) => StatusMessage::err("No packages found in the game library"),
            (found, 0) => StatusMessage::ok(format!("Found {found} games")),
            (found, fresh) => {
                StatusMessage::ok(format!("Found {found} games, {fresh} without metadata"))
            }
        });
        self.clear_status_in(STATUS_LINGER);
        self.request_redraw();
    }

    /// Resolve metadata and art for everything in the game folder.
    #[cfg(feature = "game-library")]
    pub(super) fn library_update_metadata(&mut self) {
        if self.library_scan.is_some() {
            return;
        }
        let config = crate::paths::library_root();
        let Some(state) = self.launcher_state() else {
            return;
        };
        let Some(folder) = state.library_folder() else {
            self.set_launcher_status(StatusMessage::err("No game library set"));
            self.clear_status_in(STATUS_LINGER);
            return;
        };
        let cache = state.setup.library_cache(&config);
        let session = state.openretro.clone();
        // What the last scan worked out about each package, so this one
        // opens only the archives it has not seen.
        let held = state
            .library
            .db
            .known()
            .iter()
            .filter_map(|k| Some((k.file.clone(), k.slave_sha1.clone()?)))
            .collect();
        self.library_scan = Some(crate::gamelib::Scan::start(folder, cache, session, held));
        self.set_launcher_status(StatusMessage::busy("Starting the scan..."));
        self.request_redraw();
    }

    /// The rate the given list's scrolling runs at.
    pub(super) fn scroll_rate_of(
        &mut self,
        list: ScrollList,
    ) -> Option<&mut crate::video::launcher::ScrollRate> {
        let state = self.launcher_state_mut()?;
        Some(match list {
            #[cfg(feature = "game-library")]
            ScrollList::Games => &mut state.library.scroll_rate,
            #[cfg(feature = "game-library")]
            ScrollList::Favourites => &mut state.library.favourite_scroll_rate,
            ScrollList::HostDisks => state.setup.host_disk_scroll_rate(),
        })
    }

    /// Put a list's scrolling back to its first stage, for a press that is
    /// deliberately a new one.
    pub(super) fn reset_scroll_rate(&mut self, control: UiControl) {
        let Some((_, list)) = scroll_arrow_of(control) else {
            return;
        };
        if let Some(rate) = self.scroll_rate_of(list) {
            rate.reset();
        }
    }

    /// Keep a held scroll arrow running.
    ///
    /// The button going down scrolled one row already; this is what happens
    /// if it is not let go. Nothing moves for [`SCROLL_HOLD_DELAY`], so a
    /// deliberate single click stays a single row, and after that a repeat
    /// lands every [`SCROLL_HOLD_EVERY`] -- close enough together that the
    /// list's [`ScrollRate`](crate::video::launcher::ScrollRate) counts them
    /// as one run and works through its stages, exactly as it does for a
    /// held arrow key. Letting go, or sliding off the arrow, stops it.
    pub(super) fn repeat_held_scroll(&mut self) {
        let Some((control, due)) = self.scroll_hold else {
            return;
        };
        let Some((direction, list)) = scroll_arrow_of(control) else {
            return;
        };
        // Sliding off the arrow stops it, the way letting go does: a button
        // that kept firing from under the pointer would be a button you
        // cannot get away from.
        if self.cursor_pos.and_then(|p| self.main_ui_control_at(p)) != Some(control) {
            self.scroll_hold = None;
            return;
        }
        let now = Instant::now();
        if now < due {
            return;
        }
        self.scroll_hold = Some((control, now + SCROLL_HOLD_EVERY));
        let Some(rate) = self.scroll_rate_of(list) else {
            return;
        };
        let rows = direction * rate.rows_for_step(now).max(1) as isize;
        self.activate_ui_control_with_event_loop(scroll_arrow_for(list, rows), None);
        self.request_redraw();
    }

    /// Move a scan along, and take its results as they arrive.
    ///
    /// Every message is acted on rather than only the newest: a poll that
    /// catches up on several at once still carries the one that delivered
    /// the metadata, and dropping it would lose a whole scan's work. Only
    /// the last one's wording reaches the status line, which is the part
    /// that genuinely only wants the newest.
    #[cfg(feature = "game-library")]
    pub(super) fn poll_library_scan(&mut self) {
        use crate::gamelib::Progress;
        let Some(scan) = &self.library_scan else {
            return;
        };
        let said = scan.poll();
        if said.is_empty() {
            return;
        }
        let config = crate::paths::library_root();
        let mut status = None;
        for progress in said {
            let text = progress.message();
            let kind = match progress {
                // The metadata, as soon as it is known: names, years and
                // publishers fill in while the art is still being
                // fetched, and a scan interrupted after this point has
                // still delivered everything but the pictures.
                Progress::Matched { known, .. } => {
                    if let Some(state) = self.launcher_state_mut() {
                        state.library.db.set_known(known);
                        state.save_library_database(&config);
                        // Rebuilt against what was just resolved, so a
                        // changed name sorts where it now belongs.
                        state.library.games = Default::default();
                        state.refresh_library(&config);
                    }
                    StatusKind::Busy
                }
                // Art that has just landed: only the ones that came back
                // empty are looked for again, so nothing already decoded
                // is thrown away and read a second time.
                Progress::Art { .. } => {
                    if let Some(state) = self.launcher_state_mut() {
                        state.library.covers.forget_missing();
                    }
                    StatusKind::Busy
                }
                Progress::Done { complete, .. } => {
                    if let Some(state) = self.launcher_state_mut() {
                        state.library.covers.forget();
                    }
                    self.library_scan = None;
                    match complete {
                        true => StatusKind::Ok,
                        false => StatusKind::Busy,
                    }
                }
                Progress::Failed(_) => {
                    self.library_scan = None;
                    StatusKind::Error
                }
                _ => StatusKind::Busy,
            };
            status = Some(StatusMessage { text, kind });
        }
        if let Some(status) = status {
            let settled = !matches!(status.kind, StatusKind::Busy);
            self.set_launcher_status(status);
            if settled {
                self.clear_status_in(STATUS_LINGER);
            }
        }
        self.request_redraw();
    }

    /// Stop a scan and forget it. Pressing Run is the end of the launcher,
    /// and a worker fetching art for a page nobody is looking at is work
    /// taken from the machine that was just started.
    #[cfg(feature = "game-library")]
    pub(super) fn stop_library_scan(&mut self) {
        if let Some(scan) = self.library_scan.take() {
            scan.stop();
            log::info!("game library: scan stopped");
        }
    }

    /// Blink the caret in whatever is being typed into, and answer whether
    /// there is anything.
    ///
    /// A redraw is asked for when the phase turns over and not otherwise: a
    /// caret that changes twice a second is no reason to repaint sixty
    /// times a second. With nothing being typed into it is left lit, so the
    /// next box to open starts visible rather than starting dark.
    pub(super) fn blink_caret(&mut self) -> bool {
        let typing = self.launcher_state().is_some_and(|state| {
            #[cfg(feature = "game-library")]
            let dialog = state.login.is_some() || state.meta.is_some();
            #[cfg(not(feature = "game-library"))]
            let dialog = false;
            state.editing().is_some() || dialog
        }) || matches!(self.ui.panel, Some(Panel::Console(_)));
        let light = |on: bool, at: Option<Instant>, app: &mut Self| {
            app.caret_flip_at = at;
            if crate::video::caret_lit() != on {
                crate::video::set_caret_lit(on);
                app.request_redraw();
            }
        };
        if !typing {
            light(true, None, self);
            return false;
        }
        let now = Instant::now();
        match self.caret_flip_at {
            // Opening a box starts the phase, lit.
            None => light(true, Some(now + CARET_BLINK), self),
            Some(at) if now >= at => {
                let on = !crate::video::caret_lit();
                light(on, Some(now + CARET_BLINK), self);
            }
            Some(_) => {}
        }
        true
    }

    /// Say what the WHDLoad machine-type setting just became.
    ///
    /// The row cycles between two words, "Auto" and "Copperline", and a
    /// word is not an explanation: the first takes the machine from the
    /// slave's own header, the second from whatever this configuration
    /// describes. The line says which, and takes itself down again -- it
    /// answers a press, rather than warning of something to be dealt with.
    pub(super) fn say_whdload_machine(&mut self, field: crate::video::launcher::LauncherField) {
        use crate::video::launcher::LauncherField as F;
        if field != F::WhdloadMachine {
            return;
        }
        let Some(state) = self.launcher_state_mut() else {
            return;
        };
        let said = match state.setup.whdload_machine() {
            crate::config::WhdloadMachine::Auto => "WHDLoad uses the Slave file machine type...",
            crate::config::WhdloadMachine::Copperline => {
                "WHDLoad uses the Copperline defined machine type..."
            }
        };
        state.status = Some(StatusMessage::ok(said));
        self.clear_status_in(WHDLOAD_MACHINE_LINGER);
        self.request_redraw();
    }

    /// Have the status line clear itself after `linger`.
    pub(super) fn clear_status_in(&mut self, linger: std::time::Duration) {
        self.status_until = Some(std::time::Instant::now() + linger);
    }

    pub(super) fn launcher_create_image(&mut self, field: crate::video::launcher::LauncherField) {
        use crate::video::launcher::LauncherField as F;
        // Whatever is half-typed counts: pressing a button is as much an
        // end to typing as Enter is, and a size typed but not committed
        // would otherwise be silently thrown away.
        if let Some(state) = self.launcher_state_mut() {
            state.edit_commit();
            if state.editing().is_some() {
                // The commit was refused -- an invalid name -- and the
                // status line says so. Nothing further can be trusted.
                return;
            }
        }
        // The geometry editor's two buttons write no file: Save takes the
        // figures as they stand and returns, Auto fills them in from the
        // size so a hand-set geometry can be started over.
        if matches!(field, F::NewGeomSave | F::NewGeomAuto) {
            if let Some(state) = self.launcher_state_mut() {
                if field == F::NewGeomAuto {
                    state.workshop.geometry_from_size();
                } else {
                    state.tab = crate::video::launcher::LauncherTab::CreateHard;
                }
                state.status = None;
            }
            return;
        }
        if self.image_job.is_some() {
            // The status line already says which one, and starting a second
            // would leave the first writing with nothing watching for it.
            return;
        }
        let floppy = field == F::NewFloppyCreate;
        let Some(state) = self.launcher_state() else {
            return;
        };
        let suggested = state.workshop.suggested_name(floppy);
        let spec = if floppy {
            ImageToMake::Floppy(state.workshop.floppy_spec())
        } else {
            ImageToMake::Hard(state.workshop.hard_spec())
        };

        self.suspend_live_audio_for_host_io();
        let (kind, ext) = if floppy {
            ("Amiga floppy image", vec!["adf"])
        } else {
            // The same bytes either way: .hdf is what emulators look for,
            // .img what a card writer expects, so both are offered.
            ("Amiga hard disk image", vec!["hdf", "img"])
        };
        let picked = rfd::FileDialog::new()
            .set_title("Create disk image")
            .add_filter(kind, &ext)
            .set_file_name(&suggested)
            .save_file();
        self.finish_host_io_pause();

        let Some(path) = picked else { return };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let claimed = spec.bytes_on_disk();
        let size = crate::config::format_size(claimed as usize);

        // Only a fully-written image needs its room now; a sparse one takes
        // what it uses, and making a large one on a small drive is a
        // perfectly ordinary thing to do.
        if !spec.is_sparse() {
            if let Some(free) = free_space_for_new_file(&path) {
                if claimed > free {
                    let where_to = path.parent().unwrap_or(&path).display().to_string();
                    warn!(
                        "create disk image {}: needs {claimed} bytes, {free} free",
                        path.display()
                    );
                    self.set_launcher_status(crate::video::launcher::StatusMessage::err(format!(
                        "Not enough free space to create {name} ({size}) -- {} free on {where_to}",
                        crate::config::format_size(free as usize)
                    )));
                    return;
                }
            }
        }

        // Writing gigabytes takes as long as it takes, and doing it on this
        // thread would stop the loop servicing events -- which the host
        // reads as a hung application. It goes to a worker, the panel says
        // what it is waiting for, and `poll_image_job` collects the result.
        let (tx, rx) = std::sync::mpsc::channel();
        let job_path = path.clone();
        std::thread::spawn(move || {
            let made = match &spec {
                ImageToMake::Floppy(spec) => crate::diskimage::create_floppy(&job_path, spec),
                ImageToMake::Hard(spec) => crate::diskimage::create_hard(&job_path, spec),
            };
            let _ = tx.send(made);
        });
        self.image_job = Some(ImageJob {
            rx,
            path,
            name: name.clone(),
        });
        self.set_launcher_status(crate::video::launcher::StatusMessage::busy(format!(
            "Creating {name} ({size})..."
        )));
    }

    /// Collect a finished image write and report it, or leave the job
    /// running. Called once per pass while one is outstanding.
    pub(super) fn poll_image_job(&mut self) {
        let Some(job) = &self.image_job else { return };
        let made = match job.rx.try_recv() {
            Ok(made) => made,
            // Still writing. A disconnected channel means the worker died
            // without sending, which nothing here does, but treating it as
            // "still going" would hang the status line forever.
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(std::io::Error::other(
                "the image writer stopped without saying why",
            )),
        };
        let job = self.image_job.take().expect("checked above");
        let status = match made {
            Ok(made) => {
                info!(
                    "created {} ({} bytes){}",
                    job.path.display(),
                    made.bytes,
                    match made.geometry {
                        Some(g) => format!(", {}/{}/{}", g.cylinders, g.surfaces, g.sectors),
                        None => String::new(),
                    }
                );
                crate::video::launcher::StatusMessage::ok(format!(
                    "Created {} ({})",
                    job.name,
                    crate::config::format_size(made.bytes as usize)
                ))
            }
            Err(e) => {
                warn!("create disk image {}: {e}", job.path.display());
                crate::video::launcher::StatusMessage::err(format!("Could not create: {e}"))
            }
        };
        self.set_launcher_status(status);
        self.request_redraw();
    }

    pub(super) fn launcher_add_zorro(&mut self) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Add Zorro board metadata")
            .add_filter("Board metadata", &["toml"])
            .pick_file();
        if let Some(path) = picked {
            if let Some(state) = self.launcher_state_mut() {
                state.setup.add_zorro(path);
                state.status = None;
            }
        }
        self.finish_host_io_pause();
    }

    /// Pick a file for a plugin board's file-typed config option.
    pub(super) fn launcher_board_browse(&mut self, board: usize, opt: usize) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Choose plugin file")
            .pick_file();
        if let Some(path) = picked {
            if let Some(state) = self.launcher_state_mut() {
                state.edit_cancel();
                state
                    .setup
                    .zorro_option_set(board, opt, path.to_string_lossy().into_owned());
                state.status = None;
            }
        }
        self.finish_host_io_pause();
    }

    pub(super) fn launcher_load(&mut self) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Load configuration")
            .add_filter("Copperline config", &["toml"])
            .pick_file();
        let mut run_at_once = false;
        if let Some(path) = picked {
            match MachineSetup::load_from(&path) {
                Ok(setup) => {
                    if let Some(state) = self.launcher_state_mut() {
                        // `[emulation] auto_launch`: the file asks to run
                        // the moment it is opened, configuration screen
                        // skipped. Loaded first so a failed run leaves the
                        // launcher showing the loaded setup and the error.
                        run_at_once = setup.auto_launch();
                        state.setup = setup;
                        // The page being looked at may not exist under the
                        // loaded machine (the second boot page, emptied).
                        state.tab = state.setup.settle_tab(state.tab);
                        // Re-read host device lists so the loaded setup's pickers
                        // are populated, not stuck on "Default"/"None".
                        state.setup.refresh_host_devices();
                        // The loaded configuration may name a different
                        // library and a different cache. Everything the
                        // page held belongs to the one before it.
                        #[cfg(feature = "game-library")]
                        {
                            state.library = Default::default();
                        }
                        state.status = Some(StatusMessage::ok(format!(
                            "Loaded {}",
                            display_file_name(&path)
                        )));
                    }
                }
                Err(e) => {
                    warn!("config load failed ({}): {e:#}", path.display());
                    self.set_launcher_status(StatusMessage::err(format!(
                        "Load failed: {}",
                        short_status_error(&e)
                    )));
                }
            }
        }
        self.finish_host_io_pause();
        if run_at_once {
            self.run_honors_power_on = true;
            self.launcher_run();
            self.run_honors_power_on = false;
        }
    }

    /// Open or put away the Save menu. Every other click while it is up
    /// arrives here too, which is how clicking off it closes it.
    pub(super) fn launcher_toggle_save_dialog(&mut self) {
        if let Some(state) = self.launcher_state_mut() {
            state.save_dialog = !state.save_dialog;
        }
    }

    /// Save the configuration as the one Copperline starts with.
    ///
    /// The same TOML as Save As, in a fixed place. It replaces whatever
    /// was there without asking: pressing Save default is a statement about
    /// what the default should be now, and the thing it overwrites is a
    /// previous answer to that same question rather than anything a person
    /// would have to go and recreate.
    pub(super) fn launcher_save_default(&mut self) {
        self.launcher_close_save_dialog();
        let Some(text) = self.launcher_toml_for_save() else {
            return;
        };
        let Some(path) = crate::paths::default_config_file() else {
            // No host-data directory at all (no HOME or USERPROFILE), so
            // there is nowhere the next launch would look.
            warn!("no host data directory to save a default configuration in");
            self.set_launcher_status(StatusMessage::err(
                "Failed to set the default config (see log)",
            ));
            return;
        };
        let written = crate::paths::ensure_parent(&path).and_then(|()| std::fs::write(&path, text));
        match written {
            Ok(()) => {
                info!("saved the default configuration to {}", path.display());
                self.set_launcher_status(StatusMessage::ok(
                    "Saved the running config as the default",
                ));
            }
            Err(e) => {
                warn!("default save failed ({}): {e}", path.display());
                self.set_launcher_status(StatusMessage::err(
                    "Failed to set the default config (see log)",
                ));
            }
        }
    }

    /// Ask before deleting the saved default.
    ///
    /// Only when there is one. With no default saved there is nothing to be
    /// sure about, and a dialog that asks anyway is a dialog that teaches
    /// people to dismiss dialogs without reading them.
    pub(super) fn launcher_reset_default(&mut self) {
        self.launcher_close_save_dialog();
        let saved = crate::paths::default_config_file().is_some_and(|path| path.is_file());
        if !saved {
            self.set_launcher_status(StatusMessage::ok("No default config currently set"));
            return;
        }
        if let Some(state) = self.launcher_state_mut() {
            state.confirm_reset = true;
        }
    }

    pub(super) fn launcher_close_confirm(&mut self) {
        if let Some(state) = self.launcher_state_mut() {
            state.confirm_reset = false;
        }
    }

    /// Delete the saved default, so Copperline starts from factory settings
    /// again.
    ///
    /// Nothing else goes with it. What it removes is one file that was
    /// written by one button press, and everything the emulator has
    /// produced -- states, screenshots, NVRAM -- is untouched.
    pub(super) fn launcher_reset_default_confirmed(&mut self) {
        self.launcher_close_confirm();
        let Some(path) = crate::paths::default_config_file() else {
            return;
        };
        if !path.is_file() {
            self.set_launcher_status(StatusMessage::ok("No default config currently set"));
            return;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                info!("removed the default configuration {}", path.display());
                self.set_launcher_status(StatusMessage::ok(
                    "Reset the current default config to factory settings",
                ));
            }
            Err(e) => {
                warn!("default reset failed ({}): {e}", path.display());
                self.set_launcher_status(StatusMessage::err(
                    "Unable to reset the default config (see log)",
                ));
            }
        }
    }

    pub(super) fn launcher_close_save_dialog(&mut self) {
        if let Some(state) = self.launcher_state_mut() {
            state.save_dialog = false;
        }
    }

    /// The configuration as TOML, with a value typed but not committed
    /// folded in first. A value the commit refuses keeps the focus and
    /// blocks the save: writing the file anyway would save the previous
    /// value while the box still shows the rejected one.
    pub(super) fn launcher_toml_for_save(&mut self) -> Option<String> {
        if let Some(state) = self.launcher_state_mut() {
            state.edit_commit();
            if state.editing().is_some() {
                self.request_redraw();
                return None;
            }
        }
        match self.launcher_state().map(|s| s.setup.to_toml()) {
            Some(Ok(text)) => Some(text),
            Some(Err(e)) => {
                self.set_launcher_status(StatusMessage::err(format!(
                    "Save failed: {}",
                    short_status_error(&e)
                )));
                None
            }
            None => None,
        }
    }

    pub(super) fn launcher_save(&mut self) {
        self.launcher_close_save_dialog();
        let Some(toml) = self.launcher_toml_for_save() else {
            return;
        };
        self.suspend_live_audio_for_host_io();
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save configuration")
            .add_filter("Copperline config", &["toml"])
            .set_file_name("machine.toml");
        // Where configurations are kept, which is a better first answer than
        // wherever the last unrelated dialog happened to end up.
        if let Some(dir) = crate::paths::configs_dir() {
            dialog = dialog.set_directory(dir);
        }
        let picked = dialog.save_file();
        if let Some(path) = picked {
            match std::fs::write(&path, toml) {
                Ok(()) => self.set_launcher_status(StatusMessage::ok(format!(
                    "Saved {}",
                    display_file_name(&path)
                ))),
                Err(e) => {
                    warn!("config save failed ({}): {e}", path.display());
                    self.set_launcher_status(StatusMessage::err("Save failed (see log)"));
                }
            }
        }
        self.finish_host_io_pause();
    }

    /// Run the opened configuration at once when it asks for that --
    /// `[emulation] auto_launch` in the file the launcher opened showing.
    /// Called once at startup, after the launcher opens: the screen is
    /// skipped, and if the run fails the launcher is still there under it
    /// with the error on its status line. Command-line launches never come
    /// this way at all.
    ///
    /// An automatic run keeps the file's own `power_on`: nobody pressed
    /// Run, so nobody overrode a configuration that wants to start at the
    /// test screen.
    pub fn auto_launch_if_asked(&mut self) {
        let asked = self.launcher_state().is_some_and(|s| s.setup.auto_launch());
        if asked {
            self.run_honors_power_on = true;
            self.launcher_run();
            self.run_honors_power_on = false;
        }
    }

    /// Build and start the configured machine (the Run button). Validation,
    /// WHDLoad staging, AROS resolution, audio-device and
    /// machine-construction errors all stay in the panel as a status line;
    /// only success swaps the live machine.
    pub(super) fn launcher_run(&mut self) {
        // Capture a name/option typed but not yet committed with Enter. A
        // value the commit refuses keeps the focus and blocks the run,
        // which would otherwise boot against the previous value while the
        // box shows the rejected one.
        if let Some(state) = self.launcher_state_mut() {
            state.edit_commit();
            if state.editing().is_some() {
                self.request_redraw();
                return;
            }
        }
        // Whoever pressed Run is done with the launcher, and a scan still
        // fetching art would be taking host time from the machine that is
        // about to start.
        #[cfg(feature = "game-library")]
        self.stop_library_scan();
        if let Some(state) = self.launcher_state_mut() {
            state.prepare_netplay_machine();
        }
        self.remember_netplay_setup();
        let raw = match self.launcher_state().map(|s| s.setup.to_raw()) {
            Some(raw) => raw,
            None => return,
        };
        let options = match self.launcher_state().unwrap().netplay.connection_options() {
            Ok(options) => options,
            Err(error) => {
                self.set_launcher_status(StatusMessage::err(short_status_error(&error)));
                self.request_redraw();
                return;
            }
        };
        if let Err(e) = self.stage_and_run_with_netplay(raw, options) {
            // The status line is one shortened sentence; the log keeps the
            // whole chain (which names the underlying cause).
            warn!("run failed: {e:#}");
            self.set_launcher_status(StatusMessage::err(short_status_error(&e)));
        }
    }

    /// Stage any configured WHDLoad game, validate the configuration, and
    /// boot it. `raw` is the user's own configuration and is what the
    /// session remembers; the WHDLoad derivation (machine profile, fast
    /// RAM, ROM, the two staged mounts -- whdload::apply_to_raw) happens on
    /// a copy, so it is rebuilt fresh on every boot and a later Save writes
    /// the setup, not the derivation.
    pub(super) fn stage_and_run(&mut self, raw: RawConfig) -> Result<()> {
        self.stage_and_run_with_netplay(raw, None)
    }

    fn stage_and_run_with_netplay(
        &mut self,
        raw: RawConfig,
        options: Option<crate::netplay::ConnectionOptions>,
    ) -> Result<()> {
        if options.is_some() {
            // These endpoints belong to App and survive a machine replacement.
            #[cfg(feature = "control")]
            anyhow::ensure!(
                self.control.is_none(),
                "Netplay requires restarting Copperline without the control endpoint"
            );
            #[cfg(feature = "gdb")]
            anyhow::ensure!(
                self.gdb.is_none(),
                "Netplay requires restarting Copperline without the GDB endpoint"
            );
        }
        let mut staged = raw.clone();
        let (game, opts) = crate::whdload::game_and_options(&staged);
        if let Some(game) = game {
            anyhow::ensure!(
                options.is_none(),
                "Netplay needs a floppy session; clear the WHDLoad game first"
            );
            let prepared = crate::whdload::prepare(&game, &opts)?;
            crate::whdload::apply_to_raw(&mut staged, &prepared);
            info!(
                "whdload: booting {} ({}) from {}, saves persist in {}",
                prepared.slave_rel.display(),
                prepared.slave.name.as_deref().unwrap_or("unnamed slave"),
                game.display(),
                prepared.game_dir.display()
            );
        }
        // The same validation Run has always used: the raw view through the
        // config pipeline (MachineSetup::build_config is exactly this over
        // its own to_raw()).
        let mut cfg = Config::try_from(staged)?;
        if options.is_some() {
            crate::netplay::prepare_config(&mut cfg)?;
        }
        crate::config::resolve_bundled_rom(&mut cfg)?;
        self.build_and_run_machine(&cfg, raw, options)
    }

    /// Build a machine for `cfg` and swap it in (shared by the configuration
    /// screen's Run and the dropped-WHDLoad-game reboot): session audio
    /// sink, real-drive handover, then [`Self::run_machine`]. On failure the
    /// running machine stays as it was; the caller reports the error in its
    /// own place (panel status line or OSD).
    fn build_and_run_machine(
        &mut self,
        cfg: &Config,
        raw: RawConfig,
        options: Option<crate::netplay::ConnectionOptions>,
    ) -> Result<()> {
        // Remember the session's realtime request so later live sink rebuilds
        // (device switch, disconnect recovery) reuse it.
        self.realtime_priority = cfg.emulation.realtime_priority;
        let realtime = crate::priority::requested(self.realtime_priority);
        // The configured Audio output drives the session selection (default
        // device, a named device, or Disabled).
        self.audio_output = crate::audio::AudioOutput::from_config(
            cfg.audio.output_enabled,
            cfg.audio.output_device.as_deref(),
        );
        let audio: Box<dyn AudioSink> =
            crate::audio::open_output_sink(realtime, &self.audio_output)
                .context("Audio init failed")?;
        // Let go of any real floppy drive the outgoing machine holds before
        // building the new one. The interface can only be open once, and the
        // machine being replaced is not dropped until `run_machine` swaps it
        // in -- so without this the new machine tries to open a device its
        // predecessor still owns, and is told it is in use.
        #[cfg(feature = "fluxbridge")]
        self.emu.bus_mut().floppy.release_bridges();
        // This path boots a fresh machine, never a save state, so a real
        // ROM is required here.
        let built = crate::emulator::build_machine(cfg, audio, true, false).and_then(|mut emu| {
            let session = options
                .map(|options| crate::netplay::Session::new(options, &mut emu, cfg))
                .transpose()?;
            Ok((emu, session))
        });
        match built {
            Ok((emu, session)) => {
                self.run_machine(emu, cfg, raw);
                if let Some(session) = session {
                    self.attach_netplay(session);
                }
                Ok(())
            }
            Err(e) => {
                // The machine that is staying put had its drives taken away
                // above; give them back rather than leaving it with empty bays
                // because a different configuration failed to build.
                #[cfg(feature = "fluxbridge")]
                self.attach_configured_bridges();
                Err(e)
            }
        }
    }

    /// Replace the live machine with a freshly built one (configuration screen
    /// Run), refreshing the host-side presentation/runtime state to match and
    /// powering it on. The previous (placeholder or running) machine, and its
    /// audio sink, are dropped here.
    pub(super) fn run_machine(&mut self, emu: Emulator, cfg: &Config, raw: RawConfig) {
        // Anything the on-screen keyboard is holding belongs to the
        // machine being replaced, and has to be handed back while that
        // machine is still here: the new one would otherwise come up with
        // caps drawn latched that its keyboard MCU never heard of.
        self.release_keyboard_panel_holds();
        self.netplay = None;
        self.netplay_input = Default::default();
        self.emu = emu;
        // Any heat map the analyzer pane armed went with the machine that
        // was just replaced, so the pane owns nothing on this one until it
        // arms a map here.
        self.heatmap_armed_by_panel = false;
        // The real machine may bridge serial to MIDI; the config-screen
        // placeholder never does, so recompute now that the machine is live.
        #[cfg(feature = "midi")]
        {
            self.serial_is_midi = self.emu.bus_mut().midi_serial_mut().is_some();
        }
        self.machine_config = raw;
        self.runahead_machine_block = cfg.runahead_machine_block_reason();
        // Re-derive the sampler from the launcher's parallel config and attach it
        // to the fresh machine (the printer attaches inside build_machine, since
        // its byte sink is Send).
        self.sampler = crate::sampler::SamplerRequest::from_config(&cfg.parallel);
        self.attach_session_sampler();
        self.disk_playlists = cfg.floppy_playlists.clone();
        self.disk_write_protected = std::array::from_fn(|i| {
            cfg.floppy.drives[i]
                .as_ref()
                .map(|d| d.write_protected)
                .unwrap_or(true)
        });
        self.disk_playlist_index = [0; 4];
        self.overscan = crate::config::resolve_overscan(cfg.overscan);
        self.tv_centre = cfg.tv_centre;
        self.apply_pixel_aspect(crate::config::resolve_pixel_aspect(cfg.pixel_aspect));
        // The bezel before the scaling: the canvas rule reads both, and
        // adopting them in this order moves the canvas at most once.
        self.apply_bezel_style(crate::config::resolve_bezel(cfg.bezel));
        self.apply_display_scaling(cfg.scaling);
        self.apply_autocrop(cfg.autocrop);
        // Apply the configured start-up window state; the runtime toggles
        // (Cmd+F, Cmd+Shift+F) take over from here. Reuse the toggles so the
        // surface/window resize stays in one place.
        let is_fullscreen = self
            .render
            .as_ref()
            .map(|r| r.window.fullscreen().is_some());
        if is_fullscreen == Some(!cfg.full_screen) {
            self.toggle_fullscreen();
        }
        if crate::video::status_bar_hidden() == cfg.status_bar {
            self.toggle_status_bar();
        }
        self.warp_speed = cfg.emulation.warp_speed;
        // Reset the host joystick source to the new machine's configured
        // start-up mode (a previous live Cmd+J toggle does not carry over).
        self.joystick_input_mode = cfg.joystick_input_mode;
        self.set_mouse_sensitivity(cfg.mouse_sensitivity);
        self.mouse_capture = cfg.mouse_capture;
        self.autofire_hz = cfg.autofire_hz;
        self.run_ahead_frames = cfg
            .emulation
            .run_ahead_frames
            .min(crate::config::RUN_AHEAD_MAX_FRAMES);
        if let (true, Some(reason)) = (self.run_ahead_frames > 0, self.runahead_block_reason()) {
            warn!(
                "run-ahead ({} frames) configured but inactive: {reason}",
                self.run_ahead_frames
            );
        }
        // Rewind history belongs to the machine that recorded it, so the new
        // machine starts a fresh ring under its own config (or none at all).
        self.rewind_budget_mb = cfg.emulation.rewind_budget_mb;
        self.rewind_interval_frames = cfg.emulation.rewind_interval_frames;
        self.rewind_armed = cfg.emulation.rewind;
        if self.rewind_armed {
            self.arm_rewind_ring();
        } else if !self.debugger_wants_time_travel() {
            self.emu.disable_time_travel();
        }
        self.keyboard_joy_held = [keymap::HeldKeys::default(); keymap::MAPPING_COUNT];
        self.about_machine_lines = crate::config::about_machine_lines(cfg);
        // The threaded path picks the new settings up from the next render
        // job; the recreated deinterlacer covers the synchronous fallback.
        self.deinterlace = crate::config::resolve_deinterlace(cfg.deinterlace);
        self.phosphor = crate::config::resolve_phosphor(cfg.phosphor);
        self.deinterlacer = Deinterlacer::with_settings(self.deinterlace, self.phosphor);
        let shader = crate::config::resolve_shader(cfg.shader.clone());
        self.custom_shader_path = match &shader {
            crate::config::ShaderMode::Custom(path) => Some(path.clone()),
            _ => None,
        };
        self.crt_shader_kind = shader.kind();
        // The device already exists here, so a user shader is compiled now
        // rather than in `resumed`; a bad one falls back to no shader. With
        // none configured, the previous machine's pipeline is dropped rather
        // than left loaded for the CRT Shader item to cycle back to.
        let mut shader_error = None;
        if self.crt_shader_kind == crate::config::ShaderKind::Custom {
            if let Err(msg) = self.reload_custom_shader() {
                shader_error = Some(msg);
                self.crt_shader_kind = crate::config::ShaderKind::None;
            }
        } else if let Some(r) = self.render.as_mut() {
            r.crt_shader.clear_custom();
        }
        self.shader_strength = crate::config::resolve_shader_strength(cfg.shader_strength);
        // The style itself was adopted with the display settings above.
        self.bezel_last = last_bezel_style(self.bezel);
        self.bezel_stickers_path =
            crate::config::resolve_bezel_stickers(cfg.bezel_stickers.clone());
        let sticker_error = self.reload_bezel_stickers().err();
        self.perf_overlay = crate::config::resolve_perf_overlay(cfg.perf_overlay);
        self.perf = PerfOverlay::default();
        self.set_tint(crate::config::resolve_tint(cfg.tint));
        crate::video::set_menu_scale(cfg.menu_scale);
        #[cfg(feature = "mt32")]
        {
            crate::video::set_mt32_lcd(cfg.serial.mt32_lcd);
            // The panel belongs to a module that is both fitted and asked
            // for: a machine built without one would otherwise keep the
            // last one's strip, dead and taking up room.
            let fitted = self
                .emu
                .bus_mut()
                .midi_serial_mut()
                .is_some_and(|sink| sink.mt32_selected());
            self.set_mt32_panel_shown(fitted && cfg.serial.mt32_panel);
            self.mt32_panel.reset();
            self.tell_panel_the_rom_version();
            self.report_mt32_fault();
        }
        #[cfg(feature = "coppersynth")]
        {
            // Coppersynth's fascia follows the same rule as the MT-32's:
            // no synth on the new machine, no strip under its display.
            let fitted = self
                .emu
                .bus_mut()
                .midi_serial_mut()
                .is_some_and(|sink| sink.csynth_selected());
            self.set_csynth_panel_shown(fitted && cfg.serial.coppersynth_panel);
        }
        self.ui.menu_open = false;
        self.ui.panel = None;
        // Pressing Run is a statement -- start it -- so the button powers
        // on whatever `power_on` says. An automatic launch pressed nothing:
        // it keeps the configuration's own power state, the same one a
        // command-line start of the same file would have.
        self.powered_on = !self.run_honors_power_on || cfg.emulation.power_on;
        self.cpu_halted = false;
        self.paused = false;
        // The machine's configured warp boot ([emulation] warp_boot /
        // warp_until) starts fresh with the machine: run_machine is the
        // launcher path's power-on, so construct and engage the gate here
        // the way main.rs does for a direct CLI launch. A machine starting
        // powered off keeps the gate armed instead; the power button's
        // first press engages it, exactly as it does for a CLI start.
        self.warp_boot = crate::warpboot::gate_from_settings(
            cfg.emulation.warp_boot,
            cfg.emulation.warp_boot_idle,
            cfg.emulation.warp_until,
        );
        if self.powered_on {
            self.engage_warp_boot();
        }
        self.sync_live_audio_suspension();
        self.reset_render_pipeline();
        // The last overlay set here is the one that gets drawn, so a shader
        // or sticker folder that failed to load has to travel in this
        // message rather than in one of its own.
        let mut notes = Vec::new();
        if let Some(msg) = shader_error {
            notes.push(format!("CRT shader: off, custom failed: {msg}"));
        }
        if let Some(msg) = sticker_error {
            notes.push(format!("stickers: off, {msg}"));
        }
        self.show_osd(if notes.is_empty() {
            "Machine started".to_string()
        } else {
            format!("Machine started ({})", notes.join("; "))
        });
        self.request_redraw();
    }
}
