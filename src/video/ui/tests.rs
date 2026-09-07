use super::*;
use crate::config::{JoystickInputMode, PixelAspect, WarpSpeed};

#[cfg(feature = "game-library")]
#[test]
fn the_az_row_appears_only_once_the_list_is_worth_indexing() {
    use crate::gamelib::{Game, Known, Library};
    let rect = Rect {
        x: 0,
        y: 0,
        w: 716,
        h: 581,
    };
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::WhdloadLibrary;
    let stock = |n: usize| {
        (0..n)
            .map(|at| Known {
                file: format!("game{at}.lha"),
                game: Some(Game {
                    name: format!("Game {at}"),
                    ..Game::default()
                }),
                manual: false,
                slave_sha1: None,
            })
            .collect::<Vec<_>>()
    };
    let fill = |state: &mut LauncherState, n: usize| {
        state.library.db.set_known(stock(n));
        state.library.games = Library::known(std::path::Path::new("/games"), &state.library.db);
    };
    let whdload_entry = state.setup.whdload_enabled();
    let first = library_az_rects(rect, whdload_entry)[0];
    let hit = |state: &LauncherState| {
        launcher_control_at(rect, state, (first.x as i32 + 1, first.y as i32 + 1))
    };

    // A short list is read rather than indexed, so the row is not there
    // -- and the pixels it would occupy answer to nothing.
    fill(&mut state, LIBRARY_AZ_MIN_GAMES - 1);
    assert_eq!(hit(&state), None, "the row is up on a short list");

    // One more game and it appears.
    fill(&mut state, LIBRARY_AZ_MIN_GAMES);
    assert_eq!(
        hit(&state),
        Some(UiControl::LauncherLibraryJump(0)),
        "the row is missing on a list long enough for it"
    );
}

#[cfg(feature = "game-library")]
#[test]
fn the_favourites_arrows_appear_and_its_rows_follow_the_scroll() {
    let rect = Rect {
        x: 0,
        y: 0,
        w: 716,
        h: 581,
    };
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::WhdloadLibrary;
    let whdload_entry = state.setup.whdload_enabled();
    let rows = library_favourite_rows(rect, whdload_entry);
    assert!(rows > 1, "the box holds rows to scroll: {rows}");

    // A list that fits has no arrows: the corner is a row's, not a
    // button's, so a click there picks the game under it.
    for at in 0..rows {
        state
            .library
            .db
            .toggle_favourite(&format!("Game{at}.lha"), &format!("Game {at}"));
    }
    let [(up, up_at), (down, down_at)] = library_favourite_arrow_rects(rect, whdload_entry);
    let hit = |state: &LauncherState, at: Rect| {
        launcher_control_at(rect, state, (at.x as i32 + 2, at.y as i32 + 2))
    };
    assert_ne!(hit(&state, up_at), Some(up));
    assert_ne!(hit(&state, down_at), Some(down));

    // One more than fits, and they appear.
    state.library.db.toggle_favourite("GameN.lha", "Game N");
    assert_eq!(hit(&state, up_at), Some(up));
    assert_eq!(hit(&state, down_at), Some(down));

    // The rows are drawn positions: scrolled down, the top row is the
    // scroll's, so clicking it removes the right favourite.
    let first = library_favourite_row_rect(rect, whdload_entry, 0);
    let click = (first.x as i32 + 40, first.y as i32 + 2);
    assert_eq!(
        launcher_control_at(rect, &state, click),
        Some(UiControl::LauncherLibraryFavouritePick(0))
    );
    state.scroll_favourites(1, rows);
    assert_eq!(state.library.favourite_scroll, 1);
    assert_eq!(
        launcher_control_at(rect, &state, click),
        Some(UiControl::LauncherLibraryFavouritePick(0)),
        "still the top drawn row; the window under it moved"
    );

    // And the last drawn row is the last one there is: scrolled to the
    // end, the rows below it are not clickable.
    state.scroll_favourites(100, rows);
    let past = library_favourite_row_rect(rect, whdload_entry, rows - 1);
    assert_eq!(
        launcher_control_at(rect, &state, (past.x as i32 + 40, past.y as i32 + 2)),
        Some(UiControl::LauncherLibraryFavouritePick(rows - 1))
    );
}

#[cfg(feature = "game-library")]
#[test]
fn a_balanced_wrap_does_not_leave_one_word_alone() {
    let g = font::GLYPH_W;
    let text = "Update the \"Game library\" path in the WHDLoad settings";
    // Greedy: a full line, then the one word that did not fit.
    let greedy = wrap_to_width(text, 45 * g);
    assert_eq!(greedy.len(), 2);
    assert_eq!(greedy[1], "settings");

    // Balanced: the same two lines, broken where it reads.
    let even = wrap_balanced(text, 45 * g);
    assert_eq!(even.len(), 2, "the line count is what it was");
    assert!(
        even[1].split_whitespace().count() > 1,
        "still one word alone: {even:?}"
    );
    let longest = even.iter().map(|l| l.chars().count()).max().unwrap();
    assert!(
        longest < greedy[0].chars().count(),
        "no narrower than greedy: {even:?}"
    );
    // What is said is still what was passed in.
    assert_eq!(even.join(" "), text);

    // Text that fits is left alone rather than split for the sake of it.
    assert_eq!(wrap_balanced("Short", 45 * g), vec!["Short".to_string()]);
}

#[test]
fn an_edit_window_keeps_the_caret_in_the_box() {
    // Short enough to fit: the whole line is shown from the start, and
    // the caret sits where it is.
    assert_eq!(edit_window(5, 0, 10), (0, 0));
    assert_eq!(edit_window(5, 5, 10), (0, 5));

    // Longer than the box. Near the front nothing scrolls yet...
    assert_eq!(edit_window(40, 0, 10), (0, 0));
    assert_eq!(edit_window(40, 4, 10), (0, 4));
    // ...and past half a box the text starts moving under a caret that
    // stays put, rather than the caret walking into the edge.
    assert_eq!(edit_window(40, 20, 10), (15, 5));
    assert_eq!(edit_window(40, 21, 10), (16, 5));
    // At the end the window stops: the tail is shown and the caret
    // walks the last cells, which is where typing leaves it.
    assert_eq!(edit_window(40, 40, 10), (30, 10));
    assert_eq!(edit_window(40, 38, 10), (30, 8));

    // The caret is always inside the box it is drawn in.
    for len in 0..40 {
        for caret in 0..=len {
            for cells in 1..12 {
                let (first, cell) = edit_window(len, caret, cells);
                assert!(first <= caret, "{len}/{caret}/{cells}: {first}");
                assert!(cell <= cells, "{len}/{caret}/{cells}: {cell}");
            }
        }
    }
}

#[test]
fn clip_path_keeps_the_file_name() {
    let g = font::GLYPH_W;
    // Fits: returned unchanged.
    assert_eq!(clip_path_keep_name("/a/b.txt", 100 * g), "/a/b.txt");

    // A long Unix path keeps the whole file name after a "..." prefix.
    let unix = "/Users/me/Documents/amiga/captures/printer-output.txt";
    let out = clip_path_keep_name(unix, 24 * g);
    assert!(out.starts_with("..."), "{out}");
    assert!(out.ends_with("printer-output.txt"), "{out}");
    assert!(out.chars().count() <= 24, "{out}");

    // A long Windows path: backslash separators, file name preserved.
    let win = r"C:\Users\me\Documents\amiga\captures\printer-output.txt";
    let out = clip_path_keep_name(win, 24 * g);
    assert!(out.contains('\\'), "{out}");
    assert!(out.ends_with("printer-output.txt"), "{out}");
    assert!(out.chars().count() <= 24, "{out}");
}

/// The shortcuts panel is sized from its row count, so adding a row must
/// not push the table (or the notes under it) off the display.
/// The calibration prompt says what to do next, so it has to be
/// readable: wrapped inside the panel, and clear of the buttons under
/// it however many lines it takes.
#[test]
fn the_calibration_prompt_wraps_inside_its_panel() {
    let rect = panel_rect(&Panel::Calibration(
        crate::gamepad::CalibrationSession::new(),
    ));
    let chars = rect.w.saturating_sub(32) / font::GLYPH_W;
    let status =
        "All steps captured. Push controls to test, hold any button then hit save to finish.";
    let lines = wrap_text(status, chars, chars);
    assert!(lines.len() > 1, "this one needs more than a line");
    for line in &lines {
        assert!(
            line.chars().count() <= chars,
            "{line:?} runs past the panel's edge"
        );
    }
    // Where the rows leave off, plus the wrapped prompt, still above the
    // buttons along the bottom.
    let rows = crate::gamepad::CalibrationSession::step_count();
    let y = rect.y + 64 + rows * CAL_ROW_H + 6 + lines.len() * (font::GLYPH_H + 2);
    let buttons = cal_button_rects(rect)[0].1;
    assert!(
        y <= buttons.y,
        "the prompt reaches {y}, the buttons start at {}",
        buttons.y
    );
    // And the whole panel, sized from the step count, stays on screen.
    assert!(
        rect.h <= present_height(),
        "calibration panel is {}px tall, taller than the {}px display",
        rect.h,
        present_height()
    );
}

#[test]
fn the_shortcuts_panel_fits_on_screen() {
    let h = shortcuts_panel_height();
    assert!(
        h <= present_height(),
        "shortcuts panel is {h}px tall, taller than the {}px display",
        present_height()
    );
    // Sized to exactly hold header + rows + notes.
    assert!(
        h >= TITLE_H
            + SHORTCUT_ROWS.len() * SHORTCUT_ROW_H
            + SHORTCUT_NOTES.len() * SHORTCUT_NOTE_H
    );
}

/// The ROM tab's identification lines sit under the path row: the
/// greyed Name/Version/Revision prefixes stand either way, and an
/// image no checksum names leaves the values after them blank.
#[test]
fn the_rom_tab_draws_its_identification_line_under_the_path_row() {
    use super::super::window::{texture_height, texture_width};
    let scale = 1;
    let (w, h) = (texture_width(scale), texture_height(scale));
    // Pixels painted in the info-line ink on a given ROM-tab row
    // (the Name line is index 2, under the section heading and the
    // path row). The prefixes draw dimmed, the values in full text
    // colour; both count.
    let row_pixels = |frame: &[u8], rect: Rect, row: usize| {
        let row_y = launcher_row_y(rect, row);
        let mut lit = 0;
        for y in row_y..row_y + LAUNCH_ROW_H {
            for x in launcher_pane_x(rect)..rect.x + rect.w - LAUNCH_MARGIN {
                let p = (y * w + x) * 4;
                if frame[p..p + 4] == PANEL_TEXT_DIM.to_le_bytes()
                    || frame[p..p + 4] == PANEL_TEXT.to_le_bytes()
                {
                    lit += 1;
                }
            }
        }
        lit
    };
    let note_row_pixels = |frame: &[u8], rect: Rect| row_pixels(frame, rect, 2);
    let panel_of = |state: LauncherState| Panel::Launcher(Box::new(state));

    let mut setup = launcher::MachineSetup::default();
    setup.set_path(
        LauncherField::Rom,
        std::path::PathBuf::from("mystery-dump.rom"),
    );
    let mut unknown = LauncherState::new(setup.clone());
    unknown.tab = LauncherTab::Rom;
    let mut known = LauncherState::new(setup);
    known.tab = LauncherTab::Rom;
    known.set_rom_note_for_test(LauncherField::Rom, "Kickstart 3.1 (40.68) A1200");

    let mut blank_frame = vec![0u8; w * h * 4];
    let ui = UiState {
        panel: Some(panel_of(unknown)),
        ..Default::default()
    };
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    draw(&mut blank_frame, scale, &ui, None, None);
    let blank_ink = note_row_pixels(&blank_frame, rect);
    assert!(
        blank_ink > 0,
        "the Name: prefix stands even over an unrecognised image"
    );

    let mut frame = vec![0u8; w * h * 4];
    let ui = UiState {
        panel: Some(panel_of(known)),
        ..Default::default()
    };
    draw(&mut frame, scale, &ui, None, None);
    assert!(
        note_row_pixels(&frame, rect) > blank_ink,
        "the identification's value adds ink beyond the prefix"
    );
}

/// The launcher panel is a fixed box with no row scrolling, so a tab's
/// rows have to fit between the content top (below the nav row on pages
/// that have one) and the status line at the bottom. Nothing may reach
/// the action buttons or hang off the panel. Adding one row too many to
/// a tab fails here rather than silently drawing over the Save button.
#[test]
fn every_launcher_tab_row_fits_inside_the_panel() {
    use crate::config::{ParallelDevice, SerialMode};
    let rect = panel_rect(&Panel::Launcher(Box::new(LauncherState::new(
        launcher::MachineSetup::default(),
    ))));
    let devices = [
        ParallelDevice::None,
        ParallelDevice::Printer,
        ParallelDevice::Sampler,
    ];
    // Every serial mode, so a future one that grows its own rows is
    // swept here the day it is added.
    let modes = [
        SerialMode::Off,
        SerialMode::Stdout,
        SerialMode::Midi,
        SerialMode::Tcp,
        SerialMode::TcpConnect,
        SerialMode::Pty,
    ];
    // The strip tabs, plus the sub-pages and A/V categories reached from a
    // nav row rather than the strip.
    let off_strip = [
        LauncherTab::IoParallel,
        LauncherTab::IoNetworking,
        LauncherTab::IoAudio,
        LauncherTab::Cd,
        LauncherTab::HostFs,
        LauncherTab::Whdload,
        LauncherTab::BootPriority,
        LauncherTab::BootPriorityMore(2),
        LauncherTab::Lide,
        LauncherTab::AvVideo,
        LauncherTab::AvDisplay,
        LauncherTab::AvEmulation,
        LauncherTab::AvPaths,
    ];
    for &tab in launcher::TABS.iter().chain(off_strip.iter()) {
        // The row grid always ends above the status line; on tabs with a top
        // nav it starts a nav block lower, leaving it less room -- two nav
        // rows' worth where the nav wraps (the A/V pages' five chips, the
        // Storage links), which is what the draw path reserves too.
        let bound = launcher_status_y(rect);
        let row_offset = if tab.has_top_nav() {
            launcher_nav_block_h(tab)
        } else {
            0
        };
        for &device in &devices {
            for &mode in &modes {
                let rows = launcher::rows(tab, device, mode, false, false);
                // The boot order is paged: both of its pages carry the whole
                // table, and each draws its own share of it under the column
                // titles. A page never shows more than that many rows, so
                // that is what has to fit -- see `BOOTPRI_PAGE_ROWS`.
                let drawn = if matches!(
                    tab,
                    LauncherTab::BootPriority | LauncherTab::BootPriorityMore(_)
                ) {
                    rows.len().min(launcher::BOOTPRI_PAGE_ROWS + 1)
                } else {
                    rows.len()
                };
                for (i, r) in rows.iter().take(drawn).enumerate() {
                    let row_y = launcher_row_y(rect, i) + row_offset;
                    let (prev, value, next) = launcher_cycle_rects(rect, row_y);
                    let (browse, clear) = launcher_path_rects(rect, row_y);
                    // Every control a row can draw, whatever its kind:
                    // the widest and lowest of them must still fit.
                    let boxes = [
                        prev,
                        value,
                        next,
                        browse,
                        clear,
                        launcher_toggle_rect(rect, row_y),
                        launcher_drive_name_rect(rect, row_y),
                        launcher_bootable_rect(rect, row_y),
                        // The widest of these: a serial address box is
                        // sized for a host name and a port.
                        launcher_text_rect(rect, row_y, r.field),
                    ];
                    for b in boxes {
                        let label = r.label;
                        assert!(
                            b.y >= launcher_content_top(rect),
                            "{tab:?} row {i} ({label:?}) starts above the content area"
                        );
                        assert!(
                            b.y + b.h <= bound,
                            "{tab:?} row {i} ({label:?}) reaches {}, past the {bound} limit",
                            b.y + b.h
                        );
                        assert!(
                            b.x >= rect.x && b.x + b.w <= rect.x + rect.w,
                            "{tab:?} row {i} ({label:?}) spills outside the panel"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn frame_analyzer_controls_hit_test() {
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::FrameAnalyzer(FrameAnalyzerPanel::new())),
    };
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    let raster = analyzer_raster_rect(rect);
    assert_eq!(
        ui.control_at((raster.x as i32 + raster.w as i32 / 2, raster.y as i32 + 2)),
        Some(UiControl::AnalyzerPick {
            x: 511,
            y: 8,
            scanline: false,
        })
    );
    let scanline = analyzer_scanline_rect(rect);
    assert_eq!(
        ui.control_at((
            scanline.x as i32 + scanline.w as i32 / 2,
            scanline.y as i32 + 2
        )),
        Some(UiControl::AnalyzerPick {
            x: 511,
            y: 60,
            scanline: true,
        })
    );
    let (control, button) = analyzer_button_rects(rect)[1];
    assert_eq!(control, UiControl::AnalyzerFrame);
    assert_eq!(
        ui.control_at((button.x as i32 + 2, button.y as i32 + 2)),
        Some(UiControl::AnalyzerFrame)
    );
    let underlay = analyzer_underlay_rect(rect);
    assert_eq!(
        ui.control_at((underlay.x as i32 + 2, underlay.y as i32 + 2)),
        Some(UiControl::AnalyzerUnderlay)
    );
    let cpu_wait = analyzer_cpu_wait_rect(rect);
    assert_eq!(
        ui.control_at((cpu_wait.x as i32 + 2, cpu_wait.y as i32 + 2)),
        Some(UiControl::AnalyzerCpuWait)
    );
    // The checkboxes must not overlap the transport buttons, each other,
    // or run past the panel.
    let scrub = analyzer_scrub_rect(rect);
    for (_, button) in analyzer_button_rects(rect) {
        assert!(button.x + button.w <= underlay.x || underlay.x + underlay.w <= button.x);
    }
    assert!(underlay.x + underlay.w <= scrub.x);
    assert!(scrub.x + scrub.w <= cpu_wait.x);
    assert!(cpu_wait.x + cpu_wait.w <= rect.x + rect.w);
    // The gutter sits between the raster and the counters column.
    let gutter = analyzer_gutter_rect(rect);
    assert!(raster.x + raster.w <= gutter.x);
    assert!(gutter.x + gutter.w <= analyzer_counters_x(rect));
}

/// A Frame Analyzer panel in `tab`, with `presets` on the Memory tab.
fn analyzer_ui(tab: AnalyzerTab, presets: Vec<HeatPreset>) -> UiState {
    let mut panel = FrameAnalyzerPanel::new();
    panel.tab = tab;
    panel.heat_presets = presets;
    UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::FrameAnalyzer(panel)),
    }
}

#[test]
fn analyzer_resources_rows_hit_test_only_on_their_tab() {
    let ui = analyzer_ui(AnalyzerTab::Resources, Vec::new());
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    let rows = analyzer_resource_row_rects(rect);
    let (control, row) = rows[2];
    let pos = (row.x as i32 + 4, row.y as i32 + 4);
    assert_eq!(ui.control_at(pos), Some(control));
    // The same spot on the Beam tab belongs to that tab's controls.
    let ui = analyzer_ui(AnalyzerTab::Beam, Vec::new());
    assert_ne!(ui.control_at(pos), Some(control));
}

fn heat_preset(label: &str, base: u32, span: u32) -> HeatPreset {
    HeatPreset {
        label: label.to_string(),
        base,
        span,
    }
}

/// A synthetic Memory-tab view: a black window with `lit` cells set to
/// their toucher's colour, and a full census.
fn heat_view(lit: &[(usize, crate::heatmap::Toucher)]) -> AnalyzerHeatView {
    use crate::heatmap::Toucher;
    let mut image = vec![0xFF00_0000u32; heatmap::CELLS];
    for (cell, toucher) in lit {
        image[*cell] = toucher.colour();
    }
    let touchers = [
        Toucher::CpuRead,
        Toucher::CpuWrite,
        Toucher::Blitter,
        Toucher::Copper,
        Toucher::Disk,
        Toucher::Bitplane,
        Toucher::Sprite,
        Toucher::Audio,
    ];
    AnalyzerHeatView {
        image,
        base: 0,
        span: heatmap::DEFAULT_SPAN,
        bytes_per_cell: heatmap::DEFAULT_SPAN / heatmap::CELLS as u32,
        frame: 4321,
        census: touchers
            .iter()
            .map(|toucher| {
                let cells = lit.iter().filter(|(_, t)| t == toucher).count();
                AnalyzerHeatCensusRow {
                    name: toucher.name(),
                    colour: toucher.colour(),
                    cells,
                    bytes: cells as u64 * u64::from(heatmap::DEFAULT_SPAN / heatmap::CELLS as u32),
                }
            })
            .collect(),
        selected: None,
        resources: Vec::new(),
    }
}

fn analyzer_view(
    trace: Option<AnalyzerTraceView>,
    heat: Option<AnalyzerHeatView>,
) -> Box<FrameAnalyzerView> {
    Box::new(FrameAnalyzerView {
        running: false,
        status: "paused frame 4321".to_string(),
        trace,
        underlay: None,
        scrub: false,
        heat,
        resources: None,
        blits: None,
    })
}

#[test]
fn analyzer_tabs_hit_test_and_gate_their_tab_controls() {
    for (index, tab) in ANALYZER_TABS.iter().enumerate() {
        let ui = analyzer_ui(AnalyzerTab::Beam, Vec::new());
        let rect = panel_rect(ui.panel.as_ref().unwrap());
        let button = analyzer_tab_rect(rect, index);
        assert_eq!(
            ui.control_at((button.x as i32 + 2, button.y as i32 + 2)),
            Some(UiControl::AnalyzerTab(*tab))
        );
    }

    // Beam tab: the beam controls hit, the map does not exist.
    let ui = analyzer_ui(AnalyzerTab::Beam, Vec::new());
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    let map = analyzer_heat_map_rect(rect);
    let underlay = analyzer_underlay_rect(rect);
    assert_eq!(
        ui.control_at((underlay.x as i32 + 2, underlay.y as i32 + 2)),
        Some(UiControl::AnalyzerUnderlay)
    );
    let in_map = (map.x as i32 + map.w as i32 / 2, map.y as i32 + 4);
    assert!(
        !matches!(
            ui.control_at(in_map),
            Some(UiControl::AnalyzerHeatPick { .. })
        ),
        "the heat map is not drawn on the Beam tab, so it must not be clickable"
    );

    // Memory tab: the map hits, and none of the beam-only controls do.
    let ui = analyzer_ui(AnalyzerTab::Memory, Vec::new());
    assert!(matches!(
        ui.control_at(in_map),
        Some(UiControl::AnalyzerHeatPick { .. })
    ));
    for beam_only in [
        (underlay.x as i32 + 2, underlay.y as i32 + 2),
        (
            analyzer_scrub_rect(rect).x as i32 + 2,
            analyzer_scrub_rect(rect).y as i32 + 2,
        ),
        (
            analyzer_cpu_wait_rect(rect).x as i32 + 2,
            analyzer_cpu_wait_rect(rect).y as i32 + 2,
        ),
        (
            analyzer_button_rects(rect)[2].1.x as i32 + 2,
            analyzer_button_rects(rect)[2].1.y as i32 + 2,
        ),
    ] {
        assert_eq!(
            ui.control_at(beam_only),
            Some(UiControl::PanelBody),
            "beam-only controls are inert on the Memory tab"
        );
    }
    // Run and Frame stay on both tabs.
    for slot in 0..2 {
        let (control, button) = analyzer_button_rects(rect)[slot];
        assert_eq!(
            ui.control_at((button.x as i32 + 2, button.y as i32 + 2)),
            Some(control)
        );
    }
    // The scanline strip belongs to the beam view too.
    let scanline = analyzer_scanline_rect(rect);
    assert!(!matches!(
        ui.control_at((scanline.x as i32 + 4, scanline.y as i32 + 4)),
        Some(UiControl::AnalyzerPick { .. })
    ));
}

#[test]
fn heat_map_clicks_map_to_grid_cells() {
    let ui = analyzer_ui(AnalyzerTab::Memory, Vec::new());
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    let map = analyzer_heat_map_rect(rect);
    let last = (heatmap::GRID - 1) as u8;
    let pick =
        |dx: usize, dy: usize| ui.control_at((map.x as i32 + dx as i32, map.y as i32 + dy as i32));
    assert_eq!(pick(0, 0), Some(UiControl::AnalyzerHeatPick { x: 0, y: 0 }));
    assert_eq!(
        pick(map.w - 1, map.h - 1),
        Some(UiControl::AnalyzerHeatPick { x: last, y: last }),
        "the map's last pixel is the grid's last cell"
    );
    assert_eq!(
        pick(map.w - 1, 0),
        Some(UiControl::AnalyzerHeatPick { x: last, y: 0 })
    );
    assert_eq!(
        pick(map.w / 2, map.h / 2),
        Some(UiControl::AnalyzerHeatPick { x: 128, y: 128 })
    );
    // One pixel past the map is not a pick.
    assert_ne!(
        ui.control_at((map.x as i32 + map.w as i32, map.y as i32 + 2)),
        Some(UiControl::AnalyzerHeatPick { x: last, y: 0 })
    );
}

#[test]
fn heat_presets_hit_by_index_and_vanish_when_there_are_none() {
    let presets = vec![
        heat_preset("Chip", 0, 0x0020_0000),
        heat_preset("24-bit", 0, heatmap::DEFAULT_SPAN),
    ];
    let ui = analyzer_ui(AnalyzerTab::Memory, presets.clone());
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    let rects = analyzer_preset_rects(rect, &presets);
    assert_eq!(rects.len(), 2);
    for (index, (control, button)) in rects.iter().enumerate() {
        assert_eq!(*control, UiControl::AnalyzerHeatPreset(index as u8));
        assert_eq!(
            ui.control_at((button.x as i32 + 2, button.y as i32 + 2)),
            Some(*control)
        );
        // Presets sit above the map, never over it.
        assert!(button.y + button.h <= analyzer_heat_map_rect(rect).y);
    }
    // With no presets the row is empty: the same points are panel body.
    let empty = analyzer_ui(AnalyzerTab::Memory, Vec::new());
    for (_, button) in rects {
        assert_eq!(
            empty.control_at((button.x as i32 + 2, button.y as i32 + 2)),
            Some(UiControl::PanelBody)
        );
    }
    assert!(analyzer_preset_rects(rect, &[]).is_empty());
}

/// The tab row shifted the beam layout down; the content it pushed
/// down must still clear the bottom-anchored transport row.
#[test]
fn analyzer_tab_row_leaves_both_tabs_room_above_the_buttons() {
    let ui = analyzer_ui(AnalyzerTab::Beam, Vec::new());
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    let tabs = analyzer_tab_rect(rect, ANALYZER_TABS.len() - 1);
    assert!(tabs.y >= rect.y + TITLE_H, "tabs sit under the title bar");
    assert!(tabs.x + tabs.w < rect.x + rect.w);
    let content_top = analyzer_content_top(rect);
    assert!(content_top >= tabs.y + tabs.h);
    let buttons_top = analyzer_button_rects(rect)[0].1.y;
    // Beam: the legend and marker-count lines follow the scanline
    // strip (strip bottom + 14 for the legend, + 18 for the count).
    let scanline = analyzer_scanline_rect(rect);
    assert!(
        scanline.y + scanline.h + 14 + 18 + font::GLYPH_H <= buttons_top,
        "beam tab content runs into the transport row"
    );
    // Memory: the map plus its readout line.
    let map = analyzer_heat_map_rect(rect);
    assert!(
        map.y + map.h + 10 + font::GLYPH_H <= buttons_top,
        "the heat map runs into the transport row"
    );
    assert_eq!(map.w, map.h, "the map is square");
    // The census column fits between the map and the panel edge.
    let census_x = analyzer_heat_census_x(rect);
    assert!(census_x >= map.x + map.w);
    assert!(census_x + 12 + 27 * font::GLYPH_W <= rect.x + rect.w);
}

#[test]
fn heat_map_draws_its_cells_and_leaves_the_rest_black() {
    use super::super::window::{texture_height, texture_width};
    use crate::heatmap::Toucher;

    let scale = 1;
    let (w, h) = (texture_width(scale), texture_height(scale));
    let mut frame = vec![0u8; w * h * 4];
    let mut panel = FrameAnalyzerPanel::new();
    panel.tab = AnalyzerTab::Memory;
    panel.heat_presets = vec![heat_preset("Chip", 0, 0x0020_0000)];
    let rect = panel_rect(&Panel::FrameAnalyzer(panel.clone()));
    let map = analyzer_heat_map_rect(rect);
    // One lit cell in the middle of the grid, away from the outline.
    let cell = 128 * heatmap::GRID + 128;
    let view = analyzer_view(None, Some(heat_view(&[(cell, Toucher::Blitter)])));
    draw_frame_analyzer(&mut frame, rect, &panel, &view, None, scale);

    let pixel = |x: usize, y: usize| -> [u8; 4] {
        frame[(y * w + x) * 4..(y * w + x) * 4 + 4]
            .try_into()
            .unwrap()
    };
    // Sample where the map's own nearest mapping puts that cell.
    let lit = (0..map.w)
        .find(|x| x * heatmap::GRID / map.w == 128)
        .unwrap();
    assert_eq!(
        pixel(map.x + lit, map.y + lit),
        heat_rgba(Toucher::Blitter.colour()).to_le_bytes(),
        "the lit cell is painted in its toucher's colour"
    );
    // A cell nothing touched stays black (not the untouched-frame zero).
    assert_eq!(
        pixel(map.x + 40, map.y + 40),
        rgba(0, 0, 0).to_le_bytes(),
        "cold cells are black"
    );
}

#[test]
fn the_beam_tab_draws_below_the_tab_row() {
    use super::super::window::{texture_height, texture_width};

    let scale = 1;
    let (w, h) = (texture_width(scale), texture_height(scale));
    let mut frame = vec![0u8; w * h * 4];
    let panel = FrameAnalyzerPanel::new();
    assert_eq!(panel.tab, AnalyzerTab::Beam, "the beam view opens first");
    let rect = panel_rect(&Panel::FrameAnalyzer(panel.clone()));
    let trace = AnalyzerTraceView {
        frame: 1,
        seconds: 0.0,
        rows: 4,
        cols: 4,
        line_cck: 4,
        visible_start_vpos: 0,
        visible_lines: 2,
        display_hpos_start: 0,
        display_hpos_end: 4,
        owner_cck: [0; 9],
        blitter_busy_cck: 0,
        blitter_starve_cck: [0; 9],
        partial: false,
        selected_vpos: 0,
        selected_hpos: 0,
        selected_owner: "idle",
        selected_owner_code: b'.',
        owners: vec![b'.'; 16],
        records: None,
        markers: Vec::new(),
        selected_blit: None,
        diw_v: None,
        diw_h_cck: None,
        ddf_cck: None,
        cpu_waits: vec![b'.'; 16],
        cpu_wait_cck: 0,
        cpu_wait_by_class: [0; 9],
        cpu_wait_by_kind: [0; 4],
        top_stalled_pcs: Vec::new(),
        selected_cpu_wait_code: b'.',
    };
    let view = analyzer_view(Some(trace), None);
    draw_frame_analyzer(&mut frame, rect, &panel, &view, None, scale);

    let pixel = |x: usize, y: usize| -> [u8; 4] {
        frame[(y * w + x) * 4..(y * w + x) * 4 + 4]
            .try_into()
            .unwrap()
    };
    // The open tab reads as pressed, the other as a plain button.
    let beam = analyzer_tab_rect(rect, 0);
    let memory = analyzer_tab_rect(rect, 1);
    // Sampled inside the bevel but left of the centred label.
    assert_eq!(pixel(beam.x + 3, beam.y + 3), ENTRY_BG.to_le_bytes());
    assert_eq!(pixel(memory.x + 3, memory.y + 3), BUTTON_FACE.to_le_bytes());
    // The raster moved down with the rest of the content and is still
    // painted (idle slots, not the untouched frame).
    let raster = analyzer_raster_rect(rect);
    assert!(raster.y > beam.y + beam.h);
    assert_eq!(
        pixel(raster.x + 4, raster.y + raster.h / 2),
        owner_color(b'.').to_le_bytes()
    );
}

#[test]
fn the_memory_tab_draws_without_a_beam_trace_or_a_map() {
    use super::super::window::{texture_height, texture_width};
    use crate::heatmap::Toucher;

    let scale = 1;
    let (w, h) = (texture_width(scale), texture_height(scale));
    let mut panel = FrameAnalyzerPanel::new();
    panel.tab = AnalyzerTab::Memory;
    panel.heat_presets = vec![
        heat_preset("Chip", 0, 0x0020_0000),
        heat_preset("24-bit", 0, heatmap::DEFAULT_SPAN),
    ];
    panel.heat_selected = Some(7 * heatmap::GRID + 9);
    let rect = panel_rect(&Panel::FrameAnalyzer(panel.clone()));
    let map = analyzer_heat_map_rect(rect);
    let pixel = |frame: &[u8], x: usize, y: usize| -> [u8; 4] {
        frame[(y * w + x) * 4..(y * w + x) * 4 + 4]
            .try_into()
            .unwrap()
    };

    // No beam trace at all: the memory view still paints its map.
    let mut frame = vec![0u8; w * h * 4];
    let mut heat = heat_view(&[(0, Toucher::CpuWrite)]);
    heat.selected = Some(AnalyzerHeatCell {
        cell: 7 * heatmap::GRID + 9,
        toucher: Some(Toucher::Sprite.name()),
        colour: Toucher::Sprite.colour(),
        age_frames: Some(3),
    });
    let view = analyzer_view(None, Some(heat));
    draw_frame_analyzer(&mut frame, rect, &panel, &view, None, scale);
    assert_eq!(
        pixel(&frame, map.x + map.w / 2, map.y + map.h / 2),
        rgba(0, 0, 0).to_le_bytes()
    );
    assert_ne!(
        pixel(&frame, map.x, map.y),
        [0, 0, 0, 0],
        "the map is outlined even when every cell is cold"
    );

    // No map either: the not-armed line and the presets, nothing else.
    let mut bare = vec![0u8; w * h * 4];
    let view = analyzer_view(None, None);
    draw_frame_analyzer(&mut bare, rect, &panel, &view, None, scale);
    for y in map.y..map.y + map.h {
        for x in map.x..map.x + map.w {
            assert_eq!(
                pixel(&bare, x, y),
                [0, 0, 0, 0],
                "an unarmed map paints nothing at ({x}, {y})"
            );
        }
    }
    let presets = analyzer_preset_rects(rect, &panel.heat_presets);
    assert_eq!(presets.len(), 2);
    assert_ne!(
        pixel(&bare, presets[0].1.x + 2, presets[0].1.y + 2),
        [0, 0, 0, 0],
        "the presets are how an unarmed map gets armed, so they stay"
    );
}

#[test]
fn memory_entry_parsers_find_and_region() {
    let mut panel = DebuggerPanel::new();
    panel.entry = "C0 FFEE".into();
    assert_eq!(panel.find_pattern(), Some(vec![0xC0, 0xFF, 0xEE]));
    panel.entry = "C0FFE".into(); // odd number of hex digits
    assert_eq!(panel.find_pattern(), None);
    panel.entry = String::new();
    assert_eq!(panel.find_pattern(), None);

    panel.entry = "C00000 1000".into();
    assert_eq!(panel.region_spec(), Some((0xC0_0000, 0x1000)));
    panel.entry = "C00000".into(); // missing length
    assert_eq!(panel.region_spec(), None);
    panel.entry = "C00000 0".into(); // empty region
    assert_eq!(panel.region_spec(), None);
}

#[test]
fn catch_spec_parses_irq_trap_and_vector_forms() {
    assert_eq!(parse_catch_spec("irq 3"), Some(27));
    assert_eq!(parse_catch_spec("IRQ 7"), Some(31));
    assert_eq!(parse_catch_spec("trap 0"), Some(32));
    assert_eq!(parse_catch_spec("trap 15"), Some(47));
    assert_eq!(parse_catch_spec("vec 4"), Some(4));
    assert_eq!(parse_catch_spec("irq 0"), None); // no level-0 interrupt
    assert_eq!(parse_catch_spec("irq 8"), None);
    assert_eq!(parse_catch_spec("trap 16"), None);
    assert_eq!(parse_catch_spec("vec 1"), None); // reset vectors excluded
    assert_eq!(parse_catch_spec("C033C2"), None); // plain address is not a catch
    assert_eq!(parse_catch_spec("irq 3 4"), None);
}

#[test]
fn analyzer_marker_radius_and_label() {
    let marker = AnalyzerMarker {
        vpos: 100,
        hpos: 50,
        offset: 0x180,
        value: 0x0F00,
        source: "copper",
        copper_addr: None,
        colour: PANEL_TEXT_ACCENT,
    };
    // Within a line and two colour clocks counts as near.
    assert!(marker.near(100, 50));
    assert!(marker.near(101, 52));
    assert!(marker.near(99, 48));
    assert!(!marker.near(102, 50));
    assert!(!marker.near(100, 53));
    assert_eq!(marker.label(), "copper COLOR00=$0F00 v100 h50");
}

#[test]
fn analyzer_underlay_sample_maps_display_box_to_framebuffer() {
    // A trace shaped like a standard PAL frame: 312 lines of 227 cck,
    // display box starting at the framebuffer anchor.
    let trace = AnalyzerTraceView {
        frame: 1,
        seconds: 0.0,
        rows: 312,
        cols: 227,
        line_cck: 227,
        visible_start_vpos: 0x1A,
        visible_lines: 285,
        display_hpos_start: 0x30,
        display_hpos_end: 0x30 + (FB_WIDTH as u32 / 4),
        owner_cck: [0; 9],
        blitter_busy_cck: 0,
        blitter_starve_cck: [0; 9],
        partial: false,
        selected_vpos: 0,
        selected_hpos: 0,
        selected_owner: "idle",
        selected_owner_code: b'.',
        owners: vec![b'.'; 312 * 227],
        records: None,
        markers: Vec::new(),
        selected_blit: None,
        diw_v: None,
        diw_h_cck: None,
        ddf_cck: None,
        cpu_waits: vec![b'.'; 312 * 227],
        cpu_wait_cck: 0,
        cpu_wait_by_class: [0; 9],
        cpu_wait_by_kind: [0; 4],
        top_stalled_pcs: Vec::new(),
        selected_cpu_wait_code: b'.',
    };
    let mut fb = vec![0u32; FB_WIDTH * 285];
    fb[0] = 0xFF11_2233; // beam (0x1A, 0x30): framebuffer origin
    let underlay = AnalyzerUnderlayView {
        fb: std::rc::Rc::new(fb),
        rows: 285,
        width: FB_WIDTH,
    };
    let rect = Rect {
        x: 0,
        y: 0,
        w: 448,
        h: 246,
    };
    // Heatmap pixel exactly at the display box origin lands on fb[0]:
    // the first x whose hi-res mapping reaches display_hpos_start * 4.
    let x0 = (0..rect.w)
        .find(|x| x * trace.cols * 4 / rect.w >= 0x30 * 4)
        .unwrap();
    assert_eq!(
        underlay_sample(&underlay, &trace, rect, x0, 0x1A),
        Some(0xFF11_2233)
    );
    // Left of the display box or above the visible window: no sample.
    assert_eq!(underlay_sample(&underlay, &trace, rect, 0, 0x1A), None);
    assert_eq!(underlay_sample(&underlay, &trace, rect, x0, 0), None);
}

#[test]
fn panel_close_button_hit_tests() {
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::About),
    };
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    let close = close_button_rect(rect);
    let pos = (close.x as i32 + 2, close.y as i32 + 2);
    assert_eq!(ui.control_at(pos), Some(UiControl::PanelClose));
    // Panel body swallows clicks.
    let body = (rect.x as i32 + 5, (rect.y + TITLE_H + 5) as i32);
    assert_eq!(ui.control_at(body), Some(UiControl::PanelBody));
    // Outside the panel: nothing.
    assert_eq!(ui.control_at((0, 0)), None);
}

/// Clicking a serial address box opens *that* edit, not the Create Image
/// one the free-text widget was first built for.
#[cfg(feature = "midi")]
#[test]
fn the_serial_address_box_hit_tests_to_its_own_edit() {
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::IoPorts;
    while state.setup.serial_mode() != crate::config::SerialMode::TcpConnect {
        state.setup.cycle(LauncherField::SerialMode, true);
    }
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    let Some(Panel::Launcher(state)) = ui.panel.as_ref() else {
        unreachable!()
    };
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    let index = launcher::rows(
        state.tab,
        state.setup.parallel_device(),
        state.setup.serial_mode(),
        state.setup.midi_out_is_mt32(),
        state.setup.midi_out_is_csynth(),
    )
    .iter()
    .filter(|r| state.setup.row_on_page(state.tab, r.field))
    .position(|r| r.field == LauncherField::SerialConnect)
    .expect("no Connect row in tcp-connect mode");
    // The serial page sits under the I/O Ports nav row, so its rows
    // start a nav block lower.
    let row_y = launcher_row_y(rect, index) + LAUNCH_NAV_BLOCK_H;
    let (host_box, port_box) = launcher_serial_addr_rects(rect, row_y);
    assert_eq!(
        ui.control_at((host_box.x as i32 + 4, host_box.y as i32 + 4)),
        Some(UiControl::LauncherSerialHostEdit(
            LauncherField::SerialConnect
        ))
    );
    assert_eq!(
        ui.control_at((port_box.x as i32 + 4, port_box.y as i32 + 4)),
        Some(UiControl::LauncherSerialPortEdit(
            LauncherField::SerialConnect
        ))
    );
    // The pair spans the steppers' run exactly: host starts at the left
    // arrows' left edge, port ends at the right arrows' right edge.
    let (prev, _, next) = launcher_cycle_rects(rect, row_y);
    assert_eq!(host_box.x, prev.x);
    assert_eq!(port_box.x + port_box.w, next.x + next.w);
}

#[test]
fn fixed_ram_pattern_box_hit_tests_to_its_own_edit() {
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::Memory;
    state.setup.cycle(LauncherField::RamInit, false); // Zero -> Fixed
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    let Some(Panel::Launcher(state)) = ui.panel.as_ref() else {
        unreachable!()
    };
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    let index = launcher::rows(
        state.tab,
        state.setup.parallel_device(),
        state.setup.serial_mode(),
        state.setup.midi_out_is_mt32(),
        state.setup.midi_out_is_csynth(),
    )
    .iter()
    .filter(|r| state.setup.row_on_page(state.tab, r.field))
    .position(|r| r.field == LauncherField::RamPattern)
    .expect("no fixed RAM pattern row on Memory page");
    let row_y = launcher_row_y(rect, index);
    let box_rect = launcher_text_rect(rect, row_y, LauncherField::RamPattern);
    assert_eq!(
        ui.control_at((box_rect.x as i32 + 4, box_rect.y as i32 + 4)),
        Some(UiControl::LauncherRamPatternEdit)
    );
}

#[test]
fn debugger_controls_hit_test_and_entry_edits() {
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Debugger(DebuggerPanel::new())),
    };
    let rect = panel_rect(ui.panel.as_ref().unwrap());
    let tab = debug_tab_rect(rect, 3);
    assert_eq!(
        ui.control_at((tab.x as i32 + 2, tab.y as i32 + 2)),
        Some(UiControl::DebugTab(DebugTab::Video))
    );
    let tab = debug_tab_rect(rect, 4);
    assert_eq!(
        ui.control_at((tab.x as i32 + 2, tab.y as i32 + 2)),
        Some(UiControl::DebugTab(DebugTab::Audio))
    );
    let tab = debug_tab_rect(rect, 6);
    assert_eq!(
        ui.control_at((tab.x as i32 + 2, tab.y as i32 + 2)),
        Some(UiControl::DebugTab(DebugTab::IoMap))
    );
    let tab = debug_tab_rect(rect, 7);
    assert_eq!(
        ui.control_at((tab.x as i32 + 2, tab.y as i32 + 2)),
        Some(UiControl::DebugTab(DebugTab::Break))
    );
    // All eight tabs fit inside the panel.
    let last = debug_tab_rect(rect, 7);
    assert!(last.x + last.w <= rect.x + rect.w);
    let (control, step) = debug_button_rects(rect)[1];
    assert_eq!(control, UiControl::DebugStep);
    assert_eq!(
        ui.control_at((step.x as i32 + 2, step.y as i32 + 2)),
        Some(UiControl::DebugStep)
    );

    // Break-tab toggle buttons hit-test only while that tab is active.
    let mut panel = DebuggerPanel::new();
    panel.tab = DebugTab::Break;
    let ui_break = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Debugger(panel)),
    };
    let (control, toggle) = break_tab_button_rects(rect)[0];
    assert_eq!(control, UiControl::DebugBreakToggle);
    let pos = (toggle.x as i32 + 2, toggle.y as i32 + 2);
    assert_eq!(ui_break.control_at(pos), Some(UiControl::DebugBreakToggle));
    // On another tab the same position is just panel body.
    assert_eq!(ui.control_at(pos), Some(UiControl::PanelBody));

    // Audio-tab mute buttons hit-test only while the Audio tab is active.
    let mut panel = DebuggerPanel::new();
    panel.tab = DebugTab::Audio;
    let ui_audio = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Debugger(panel)),
    };
    let (control, mute0) = audio_tab_button_rects(rect)[0];
    assert_eq!(control, UiControl::DebugAudioMute(0));
    let pos = (mute0.x as i32 + 2, mute0.y as i32 + 2);
    assert_eq!(ui_audio.control_at(pos), Some(UiControl::DebugAudioMute(0)));
    // The CD mute is the fifth (index 4) button.
    let (cd_control, cd_mute) = audio_tab_button_rects(rect)[4];
    assert_eq!(cd_control, UiControl::DebugAudioMute(4));
    let cd_pos = (cd_mute.x as i32 + 2, cd_mute.y as i32 + 2);
    assert_eq!(
        ui_audio.control_at(cd_pos),
        Some(UiControl::DebugAudioMute(4))
    );
    // The line-mixed source slots continue below the CD row (last slot
    // = AUDIO_MAX_ROWS - 1); the click dispatcher decides whether a
    // fitted source actually occupies one.
    let (last_control, last_mute) = audio_tab_button_rects(rect)[AUDIO_MAX_ROWS - 1];
    assert_eq!(last_control, UiControl::DebugAudioMute(AUDIO_MAX_ROWS - 1));
    let last_pos = (last_mute.x as i32 + 2, last_mute.y as i32 + 2);
    assert_eq!(
        ui_audio.control_at(last_pos),
        Some(UiControl::DebugAudioMute(AUDIO_MAX_ROWS - 1))
    );
    // On another tab that position does not resolve to a mute.
    assert_eq!(ui.control_at(pos), Some(UiControl::PanelBody));

    let mut panel = DebuggerPanel::new();
    for ch in ['c', '0', '0', '3', 'C'] {
        panel.push_entry_char(ch);
    }
    assert_eq!(panel.entry, "C003C");
    assert_eq!(panel.entry_addr(), Some(0xC003C));
    // Punctuation is rejected (letters/digits/space are kept for spec
    // mnemonics).
    panel.push_entry_char('!');
    assert_eq!(panel.entry, "C003C");
    panel.backspace_entry();
    assert_eq!(panel.entry, "C003");
    // Capped at the entry length (room for a conditional breakpoint spec).
    for _ in 0..50 {
        panel.push_entry_char('F');
    }
    assert_eq!(panel.entry.len(), 40);
}

#[test]
fn flag_decoders_name_set_bits() {
    assert_eq!(dmacon_flags(0), "-");
    let flags = dmacon_flags(0x8390 & 0x7FFF);
    assert!(flags.contains("DMAEN"));
    assert!(flags.contains("BPLEN"));
    assert!(flags.contains("COPEN"));
    assert!(flags.contains("DSKEN"));
    assert!(!flags.contains("BLTEN"));

    let ints = int_flags((1 << 5) | (1 << 6) | (1 << 14));
    assert_eq!(ints, "INTEN BLIT VERTB");

    assert_eq!(sr_flags(0x2700), "S IPL7 xnzvc");
    assert_eq!(sr_flags(0x0015), "U IPL0 XnZvC");
    assert_eq!(sr_flags(0xA01F), "T S IPL0 XNZVC");
}

#[test]
fn hex_dump_row_formats_address_hex_and_ascii() {
    let bytes: Vec<u8> = (0x41..0x51).collect();
    let row = hex_dump_row(0xC00000, &bytes);
    assert!(row.starts_with("C00000: 41 42 43"));
    assert!(row.ends_with("ABCDEFGHIJKLMNOP"));
}

#[test]
fn parse_hex_entry() {
    assert_eq!(parse_hex_u32("C00000"), Some(0xC00000));
    assert_eq!(parse_hex_u32(""), None);
    assert_eq!(parse_hex_u32("xyz"), None);
}

#[test]
fn entry_box_parses_address_and_poke_tokens() {
    let mut panel = DebuggerPanel::new();
    // The entry only accepts hex, space, and the P/S/R register letters.
    for ch in "C00000 DEAD".chars() {
        panel.push_entry_char(ch);
    }
    assert_eq!(panel.entry, "C00000 DEAD");
    // The address consumers see just the first token.
    assert_eq!(panel.entry_addr(), Some(0xC00000));
    // Memory poke takes both tokens; the address is forced even.
    assert_eq!(panel.poke_target(), Some((0xC00000, 0xDEAD)));

    // Leading/doubled spaces are collapsed, and punctuation never makes it
    // in (letters are allowed now, for register names and condition
    // mnemonics).
    let mut panel = DebuggerPanel::new();
    for ch in "  D0  1234!".chars() {
        panel.push_entry_char(ch);
    }
    assert_eq!(panel.entry, "D0 1234");
    assert_eq!(panel.reg_poke(), Some((0, 0x1234)));
}

#[test]
fn break_spec_parses_address_condition_and_ignore() {
    // Bare address: plain breakpoint.
    assert_eq!(parse_break_spec("C033C2"), Some((0xC033C2, None, 0)));

    // Address plus a register/immediate condition.
    let (addr, cond, ignore) = parse_break_spec("C033C2 D0 EQ 5").unwrap();
    assert_eq!(addr, 0xC033C2);
    assert_eq!(ignore, 0);
    assert_eq!(
        cond,
        Some(BreakCond {
            lhs: CondOperand::Data(0),
            op: CondOp::Eq,
            rhs: CondOperand::Imm(5),
        })
    );

    // Memory operand, bit-test op, and a trailing ignore count.
    let (_, cond, ignore) = parse_break_spec("40 MC00002 AND 4000 IGN A").unwrap();
    assert_eq!(ignore, 0xA);
    assert_eq!(
        cond,
        Some(BreakCond {
            lhs: CondOperand::Mem(0xC00002),
            op: CondOp::And,
            rhs: CondOperand::Imm(0x4000),
        })
    );

    // Ignore count with no condition.
    assert_eq!(parse_break_spec("1234 IGN 3"), Some((0x1234, None, 3)));

    // Malformed condition and bad address are rejected.
    assert!(parse_break_spec("C033C2 D0 EQ").is_none());
    assert!(parse_break_spec("C033C2 D0 XX 5").is_none());
    assert!(parse_break_spec("xyz").is_none());
}

#[test]
fn register_names_map_to_gdb_indices() {
    assert_eq!(parse_reg_name("D0"), Some(0));
    assert_eq!(parse_reg_name("d7"), Some(7));
    assert_eq!(parse_reg_name("A0"), Some(8));
    assert_eq!(parse_reg_name("A7"), Some(15));
    assert_eq!(parse_reg_name("SP"), Some(15));
    assert_eq!(parse_reg_name("SR"), Some(16));
    assert_eq!(parse_reg_name("PC"), Some(17));
    assert_eq!(parse_reg_name("D8"), None);
    assert_eq!(parse_reg_name("A8"), None);
    assert_eq!(parse_reg_name("Z0"), None);
    assert_eq!(parse_reg_name(""), None);
}

/// Render each panel and the menu into a presentation-sized frame.
/// Always asserts the drawing landed inside the right region; with
/// COPPERLINE_UI_PREVIEW=1 also saves PNGs for eyeballing layout.
#[test]
fn wrap_text_keeps_long_lines_whole() {
    // Short lines pass through untouched.
    assert_eq!(wrap_text("Machine: A1200", 32, 31), vec!["Machine: A1200"]);
    // Long lines wrap at word boundaries with nothing dropped.
    let rom = "ROM: system v3.1 a1200 release image path rom";
    let wrapped = wrap_text(rom, 32, 31);
    assert!(wrapped.len() > 1);
    assert!(wrapped.iter().all(|l| l.chars().count() <= 32));
    assert_eq!(wrapped.join(" "), rom);
    // Words longer than a whole line are hard-split, not dropped.
    let long_word = "a".repeat(70);
    let wrapped = wrap_text(&long_word, 32, 31);
    assert_eq!(wrapped.concat(), long_word);
    // Empty input still yields one (blank) line.
    assert_eq!(wrap_text("", 32, 31), vec![String::new()]);
}

#[test]
fn cpu_wait_view_lights_denied_slots_and_dims_the_rest() {
    use super::super::window::{texture_height, texture_width};

    let scale = 1;
    let (w, h) = (texture_width(scale), texture_height(scale));
    let mut frame = vec![0u8; w * h * 4];
    // Two texels per colour clock and per line, so slot interiors are
    // readable past the display-box and selection outlines.
    let raster = Rect {
        x: 20,
        y: 20,
        w: 8,
        h: 8,
    };
    let mut cpu_waits = vec![b'.'; 16];
    cpu_waits[0] = b'B';
    let trace = AnalyzerTraceView {
        frame: 1,
        seconds: 0.0,
        rows: 4,
        cols: 4,
        line_cck: 4,
        visible_start_vpos: 0,
        visible_lines: 2,
        display_hpos_start: 0,
        display_hpos_end: 4,
        owner_cck: [0, 16, 0, 0, 0, 0, 0, 0, 0],
        blitter_busy_cck: 0,
        blitter_starve_cck: [0; 9],
        partial: false,
        selected_vpos: 0,
        selected_hpos: 0,
        selected_owner: "bitplane",
        selected_owner_code: b'B',
        owners: vec![b'B'; 16],
        records: None,
        markers: Vec::new(),
        selected_blit: None,
        diw_v: None,
        diw_h_cck: None,
        ddf_cck: None,
        cpu_waits,
        cpu_wait_cck: 1,
        cpu_wait_by_class: [0, 1, 0, 0, 0, 0, 0, 0, 0],
        cpu_wait_by_kind: [1, 0, 0, 0],
        top_stalled_pcs: vec![(0x1000, 1, None)],
        selected_cpu_wait_code: b'B',
    };
    let pixel = |frame: &[u8], x: usize, y: usize| -> [u8; 4] {
        frame[(y * w + x) * 4..(y * w + x) * 4 + 4]
            .try_into()
            .unwrap()
    };

    // Owner view: every slot is the bitplane colour.
    draw_owner_heatmap(&mut frame, raster, &trace, None, false, false, scale);
    assert_eq!(pixel(&frame, 21, 21), owner_color(b'B').to_le_bytes());
    assert_eq!(pixel(&frame, 22, 21), owner_color(b'B').to_le_bytes());

    // CPU wait view: the denied slot keeps the denier's colour at full
    // brightness, its neighbour drops to a quarter.
    draw_owner_heatmap(&mut frame, raster, &trace, None, false, true, scale);
    assert_eq!(pixel(&frame, 21, 21), cpu_wait_color(b'B').to_le_bytes());
    assert_eq!(
        pixel(&frame, 22, 21),
        quarter_rgba(owner_color(b'B')).to_le_bytes()
    );

    // The gutter bars a quarter of its width for the one waited clock of
    // the four on line 0, and nothing on a line without waits.
    let gutter = Rect {
        x: 40,
        y: 20,
        w: 10,
        h: 4,
    };
    draw_cpu_wait_gutter(&mut frame, gutter, &trace, scale);
    assert_eq!(pixel(&frame, 41, 20), cpu_wait_color(b'B').to_le_bytes());
    assert_eq!(pixel(&frame, 43, 20), ANALYZER_GUTTER_BG.to_le_bytes());
    assert_eq!(pixel(&frame, 41, 21), ANALYZER_GUTTER_BG.to_le_bytes());
}

#[test]
fn cpu_wait_counters_stop_at_the_column_bottom() {
    use super::super::window::{texture_height, texture_width};

    // The busiest trace the column can be asked to show: every denier
    // class and access kind non-zero and a full PC list, which needs more
    // rows than the raster is tall.
    let scale = 1;
    let (w, h) = (texture_width(scale), texture_height(scale));
    let mut frame = vec![0u8; w * h * 4];
    let trace = AnalyzerTraceView {
        frame: 1,
        seconds: 0.0,
        rows: 4,
        cols: 4,
        line_cck: 4,
        visible_start_vpos: 0,
        visible_lines: 2,
        display_hpos_start: 0,
        display_hpos_end: 4,
        owner_cck: [1; 9],
        blitter_busy_cck: 0,
        blitter_starve_cck: [0; 9],
        partial: false,
        selected_vpos: 0,
        selected_hpos: 0,
        selected_owner: "idle",
        selected_owner_code: b'.',
        owners: vec![b'.'; 16],
        records: None,
        markers: Vec::new(),
        selected_blit: None,
        diw_v: None,
        diw_h_cck: None,
        ddf_cck: None,
        cpu_waits: vec![b'.'; 16],
        cpu_wait_cck: 45,
        cpu_wait_by_class: [1, 2, 3, 4, 5, 6, 7, 8, 9],
        cpu_wait_by_kind: [10, 11, 12, 12],
        top_stalled_pcs: (0..ANALYZER_TOP_STALLED_PCS as u32)
            .map(|i| (0x1000 + i * 2, 100 - i, None))
            .collect(),
        selected_cpu_wait_code: b'.',
    };
    let (x, top, bottom) = (40usize, 20usize, 20 + 246);
    draw_cpu_wait_counters(&mut frame, x, top, bottom, &trace, scale);

    let painted = |frame: &[u8], y: usize| -> bool {
        (x..x + 200).any(|px| frame[(y * w + px) * 4 + 3] != 0)
    };
    // The heading, totals and the first class rows are drawn...
    assert!(painted(&frame, top + 2));
    assert!(painted(&frame, top + 3 * 12 + 2));
    // ...and nothing lands on or below the bound, however much was asked.
    for y in bottom..bottom + 60 {
        assert!(!painted(&frame, y), "row {y} painted past the bound");
    }
    // A bound too short for anything draws nothing at all.
    let mut bare = vec![0u8; w * h * 4];
    draw_cpu_wait_counters(&mut bare, x, top, top + 4, &trace, scale);
    assert!((top..top + 60).all(|y| !painted(&bare, y)));
}

#[test]
fn frame_analyzer_top_edge_overlays_clip_to_raster() {
    use super::super::window::{texture_height, texture_width};

    let scale = 1;
    let (w, h) = (texture_width(scale), texture_height(scale));
    let mut frame = vec![0u8; w * h * 4];
    let raster = Rect {
        x: 20,
        y: 20,
        w: 40,
        h: 20,
    };
    let trace = AnalyzerTraceView {
        frame: 1,
        seconds: 0.0,
        rows: 4,
        cols: 4,
        line_cck: 4,
        visible_start_vpos: 0,
        visible_lines: 2,
        display_hpos_start: 0,
        display_hpos_end: 4,
        owner_cck: [0; 9],
        blitter_busy_cck: 0,
        blitter_starve_cck: [0; 9],
        partial: false,
        selected_vpos: 0,
        selected_hpos: 0,
        selected_owner: "idle",
        selected_owner_code: b'.',
        owners: vec![b'.'; 16],
        records: None,
        markers: vec![AnalyzerMarker {
            vpos: 0,
            hpos: 1,
            offset: 0x096,
            value: 0x0000,
            source: "copper",
            copper_addr: None,
            colour: PANEL_TEXT_ACCENT,
        }],
        selected_blit: None,
        diw_v: None,
        diw_h_cck: None,
        ddf_cck: None,
        cpu_waits: vec![b'.'; 16],
        cpu_wait_cck: 0,
        cpu_wait_by_class: [0; 9],
        cpu_wait_by_kind: [0; 4],
        top_stalled_pcs: Vec::new(),
        selected_cpu_wait_code: b'.',
    };

    draw_owner_heatmap(&mut frame, raster, &trace, None, false, false, scale);

    let pixel = |frame: &[u8], x: usize, y: usize| -> [u8; 4] {
        frame[(y * w + x) * 4..(y * w + x) * 4 + 4]
            .try_into()
            .unwrap()
    };
    for x in raster.x - 4..raster.x + raster.w + 4 {
        assert_eq!(pixel(&frame, x, raster.y - 1), [0, 0, 0, 0]);
    }
    for y in raster.y..raster.y + raster.h {
        assert_eq!(pixel(&frame, raster.x - 1, y), [0, 0, 0, 0]);
    }
    assert_eq!(
        pixel(&frame, raster.x, raster.y),
        BUTTON_EDGE_LIGHT.to_le_bytes()
    );
}

/// A Library page with a few games in it, for the previews.
#[cfg(feature = "game-library")]
fn library_preview_state() -> LauncherState {
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::WhdloadLibrary;

    state.library.db.set_known(vec![
        crate::gamelib::Known {
            file: "GoldenAxe_v1.5_0017.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Golden Axe".to_string(),
                year: Some("1990".to_string()),
                publisher: Some("Virgin".to_string()),
                // Long enough that before the two-line cap it ran down
                // over the Run button and off the panel.
                developer: Some(
                    "Adventuresoft UK Ltd - Teoman Irmak, Matt Ellis, Antony M. Scott, \
                         Graham Lilley, Alan Bridgman, Brian Howarth, Richard Turner"
                        .to_string(),
                ),
                players: Some("1 - 2 (2)".to_string()),
                front_sha1: std::env::var("WHDLOAD_COVER_SHA1").ok(),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "JamesPond2_v2.0_AGA_1354.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "James Pond 2: Codename RoboCod".to_string(),
                year: Some("1991".to_string()),
                publisher: Some("Millennium".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        // Two releases of one game, which is what the Version row is
        // for: nothing else tells them apart in the list.
        crate::gamelib::Known {
            file: "CannonFodder2_v1.12_Fr_2578.zip".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Cannon Fodder 2".to_string(),
                year: Some("1994".to_string()),
                publisher: Some("Virgin".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "CannonFodder2_v1.11_0104.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Cannon Fodder 2".to_string(),
                year: Some("1994".to_string()),
                publisher: Some("Virgin".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "SimCity_v1.0_2193.lha".to_string(),
            game: None,
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "Lotus2_v1.2_0451.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Lotus Turbo Challenge 2".to_string(),
                year: Some("1991".to_string()),
                publisher: Some("Gremlin".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "Turrican2_v2.1_1120.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Turrican II".to_string(),
                year: Some("1991".to_string()),
                publisher: Some("Rainbow Arts".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "SensibleSoccer_v1.1_0788.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Sensible Soccer".to_string(),
                year: Some("1992".to_string()),
                publisher: Some("Renegade".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "Superfrog_v1.0_0233.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Superfrog".to_string(),
                year: Some("1993".to_string()),
                publisher: Some("Team17".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "ChaosEngine_v2.0_0912.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "The Chaos Engine".to_string(),
                year: Some("1993".to_string()),
                publisher: Some("Renegade".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "Xenon2_v1.0_0355.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Xenon 2: Megablast".to_string(),
                year: Some("1989".to_string()),
                publisher: Some("Image Works".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "Turrican_v1.3_0087.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Turrican".to_string(),
                year: Some("1990".to_string()),
                publisher: Some("Rainbow Arts".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "Lemmings_v1.1_0621.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Lemmings".to_string(),
                year: Some("1991".to_string()),
                publisher: Some("Psygnosis".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "Pinball_Dreams_v1.0_0498.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Pinball Dreams".to_string(),
                year: Some("1992".to_string()),
                publisher: Some("21st Century".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
        crate::gamelib::Known {
            file: "SpeedBall2_v1.2_0733.lha".to_string(),
            game: Some(crate::gamelib::Game {
                name: "Speedball 2".to_string(),
                year: Some("1990".to_string()),
                publisher: Some("Image Works".to_string()),
                ..crate::gamelib::Game::default()
            }),
            manual: false,
            slave_sha1: None,
        },
    ]);
    state
        .library
        .db
        .toggle_favourite("GoldenAxe_v1.5_0017.lha", "Golden Axe");
    // One whose package is not in the library, which is what the
    // Remove tick beside it is for.
    state
        .library
        .db
        .toggle_favourite("Deleted_v1.0_0001.lha", "A Deleted Game");
    // More than the box holds, so the preview covers a favourites list
    // with its scroll arrows up rather than only the short case.
    for (file, name) in [
        ("Lotus2_v1.2_0451.lha", "Lotus Turbo Challenge 2"),
        ("Turrican2_v2.1_1120.lha", "Turrican II"),
        ("SensibleSoccer_v1.1_0788.lha", "Sensible Soccer"),
        ("Superfrog_v1.0_0233.lha", "Superfrog"),
        ("ChaosEngine_v2.0_0912.lha", "The Chaos Engine"),
        ("Xenon2_v1.0_0355.lha", "Xenon 2: Megablast"),
    ] {
        state.library.db.toggle_favourite(file, name);
    }
    state.library.db_loaded = true;
    let want_art = std::env::var("WHDLOAD_COVERS").is_ok_and(|covers| {
        state.library.covers = crate::gamelib::Covers::new(covers.into());
        true
    });
    // A folder of packages, so the list has something in it. The store
    // above supplies the metadata; this is only what is on the disk.
    if let Ok(games) = std::env::var("WHDLOAD_GAMES") {
        state
            .setup
            .set_path(LauncherField::WhdloadGames, games.into());
        state.refresh_library(std::path::Path::new("/nonexistent"));
    }
    // Land on the game the preview has art and a full set of metadata
    // for, rather than on whatever sorts first: an empty art frame and
    // three filled rows is the case the other previews cover.
    let golden = state
        .library
        .games
        .entries()
        .iter()
        .position(|entry| entry.game.as_ref().is_some_and(|g| g.name == "Golden Axe"));
    if let Some(at) = golden {
        state.select_library_game(at);
    }
    // The window loop asks for the selected game's art each frame
    // and draws it when it lands; a preview renders once, so it
    // waits for the picture rather than rendering the empty box.
    if want_art {
        state.poll_library_covers();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if state.poll_library_covers() {
                break;
            }
        }
    }
    state
}

#[cfg(feature = "game-library")]
#[test]
fn a_version_runs_to_two_lines_and_no_further() {
    // A package name is longer than the column and both ends matter:
    // which game at the front, which release at the back.
    let name = "CannonFodder2_v1.11_0104.lha";
    let column = LIBRARY_COVER + 2 * LIBRARY_COVER_BEZEL;
    let lines = wrap_to_width(name, column);
    assert!(lines.len() <= LIBRARY_VERSION_LINES, "{lines:?}");
    assert_eq!(lines.concat(), name, "the whole name should be shown");

    // Two releases stay distinguishable across the wrap, which is the
    // point of showing it at all.
    let other = wrap_to_width("CannonFodder2_v1.12_Fr_2578.zip", column);
    assert_ne!(lines, other);

    // The editor stops where the page stops showing.
    assert_eq!(library_version_max(), 34);
    assert!(name.chars().count() <= library_version_max());
}

#[cfg(feature = "game-library")]
#[test]
fn cover_art_keeps_its_shape_inside_the_box() {
    let box_rect = Rect {
        x: 100,
        y: 50,
        w: 120,
        h: 130,
    };

    // A cover is portrait: it fills the height and is centred across.
    let at = fit_within(600, 800, box_rect).expect("fits");
    assert_eq!((at.w, at.h), (97, 130));
    assert_eq!(at.y, 50, "a portrait cover should fill the height");
    assert!(at.x >= box_rect.x && at.x + at.w <= box_rect.x + box_rect.w);
    // Centred to the pixel the odd margin allows.
    let (left, right) = (at.x - box_rect.x, box_rect.w - at.w - (at.x - box_rect.x));
    assert!(left.abs_diff(right) <= 1, "off centre: {left} and {right}");

    // A landscape one fills the width instead, with the margin above
    // and below.
    let at = fit_within(800, 400, box_rect).expect("fits");
    assert_eq!((at.w, at.h), (120, 60));
    assert_eq!(at.x, 100);
    let (top, bottom) = (at.y - box_rect.y, box_rect.h - at.h - (at.y - box_rect.y));
    assert!(top.abs_diff(bottom) <= 1, "off centre: {top} and {bottom}");

    // Square art in a not-quite-square box still keeps its shape.
    let at = fit_within(64, 64, box_rect).expect("fits");
    assert_eq!(at.w, at.h);

    // A picture far wider than it is tall does not collapse to nothing,
    // and one with no pixels is not drawn at all.
    assert_eq!(fit_within(4000, 1, box_rect).expect("fits").h, 1);
    assert!(fit_within(0, 10, box_rect).is_none());
    assert!(fit_within(10, 0, box_rect).is_none());
    assert!(fit_within(10, 10, Rect { w: 0, ..box_rect }).is_none());
}

#[test]
fn panels_render_into_their_rects() {
    use super::super::window::{texture_height, texture_width};

    let scale = 1;
    let (w, h) = (texture_width(scale), texture_height(scale));
    let save_at = |frame: &[u8], name: &str, w: usize, h: usize| {
        if !crate::envcfg::flag("COPPERLINE_UI_PREVIEW") {
            return;
        }
        let path = format!("target/ui-preview-{name}.png");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(frame).unwrap();
        eprintln!("saved {path}");
    };
    // Almost every preview is drawn at the base scale into the same
    // frame size, so most callers need say nothing about it.
    let save = |frame: &[u8], name: &str| save_at(frame, name, w, h);
    let panel_has_title_bar = |frame: &[u8], panel: &Panel| {
        let rect = panel_rect(panel);
        let probe = ((rect.y + 10) * w + rect.x + 4) * 4;
        let pixel = &frame[probe..probe + 4];
        pixel == PANEL_TITLE_BG.to_le_bytes()
    };

    let mut frame = vec![0u8; w * h * 4];
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::About),
    };
    let data = PanelViewData::About(crate::video::about::AboutView {
        machine_lines: vec![
            "Machine: A1200".to_string(),
            "CPU: M68EC020 @ 14 MHz".to_string(),
            "Chipset: AGA (Alice/Lisa, PAL)".to_string(),
            "RAM: 2048K chip, 4096K fast".to_string(),
            "ROM: system v3.1 a1200 release image path rom".to_string(),
            "Floppy drives: 1".to_string(),
        ],
        // Deep into the entrance so the snapshot shows the settled page.
        elapsed_ms: 60_000,
        machine_fitted: true,
    });
    draw(&mut frame, scale, &ui, None, Some(&data));
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "about");

    // Mid-entrance: the title half printed, the wave half drawn in.
    // The entrance is a pure function of elapsed time, so comparing
    // against the just-opened page makes deterministic assertions:
    // by mid-entrance the title's first slot has its settled letter
    // and wave columns stand on the left, while the strip's right
    // edge is still untouched -- the reveal front has not reached it.
    let about_at = |elapsed_ms: u64| {
        let mut frame = vec![0u8; w * h * 4];
        let data = PanelViewData::About(crate::video::about::AboutView {
            machine_lines: vec![crate::config::ABOUT_PLACEHOLDER_LINE.to_string()],
            elapsed_ms,
            machine_fitted: false,
        });
        draw(&mut frame, scale, &ui, None, Some(&data));
        frame
    };
    let opened = about_at(0);
    let mid = about_at(2_500);
    let region_differs = |a: &[u8], b: &[u8], x0: usize, x1: usize, y0: usize, y1: usize| {
        (y0..y1)
            .any(|y| a[(y * w + x0) * 4..(y * w + x1) * 4] != b[(y * w + x0) * 4..(y * w + x1) * 4])
    };
    let rect = panel_rect(&Panel::About);
    let title_y = rect.y + TITLE_H + 14;
    let slot0 = rect.x + (rect.w - 240) / 2;
    assert!(
        region_differs(&mid, &opened, slot0, slot0 + 24, title_y, title_y + 24),
        "title's first letter should have settled by mid-entrance"
    );
    let base = rect.y + rect.h - 8;
    assert!(
        region_differs(&mid, &opened, rect.x, rect.x + rect.w / 4, base - 8, base),
        "wave columns should have arrived on the left by mid-entrance"
    );
    assert!(
        !region_differs(
            &mid,
            &opened,
            rect.x + rect.w - 24,
            rect.x + rect.w,
            base - 60,
            base
        ),
        "the reveal front should not have reached the right edge yet"
    );
    save(&mid, "about-opening");

    let mut frame = vec![0u8; w * h * 4];
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Shortcuts),
    };
    draw(
        &mut frame,
        scale,
        &ui,
        None,
        Some(&PanelViewData::Shortcuts),
    );
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "shortcuts");

    let mut frame = vec![0u8; w * h * 4];
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::DropChooser(DropChooserState {
            disks: vec![
                std::path::PathBuf::from("turrican2-disk1.adf"),
                std::path::PathBuf::from("turrican2-disk2.adf"),
            ],
            disk_label: "turrican2-disk1.adf".to_string(),
            drives: vec![
                DropDriveEntry {
                    drive: 0,
                    label: "DF0: workbench.adf".to_string(),
                },
                DropDriveEntry {
                    drive: 1,
                    label: "DF1 (empty)".to_string(),
                },
            ],
        })),
    };
    draw(&mut frame, scale, &ui, Some(UiControl::DropDrive(1)), None);
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    // The hovered drive button renders inside the panel rect.
    let panel = ui.panel.as_ref().unwrap();
    if let Panel::DropChooser(state) = panel {
        let rect = panel_rect(panel);
        let buttons = drop_chooser_button_rects(rect, state);
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[1].0, UiControl::DropDrive(1));
        let button = buttons[1].1;
        assert!(button.x >= rect.x && button.x + button.w <= rect.x + rect.w);
        assert!(button.y >= rect.y && button.y + button.h <= rect.y + rect.h);
        let probe = ((button.y + 2) * w + button.x + 2) * 4;
        assert_eq!(&frame[probe..probe + 4], &BUTTON_FACE_HOVER.to_le_bytes());
    } else {
        unreachable!();
    }
    save(&frame, "drop-chooser");

    // The pre-drop hover hint dims the display without opening a panel.
    let mut frame = vec![0xFFu8; w * h * 4];
    draw_drop_hint(&mut frame, scale);
    // The scrim darkens the display area but not the status bar below.
    assert!(frame[0] < 0xFF);
    assert_eq!(frame[present_height() * w * 4], 0xFF);
    save(&frame, "drop-hint");

    let mut frame = vec![0u8; w * h * 4];
    let session = crate::gamepad::CalibrationSession::new();
    let rows = (0..crate::gamepad::CalibrationSession::step_count())
        .map(|index| CalRow {
            label: crate::gamepad::CalibrationSession::step_label(index),
            binding: if index == 0 {
                "axis 10031-".to_string()
            } else {
                String::new()
            },
            current: index == 1,
        })
        .collect();
    let data = PanelViewData::Calibration(CalibrationView {
        pad_line: "Controller: USB Retro Pad".to_string(),
        rows,
        status: "Push and hold the control on the pad.".to_string(),
    });
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Calibration(session)),
    };
    draw(
        &mut frame,
        scale,
        &ui,
        Some(UiControl::CalCancel),
        Some(&data),
    );
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "calibration");

    // Input Mapping: self-contained, with a row armed for capture so the
    // highlighted state is drawn too.
    let mut frame = vec![0u8; w * h * 4];
    let mut map_panel = InputMapPanel::new(crate::keymap::KeyMap::default());
    map_panel.capturing = Some(crate::keymap::JoyControl::Fire);
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::InputMap(Box::new(map_panel))),
    };
    draw(&mut frame, scale, &ui, Some(UiControl::RemapSave), None);
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "input-mapping");
    let mut frame = vec![0u8; w * h * 4];
    let mut lines = vec![
        DbgLine::plain("PC 00FC0E44   SR 2700 [S IPL7 xnzvc]"),
        DbgLine::plain(""),
        DbgLine::plain("D0 00000000   D1 00000001   D2 00C00FFC   D3 DEADBEEF"),
        DbgLine::plain("A0 00DFF000   A1 00C00000   A2 00000000   A3 00FC0000"),
        DbgLine::plain(""),
    ];
    for i in 0..20 {
        let line = format!("00FC{:04X}  MOVE.W #$4000,(A0)", 0x0E44 + i * 4);
        lines.push(if i == 0 {
            DbgLine::hilit(line)
        } else {
            DbgLine::plain(line)
        });
    }
    let data = PanelViewData::Debugger(Box::new(DebuggerView {
        running: false,
        reverse_available: true,
        status: "paused frame 1234 24.68s".to_string(),
        lines,
        bitmap: None,
        video: None,
        audio: None,
    }));
    let mut panel = DebuggerPanel::new();
    panel.entry = "C00000".to_string();
    panel.entry_active = true;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Debugger(panel)),
    };
    draw(
        &mut frame,
        scale,
        &ui,
        Some(UiControl::DebugStep),
        Some(&data),
    );
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "debugger");

    // Break tab: toggle buttons plus the breakpoint/watch listing.
    let mut frame = vec![0u8; w * h * 4];
    let mut lines: Vec<DbgLine> = (0..BREAK_TAB_HEADER_LINES)
        .map(|_| DbgLine::plain(""))
        .collect();
    lines.push(DbgLine::hilit("Breakpoint at $C033C2"));
    lines.push(DbgLine::plain(""));
    lines.push(DbgLine::plain("Breakpoints:"));
    lines.push(DbgLine::plain("  $C033C2"));
    lines.push(DbgLine::plain("Watchpoints (word):"));
    lines.push(DbgLine::plain("  $C09580  now 0012"));
    lines.push(DbgLine::plain("Register watches (stop on write):"));
    lines.push(DbgLine::plain("  DMACON ($096)"));
    let data = PanelViewData::Debugger(Box::new(DebuggerView {
        running: false,
        reverse_available: true,
        status: "paused frame 1234 24.68s".to_string(),
        lines,
        bitmap: None,
        video: None,
        audio: None,
    }));
    let mut panel = DebuggerPanel::new();
    panel.tab = DebugTab::Break;
    panel.entry = "DFF096".to_string();
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Debugger(panel)),
    };
    draw(
        &mut frame,
        scale,
        &ui,
        Some(UiControl::DebugRegToggle),
        Some(&data),
    );
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "debugger-break");

    // Waveform tab: Arm/Stop buttons plus a capture status listing.
    let mut frame = vec![0u8; w * h * 4];
    let mut lines: Vec<DbgLine> = (0..WAVEFORM_TAB_HEADER_LINES)
        .map(|_| DbgLine::plain(""))
        .collect();
    lines.push(DbgLine::hilit(
        "waveform capturing: trigger pc=0xC033C2, duration 2f, signals all",
    ));
    lines.push(DbgLine::plain("  -> out.vcd"));
    lines.push(DbgLine::plain("  14204 / 141748 cck, 35872 samples"));
    lines.push(DbgLine::plain(""));
    lines.push(DbgLine::plain(
        "Trigger:  NOW  PC=ADDR  BEAM=VPOS[:HPOS]  REG=OFF  TIME=SECS",
    ));
    let data = PanelViewData::Debugger(Box::new(DebuggerView {
        running: true,
        reverse_available: false,
        status: "running frame 1234 24.68s".to_string(),
        lines,
        bitmap: None,
        video: None,
        audio: None,
    }));
    let mut panel = DebuggerPanel::new();
    panel.tab = DebugTab::Waveform;
    panel.entry = "PC=C033C2 2F".to_string();
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Debugger(panel)),
    };
    draw(
        &mut frame,
        scale,
        &ui,
        Some(UiControl::DebugWaveArm),
        Some(&data),
    );
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    // The Arm button must be enabled: its entry spec parses.
    assert!(crate::waveform::parse_wave_args("PC=C033C2 2F".split_whitespace()).is_ok());
    save(&frame, "debugger-waveform");

    // Audio tab: the four Paula channels plus every line-mixed source
    // row (CD, MIDI synth, Toccata, MHI), with representative state,
    // mute buttons (AUD2 and MHI shown muted), and synthetic scope
    // traces.
    let mut frame = vec![0u8; w * h * 4];
    let wave = |amp: f32, cycles: f32| -> Vec<i8> {
        (0..220)
            .map(|i| {
                let t = i as f32 / 220.0 * cycles * std::f32::consts::TAU;
                (amp * t.sin()) as i8
            })
            .collect()
    };
    let header = "DMACON 8203  DMAEN on  AUDEN 1 1 . .   ADKCON 0000  -".to_string();
    let channels = vec![
        AudioRowView {
            text: vec![
                DbgLine::hilit("AUD0 [Running]  DMA on  IRQ -"),
                DbgLine::plain("  LC 021A3C  LEN 0140  PER 01B0  VOL 40"),
                DbgLine::plain("  PTR 021B1C  words 00E2  acc 00A4  ph1  out -12"),
                DbgLine::plain("  pending: next-word"),
            ],
            muted: false,
            scope: wave(96.0, 3.0),
        },
        AudioRowView {
            text: vec![
                DbgLine::plain("AUD1 [StartPending]  DMA on  IRQ pend"),
                DbgLine::plain("  LC 030000  LEN 0080  PER 00F0  VOL 3F"),
                DbgLine::plain("  PTR 030000  words 0080  acc 0000  ph0  out 0"),
                DbgLine::plain("  pending: dma-req"),
            ],
            muted: false,
            scope: wave(60.0, 6.0),
        },
        AudioRowView {
            text: vec![
                DbgLine::plain("AUD2 [Off]  DMA off  IRQ -"),
                DbgLine::plain("  LC 000000  LEN 0000  PER 0000  VOL 00"),
                DbgLine::plain("  PTR 000000  words 0000  acc 0000  ph0  out 0"),
            ],
            muted: true,
            scope: wave(40.0, 2.0),
        },
        AudioRowView {
            text: vec![
                DbgLine::plain("AUD3 [Manual]  DMA off  IRQ -"),
                DbgLine::plain("  LC 000000  LEN 0000  PER 0140  VOL 20"),
                DbgLine::plain("  PTR 000000  words 0000  acc 0050  ph1  out 7"),
                DbgLine::plain("  pending: dma-disable manual"),
            ],
            muted: false,
            scope: wave(48.0, 9.0),
        },
    ];
    let extras = vec![
        AudioExtraRow {
            kind: AudioExtraKind::Cd,
            row: AudioRowView {
                text: vec![
                    DbgLine::hilit("CD-DA  playing"),
                    DbgLine::plain("  peak  72"),
                ],
                muted: false,
                scope: wave(72.0, 4.0),
            },
        },
        AudioExtraRow {
            kind: AudioExtraKind::Synth,
            row: AudioRowView {
                text: vec![
                    DbgLine::hilit("MIDI  MT-32  sounding"),
                    DbgLine::plain("  peak  58"),
                ],
                muted: false,
                scope: wave(58.0, 5.0),
            },
        },
        AudioExtraRow {
            kind: AudioExtraKind::Toccata,
            row: AudioRowView {
                text: vec![
                    DbgLine::hilit("Toccata  playing"),
                    DbgLine::plain("  44100 Hz 16-bit stereo  FIFO  612/1024"),
                ],
                muted: false,
                scope: wave(84.0, 7.0),
            },
        },
        AudioExtraRow {
            kind: AudioExtraKind::Mhi,
            row: AudioRowView {
                text: vec![
                    DbgLine::hilit("MHI  playing  44100 Hz"),
                    DbgLine::plain("  queue 3  vol 100  pan 50  B/M/T 50/50/50"),
                ],
                muted: true,
                scope: wave(66.0, 11.0),
            },
        },
    ];
    let audio = AudioScopeView {
        header,
        channels,
        extras,
    };
    let data = PanelViewData::Debugger(Box::new(DebuggerView {
        running: false,
        reverse_available: true,
        status: "paused frame 1234 24.68s".to_string(),
        lines: Vec::new(),
        bitmap: None,
        video: None,
        audio: Some(audio),
    }));
    let mut panel = DebuggerPanel::new();
    panel.tab = DebugTab::Audio;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Debugger(panel)),
    };
    draw(
        &mut frame,
        scale,
        &ui,
        Some(UiControl::DebugAudioMute(0)),
        Some(&data),
    );
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "debugger-audio");

    // IO Map tab: the register grid with a selection and decode pane.
    let mut frame = vec![0u8; w * h * 4];
    let mut lines: Vec<DbgLine> = Vec::new();
    lines.push(DbgLine::plain(
        "custom registers $DFF000-$DFF1FE  (page 2/4; arrows/wheel move, $ box jumps)",
    ));
    lines.push(DbgLine::plain(""));
    for row in 0..26 {
        let mut text = String::new();
        for col in 0..3 {
            let off = 0x0A0 + (col * 26 + row) * 2;
            let cursor = if off == 0x0100 { '>' } else { ' ' };
            text.push_str(&format!(
                "{cursor}{off:03X} {:<8} {:04X}   ",
                crate::debugger::custom_reg_name(off as u16),
                0x2200 + off
            ));
        }
        lines.push(if row == 16 {
            DbgLine::hilit(text.trim_end().to_string())
        } else {
            DbgLine::plain(text.trim_end().to_string())
        });
    }
    lines.push(DbgLine::plain(""));
    lines.push(DbgLine::hilit("$100 BPLCON0 = $5A00".to_string()));
    lines.push(DbgLine::plain("  HAM COLOR".to_string()));
    lines.push(DbgLine::plain("  BPU=5".to_string()));
    let data = PanelViewData::Debugger(Box::new(DebuggerView {
        running: false,
        reverse_available: true,
        status: "paused frame 1234 24.68s".to_string(),
        lines,
        bitmap: None,
        video: None,
        audio: None,
    }));
    let mut panel = DebuggerPanel::new();
    panel.tab = DebugTab::IoMap;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Debugger(panel)),
    };
    draw(&mut frame, scale, &ui, None, Some(&data));
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "debugger-iomap");

    // Video tab: layer-isolation toggles (plane 2 and sprite 5 hidden),
    // sprite rows with synthetic thumbnails, and an AGA palette grid.
    let mut frame = vec![0u8; w * h * 4];
    let sprites = (0..8)
        .map(|sprite| {
            let rows = 16 + sprite;
            let mut thumb = vec![0u32; rows * 16];
            for row in 0..rows {
                for x in 0..16usize {
                    if (x + row) % 4 == sprite % 4 {
                        thumb[row * 16 + x] =
                            rgba(80 + 20 * sprite as u32, 200 - 20 * sprite as u32, 160);
                    }
                }
            }
            SpriteRowView {
                text: format!(
                    "SPR{sprite} v44-{} h{} dma lines {rows}",
                    60 + sprite,
                    128 + sprite * 16
                ),
                thumb,
                thumb_rows: rows,
            }
        })
        .collect();
    let palette = (0..256)
        .map(|idx| {
            let idx = idx as u32;
            rgba((idx * 5) & 0xFF, (idx * 3) & 0xFF, 255 - (idx & 0xFF))
        })
        .collect();
    let data = PanelViewData::Debugger(Box::new(DebuggerView {
        running: false,
        reverse_available: true,
        status: "paused frame 1234 24.68s".to_string(),
        lines: Vec::new(),
        bitmap: None,
        video: Some(VideoView {
            header: "BPLCON0 5200: 5 planes lores  HAM   DMACON: BPLEN on SPREN on".to_string(),
            plane_mask: 0xFD,
            nplanes: 5,
            sprite_mask: 0xDF,
            sprites,
            palette,
        }),
        audio: None,
    }));
    let mut panel = DebuggerPanel::new();
    panel.tab = DebugTab::Video;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Debugger(panel)),
    };
    draw(
        &mut frame,
        scale,
        &ui,
        Some(UiControl::DebugPlaneToggle(0)),
        Some(&data),
    );
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "debugger-video");

    // Frame analyzer with the picture underlay ticked: a synthetic PAL
    // frame trace (refresh/bitplane/copper/blitter stripes) over a
    // gradient picture, to eyeball the beam-grid alignment of the
    // underlay against the white display box.
    let mut frame = vec![0u8; w * h * 4];
    let (rows, cols) = (312usize, 227usize);
    let mut owners = vec![b'.'; rows * cols];
    for vpos in 0..rows {
        for hpos in 0..cols {
            let owner = if hpos < 4 {
                b'R'
            } else if (60..260).contains(&vpos) && (0x38..0xD0).contains(&hpos) && hpos % 2 == 0 {
                b'B'
            } else if hpos == 0x28 && vpos % 8 == 0 {
                b'C'
            } else if (100..140).contains(&vpos) && (0x10..0x28).contains(&hpos) {
                b'L'
            } else if (0x0D..0x11).contains(&hpos) && vpos % 2 == 0 {
                b'A'
            } else {
                b'.'
            };
            owners[vpos * cols + hpos] = owner;
        }
    }
    // CPU waits: a code loop in chip RAM losing every other fetch slot to
    // the display on a third of the lines, and the blitter (nice, then
    // BLTPRI on its last rows) holding it off across its stripe.
    let mut cpu_waits = vec![b'.'; rows * cols];
    for vpos in 0..rows {
        for hpos in 0..cols {
            let wait = if (60..260).contains(&vpos)
                && vpos % 3 == 0
                && (0x38..0xD0).contains(&hpos)
                && hpos % 2 == 0
            {
                b'B'
            } else if (100..130).contains(&vpos) && (0x10..0x28).contains(&hpos) && hpos % 4 != 3 {
                b'L'
            } else if (130..140).contains(&vpos) && (0x10..0x28).contains(&hpos) {
                b'N'
            } else {
                b'.'
            };
            cpu_waits[vpos * cols + hpos] = wait;
        }
    }
    let underlay_rows = 285usize;
    let mut under_fb = vec![0u32; FB_WIDTH * underlay_rows];
    for (i, pix) in under_fb.iter_mut().enumerate() {
        let (x, y) = (i % FB_WIDTH, i / FB_WIDTH);
        // Gradient plus vertical bars so structure is visible through
        // the dimming.
        let bar = if (x / 64) % 2 == 0 { 96 } else { 0 };
        *pix = rgba(
            (x * 255 / FB_WIDTH) as u32,
            (y * 255 / underlay_rows) as u32 / 2 + bar,
            160,
        );
    }
    let trace = AnalyzerTraceView {
        frame: 1234,
        seconds: 24.68,
        rows,
        cols,
        line_cck: 227,
        visible_start_vpos: 0x1A,
        visible_lines: underlay_rows,
        display_hpos_start: 0x30,
        display_hpos_end: 227,
        owner_cck: [4400, 19000, 0, 0, 1600, 900, 2400, 6200, 36000],
        blitter_busy_cck: 3000,
        blitter_starve_cck: [0, 400, 0, 0, 0, 0, 0, 200, 0],
        partial: false,
        selected_vpos: 120,
        selected_hpos: 0x40,
        selected_owner: "bitplane",
        selected_owner_code: b'B',
        owners,
        records: None,
        markers: vec![AnalyzerMarker {
            vpos: 0x40,
            hpos: 0x28,
            offset: 0x180,
            value: 0x0F00,
            source: "copper",
            copper_addr: None,
            colour: PANEL_TEXT_ACCENT,
        }],
        selected_blit: Some("in blit #2 (20x100 D $060000)".to_string()),
        // A standard PAL display window and fetch bounds, so the
        // preview shows the DIW box and DDF verticals.
        diw_v: Some((0x2C, 0x12C)),
        diw_h_cck: Some((0x81 / 2, 0x1C1 / 2)),
        ddf_cck: Some((0x38, 0xD0)),
        cpu_waits,
        cpu_wait_cck: 6420,
        cpu_wait_by_class: [0, 5016, 0, 0, 0, 0, 1080, 324, 0],
        cpu_wait_by_kind: [3900, 1800, 720, 0],
        top_stalled_pcs: vec![
            (0x0002_1A3C, 2140, Some("[exec] AllocMem".into())),
            (0x0002_1A40, 1690, None),
            (0x0002_0F10, 610, None),
        ],
        selected_cpu_wait_code: b'B',
    };
    let data = PanelViewData::FrameAnalyzer(Box::new(FrameAnalyzerView {
        running: false,
        status: "paused frame 1234 24.68s".to_string(),
        scrub: true,
        heat: None,
        resources: None,
        blits: None,
        trace: Some(trace),
        underlay: Some(AnalyzerUnderlayView {
            fb: std::rc::Rc::new(under_fb),
            rows: underlay_rows,
            width: FB_WIDTH,
        }),
    }));
    let mut panel = FrameAnalyzerPanel::new();
    panel.show_underlay = true;
    panel.show_scrub = true;
    panel.selected_vpos = 120;
    panel.selected_hpos = 0x40;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::FrameAnalyzer(panel)),
    };
    draw(
        &mut frame,
        scale,
        &ui,
        Some(UiControl::AnalyzerUnderlay),
        Some(&data),
    );
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "frame-analyzer");

    // The same trace in the CPU wait view: denied slots lit, the rest
    // dimmed, the wait breakdown and stalled PCs in the counters column.
    let mut frame = vec![0u8; w * h * 4];
    let mut panel = FrameAnalyzerPanel::new();
    panel.show_cpu_wait = true;
    panel.selected_vpos = 120;
    panel.selected_hpos = 0x40;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::FrameAnalyzer(panel)),
    };
    draw(&mut frame, scale, &ui, None, Some(&data));
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "frame-analyzer-cpu-wait");

    // Frame analyzer, Memory tab: the address space instead of the
    // beam. A window with a few busy regions so the map, the census
    // column and the selected-cell readout all have something to say.
    let mut frame = vec![0u8; w * h * 4];
    let mut lit = Vec::new();
    for cell in 0..heatmap::CELLS {
        let (cx, cy) = (cell % heatmap::GRID, cell / heatmap::GRID);
        // A bitplane buffer as a solid block, a copper list as a
        // column, blitter and CPU traffic scattered through the heap.
        let toucher = if (24..56).contains(&cy) && cx < 200 {
            Some(crate::heatmap::Toucher::Bitplane)
        } else if cx == 12 && (8..40).contains(&cy) {
            Some(crate::heatmap::Toucher::Copper)
        } else if (60..70).contains(&cy) && (cx / 8) % 3 == 0 {
            Some(crate::heatmap::Toucher::Blitter)
        } else if cy > 200 && (cx * cy) % 97 == 0 {
            Some(crate::heatmap::Toucher::CpuWrite)
        } else if cy == 3 && cx % 5 == 0 {
            Some(crate::heatmap::Toucher::Audio)
        } else {
            None
        };
        if let Some(toucher) = toucher {
            lit.push((cell, toucher));
        }
    }
    let mut heat = heat_view(&lit);
    let selected = 40 * heatmap::GRID + 100;
    heat.selected = Some(AnalyzerHeatCell {
        cell: selected,
        toucher: Some(crate::heatmap::Toucher::Bitplane.name()),
        colour: crate::heatmap::Toucher::Bitplane.colour(),
        age_frames: Some(1),
    });
    let data = PanelViewData::FrameAnalyzer(analyzer_view(None, Some(heat)));
    let mut panel = FrameAnalyzerPanel::new();
    panel.tab = AnalyzerTab::Memory;
    panel.heat_selected = Some(selected);
    panel.heat_presets = vec![
        heat_preset("Chip", 0, 0x0020_0000),
        heat_preset("Slow", 0x00C0_0000, 0x0010_0000),
        heat_preset("Fast", 0x0020_0000, 0x0080_0000),
        heat_preset("24-bit", 0, heatmap::DEFAULT_SPAN),
    ];
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::FrameAnalyzer(panel)),
    };
    draw(
        &mut frame,
        scale,
        &ui,
        Some(UiControl::AnalyzerTab(AnalyzerTab::Memory)),
        Some(&data),
    );
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "frame-analyzer-memory");

    // Frame analyzer, Resources tab: the registry table over a decoded
    // bitmap preview.
    let mut frame = vec![0u8; w * h * 4];
    let preview = crate::video::resource_preview::decode_bitmap(
        &{
            // A 32x16x2 stripe picture: plane 0 lights the first byte
            // column, plane 1 the last.
            let mut data = vec![0u8; 4 * 16 * 2];
            for row in 0..16 {
                data[row * 4] = 0xF0;
                data[64 + row * 4 + 3] = 0xFF;
            }
            data
        },
        32,
        16,
        2,
        false,
        false,
        false,
        &[0xFF20_2020, 0xFF00_00FF, 0xFF00_FF00, 0xFFFF_0000],
    );
    let resources_view = AnalyzerResourcesView {
        rows: vec![
            AnalyzerResourceRowView {
                text: "screen       bitmap     $020000     51200  320x256x5 ilv".to_string(),
                selected: true,
            },
            AnalyzerResourceRowView {
                text: "pal          palette    $030000        64  32 entries".to_string(),
                selected: false,
            },
            AnalyzerResourceRowView {
                text: "cop          copperlist $040000      1000".to_string(),
                selected: false,
            },
        ],
        hidden_above: 0,
        hidden_below: 0,
        detail: Some(AnalyzerResourceDetail::Bitmap(preview)),
        exportable: true,
    };
    let data = PanelViewData::FrameAnalyzer(Box::new(FrameAnalyzerView {
        running: false,
        status: "paused frame 4321".to_string(),
        trace: None,
        underlay: None,
        scrub: false,
        heat: None,
        resources: Some(resources_view),
        blits: None,
    }));
    let mut panel = FrameAnalyzerPanel::new();
    panel.tab = AnalyzerTab::Resources;
    panel.resource_selected = Some(0x2_0000);
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::FrameAnalyzer(panel)),
    };
    draw(&mut frame, scale, &ui, None, Some(&data));
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "frame-analyzer-resources");

    // Console: a session transcript over the prompt line.
    let mut frame = vec![0u8; w * h * 4];
    let mut console = ConsolePanel::default();
    console.push_output("Copperline debugger console. Type HELP for commands.");
    console.push_output("> B C033C2");
    console.push_output("breakpoint $C033C2 set");
    console.push_output("> RUN");
    console.push_output("running (PAUSE stops; breakpoints report here or on stop)");
    console.push_output("> PAUSE");
    console.push_output("!Breakpoint at $C033C2");
    console.push_output("pc $C033C2  MOVE.W #$4000,$00DFF09A   sr 2300  beam v44 h101  frame 1234");
    console.push_output("> D");
    console.push_output("C033C2  MOVE.W #$4000,$00DFF09A");
    console.push_output("C033C8  RTS");
    console.input = "MEM C00000 40".to_string();
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Console(console)),
    };
    draw(&mut frame, scale, &ui, None, None);
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "console");

    // Configuration screen: an A1200 on the Memory tab.
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.setup.select_model(Some(MachineModel::A1200));
    state
        .setup
        .set_path(LauncherField::Rom, std::path::PathBuf::from("kick31.rom"));
    state.tab = LauncherTab::Memory;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, Some(UiControl::LauncherRun), None);
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "launcher");

    // Configuration screen: the Zorro tab with a WASM plugin board whose
    // config-option schema renders an editable field per option.
    let manifest_path = std::env::temp_dir().join(format!(
        "copperline-ui-preview-board-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &manifest_path,
        r#"
            name = "Demo NIC"
            zorro = 2
            type = "wasm"
            size = "64K"
            manufacturer = 5192
            product = 16
            wasm = "demo.wasm"
            [config]
            mode = "bridged"
            [[option]]
            key = "mode"
            label = "Mode"
            type = "enum"
            choices = ["bridged", "nat"]
            [[option]]
            key = "verbose"
            label = "Verbose"
            type = "bool"
            [[option]]
            key = "mtu"
            label = "MTU"
            type = "int"
            default = 1500
            [[option]]
            key = "rom"
            label = "Boot ROM"
            type = "file"
            [[option]]
            key = "mac"
            label = "MAC address"
            type = "string"
            default = "02:00:10:00:00:01"
        "#,
    )
    .unwrap();
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.setup.add_zorro(manifest_path.clone());
    state.tab = LauncherTab::Zorro;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "launcher-zorro");
    let _ = std::fs::remove_file(&manifest_path);

    // Configuration screen: the Storage tab on an A1200, with an IDE
    // master mounted from a host directory and given a volume-name
    // override (the editable box beside Browse).
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.setup.select_model(Some(MachineModel::A1200));
    state.setup.set_path(
        LauncherField::IdeMaster,
        std::path::PathBuf::from("/host/games"),
    );
    state
        .setup
        .set_drive_name(LauncherField::IdeMaster, "Games".to_string());
    state.tab = LauncherTab::Storage;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "launcher-storage");

    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.toggle_netplay();
    state.netplay.peer = "192.168.1.11:19732".into();
    state.netplay.code = "8b21488dae9544f591adf03e291ce976".into();
    state.tab = LauncherTab::Netplay;
    let ui = UiState {
        panel: Some(Panel::Launcher(Box::new(state))),
        ..UiState::default()
    };
    draw(&mut frame, scale, &ui, None, None);
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "launcher-netplay");

    #[cfg(feature = "netplay-internet")]
    {
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.toggle_netplay();
        state.netplay.internet = true;
        state.netplay.code = "CLNI1.eyJlbmRwb2ludCI6...".into();
        state.tab = LauncherTab::Netplay;
        let ui = UiState {
            panel: Some(Panel::Launcher(Box::new(state))),
            ..UiState::default()
        };
        let mut frame = vec![0u8; w * h * 4];
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-netplay-internet");
    }

    // Configuration screen: the Input tab, with the live routing
    // summary spelled out under the rows (two joysticks, so the
    // numpad stand-in line shows).
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    // Port 1: Mouse -> Joystick, making a two-stick setup.
    state.setup.cycle(LauncherField::Port1Device, true);
    state.tab = LauncherTab::Input;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    // The summary header landed below the rows: some text pixel is lit
    // on its line inside the settings pane.
    let rect = panel_rect(&Panel::Launcher(Box::new(LauncherState::new(
        launcher::MachineSetup::default(),
    ))));
    let header_y = launcher_row_y(
        rect,
        launcher::rows(
            LauncherTab::Input,
            crate::config::ParallelDevice::None,
            crate::config::SerialMode::default(),
            false,
            false,
        )
        .len()
            + 1,
    );
    let row = &frame[(header_y * w + launcher_pane_x(rect)) * 4
        ..(header_y * w + launcher_pane_x(rect) + 200) * 4];
    assert!(
        row.chunks_exact(4)
            .any(|px| px == PANEL_TEXT_DIM.to_le_bytes()),
        "routing summary header not drawn"
    );
    save(&frame, "launcher-input");

    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::IoPorts;
    state.setup.cycle(LauncherField::ParallelDevice, true); // None -> Printer
    state.setup.cycle(LauncherField::ParallelDevice, true); // Printer -> Sampler
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-io-ports");

    // The CPU tab on the default (68000) machine: the JIT accelerator
    // row is greyed with its "needs 68020+" reason instead of a toggle.
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::Cpu;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-cpu");

    // I/O Ports with the A2065 on the NAT backend, to check the
    // non-determinism warning under the rows.
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::IoPorts;
    // Not fitted -> Isolated -> Loopback -> NAT (where the NAT cannot
    // come up this wraps back to Not fitted and no warning is shown).
    for _ in 0..3 {
        state.setup.cycle(LauncherField::Ethernet, true);
    }
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-ethernet-warning");

    // I/O Ports with the printer selected and a long output path set, to
    // check the "Output file" value and the Browse/Clear placement.
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::IoPorts;
    state.setup.cycle(LauncherField::ParallelDevice, true); // None -> Printer
    state.setup.set_path(
        LauncherField::ParallelOutput,
        std::path::PathBuf::from("/Users/me/Documents/amiga/captures/printer-output.txt"),
    );
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-printer");

    // I/O Ports with the serial port dialling out: the Connect address
    // box the tcp-connect mode brings with it, mid-edit so the caret and
    // the highlight show too.
    #[cfg(feature = "midi")]
    {
        let mut frame = vec![0u8; w * h * 4];
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.tab = LauncherTab::IoPorts;
        while state.setup.serial_mode() != crate::config::SerialMode::TcpConnect {
            state.setup.cycle(LauncherField::SerialMode, true);
        }
        state.begin_edit_serial_addr(LauncherField::SerialConnect, false);
        for c in "bbs.example.com:1337".chars() {
            state.edit_push(c);
        }
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-serial-tcp");
    }

    // The Host Folder sub-page reached from the Storage tab.
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::HostFs;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-host-mounts");

    // The WHDLoad sub-page reached from the Storage tab, with a game
    // chosen so the full host path shows.
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::Whdload;
    state.setup.set_path(
        LauncherField::WhdloadGame,
        std::path::PathBuf::from("/Users/me/Amiga/whdload/Turrican.lha"),
    );
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-whdload");

    // The ROM tab: each path row with the identification of the image on
    // it greyed underneath.
    let mut frame = vec![0u8; w * h * 4];
    let mut setup = launcher::MachineSetup::default();
    setup.select_model(Some(MachineModel::Cd32));
    setup.set_path(
        LauncherField::Rom,
        std::path::PathBuf::from("kick40060.CD32"),
    );
    setup.set_path(
        LauncherField::ExtendedRom,
        std::path::PathBuf::from("cd32ext.rom"),
    );
    let mut state = LauncherState::new(setup);
    state.tab = LauncherTab::Rom;
    state.set_rom_note_for_test(LauncherField::Rom, "Kickstart 3.1 (40.60) CD32");
    state.set_rom_note_for_test(LauncherField::ExtendedRom, "CD32 extended ROM (40.60)");
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
    save(&frame, "launcher-rom");

    // The Library page with the A-Z shortcut row up, since that is the
    // part whose fit across the width is worth looking at.
    #[cfg(feature = "game-library")]
    {
        let mut frame = vec![0u8; w * h * 4];
        let mut state = library_preview_state();
        // The row appears on the size of the list, so the preview
        // needs a list that reaches it.
        let mut more: Vec<crate::gamelib::Known> = state.library.db.known().to_vec();
        for (at, name) in [
            "Agony",
            "Battle Squadron",
            "Deluxe Galaga",
            "Elite",
            "Frontier",
            "Hired Guns",
            "It Came from the Desert",
            "Kick Off 2",
            "Moonstone",
            "North & South",
            "Obitus",
            "Rick Dangerous",
            "Utopia",
            "Walker",
            "Xenon",
            "Yo! Joe!",
        ]
        .into_iter()
        .enumerate()
        {
            more.push(crate::gamelib::Known {
                file: format!("filler{at}.lha"),
                game: Some(crate::gamelib::Game {
                    name: name.to_string(),
                    ..crate::gamelib::Game::default()
                }),
                manual: false,
                slave_sha1: None,
            });
        }
        state.library.db.set_known(more);
        state.library.games =
            crate::gamelib::Library::known(std::path::Path::new("/games"), &state.library.db);
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        // With a letter under the pointer: the hover is the part that
        // has to read at this size.
        let over = launcher::az_bucket_of("Golden Axe");
        let hover = Some(UiControl::LauncherLibraryJump(over));
        draw(&mut frame, scale, &ui, hover, None);
        save(&frame, "launcher-whdload-library-az");
    }

    // The Library page, with WHDLoad beside it in the strip and
    // a couple of games standing in for a real folder.
    #[cfg(feature = "game-library")]
    {
        let mut frame = vec![0u8; w * h * 4];
        let state = library_preview_state();
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-whdload-library");
    }

    // The sign-in dialog, over the Configuration page it is opened
    // from, with something typed into both boxes.
    #[cfg(feature = "game-library")]
    {
        let mut frame = vec![0u8; w * h * 4];
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.tab = LauncherTab::Whdload;
        let mut login = launcher::LoginDialog {
            user: "hobbo91".to_string(),
            ..Default::default()
        };
        login.focus_on(launcher::LoginField::Pass);
        for c in "not-a-real-password".chars() {
            login.insert(c);
        }
        // Part-way back through it, which is what the caret is for.
        for _ in 0..8 {
            login.caret_move(launcher::CaretMove::Left);
        }
        state.login = Some(login);
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-openretro-login");
    }

    // The metadata editor, over the Library page it is opened from.
    #[cfg(feature = "game-library")]
    {
        let mut frame = vec![0u8; w * h * 4];
        let mut state = library_preview_state();
        state.open_meta_editor();
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-meta-editor");
    }

    // Both lists run to their far end, where the arrows swap over:
    // the one pointing back into the list lights and the one pointing
    // off it greys.
    #[cfg(feature = "game-library")]
    {
        let mut frame = vec![0u8; w * h * 4];
        let state = library_preview_state();
        let mut ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        let rect = launcher_panel_rect(&ui).expect("the launcher is up");
        if let Some(Panel::Launcher(state)) = ui.panel.as_mut() {
            let whdload_entry = state.setup.whdload_enabled();
            state.scroll_library(isize::MAX, library_visible_rows(rect, whdload_entry));
            state.scroll_favourites(isize::MAX, library_favourite_rows(rect, whdload_entry));
        }
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-whdload-library-scrolled");
    }

    // An empty Library page: nothing scanned yet, so Scan is greyed
    // and the art frame is drawn at the size it reserves.
    #[cfg(feature = "game-library")]
    {
        let mut frame = vec![0u8; w * h * 4];
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.tab = LauncherTab::WhdloadLibrary;
        state.library.db_loaded = true;
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-whdload-library-empty");
    }

    // One of two releases of the same game: the Version row says which,
    // since nothing else in the list does.
    #[cfg(feature = "game-library")]
    {
        let mut frame = vec![0u8; w * h * 4];
        let mut state = library_preview_state();
        state.select_library_game(0);
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-whdload-library-version");
    }

    // The Storage tab, whose six sub-page links wrap onto a second row.
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::Storage;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-storage");

    // The Create Image workshop: its two pages, and the geometry editor
    // behind the hard-drive one.
    for (tab, name) in [
        (LauncherTab::CreateFloppy, "launcher-new-floppy"),
        (LauncherTab::CreateHard, "launcher-new-hard"),
        (LauncherTab::CreateGeometry, "launcher-new-geometry"),
    ] {
        let mut frame = vec![0u8; w * h * 4];
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.tab = tab;
        state.workshop.geometry_custom = true;
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, name);
    }

    // The Boot Priority sub-page: an A1200 with two IDE drives -- the master
    // bootable at 0, the slave with its Bootable box cleared -- and one
    // SCSI unit carrying a disk of its own.
    let mut frame = vec![0u8; w * h * 4];
    let mut setup = launcher::MachineSetup::default();
    setup.select_model(Some(MachineModel::A1200));
    // A controller with one unit filled: the page lists that unit and
    // leaves the empty six out.
    setup.cycle(LauncherField::ScsiController, true);
    setup.set_path(LauncherField::IdeMaster, std::path::PathBuf::from("wb.hdf"));
    setup.set_drive_bootpri(LauncherField::IdeMasterBoot, Some(0));
    setup.set_path(
        LauncherField::IdeSlave,
        std::path::PathBuf::from("games.hdf"),
    );
    setup.toggle_drive_boot(LauncherField::IdeSlaveBoot);
    setup.set_path(
        LauncherField::ScsiUnit0,
        std::path::PathBuf::from("work.hdf"),
    );
    let mut state = LauncherState::new(setup);
    state.tab = LauncherTab::BootPriority;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-boot-priority");

    // The same page on a machine with every bay filled: two IDE drives, a
    // full seven-unit SCSI chain and a RIPPLE board's four, which is more
    // boot order than one page holds. The first page fills and offers the
    // rest; the second carries them under the same column titles.
    let mut full = launcher::MachineSetup::default();
    full.select_model(Some(MachineModel::A1200));
    full.cycle(LauncherField::ScsiController, true);
    full.cycle(LauncherField::LideBoard, true);
    for f in [
        LauncherField::IdeMaster,
        LauncherField::IdeSlave,
        LauncherField::ScsiUnit0,
        LauncherField::ScsiUnit1,
        LauncherField::ScsiUnit2,
        LauncherField::ScsiUnit3,
        LauncherField::ScsiUnit4,
        LauncherField::ScsiUnit5,
        LauncherField::ScsiUnit6,
        LauncherField::LideDrive0,
        LauncherField::LideDrive1,
        LauncherField::LideDrive2,
        LauncherField::LideDrive3,
    ] {
        full.set_path(f, std::path::PathBuf::from("disk.hdf"));
    }
    for (tab, name) in [
        (LauncherTab::BootPriority, "launcher-boot-priority-full"),
        (
            LauncherTab::BootPriorityMore(2),
            "launcher-boot-priority-page2",
        ),
    ] {
        let mut frame = vec![0u8; w * h * 4];
        let mut state = LauncherState::new(full.clone());
        state.tab = tab;
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, name);
    }

    // The runtime menu, opened over a running machine: the top level,
    // with a category open beside it.
    let mut frame = vec![0u8; w * h * 4];
    let slots: [Option<String>; menu::SAVE_SLOTS] =
        std::array::from_fn(|i| (i == 2).then(|| "2026/07/31 14:05".to_string()));
    let devices = ["Built-in Output".to_string()];
    let none: [String; 0] = [];
    let rows = menu::build(&menu::MenuState {
        player: false,
        player_save_states: false,
        paused: false,
        cartridge: None,
        fullscreen: false,
        status_bar_hidden: false,
        bezel: crate::config::BezelStyle::None,
        perf_overlay: false,
        warp: false,
        warp_speed: WarpSpeed::Max,
        rewind: false,
        recording: false,
        input_recording: false,
        autofire_hz: 0,
        run_ahead_frames: 0,
        joystick_input_mode: JoystickInputMode::Gamepad,
        keyboard_panel: false,
        port_devices: [
            crate::bus::PortDevice::Mouse,
            crate::bus::PortDevice::Joystick,
        ],
        pixel_aspect: PixelAspect::Tv,
        scaling: crate::config::DisplayScaling::Smooth,
        autocrop: false,
        tv_centre: crate::config::TvCentre::default(),
        tv_centre_applies: true,
        shader: crate::config::ShaderKind::None,
        shader_strength: 1.0,
        custom_shader_available: false,
        tint: crate::config::Tint::None,
        menu_scale: crate::config::MenuScale::Normal,
        floppy_speed: 100,
        floppy_speed_applies: true,
        audio_filter: crate::config::AudioFilterMode::Auto,
        audio_output: menu::AudioOutputChoice::Default,
        audio_devices: &devices,
        midi_in: "",
        midi_out: "",
        midi_inputs: &none,
        midi_outputs: &none,
        mt32_available: false,
        mt32_selected: false,
        mt32_attached: false,
        mt32_input: false,
        mt32_panel: false,
        mt32_control_rom: None,
        mt32_pcm_rom: None,
        csynth_available: false,
        csynth_attached: false,
        csynth_panel: false,
        csynth_mt32_mode: "auto",
        csynth_custom_font: false,
        mt32_lcd: crate::config::Mt32Lcd::Oled,
        sampler_input: "",
        sampler_inputs: &none,
        sampler_gain: 0.0,
        save_slots: &slots,
    });
    let mut nav = menu::MenuNav::default();
    nav.point_at(5);
    nav.descend(&rows);
    let ui = UiState {
        menu_open: true,
        menu_rows: rows,
        menu_nav: nav,
        ..Default::default()
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "menu-open");

    // The same menu at 2x, which has to stay inside the display.
    let mut frame = vec![0u8; w * h * 4];
    crate::video::set_menu_scale(crate::config::MenuScale::Large);
    draw(&mut frame, scale, &ui, None, None);
    let levels = ui.menu_nav.levels(&ui.menu_rows);
    for column in menu_columns(&levels, &ui.menu_nav) {
        assert!(
            column.x + column.w <= texture_width(1) && column.y + column.h <= present_height(),
            "2x menu column {column:?} leaves the display"
        );
    }
    crate::video::set_menu_scale(crate::config::MenuScale::Normal);
    save(&frame, "menu-open-2x");

    // A/V & Emu: the Audio category (the default landing), with the
    // Audio / Video / Emulation nav buttons at the top, Audio highlighted.
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::AvAudio;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-av-audio");

    // The Video category, reached from the same nav row.
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::AvVideo;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-av-video");

    // The Display category: the host window's own settings, under the same
    // nav row (which wraps -- five categories, four to a row).
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::AvDisplay;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-av-display");

    // The Emulation category: pacing, warp speed, and the warp-boot rows.
    let mut frame = vec![0u8; w * h * 4];
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.tab = LauncherTab::AvEmulation;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-av-emulation");

    // The Floppy tab with two drives wired in: each drive is a greyed "DFn:"
    // heading with indented settings; DF2/DF3 are hidden until enabled.
    let mut frame = vec![0u8; w * h * 4];
    let mut setup = launcher::MachineSetup::default();
    while setup.value_label(LauncherField::FloppyDrives) != "2" {
        setup.cycle(LauncherField::FloppyDrives, true);
    }
    setup.set_path(
        LauncherField::Df0Image,
        std::path::PathBuf::from("workbench.adf"),
    );
    let mut state = LauncherState::new(setup);
    state.tab = LauncherTab::Floppy;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-floppy");

    // The Floppy tab with DF1 turned over to a real drive: its media row
    // shows the interface and a Configure button, and its Physical drive box
    // is ticked while DF0 stays an ordinary image drive.
    let mut frame = vec![0u8; w * h * 4];
    let mut setup = launcher::MachineSetup::default();
    while setup.value_label(LauncherField::FloppyDrives) != "2" {
        setup.cycle(LauncherField::FloppyDrives, true);
    }
    setup.set_path(
        LauncherField::Df0Image,
        std::path::PathBuf::from("workbench.adf"),
    );
    setup.set_drive_bridged(1, true);
    let mut state = LauncherState::new(setup);
    state.tab = LauncherTab::Floppy;
    let ui = UiState {
        menu_open: false,
        menu_rows: Vec::new(),
        menu_nav: menu::MenuNav::default(),
        panel: Some(Panel::Launcher(Box::new(state))),
    };
    draw(&mut frame, scale, &ui, None, None);
    save(&frame, "launcher-floppy-bridge");

    // A drive row holding a real disk must not swallow the rest of the
    // panel. The Unmount button replaces that row's own two buttons and
    // nothing else: Run, Defaults, Load, Save and the page links all kept
    // working before, and an early return here quietly killed every one
    // of them until the disk was cleared.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut setup = launcher::MachineSetup::default();
        setup.select_model(Some(crate::config::MachineModel::A1200));
        setup.set_host_disks_for_test(vec![launcher::HostDiskRow {
            id: "disk4".to_string(),
            fingerprint: None,
            volume: "SanDisk".to_string(),
            size: "31.9 GB".to_string(),
            mounted: Vec::new(),
            writable: false,
            attach: None,
        }]);
        setup.select_host_disk(0);
        setup.mount_host_disks().expect("A1200 has IDE");
        let mut state = LauncherState::new(setup);
        state.tab = LauncherTab::Storage;
        let panel = Panel::Launcher(Box::new(state));
        let rect = panel_rect(&panel);
        let Panel::Launcher(state) = &panel else {
            unreachable!()
        };
        for (control, button) in launcher_action_rects(rect) {
            let centre = (
                (button.x + button.w / 2) as i32,
                (button.y + button.h / 2) as i32,
            );
            assert_eq!(
                launcher_control_at(rect, state, centre),
                Some(control),
                "a mounted disk must not block {control:?}"
            );
        }
    }

    // A preview of the Paths page and of the Save menu, for eyes.
    {
        let mut frame = vec![0u8; w * h * 4];
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.tab = LauncherTab::AvPaths;
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-paths");

        // And with two rows given directories of their own, which is
        // the only state in which a Reset button exists. `set_path` on
        // a Paths row adopts into the process-wide store that
        // paths::tests assert against, hence the guard.
        let _guard = crate::paths::adopted_store_lock();
        let mut frame = vec![0u8; w * h * 4];
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.tab = LauncherTab::AvPaths;
        state.setup.set_path(
            LauncherField::PathsBase,
            std::path::PathBuf::from("/Volumes/AMIGA"),
        );
        state.setup.set_path(
            LauncherField::PathsScreenshots,
            std::path::PathBuf::from("/Users/someone/Pictures/Amiga screenshots"),
        );
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-paths-set");

        // Drawn at a scale of more than one, which is where anything
        // handing an unscaled rect to a scaled draw shows up: at scale
        // one the two are the same and the mistake is invisible.
        let big = 2;
        let (bw, bh) = (texture_width(big), texture_height(big));
        let mut frame = vec![0u8; bw * bh * 4];
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.save_dialog = true;
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        // With the pointer on the button whose description is longest,
        // which is the one that has to fit.
        draw(
            &mut frame,
            big,
            &ui,
            Some(UiControl::LauncherSaveDefault),
            None,
        );
        save_at(&frame, "launcher-save-menu", bw, bh);

        let mut frame = vec![0u8; w * h * 4];
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.confirm_reset = true;
        let ui = UiState {
            menu_open: false,
            menu_rows: Vec::new(),
            menu_nav: menu::MenuNav::default(),
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-confirm-reset");
    }

    // A Paths row must offer exactly the buttons it draws. A Reset
    // that is not there but still answers would put a row back to
    // inheriting on a click meant for nothing at all, and on the base
    // -- where Browse and Reset swap places rather than sitting side
    // by side -- the two would land on each other's rectangles.
    {
        // `set_path` on a Paths row adopts into the process-wide store
        // that paths::tests assert against.
        let _guard = crate::paths::adopted_store_lock();
        let probe = |set: bool, field: LauncherField| {
            let mut setup = launcher::MachineSetup::default();
            if set {
                setup.set_path(field, std::path::PathBuf::from("/probe/dir"));
            }
            let mut state = LauncherState::new(setup);
            state.tab = LauncherTab::AvPaths;
            let panel = Panel::Launcher(Box::new(state));
            let rect = panel_rect(&panel);
            let row = launcher::rows(
                LauncherTab::AvPaths,
                Default::default(),
                Default::default(),
                false,
                false,
            )
            .iter()
            .position(|r| r.field == field)
            .expect("the field has a row");
            let row_y = launcher_row_y(rect, row) + launcher_nav_block_h(LauncherTab::AvPaths);
            let (browse, reset) = launcher_path_rects(rect, row_y);
            let at = |r: Rect| {
                panel_control_at(&panel, ((r.x + r.w / 2) as i32, (r.y + r.h / 2) as i32))
            };
            (
                at(browse) == Some(UiControl::LauncherBrowse(field)),
                at(reset) == Some(UiControl::LauncherClear(field)),
            )
        };
        // An ordinary row: Browse always, Reset only once it was set.
        assert_eq!(probe(false, LauncherField::PathsScreenshots), (true, false));
        assert_eq!(probe(true, LauncherField::PathsScreenshots), (true, true));
        // The base swaps them.
        assert_eq!(probe(false, LauncherField::PathsBase), (true, false));
        assert_eq!(probe(true, LauncherField::PathsBase), (false, true));
    }

    // A Clear with nothing behind it takes no clicks. The drive rows
    // and the floppy rows draw their own buttons rather than going
    // through the Path arm, so each stands trial here: empty, the
    // button is greyed and dead; with an image chosen, it answers.
    {
        let probe = |set: bool, tab: LauncherTab, field: LauncherField, kind: RowKind| {
            let mut setup = launcher::MachineSetup::default();
            // A machine that has all the rows: the default model
            // carries no IDE port, and a row that does not apply
            // takes no clicks whatever its buttons say.
            setup.select_model(Some(MachineModel::A1200));
            if set {
                setup.set_path(field, std::path::PathBuf::from("/probe/disk.img"));
            }
            let idx = launcher::rows(tab, Default::default(), Default::default(), false, false)
                .iter()
                .filter(|r| setup.row_on_page(tab, r.field))
                .position(|r| r.field == field && r.kind == kind)
                .expect("the field has a visible row");
            let mut state = LauncherState::new(setup);
            state.tab = tab;
            let panel = Panel::Launcher(Box::new(state));
            let rect = panel_rect(&panel);
            let nav = if tab.has_top_nav() {
                launcher_nav_block_h(tab)
            } else {
                0
            };
            let row_y = launcher_row_y(rect, idx) + nav;
            let (_, clear) = launcher_path_rects(rect, row_y);
            let got = panel_control_at(
                &panel,
                (
                    (clear.x + clear.w / 2) as i32,
                    (clear.y + clear.h / 2) as i32,
                ),
            );
            got == Some(UiControl::LauncherClear(field))
        };
        for (tab, field, kind) in [
            (
                LauncherTab::Floppy,
                LauncherField::Df0Image,
                RowKind::FloppyMedia,
            ),
            (
                LauncherTab::Storage,
                LauncherField::IdeMaster,
                RowKind::Drive,
            ),
        ] {
            assert!(!probe(false, tab, field, kind), "{field:?} empty must grey");
            assert!(probe(true, tab, field, kind), "{field:?} set must answer");
        }
    }

    // The FMV module's action is live even with no custom path: Remove writes
    // the explicit empty-slot state, then the same button offers Default and
    // restores the bundled ROM. This is a tri-state module control, not an
    // ordinary path Clear that goes dead while the default is in force.
    {
        let probe = |setup: launcher::MachineSetup| {
            let mut state = LauncherState::new(setup);
            state.tab = LauncherTab::Rom;
            let panel = Panel::Launcher(Box::new(state));
            let rect = panel_rect(&panel);
            let setup = match &panel {
                Panel::Launcher(state) => &state.setup,
                _ => unreachable!(),
            };
            let idx = launcher::rows(
                LauncherTab::Rom,
                Default::default(),
                Default::default(),
                false,
                false,
            )
            .iter()
            .filter(|r| setup.row_on_page(LauncherTab::Rom, r.field))
            .position(|r| r.field == LauncherField::FmvRom)
            .expect("the FMV ROM row is visible on CD32");
            let nav = if LauncherTab::Rom.has_top_nav() {
                launcher_nav_block_h(LauncherTab::Rom)
            } else {
                0
            };
            let row_y = launcher_row_y(rect, idx) + nav;
            let (_, action) = launcher_path_rects(rect, row_y);
            let control = panel_control_at(
                &panel,
                (
                    (action.x + action.w / 2) as i32,
                    (action.y + action.h / 2) as i32,
                ),
            );
            (launcher_clear_label(setup, LauncherField::FmvRom), control)
        };

        let mut setup = launcher::MachineSetup::default();
        setup.select_model(Some(MachineModel::Cd32));
        assert_eq!(
            probe(setup.clone()),
            (
                "Remove",
                Some(UiControl::LauncherClear(LauncherField::FmvRom))
            )
        );
        setup.toggle_fmv_module();
        assert_eq!(
            probe(setup),
            (
                "Default",
                Some(UiControl::LauncherClear(LauncherField::FmvRom))
            )
        );
    }

    // The confirm over Reset default answers every click on the panel:
    // Yes only on its own button, and anything else -- Run included --
    // cancels. A question about deleting something must not be
    // answerable by a click that missed.
    {
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.confirm_reset = true;
        let panel = Panel::Launcher(Box::new(state));
        let rect = panel_rect(&panel);
        let centre = |r: Rect| ((r.x + r.w / 2) as i32, (r.y + r.h / 2) as i32);
        let (yes, cancel) = launcher_confirm_button_rects(rect);
        assert_eq!(
            panel_control_at(&panel, centre(yes)),
            Some(UiControl::LauncherConfirmReset)
        );
        // Named places again, not the panel's centre: that sits inside
        // the dialog and would land on a button if it ever resized.
        let dialog = launcher_confirm_rect(rect);
        let title = Rect {
            x: dialog.x,
            y: dialog.y,
            w: dialog.w,
            h: TITLE_H,
        };
        let [load, _, _, run] = launcher_action_rects(rect);
        for elsewhere in [cancel, title, load.1, run.1] {
            assert_eq!(
                panel_control_at(&panel, centre(elsewhere)),
                Some(UiControl::LauncherCancelReset),
                "a click off Yes should cancel"
            );
        }
        // And here too the gadget answers only for itself.
        assert_eq!(
            panel_control_at(&panel, centre(close_button_rect(dialog))),
            Some(UiControl::LauncherDialogClose)
        );
        // The two dialogs are the same height, so they read as one
        // window asking two things rather than two boxes.
        assert_eq!(dialog.h, launcher_save_dialog_rect(rect).h);
    }

    // The Save dialog: every button reachable, nothing under it
    // reachable while it is up, and anything that is not one of the
    // three -- its close gadget included -- putting it away.
    {
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.save_dialog = true;
        let panel = Panel::Launcher(Box::new(state));
        let rect = panel_rect(&panel);
        let dialog = launcher_save_dialog_rect(rect);
        let centre = |r: Rect| ((r.x + r.w / 2) as i32, (r.y + r.h / 2) as i32);
        let items = launcher_save_dialog_rects(rect);
        for (control, item) in items {
            assert_eq!(
                panel_control_at(&panel, centre(item)),
                Some(control),
                "{control:?} is not reachable in the Save dialog"
            );
            assert!(
                item.x >= dialog.x && item.x + item.w <= dialog.x + dialog.w,
                "{control:?} is outside the dialog"
            );
            // Readable rather than truncated by the button it sits in,
            // which is what sizes the dialog in the first place.
            let fits = item.w.saturating_sub(8) / font::GLYPH_W;
            let label = launcher_action_label(control);
            assert!(
                label.chars().count() <= fits,
                "{label:?} does not fit its button"
            );
        }
        for (a, b) in items.iter().zip(items.iter().skip(1)) {
            assert!(a.1.x + a.1.w <= b.1.x, "the buttons overlap each other");
            assert_eq!(a.1.y, b.1.y, "the buttons should be one row");
        }
        // The close gadget, the dialog's own body below the buttons,
        // and the bar underneath: none of them does anything but put
        // the dialog away. Probed at named places rather than at the
        // panel's centre, which is inside the dialog and moves onto a
        // button the moment the dialog changes height.
        let close = Rect {
            x: dialog.x + dialog.w - 18,
            y: dialog.y + 4,
            w: 12,
            h: 12,
        };
        let body = Rect {
            x: dialog.x,
            y: dialog.y + TITLE_H,
            w: dialog.w,
            h: SAVE_DIALOG_MARGIN,
        };
        let [load, _, _, run] = launcher_action_rects(rect);
        for elsewhere in [body, load.1, run.1] {
            assert_eq!(
                panel_control_at(&panel, centre(elsewhere)),
                Some(UiControl::LauncherSave),
                "a click off the three should only put the dialog away"
            );
        }
        // The gadget answers as itself, and nothing else does. It is
        // drawn lit when the pointer is on it, so sharing a control
        // with "anywhere else" lit it up for every hover in the dialog.
        assert_eq!(
            panel_control_at(&panel, centre(close)),
            Some(UiControl::LauncherDialogClose)
        );
        for elsewhere in [body, load.1, run.1]
            .into_iter()
            .chain(items.map(|(_, item)| item))
        {
            assert_ne!(
                panel_control_at(&panel, centre(elsewhere)),
                Some(UiControl::LauncherDialogClose),
                "something that is not the gadget lights the gadget"
            );
        }
        // Every description fits the two lines reserved for it. The
        // dialog cannot grow to suit a longer one -- resizing under a
        // pointer that is crossing the buttons would move them -- so a
        // sentence that does not fit is silently cut off instead.
        let chars = (dialog.w - 2 * SAVE_DIALOG_MARGIN) / font::GLYPH_W;
        for (control, _) in items {
            let help = save_dialog_help(control);
            assert!(!help.is_empty(), "{control:?} has nothing to say");
            let lines = wrap_text(help, chars, chars);
            assert!(
                lines.len() <= SAVE_DIALOG_HELP_LINES,
                "{help:?} wraps to {} lines, and there is room for {}",
                lines.len(),
                SAVE_DIALOG_HELP_LINES
            );
        }
        // With the pointer on nothing, the dialog still says what it
        // is for rather than leaving the space empty.
        assert_eq!(
            save_dialog_help(UiControl::LauncherSave),
            save_dialog_help(UiControl::LauncherSaveAs)
        );

        // And with it closed, its buttons are not there to be hit.
        let closed = Panel::Launcher(Box::new(LauncherState::new(
            launcher::MachineSetup::default(),
        )));
        for (control, item) in items {
            assert_ne!(
                panel_control_at(&closed, centre(item)),
                Some(control),
                "{control:?} answers with the dialog closed"
            );
        }
    }

    // The Host Disk page with two disks ticked: the table, the second
    // disk landing beside the first, and a line each saying where they go.
    {
        let mut frame = vec![0u8; w * h * 4];
        let mut setup = launcher::MachineSetup::default();
        setup.set_host_disks_for_test(vec![
            launcher::HostDiskRow {
                id: "disk4".to_string(),
                fingerprint: None,
                volume: "SanDisk Extreme SD".to_string(),
                size: "31.9 GB".to_string(),
                mounted: Vec::new(),
                writable: true,
                attach: None,
            },
            launcher::HostDiskRow {
                id: "disk6".to_string(),
                fingerprint: None,
                volume: "Kingston DataTraveler".to_string(),
                size: "3.9 GB".to_string(),
                mounted: vec!["/Volumes/UNTITLED".to_string()],
                writable: false,
                attach: None,
            },
            launcher::HostDiskRow {
                id: "PhysicalDrive11".to_string(),
                fingerprint: None,
                volume: "Generic USB3.0 CRW-SD/MS Multi-Card Reader".to_string(),
                size: "512 MB".to_string(),
                mounted: Vec::new(),
                writable: true,
                attach: None,
            },
        ]);
        setup.select_model(Some(crate::config::MachineModel::A1200));
        setup.select_host_disk(0);
        setup.select_host_disk(1);
        setup.select_host_disk(2);
        let mut state = LauncherState::new(setup);
        state.tab = LauncherTab::HostDisk;
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Launcher(Box::new(state))),
            ..Default::default()
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-host-disk");

        // The same page with nothing ticked: Mount is greyed, and the
        // page says what it is waiting for.
        let mut frame = vec![0u8; w * h * 4];
        let mut setup = launcher::MachineSetup::default();
        setup.set_host_disks_for_test(vec![launcher::HostDiskRow {
            id: "disk4".to_string(),
            fingerprint: None,
            volume: "SanDisk Extreme SD".to_string(),
            size: "31.9 GB".to_string(),
            mounted: Vec::new(),
            writable: false,
            attach: None,
        }]);
        let mut state = LauncherState::new(setup);
        state.tab = LauncherTab::HostDisk;
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Launcher(Box::new(state))),
            ..Default::default()
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-host-disk-locked");

        // A list longer than the box: the arrows appear, and the window
        // is part way down so both are live.
        let mut frame = vec![0u8; w * h * 4];
        let mut setup = launcher::MachineSetup::default();
        setup.set_host_disks_for_test(
            (0..14)
                .map(|i| launcher::HostDiskRow {
                    id: format!("disk{i}"),
                    fingerprint: None,
                    volume: format!("Pretend Media {i}"),
                    size: format!("{}.0 GB", i % 9 + 1),
                    mounted: Vec::new(),
                    writable: true,
                    attach: None,
                })
                .collect(),
        );
        setup.scroll_host_disks(3, HOST_DISK_VISIBLE_ROWS);
        let mut state = LauncherState::new(setup);
        state.tab = LauncherTab::HostDisk;
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Launcher(Box::new(state))),
            ..Default::default()
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-host-disk-scrolled");

        // Storage, with a real disk on IDE master: the row names the disk
        // and offers Unmount where an image would offer Browse/Clear.
        let mut frame = vec![0u8; w * h * 4];
        let mut setup = launcher::MachineSetup::default();
        // A machine that actually has IDE, so the rows are live.
        setup.select_model(Some(crate::config::MachineModel::A1200));
        setup.set_host_disks_for_test(vec![launcher::HostDiskRow {
            id: "disk4".to_string(),
            fingerprint: None,
            volume: "SanDisk Extreme SD".to_string(),
            size: "31.9 GB".to_string(),
            mounted: Vec::new(),
            writable: false,
            attach: None,
        }]);
        setup.select_host_disk(0);
        setup
            .mount_host_disks()
            .expect("the fixture machine has IDE");
        let mut state = LauncherState::new(setup);
        state.tab = LauncherTab::Storage;
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Launcher(Box::new(state))),
            ..Default::default()
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-storage-host-disk");
    }

    // The FluxBridge settings page reached from Configure, with an
    // interface selected, which is the state its rows are drawn live in.
    // Bridging a bay samples the host, so the row starts at "None" on a
    // machine with nothing plugged in and at the interface on one that
    // has: cycle until a driver is named, since what a test machine has
    // attached is not this test's subject. Bounded because cycling is a
    // fixed-length ring. Only exists in a build with the feature --
    // without it no bay can be bridged, so there is no such page to draw.
    #[cfg(feature = "fluxbridge")]
    {
        let mut frame = vec![0u8; w * h * 4];
        let mut setup = launcher::MachineSetup::default();
        setup.set_drive_bridged(0, true);
        setup.set_bridge_edit_drive(0);
        let mut named = false;
        for _ in 0..8 {
            if setup.value_label(LauncherField::BridgeDevice) != "None" {
                named = true;
                break;
            }
            setup.cycle(LauncherField::BridgeDevice, true);
        }
        assert!(named, "the row can name an interface to draw");
        let mut state = LauncherState::new(setup);
        state.tab = LauncherTab::FluxBridge;
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Launcher(Box::new(state))),
            ..Default::default()
        };
        draw(&mut frame, scale, &ui, None, None);
        save(&frame, "launcher-fluxbridge-page");
    }
}
#[test]
fn netplay_controls_fit_and_are_reachable_by_mouse_and_navigation() {
    check_netplay_controls(false, 0);
}

#[test]
#[cfg(feature = "netplay-internet")]
fn internet_netplay_controls_fit_and_are_reachable_by_mouse_and_navigation() {
    check_netplay_controls(true, 0);
    check_netplay_controls(true, 1);
}

fn check_netplay_controls(internet: bool, player: usize) {
    use crate::video::nav::{map, NavTarget};
    let mut state = LauncherState::new(launcher::MachineSetup::default());
    state.toggle_netplay();
    state.netplay.internet = internet;
    state.netplay.player = player;
    state.tab = LauncherTab::Netplay;
    let panel = Panel::Launcher(Box::new(state));
    let rect = panel_rect(&panel);
    let Panel::Launcher(state) = &panel else {
        unreachable!()
    };
    let rows = state.rows();
    let mut controls = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if !state.row_applies(row.field) && row.kind != RowKind::Action {
            continue;
        }
        let y = launcher_row_y(rect, i);
        let targets = match row.kind {
            RowKind::Toggle => vec![(
                launcher_toggle_rect(rect, y),
                UiControl::LauncherToggle(row.field),
            )],
            RowKind::Cycle => {
                let (prev, _, next) = launcher_cycle_rects(rect, y);
                vec![
                    (
                        prev,
                        UiControl::LauncherCycle {
                            field: row.field,
                            forward: false,
                        },
                    ),
                    (
                        next,
                        UiControl::LauncherCycle {
                            field: row.field,
                            forward: true,
                        },
                    ),
                ]
            }
            RowKind::Text => {
                let at = launcher_text_rect(rect, y, row.field);
                assert!(
                    at.w >= 32 * font::GLYPH_W + 8,
                    "a full session code must be visible"
                );
                vec![(at, UiControl::LauncherNetplayEdit(row.field))]
            }
            RowKind::Action => vec![
                (
                    launcher_action_rect(rect, y),
                    UiControl::LauncherNetplayAction(LauncherField::NetplayNewCode),
                ),
                (
                    launcher_action2_rect(rect, y),
                    UiControl::LauncherNetplayAction(LauncherField::NetplayCopyCode),
                ),
            ],
            _ => panic!("unexpected netplay widget"),
        };
        for (at, control) in targets {
            if control == UiControl::LauncherNetplayAction(LauncherField::NetplayNewCode)
                && !state.row_applies(LauncherField::NetplayNewCode)
            {
                continue;
            }
            assert!(at.x >= rect.x && at.x + at.w <= rect.x + rect.w);
            assert!(at.y + at.h < launcher_status_y(rect));
            let point = ((at.x + at.w / 2) as i32, (at.y + at.h / 2) as i32);
            assert_eq!(panel_control_at(&panel, point), Some(control));
            controls.push(control);
        }
    }
    let mut ui = UiState {
        panel: Some(panel),
        ..UiState::default()
    };
    let items = map(
        &ui,
        super::super::window::texture_width(1),
        super::super::window::texture_height(1),
    );
    for control in controls {
        assert!(
            items
                .iter()
                .any(|item| item.target == crate::video::nav::normalise(NavTarget::Ui(control))),
            "missing {control:?}"
        );
    }
    let Some(Panel::Launcher(state)) = ui.panel.as_mut() else {
        unreachable!()
    };
    state.toggle_netplay();
    assert!(!control_live(
        &ui,
        UiControl::LauncherNetplayEdit(LauncherField::NetplayCode)
    ));
    assert!(control_live(
        &ui,
        UiControl::LauncherToggle(LauncherField::NetplayEnabled)
    ));
}
