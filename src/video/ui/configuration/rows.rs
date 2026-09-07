// SPDX-License-Identifier: GPL-3.0-or-later

//! Machine-setting row rendering and hit testing.

use super::*;

pub(in crate::video::ui) fn launcher_row_control_at(
    rect: Rect,
    state: &LauncherState,
    r: &launcher::Row,
    row_y: usize,
    pos: (i32, i32),
) -> Option<UiControl> {
    match r.kind {
        // Non-interactive rows.
        RowKind::SectionHeader | RowKind::BootpriHeader | RowKind::RomInfo => {}
        RowKind::Text => {
            if LauncherState::is_serial_addr(r.field) {
                let (host_box, port_box) = launcher_serial_addr_rects(rect, row_y);
                if host_box.contains(pos) {
                    return Some(UiControl::LauncherSerialHostEdit(r.field));
                }
                if port_box.contains(pos) {
                    return Some(UiControl::LauncherSerialPortEdit(r.field));
                }
            } else if launcher_text_rect(rect, row_y, r.field).contains(pos) {
                // The same widget serves two stores: a Create Image
                // word, and a serial address on the machine.
                return Some(value_box_control(r.field));
            }
        }
        RowKind::Size => {
            if launcher_size_box_rect(rect, row_y).contains(pos) {
                return Some(UiControl::LauncherNewImageEdit(r.field));
            }
            if launcher_size_unit_rect(rect, row_y).contains(pos) {
                return Some(UiControl::LauncherNewImageUnit);
            }
        }
        RowKind::Number => {
            if launcher_number_rect(rect, row_y).contains(pos) {
                return Some(UiControl::LauncherNewImageEdit(r.field));
            }
        }
        RowKind::FsFamily => {
            let labels: Vec<&str> = launcher::FsFamily::ALL.iter().map(|f| f.label()).collect();
            for (at, family) in launcher_tick_strip(rect, row_y, &labels)
                .into_iter()
                .zip(launcher::FsFamily::ALL)
            {
                if at.contains(pos) {
                    return Some(UiControl::LauncherFsFamily {
                        field: r.field,
                        family,
                    });
                }
            }
        }
        RowKind::FsVariant => {
            let labels: Vec<&str> = FS_VARIANTS.iter().map(|v| v.label()).collect();
            for (at, variant) in launcher_tick_strip(rect, row_y, &labels)
                .into_iter()
                .zip(FS_VARIANTS)
            {
                if state.workshop_fs_variant_enabled(r.field, variant) && at.contains(pos) {
                    return Some(UiControl::LauncherFsVariant {
                        field: r.field,
                        variant,
                    });
                }
            }
        }
        RowKind::Stepper => {
            let (prev, value, next) = launcher_geometry_stepper_rects(rect, row_y);
            if prev.contains(pos) {
                return Some(UiControl::LauncherCycle {
                    field: r.field,
                    forward: false,
                });
            }
            if next.contains(pos) {
                return Some(UiControl::LauncherCycle {
                    field: r.field,
                    forward: true,
                });
            }
            if value.contains(pos) {
                return Some(UiControl::LauncherNewImageEdit(r.field));
            }
        }
        RowKind::GeometryMode => {
            let (auto, custom, configure) = launcher_geometry_rects(rect, row_y);
            if auto.contains(pos) {
                return Some(UiControl::LauncherGeometryAuto);
            }
            if custom.contains(pos) {
                return Some(UiControl::LauncherGeometryCustom);
            }
            // Configure is only there once the geometry is by hand.
            if state.workshop.geometry_custom && configure.contains(pos) {
                return Some(UiControl::LauncherTab(LauncherTab::CreateGeometry));
            }
        }
        RowKind::Action => {
            if state.row_applies(r.field) && launcher_action_rect(rect, row_y).contains(pos) {
                return Some(launcher_row_action(r.field));
            }
            if let Some(second) = launcher_second_action(r.field) {
                if state.row_applies(second) && launcher_action2_rect(rect, row_y).contains(pos) {
                    return Some(launcher_row_action(second));
                }
            }
        }
        RowKind::Cycle => {
            let (prev, _value, next) = launcher_cycle_rects(rect, row_y);
            if prev.contains(pos) {
                return Some(UiControl::LauncherCycle {
                    field: r.field,
                    forward: false,
                });
            }
            if next.contains(pos) {
                return Some(UiControl::LauncherCycle {
                    field: r.field,
                    forward: true,
                });
            }
        }
        RowKind::Bootpri => {
            // No-drive / CD-image rows are skipped by the `applies` guard
            // above, so this only runs for a drive with an image. The
            // Bootable box is always live; the priority stepper/field is
            // inert while the box is cleared (the priority shows greyed).
            if launcher_bootable_rect(rect, row_y).contains(pos) {
                return Some(UiControl::LauncherDriveBootToggle(r.field));
            }
            if state.setup.drive_boot_off(r.field) {
                return None;
            }
            let (prev, value, next) = launcher_bootpri_rects(rect, row_y);
            if prev.contains(pos) {
                return Some(UiControl::LauncherCycle {
                    field: r.field,
                    forward: false,
                });
            }
            if next.contains(pos) {
                return Some(UiControl::LauncherCycle {
                    field: r.field,
                    forward: true,
                });
            }
            if value.contains(pos) {
                return Some(UiControl::LauncherDriveBootpriEdit(r.field));
            }
        }
        RowKind::Toggle => {
            if launcher_toggle_rect(rect, row_y).contains(pos) {
                return Some(UiControl::LauncherToggle(r.field));
            }
        }
        RowKind::Path => {
            let (browse, clear) = launcher_path_rects(rect, row_y);
            let (has_browse, has_clear) = launcher_path_buttons(&state.setup, r.field);
            if has_browse && browse.contains(pos) {
                return Some(UiControl::LauncherBrowse(r.field));
            }
            if has_clear && launcher_clear_enabled(&state.setup, r.field) && clear.contains(pos) {
                return Some(UiControl::LauncherClear(r.field));
            }
        }
        #[cfg(feature = "game-library")]
        RowKind::Account => {
            let (button, _) = launcher_path_rects(rect, row_y);
            if button.contains(pos) {
                return Some(UiControl::LauncherOpenRetroLogin);
            }
        }
        #[cfg(not(feature = "game-library"))]
        RowKind::Account => {}
        RowKind::FloppyMedia => {
            let drive = launcher::MachineSetup::drive_image_bay(r.field);
            if let Some(bay) = drive {
                if state.setup.drive_bridged(bay) {
                    // Bridged: one Configure button where Browse and
                    // Clear would be. There is no image to pick.
                    if launcher_bridge_configure_rect(rect, row_y).contains(pos) {
                        return Some(UiControl::LauncherBridgeConfigure(bay));
                    }
                    return None;
                }
            }
            let (browse, clear) = launcher_path_rects(rect, row_y);
            if browse.contains(pos) {
                return Some(UiControl::LauncherBrowse(r.field));
            }
            if launcher_clear_enabled(&state.setup, r.field) && clear.contains(pos) {
                return Some(UiControl::LauncherClear(r.field));
            }
        }
        RowKind::FloppyFlags => {
            let (protect, _bridge) = launcher_floppy_flag_rects(rect, row_y);
            if protect.contains(pos) {
                return Some(UiControl::LauncherToggle(r.field));
            }
            // A build without the feature has no physical-drive box to
            // hit: the whole thing is absent rather than inert.
            #[cfg(feature = "fluxbridge")]
            if _bridge.contains(pos) {
                if let Some(bay) = launcher::MachineSetup::drive_protect_bay(r.field) {
                    return Some(UiControl::LauncherDriveBridgeToggle(bay));
                }
            }
        }
        RowKind::Drive => {
            let (browse, clear) = launcher_path_rects(rect, row_y);
            // A real disk replaces both buttons with one. Only this
            // row's buttons change: everything else on the panel must
            // still be reachable, so nothing returns early here.
            if state.setup.host_disk_on_row(r.field).is_some() {
                let unmount = Rect {
                    x: browse.x,
                    y: browse.y,
                    w: clear.x + clear.w - browse.x,
                    h: browse.h,
                };
                if unmount.contains(pos) {
                    return Some(UiControl::LauncherHostDiskUnmount(r.field));
                }
            } else {
                if browse.contains(pos) {
                    return Some(UiControl::LauncherBrowse(r.field));
                }
                if launcher_clear_enabled(&state.setup, r.field) && clear.contains(pos) {
                    return Some(UiControl::LauncherClear(r.field));
                }
                // A support archive with nothing chosen can fetch
                // its own; once something is chosen there is
                // nothing to fetch, and Clear brings it back.
                #[cfg(feature = "game-library")]
                if row_archive(r.field).is_some()
                    && state.setup.path(r.field).is_none()
                    && launcher_download_rect(rect, row_y).contains(pos)
                {
                    return Some(UiControl::LauncherWhdloadDownload(r.field));
                }
            }
            // The volume name only matters once an image is chosen
            // (and never for a CD image).
            if state.setup.path(r.field).is_some()
                && state.setup.drive_name_applies(r.field)
                && launcher_drive_name_rect(rect, row_y).contains(pos)
            {
                return Some(UiControl::LauncherDriveNameEdit(r.field));
            }
            // The filesystem toggle only matters for a directory
            // mount: an HDF/gzip image already carries its own
            // filesystem inside it.
            if launcher_drive_fs_applies(&state.setup, r.field)
                && launcher_drive_fs_rect(rect, row_y).contains(pos)
            {
                return Some(UiControl::LauncherDriveFilesystemToggle(r.field));
            }
        }
    }
    None
}

/// How a row's second column presents when the setting does not apply.
///
/// Greying is the signal that a row cannot be reached; what stands in its
/// place is a per-row judgement, so it is made once, here, rather than spread
/// across the drawing code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::video::ui) enum GreyedAs {
    /// Say why, as text where the control would be: the machine-shaped
    /// constraints worth explaining ("needs 32-bit CPU").
    Reason,
    /// Nothing. The greyed label says enough, and one phrase repeated down a
    /// page of drive bays or bridge settings says less than silence.
    Blank,
    /// The control, dimmed, still showing the setting's own value -- which
    /// still means something, and will be used again once the row applies.
    DimmedValue,
    /// The control, dimmed, with the reason in its value box: there is no
    /// mouse to be sensitive, and no shader to be strong.
    DimmedReason,
}

pub(in crate::video::ui) fn greyed_presentation(
    r: &launcher::Row,
    setup: &launcher::MachineSetup,
) -> GreyedAs {
    use LauncherField as F;
    // The workshop's rows stay put when they stop applying: an unformatted
    // disk still remembers the volume name it would have had.
    if LauncherState::is_workshop(r.field) {
        return GreyedAs::DimmedValue;
    }
    // A priority the machine has no drive to apply.
    if r.kind == RowKind::Bootpri {
        return GreyedAs::DimmedReason;
    }
    match r.field {
        F::MouseSensitivity | F::MouseCapture | F::ShaderStrength => GreyedAs::DimmedReason,
        F::RamPattern
        | F::FloppySpeed
        | F::AudioChannelMode
        | F::AudioFilter
        | F::AudioStereoSeparation => GreyedAs::DimmedValue,
        // Drive select is shaped by the interface, so it only shows a
        // selection while there is one to shape it: an attached DrawBridge
        // has no drive-select line, but with no interface at all there is
        // nothing to say.
        F::BridgeCable if setup.bridge_interface_selected() => GreyedAs::DimmedValue,
        F::BridgeDevice
        | F::BridgePort
        | F::BridgeCable
        | F::BridgeDensity
        | F::BridgeReadMode
        | F::BridgeReplaySpeed => GreyedAs::Blank,
        _ => GreyedAs::Reason,
    }
}

pub(in crate::video::ui) fn draw_launcher_row(
    frame: &mut [u8],
    rect: Rect,
    state: &LauncherState,
    r: &launcher::Row,
    i: usize,
    y_offset: usize,
    hover: Option<UiControl>,
    scale: usize,
) {
    let setup = &state.setup;
    let row_y = launcher_row_y(rect, i) + y_offset;
    // A section heading is a greyed, non-interactive label grouping the rows
    // below it (the Serial:/Parallel: sections of the I/O Ports tab).
    if r.kind == RowKind::SectionHeader {
        // The FluxBridge page's heading names the installed library, so its
        // text is not the one in the row table.
        let heading;
        let text = if r.field == LauncherField::BridgeLibrary {
            heading = bridge_library_heading();
            &heading
        } else {
            r.label
        };
        draw_panel_text(
            frame,
            launcher_pane_x(rect),
            row_y + 8,
            text,
            PANEL_TEXT_DIM,
            1,
            scale,
        );
        return;
    }
    // The ROM tab's identification lines: one greyed fact per row --
    // Name, Version, Revision -- indented two spaces under the indented
    // path row, the value following its label. The prefix stands even
    // when an unrecognised image leaves the value blank.
    if r.kind == RowKind::RomInfo {
        let (name, version, revision) = state.rom_note_cells(r.field);
        let value = match r.label {
            "Version" => version,
            "Revision" => revision,
            _ => name,
        };
        // The prefix in grey, the fact itself in full text colour.
        let x = launcher_pane_x(rect) + 4 * font::GLYPH_W;
        let prefix = format!("{}: ", r.label);
        draw_panel_text(frame, x, row_y + 4, &prefix, PANEL_TEXT_DIM, 1, scale);
        draw_panel_text(
            frame,
            x + prefix.chars().count() * font::GLYPH_W,
            row_y + 4,
            &value,
            PANEL_TEXT,
            1,
            scale,
        );
        return;
    }
    // The greyed column titles above the Boot Priority rows.
    if r.kind == RowKind::BootpriHeader {
        for (x, title) in [
            (launcher_pane_x(rect), "Drive"),
            (launcher_control_x(rect), "Priority"),
            (launcher_bootable_rect(rect, row_y).x, "Status"),
        ] {
            draw_panel_text(frame, x, row_y + 8, title, PANEL_TEXT_DIM, 1, scale);
        }
        return;
    }
    // A workshop row greys on its own terms -- there is no machine setting
    // behind it to explain itself -- so it is asked directly.
    let reason = if r.field.is_netplay() {
        (!state.row_applies(r.field)).then_some("")
    } else if LauncherState::is_workshop(r.field) {
        (!state.workshop_applies(r.field)).then_some("")
    } else {
        setup.disabled_reason(r.field)
    };
    // The SoundFont row's label stays lit even while the value shows
    // the bundled default -- the setting is present either way; only
    // the value is Copperline's answer rather than the person's.
    let label_keeps_colour = matches!(r.field, LauncherField::Rom | LauncherField::ScsiRom) || {
        #[cfg(feature = "coppersynth")]
        {
            r.field == LauncherField::CsynthSoundfont
        }
        #[cfg(not(feature = "coppersynth"))]
        {
            false
        }
    };
    let label_inherits = !label_keeps_colour && launcher_path_inherits(setup, r.field);
    let label_color = if reason.is_none() && !label_inherits {
        PANEL_TEXT
    } else {
        PANEL_TEXT_DIM
    };
    // A bay on a real drive says so in place of "Disk image": there is no
    // image, and the row's value is the interface rather than a file.
    let label = if r.kind == RowKind::FloppyMedia
        && launcher::MachineSetup::drive_image_bay(r.field)
            .is_some_and(|bay| setup.drive_bridged(bay))
    {
        // Matches the tick box that turned it on. Which version of the
        // library is linked in is named on the Configure page, where there is
        // room for it.
        "  FluxBridge"
    } else {
        r.label
    };
    draw_panel_text(
        frame,
        launcher_pane_x(rect),
        row_y + 8,
        label,
        label_color,
        1,
        scale,
    );
    let greyed_as = reason.map(|_| greyed_presentation(r, setup));
    let greyed_shows_reason = greyed_as == Some(GreyedAs::DimmedReason);
    let disabled = reason.is_some();
    if let Some(reason) = reason {
        if !matches!(
            greyed_as,
            Some(GreyedAs::DimmedValue | GreyedAs::DimmedReason)
        ) {
            if greyed_as != Some(GreyedAs::Blank) {
                // Where the value the row cannot have would sit: the control
                // column's own left edge, so a reason lines up with the
                // values above and below it rather than standing in from
                // them by the width of a stepper arrow.
                draw_panel_text(
                    frame,
                    launcher_control_x(rect),
                    row_y + 8,
                    reason,
                    PANEL_TEXT_DIM,
                    1,
                    scale,
                );
            }
            return;
        }
    }
    match r.kind {
        // Drawn above with an early return.
        RowKind::SectionHeader | RowKind::BootpriHeader | RowKind::RomInfo => {}
        RowKind::Text => {
            if LauncherState::is_serial_addr(r.field) {
                // `[host] : [port]`, each half its own box with its greyed
                // default, the colon between them furniture.
                let (host_box, port_box) = launcher_serial_addr_rects(rect, row_y);
                #[cfg(feature = "midi")]
                let (host, port) = state.setup.serial_addr_parts(r.field);
                #[cfg(not(feature = "midi"))]
                let (host, port): (Option<String>, Option<u16>) = (None, None);
                // The listen box's default host is real -- the loopback
                // the run would bind -- so it shows it. Dialing out has no
                // sensible default host, so Connect keeps the prompt. (The
                // fields only exist with `midi`; without it this whole arm
                // is dead, `is_serial_addr` being always false.)
                #[cfg(feature = "midi")]
                let host_hint = if r.field == LauncherField::SerialConnect {
                    "Host/IP"
                } else {
                    crate::config::SERIAL_DEFAULT_HOST
                };
                #[cfg(not(feature = "midi"))]
                let host_hint = crate::config::SERIAL_DEFAULT_HOST;
                draw_serial_half_box(
                    frame,
                    host_box,
                    state,
                    UiControl::LauncherSerialHostEdit(r.field),
                    matches!(state.editing(),
                        Some(launcher::EditTarget::SerialHost(f)) if f == r.field),
                    host,
                    host_hint,
                    scale,
                );
                draw_panel_text(
                    frame,
                    host_box.x + host_box.w + 4,
                    row_y + 8,
                    ":",
                    PANEL_TEXT_DIM,
                    1,
                    scale,
                );
                draw_serial_half_box(
                    frame,
                    port_box,
                    state,
                    UiControl::LauncherSerialPortEdit(r.field),
                    matches!(state.editing(),
                        Some(launcher::EditTarget::SerialPort(f)) if f == r.field),
                    port.map(|p| p.to_string()),
                    &crate::config::SERIAL_DEFAULT_PORT.to_string(),
                    scale,
                );
            } else {
                draw_launcher_value_box(
                    frame,
                    launcher_text_rect(rect, row_y, r.field),
                    state,
                    r.field,
                    disabled,
                    false,
                    scale,
                );
            }
        }
        RowKind::Size => {
            // A number to type, with the unit written beside it. The unit
            // is text rather than a button: clicking it swaps MB and GB.
            draw_launcher_value_box(
                frame,
                launcher_size_box_rect(rect, row_y),
                state,
                r.field,
                disabled,
                false,
                scale,
            );
            let unit = launcher_size_unit_rect(rect, row_y);
            draw_panel_text(
                frame,
                unit.x,
                unit.y + 6,
                state.workshop.size_unit.label(),
                if lit(hover, UiControl::LauncherNewImageUnit) != 0.0 {
                    PANEL_TEXT_HILIGHT
                } else {
                    PANEL_TEXT
                },
                1,
                scale,
            );
        }
        RowKind::Number => {
            draw_launcher_value_box(
                frame,
                launcher_number_rect(rect, row_y),
                state,
                r.field,
                disabled,
                false,
                scale,
            );
        }
        RowKind::FsFamily => {
            let labels: Vec<&str> = launcher::FsFamily::ALL.iter().map(|f| f.label()).collect();
            for (at, family) in launcher_tick_strip(rect, row_y, &labels)
                .into_iter()
                .zip(launcher::FsFamily::ALL)
            {
                draw_launcher_tick_choice(
                    frame,
                    at,
                    family.label(),
                    state.workshop_fs_family_set(r.field, family),
                    disabled,
                    lit(
                        hover,
                        UiControl::LauncherFsFamily {
                            field: r.field,
                            family,
                        },
                    ),
                    scale,
                );
            }
        }
        RowKind::FsVariant => {
            // On an unformatted volume the row greys whole -- label, boxes
            // and all -- rather than disappearing, so the page keeps its
            // shape as the family above it changes.
            let labels: Vec<&str> = FS_VARIANTS.iter().map(|v| v.label()).collect();
            for (at, variant) in launcher_tick_strip(rect, row_y, &labels)
                .into_iter()
                .zip(FS_VARIANTS)
            {
                draw_launcher_tick_choice(
                    frame,
                    at,
                    variant.label(),
                    state.workshop_fs_variant_set(r.field, variant),
                    disabled || !state.workshop_fs_variant_enabled(r.field, variant),
                    lit(
                        hover,
                        UiControl::LauncherFsVariant {
                            field: r.field,
                            variant,
                        },
                    ),
                    scale,
                );
            }
        }
        RowKind::Stepper => {
            let (prev, value, next) = launcher_geometry_stepper_rects(rect, row_y);
            // Both ends light together, as on any other stepper.
            let back = UiControl::LauncherCycle {
                field: r.field,
                forward: false,
            };
            let forward = UiControl::LauncherCycle {
                field: r.field,
                forward: true,
            };
            let stepper = nav_lit(back);
            draw_text_button(
                frame,
                prev,
                "<",
                !disabled,
                stepper_light(hover, back, stepper),
                scale,
            );
            draw_text_button(
                frame,
                next,
                ">",
                !disabled,
                stepper_light(hover, forward, stepper),
                scale,
            );
            draw_launcher_value_box(frame, value, state, r.field, disabled, true, scale);
        }
        RowKind::GeometryMode => {
            // Auto and Custom sit together as one choice, the chosen one
            // lit; Configure only appears once there is something to
            // configure.
            let (auto, custom, configure) = launcher_geometry_rects(rect, row_y);
            let by_hand = state.workshop.geometry_custom;
            draw_launcher_chip(
                frame,
                auto,
                "Auto",
                !by_hand,
                lit(hover, UiControl::LauncherGeometryAuto),
                false,
                scale,
            );
            draw_launcher_chip(
                frame,
                custom,
                "Custom",
                by_hand,
                lit(hover, UiControl::LauncherGeometryCustom),
                false,
                scale,
            );
            if by_hand {
                draw_text_button(
                    frame,
                    configure,
                    "Configure",
                    true,
                    lit(hover, UiControl::LauncherTab(LauncherTab::CreateGeometry)),
                    scale,
                );
            }
        }
        RowKind::Action => {
            let label = if r.field.is_netplay() {
                state.netplay.value(r.field)
            } else {
                state.workshop_action_label(r.field)
            };
            draw_text_button(
                frame,
                launcher_action_rect(rect, row_y),
                &label,
                !disabled,
                lit(hover, launcher_row_action(r.field)),
                scale,
            );
            if let Some(second) = launcher_second_action(r.field) {
                let label = if second.is_netplay() {
                    state.netplay.value(second)
                } else {
                    state.workshop_action_label(second)
                };
                draw_text_button(
                    frame,
                    launcher_action2_rect(rect, row_y),
                    &label,
                    state.row_applies(second),
                    lit(hover, launcher_row_action(second)),
                    scale,
                );
            }
        }
        RowKind::Cycle => {
            let (prev, value, next) = launcher_cycle_rects(rect, row_y);
            // Both ends light together: the focus is on the setting,
            // and the setting is the pair of them. The pointer still
            // lights only the one it is over.
            let back = UiControl::LauncherCycle {
                field: r.field,
                forward: false,
            };
            let forward = UiControl::LauncherCycle {
                field: r.field,
                forward: true,
            };
            let stepper = nav_lit(back);
            draw_text_button(
                frame,
                prev,
                "<",
                !disabled,
                stepper_light(hover, back, stepper),
                scale,
            );
            draw_text_button(
                frame,
                next,
                ">",
                !disabled,
                stepper_light(hover, forward, stepper),
                scale,
            );
            // Clip a long value (e.g. a wordy MIDI device name) to the box so
            // it cannot spill over the ">" stepper.
            let shown = match reason {
                Some(reason) if greyed_shows_reason => reason.to_string(),
                _ => state.row_value(r.field),
            };
            let text = truncate_to_width(&shown, value.w);
            let tw = text.chars().count() * font::GLYPH_W;
            let tx = value.x + value.w.saturating_sub(tw) / 2;
            let color = if disabled {
                PANEL_TEXT_DIM
            } else {
                // Green while the focus is merely on it; white once it
                // stands open, which is the difference between choosing
                // a setting and changing it.
                if nav_open() && stepper != 0.0 {
                    PANEL_TITLE_TEXT
                } else {
                    PANEL_TEXT_HILIGHT
                }
            };
            draw_panel_text(frame, tx, value.y + 6, &text, color, 1, scale);
        }
        RowKind::Bootpri => {
            // Priority column: a `< value >` stepper whose value is also a text
            // field. Greyed and inert while the Bootable box (drawn last) is
            // cleared, where it shows the -128 the config will store.
            //
            // A row with no drive to order has no priority to step through, so
            // the stepper goes entirely and only the reason is left, sitting
            // where the value would: a priority that could be changed, and one
            // that does not exist, should not look alike.
            let no_drive = reason.is_some();
            let disabled = disabled || setup.drive_boot_off(r.field);
            let (prev, value, next) = launcher_bootpri_rects(rect, row_y);
            if !no_drive {
                // Both ends light together, as on any other stepper:
                // the focus is on the setting, and the setting is the
                // pair of them with its box between. The pointer still
                // lights only the one it is over.
                let back = UiControl::LauncherCycle {
                    field: r.field,
                    forward: false,
                };
                let forward = UiControl::LauncherCycle {
                    field: r.field,
                    forward: true,
                };
                let stepper = nav_lit(back);
                draw_text_button(
                    frame,
                    prev,
                    "<",
                    !disabled,
                    stepper_light(hover, back, stepper),
                    scale,
                );
                draw_text_button(
                    frame,
                    next,
                    ">",
                    !disabled,
                    stepper_light(hover, forward, stepper),
                    scale,
                );
                draw_rect_bevel(
                    frame,
                    scale_rect(value, scale),
                    BUTTON_EDGE_DARK,
                    BUTTON_EDGE_LIGHT,
                    scale,
                );
            }
            let editing = state.editing() == Some(EditTarget::DriveBootpri(r.field));
            light_edit_box(
                frame,
                value,
                UiControl::LauncherDriveBootpriEdit(r.field),
                editing,
                scale,
            );
            let text = if let Some(reason) = reason.filter(|_| greyed_shows_reason) {
                reason.to_string()
            } else {
                setup.value_label(r.field)
            };
            // A priority sits centred in its box; a row with no drive has no
            // box, so its reason starts where the column does -- under the
            // "Priority" heading, like every other greyed row on the page.
            let (text, tx) = if no_drive {
                (
                    truncate_to_width(&text, next.x + next.w - launcher_control_x(rect)),
                    launcher_control_x(rect),
                )
            } else {
                let text = truncate_to_width(&text, value.w.saturating_sub(8));
                let tw = text.chars().count() * font::GLYPH_W;
                (text, value.x + value.w.saturating_sub(tw) / 2)
            };
            let color = if disabled {
                PANEL_TEXT_DIM
            } else if editing {
                PANEL_TEXT_HILIGHT
            } else {
                PANEL_TEXT
            };
            if editing {
                draw_edit_line(
                    frame,
                    value.x + 4,
                    value.y + 6,
                    state.edit_buffer(),
                    state.edit_caret().at(),
                    PANEL_TEXT_HILIGHT,
                    PANEL_BG,
                    value.w.saturating_sub(8),
                    scale,
                );
            } else {
                draw_panel_text(frame, tx, value.y + 6, &text, color, 1, scale);
            }
            // Status column: the "Bootable" label then a tick box, ticked when
            // the drive is bootable.
            let cell = launcher_bootable_rect(rect, row_y);
            draw_panel_text(
                frame,
                cell.x,
                cell.y + 6,
                BOOTABLE_LABEL,
                if reason.is_some() {
                    PANEL_TEXT_DIM
                } else {
                    PANEL_TEXT
                },
                1,
                scale,
            );
            let box_rect = launcher_bootable_box(cell);
            let hovered = lit(hover, UiControl::LauncherDriveBootToggle(r.field));
            fill_rect(frame, scale_rect(box_rect, scale), ENTRY_BG, scale);
            // Green on its edge, like every other tick box: this one is
            // drawn by hand rather than by draw_tick_box, and lighting
            // its whole face was the one box that did not match.
            draw_outline(
                frame,
                box_rect,
                tick_outline(hovered).unwrap_or(BUTTON_EDGE_LIGHT),
                scale,
            );
            if !disabled {
                fill_rect(
                    frame,
                    scale_rect(
                        Rect {
                            x: box_rect.x + 3,
                            y: box_rect.y + 3,
                            w: 6,
                            h: 6,
                        },
                        scale,
                    ),
                    PANEL_TEXT_HILIGHT,
                    scale,
                );
            }
        }
        RowKind::FloppyMedia => {
            let bay = launcher::MachineSetup::drive_image_bay(r.field);
            let bridged = bay.is_some_and(|b| setup.drive_bridged(b));
            let value_x = launcher_control_x(rect);
            let (browse, clear) = launcher_path_rects(rect, row_y);
            if bridged {
                let bay = bay.expect("bridged implies a bay");
                let text = setup.drive_bridge_label(bay);
                draw_panel_text(frame, value_x, browse.y + 6, &text, PANEL_TEXT, 1, scale);
                let button = launcher_bridge_configure_rect(rect, row_y);
                draw_text_button(
                    frame,
                    button,
                    "Configure",
                    true,
                    lit(hover, UiControl::LauncherBridgeConfigure(bay)),
                    scale,
                );
            } else {
                let avail = browse.x.saturating_sub(value_x + 8);
                let text = truncate_to_width(&setup.value_label(r.field), avail);
                draw_panel_text(frame, value_x, browse.y + 6, &text, PANEL_TEXT, 1, scale);
                draw_text_button(
                    frame,
                    browse,
                    "Browse",
                    true,
                    lit(hover, UiControl::LauncherBrowse(r.field)),
                    scale,
                );
                draw_text_button(
                    frame,
                    clear,
                    "Clear",
                    launcher_clear_enabled(setup, r.field),
                    lit(hover, UiControl::LauncherClear(r.field)),
                    scale,
                );
            }
        }
        RowKind::FloppyFlags => {
            #[cfg_attr(not(feature = "fluxbridge"), allow(unused_variables))]
            let bay = launcher::MachineSetup::drive_protect_bay(r.field);
            #[cfg_attr(not(feature = "fluxbridge"), allow(unused_variables))]
            let (protect_cell, bridge_cell) = launcher_floppy_flag_rects(rect, row_y);
            let mut tick = |cell: Rect, label: &str, on: bool, hot: f32| {
                draw_panel_text(frame, cell.x, cell.y + 6, label, PANEL_TEXT, 1, scale);
                let box_rect = launcher_flag_box(cell, label);
                // The box keeps its own face: a tick box says what it
                // says with its outline, and a filled middle reads as
                // a tick that is not there.
                fill_rect(frame, scale_rect(box_rect, scale), ENTRY_BG, scale);
                draw_outline(
                    frame,
                    box_rect,
                    tick_outline(hot).unwrap_or(BUTTON_EDGE_LIGHT),
                    scale,
                );
                if on {
                    fill_rect(
                        frame,
                        scale_rect(
                            Rect {
                                x: box_rect.x + 3,
                                y: box_rect.y + 3,
                                w: 6,
                                h: 6,
                            },
                            scale,
                        ),
                        PANEL_TEXT_HILIGHT,
                        scale,
                    );
                }
            };
            tick(
                protect_cell,
                WRITE_PROTECT_LABEL,
                setup.toggle_value(r.field),
                lit(hover, UiControl::LauncherToggle(r.field)),
            );
            // Only drawn where a physical drive can actually be attached; a
            // build without the feature leaves the write-protect box alone on
            // the row rather than offering a switch that does nothing.
            #[cfg(feature = "fluxbridge")]
            if let Some(bay) = bay {
                tick(
                    bridge_cell,
                    PHYSICAL_DRIVE_LABEL,
                    setup.drive_bridged(bay),
                    lit(hover, UiControl::LauncherDriveBridgeToggle(bay)),
                );
            }
        }
        RowKind::Toggle if LauncherState::is_workshop(r.field) => {
            // A tick box rather than an On/Off button: these pages are a
            // list of choices about one thing, and ticks read as a list.
            let button = launcher_toggle_rect(rect, row_y);
            let on = state.row_toggle(r.field);
            let hot = lit(hover, UiControl::LauncherToggle(r.field));
            let box_rect = Rect {
                x: button.x,
                y: row_y + (LAUNCH_ROW_H - 10) / 2,
                w: 10,
                h: 10,
            };
            draw_tick_box(
                frame,
                box_rect.x,
                box_rect.y,
                // A setting that does not apply is not in force: showing a
                // tick on a row that cannot boot would promise a boot.
                on && !disabled,
                if disabled { PANEL_TEXT_DIM } else { TICK_GREEN },
                scale,
            );
            if !disabled {
                if let Some(edge) = tick_outline(hot) {
                    draw_outline(frame, box_rect, edge, scale);
                }
            }
        }
        RowKind::Toggle => {
            let button = launcher_toggle_rect(rect, row_y);
            let label = if state.row_toggle(r.field) {
                "On"
            } else {
                "Off"
            };
            draw_text_button(
                frame,
                button,
                label,
                true,
                lit(hover, UiControl::LauncherToggle(r.field)),
                scale,
            );
        }
        RowKind::Path => {
            let (browse, clear) = launcher_path_rects(rect, row_y);
            let value_x = launcher_control_x(rect);
            let avail = browse.x.saturating_sub(value_x + 8);
            // The printer output and the Paths rows show a whole path
            // (clipped from the front if long, so the row never overflows
            // and the end -- which is the part that identifies it -- stays);
            // other path rows show the image file name.
            let text = match setup.full_path_label(r.field) {
                Some(full) => clip_path_keep_name(&full, avail),
                None => truncate_to_width(&setup.value_label(r.field), avail),
            };
            // An inheriting row centres its `(default)`, which reads as a
            // column of its own rather than as eleven short strings
            // pretending to be paths. A row with a real path keeps the
            // left edge every other path on the page is read from.
            let inherits = launcher_path_inherits(setup, r.field);
            let (value_color, text_x) = if inherits {
                let text_w = text.chars().count() * font::GLYPH_W;
                // The bundled defaults -- the ROMs' and the SoundFont's --
                // read from the left like a chosen path would, just dimmed;
                // the Paths page's inherited rows keep their centred
                // `(default)`.
                let reads_left = matches!(
                    r.field,
                    LauncherField::Rom | LauncherField::FmvRom | LauncherField::ScsiRom
                ) || {
                    #[cfg(feature = "coppersynth")]
                    {
                        r.field == LauncherField::CsynthSoundfont
                    }
                    #[cfg(not(feature = "coppersynth"))]
                    {
                        false
                    }
                };
                if reads_left {
                    (PANEL_TEXT_DIM, value_x)
                } else {
                    (PANEL_TEXT_DIM, value_x + avail.saturating_sub(text_w) / 2)
                }
            } else {
                (PANEL_TEXT, value_x)
            };
            draw_panel_text(frame, text_x, browse.y + 6, &text, value_color, 1, scale);
            let (has_browse, has_clear) = launcher_path_buttons(setup, r.field);
            if has_browse {
                draw_text_button(
                    frame,
                    browse,
                    "Browse",
                    true,
                    lit(hover, UiControl::LauncherBrowse(r.field)),
                    scale,
                );
            }
            if has_clear {
                // "Reset" where a Paths row goes back to its default. The
                // FMV module names its physical Remove / Default action;
                // other rows clear, including the SoundFont row whose clear
                // also lands on a bundled default.
                let enabled = launcher_clear_enabled(setup, r.field);
                draw_text_button(
                    frame,
                    clear,
                    launcher_clear_label(setup, r.field),
                    enabled,
                    lit(hover, UiControl::LauncherClear(r.field)),
                    scale,
                );
            }
        }
        #[cfg(feature = "game-library")]
        RowKind::Account => {
            // Where Browse would be, because it is the same shape of thing:
            // the button that fills the column in. The column itself says
            // whether this session is signed in, and is empty when it is
            // not -- there is no account setting to report, only a session.
            let (button, _) = launcher_path_rects(rect, row_y);
            let signed_in = state.openretro.is_some();
            if signed_in {
                draw_panel_text(
                    frame,
                    launcher_control_x(rect),
                    button.y + 6,
                    "logged in",
                    PANEL_TEXT,
                    1,
                    scale,
                );
            }
            draw_text_button(
                frame,
                button,
                if signed_in { "Log out" } else { "Log in" },
                true,
                lit(hover, UiControl::LauncherOpenRetroLogin),
                scale,
            );
        }
        #[cfg(not(feature = "game-library"))]
        RowKind::Account => {}
        RowKind::Drive => {
            let (browse, clear) = launcher_path_rects(rect, row_y);
            let value_x = launcher_control_x(rect);
            // A slot holding a real disk is not something to browse for: the
            // disk was chosen from what the host has, and the only thing to
            // do with it here is give it back. Browse and Clear make way for
            // one Unmount spanning both.
            if let Some(disk) = setup.host_disk_on_row(r.field) {
                // The device and the volume on it: the device name is what
                // the Host Disk page and the host itself call it, and the
                // volume is what makes it recognisable.
                let text = truncate_to_width(
                    &setup.host_disk_label(&disk.device),
                    browse.x.saturating_sub(value_x + 8),
                );
                draw_panel_text(frame, value_x, browse.y + 6, &text, PANEL_TEXT, 1, scale);
                let unmount = Rect {
                    x: browse.x,
                    y: browse.y,
                    w: clear.x + clear.w - browse.x,
                    h: browse.h,
                };
                draw_text_button(
                    frame,
                    unmount,
                    "Unmount",
                    true,
                    lit(hover, UiControl::LauncherHostDiskUnmount(r.field)),
                    scale,
                );
                return;
            }

            // The volume-name box only appears once an image is chosen (a name
            // has nothing to label otherwise, and never labels a CD image);
            // until then the row reads like a plain path row and the path text
            // fills the full width.
            let has_image = setup.path(r.field).is_some() && setup.drive_name_applies(r.field);
            let has_fs_toggle = launcher_drive_fs_applies(setup, r.field);
            let name_box = launcher_drive_name_rect(rect, row_y);
            let fs_box = launcher_drive_fs_rect(rect, row_y);
            let text_right = if has_fs_toggle {
                fs_box.x
            } else if has_image {
                name_box.x
            } else {
                browse.x
            };
            let avail = text_right.saturating_sub(value_x + 8);
            // Host FS mounts and the WHDLoad paths show the whole host path
            // (clipped to keep the final name, with a leading "..." when
            // long), since the path is meaningful; other drives show the
            // image's file name.
            let full_path = r.field.is_filesys_dir_field() || r.field.is_whdload_path_field();
            let text = match (full_path, setup.path(r.field)) {
                (true, Some(p)) => clip_path_keep_name(&p.to_string_lossy(), avail),
                _ => truncate_to_width(&setup.value_label(r.field), avail),
            };
            draw_panel_text(frame, value_x, browse.y + 6, &text, PANEL_TEXT, 1, scale);
            if has_image {
                draw_rect_bevel(
                    frame,
                    scale_rect(name_box, scale),
                    BUTTON_EDGE_DARK,
                    BUTTON_EDGE_LIGHT,
                    scale,
                );
                let editing = state.editing() == Some(EditTarget::DriveName(r.field));
                light_edit_box(
                    frame,
                    name_box,
                    UiControl::LauncherDriveNameEdit(r.field),
                    editing,
                    scale,
                );
                let (label, color) = if let Some(name) = setup.drive_name(r.field) {
                    (name.to_string(), PANEL_TEXT)
                } else {
                    ("(volume)".to_string(), PANEL_TEXT_DIM)
                };
                if editing {
                    draw_edit_line(
                        frame,
                        name_box.x + 4,
                        name_box.y + 6,
                        state.edit_buffer(),
                        state.edit_caret().at(),
                        PANEL_TEXT_HILIGHT,
                        PANEL_BG,
                        name_box.w.saturating_sub(8),
                        scale,
                    );
                } else {
                    let shown = truncate_to_width(&label, name_box.w.saturating_sub(8));
                    draw_panel_text(
                        frame,
                        name_box.x + 4,
                        name_box.y + 6,
                        &shown,
                        color,
                        1,
                        scale,
                    );
                }
            }
            if has_fs_toggle {
                let label = if setup.drive_filesystem(r.field).ffs {
                    "FFS"
                } else {
                    "OFS"
                };
                draw_text_button(
                    frame,
                    fs_box,
                    label,
                    true,
                    lit(hover, UiControl::LauncherDriveFilesystemToggle(r.field)),
                    scale,
                );
            }
            draw_text_button(
                frame,
                browse,
                "Browse",
                true,
                lit(hover, UiControl::LauncherBrowse(r.field)),
                scale,
            );
            draw_text_button(
                frame,
                clear,
                "Clear",
                launcher_clear_enabled(setup, r.field),
                lit(hover, UiControl::LauncherClear(r.field)),
                scale,
            );
            // A support archive with nothing chosen offers to fetch its
            // own, from the same place and against the same digest the
            // packaging script uses.
            #[cfg(feature = "game-library")]
            if row_archive(r.field).is_some() && setup.path(r.field).is_none() {
                draw_text_button(
                    frame,
                    launcher_download_rect(rect, row_y),
                    "Download",
                    true,
                    lit(hover, UiControl::LauncherWhdloadDownload(r.field)),
                    scale,
                );
            }
        }
    }
}
