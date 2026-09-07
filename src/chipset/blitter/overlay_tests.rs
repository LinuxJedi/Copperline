// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

fn configured(channels: u16, direction: u16, shift: u16) -> Blitter {
    let mut blitter = Blitter::new();
    blitter.bltcon0 = (shift << 12) | (channels << 8) | 0xCA;
    blitter.bltcon1 = (shift << 12) | direction;
    blitter.bltapt = 0x20;
    blitter.bltbpt = 0x80;
    blitter.bltcpt = 0x100;
    blitter.bltdpt = 0x100;
    blitter.bltafwm = 0xA55A;
    blitter.bltalwm = 0x5AA5;
    blitter.write_bltadat(0x96E1);
    blitter.write_bltbdat(0x783C);
    blitter.write_bltcdat(0x1357);
    blitter
}

fn normal(blitter: &Blitter) -> &NormalBlitState {
    let Some(PendingBlit::Normal(state)) = &blitter.pending else {
        panic!("expected active normal blit");
    };
    state
}

fn restore(blitter: &Blitter) -> Blitter {
    let bytes = bincode::serialize(blitter).unwrap();
    let mut restored: Blitter = bincode::deserialize(&bytes).unwrap();
    assert_eq!(bytes, bincode::serialize(&restored).unwrap());
    // The debugger mirrors are deliberately outside save-state storage.
    restored.set_debug_watch_addrs(&blitter.debug_watch_addrs);
    restored
}

fn force_legacy_overlay(blitter: &mut Blitter) {
    if let Some(PendingBlit::Normal(state)) = &mut blitter.pending {
        // Exactly the pre-optimization policy. Keeping the old path intact
        // gives the differential tests the same sequencer and timing, with
        // every A/B lookup and D insertion going through the ordered map.
        state.track_overlay = state.use_d && (state.use_a || state.use_b);
    }
}

fn architectural_state(blitter: &Blitter) -> Vec<u8> {
    let mut normalized: Blitter =
        bincode::deserialize(&bincode::serialize(blitter).unwrap()).unwrap();
    if let Some(PendingBlit::Normal(state)) = &mut normalized.pending {
        // These two acceleration fields intentionally differ. Compare every
        // other serialized field, including pipeline latches and IRQ state.
        state.track_overlay = false;
        state.d_overlay.clear();
    }
    bincode::serialize(&normalized).unwrap()
}

fn assert_slot_equivalent(actual: &Blitter, reference: &Blitter, ram: &[u8], expected: &[u8]) {
    assert_eq!(ram, expected, "chip RAM");
    assert_eq!(architectural_state(actual), architectural_state(reference));
    assert_eq!(actual.current_slot_class(), reference.current_slot_class());
    assert_eq!(actual.current_slot_label(), reference.current_slot_label());
    assert_eq!(
        actual.current_slot_needs_bus(),
        reference.current_slot_needs_bus()
    );
    assert_eq!(
        actual.current_slot_counts_for_bls(),
        reference.current_slot_counts_for_bls()
    );
    assert_eq!(
        actual.bltpri_warmup_fences_cpu(),
        reference.bltpri_warmup_fences_cpu()
    );
    assert_eq!(
        actual.scheduled_slot_access_pattern(64),
        reference.scheduled_slot_access_pattern(64)
    );
    assert_eq!(
        actual.current_bus_access(ram),
        reference.current_bus_access(expected)
    );
}

fn live_writes(blitter: &mut Blitter, ram: &mut [u8], slot: usize, suppress_d: bool) {
    match slot {
        // Neither direction nor the line-mode bit reinterprets the pending
        // normal blit. These are public register writes, not state mutations.
        8 => blitter.write_bltcon1(0xF000 | BLTCON1_LINE | BLTCON1_DESC | BLTCON1_DOFF),
        // CPU source overwrites must still be hidden by A/B snapshots. C is
        // deliberately the destination and must continue to read live RAM.
        11 => {
            write_word(ram, 0x22, 0xFEDC);
            write_word(ram, 0x88, 0xBA98);
            write_word(ram, 0x100, 0x7654);
        }
        12 => {
            blitter.set_apt_high(0x1F);
            blitter.set_apt_low(0x100);
            blitter.set_bpt_low(0x100);
            blitter.set_cpt_low(0x20);
            blitter.set_dpt_low(0x20);
            blitter.bltamod = -32;
            blitter.bltbmod = 32;
            blitter.bltcmod = 16;
            blitter.bltdmod = -16;
        }
        16 => {
            blitter.write_bltadat(0x1234);
            blitter.write_bltbdat(0x5678);
            blitter.write_bltcdat(0x9ABC);
        }
        20 if suppress_d => blitter.write_bltcon0(0x0F66),
        _ => {}
    }
}

fn tick_pair(actual: &mut Blitter, reference: &mut Blitter, ram: &mut [u8], expected: &mut [u8]) {
    assert_slot_equivalent(actual, reference, ram, expected);
    assert_eq!(
        actual.tick_scheduled_slot(ram),
        reference.tick_scheduled_slot(expected)
    );
    assert_eq!(actual.take_irq_arm(), reference.take_irq_arm());
    assert_eq!(
        actual.take_debug_watched_write(),
        reference.take_debug_watched_write()
    );
    assert_slot_equivalent(actual, reference, ram, expected);
}

#[test]
fn overlay_elision_requires_disjoint_identity_mapped_packed_spans() {
    let ram = vec![0; 512];
    let tracks = |blitter: &Blitter, h, w, ram: &[u8]| {
        NormalBlitState::new(blitter, h, w, ram).track_overlay
    };
    for channels in [0x9, 0x5, 0xD, 0xF] {
        let b = configured(channels, 0, 0);
        assert!(!tracks(&b, 3, 4, &ram));
    }
    // Source/destination adjacency is safe, including a word ending exactly
    // at populated RAM's upper boundary. C may overlap D because it is live.
    let mut b = configured(0xF, 0, 0);
    b.bltapt = 0xE8;
    b.bltbpt = 0x118;
    assert!(!tracks(&b, 3, 4, &ram));
    b.bltdpt = 0x1E8;
    assert!(!tracks(&b, 3, 4, &ram));
    // Modulos of disabled channels do not affect any accessed source address.
    b = configured(0x9, 0, 0);
    b.bltbmod = -2;
    assert!(!tracks(&b, 3, 4, &ram));
    b = configured(0x5, 0, 0);
    b.bltamod = 2;
    assert!(!tracks(&b, 3, 4, &ram));

    for (apt, bpt, dpt, ram_len) in [
        (0x100, 0x80, 0x100, 512),    // A equals D
        (0x20, 0x100, 0x100, 512),    // B equals D
        (0xF0, 0x80, 0x100, 512),     // A partially overlaps D
        (0x20, 0x110, 0x100, 512),    // B partially overlaps D
        (0x300, 0x80, 0x100, 512),    // populated-RAM mask aliases A to D
        (0x20, 0x80, 0x300, 512),     // populated-RAM mask aliases D
        (0x200100, 0x80, 0x100, 512), // DMA mask aliases A to D
        (0x20, 0x80, 0x1F0, 512),     // D crosses populated RAM boundary
        (0x1F0, 0x80, 0x100, 512),    // A crosses populated RAM boundary
        (0x20, 0x178, 0x100, 384),    // B enters an unpopulated hole
        (0x20, 0x80, 0x100, 0),       // no chip RAM
    ] {
        b = configured(0xF, 0, 0);
        b.bltapt = apt;
        b.bltbpt = bpt;
        b.bltdpt = dpt;
        assert!(tracks(&b, 3, 4, &vec![0; ram_len]));
    }
    for channel in 0..3 {
        for modulo in [-2, 2] {
            b = configured(0xF, 0, 0);
            match channel {
                0 => b.bltamod = modulo,
                1 => b.bltbmod = modulo,
                _ => b.bltdmod = modulo,
            }
            assert!(tracks(&b, 3, 4, &ram));
        }
    }
    b = configured(0xF, BLTCON1_DESC, 0);
    assert!(tracks(&b, 3, 4, &ram));
    b = configured(0xF, 0, 0);
    b.bltdpt = CHIP_DMA_ADDR_MASK - 7;
    assert!(!NormalBlitState::sources_disjoint_from_d(&b, 3, 4, 1 << 22));
    b.bltdpt = u32::MAX - 7;
    assert!(!NormalBlitState::sources_disjoint_from_d(
        &b,
        3,
        4,
        usize::MAX
    ));
    assert!(!NormalBlitState::sources_disjoint_from_d(
        &b,
        u32::MAX,
        4,
        512
    ));
    assert!(!NormalBlitState::sources_disjoint_from_d(&b, 0, 4, 512));
}

#[test]
fn overlay_elision_matches_tracked_pipeline_each_slot_with_live_writes() {
    // 384 configurations cover every channel combination, shifts, fill,
    // overlap, both modulo signs, physical aliases and unpopulated addresses.
    for channels in 0..16 {
        for direction in [0, BLTCON1_DESC] {
            for shift in [0, 3, 15] {
                for layout in 0..4 {
                    let mut ram: Vec<u8> = (0..384).map(|i| (i * 37 + i / 7) as u8).collect();
                    let mut actual = configured(channels, direction, shift);
                    if direction != 0 {
                        actual.bltcon1 |= if shift == 3 { BLTCON1_IFE } else { BLTCON1_EFE };
                    }
                    match layout {
                        1 => {
                            actual.bltapt = 0x100;
                            actual.bltbpt = 0xF8;
                        }
                        2 => {
                            actual.bltamod = 2;
                            actual.bltbmod = -2;
                            actual.bltdmod = 4;
                        }
                        3 => {
                            actual.bltapt = 0x1F4;
                            actual.bltbpt = 0x2F8;
                            actual.bltdpt = 0x1FA;
                        }
                        _ => {}
                    }
                    actual.set_debug_watch_addrs(&[actual.bltdpt, actual.bltdpt + 4, 0x108]);
                    actual.start_scheduled_dims(3, 4, &ram);
                    let mut reference = restore(&actual);
                    force_legacy_overlay(&mut reference);
                    let mut expected = ram.clone();
                    let mut slot = 0;
                    while actual.busy {
                        assert!(slot < 100, "blit did not terminate");
                        let suppress_d = channels & 2 == 0;
                        live_writes(&mut actual, &mut ram, slot, suppress_d);
                        live_writes(&mut reference, &mut expected, slot, suppress_d);
                        tick_pair(&mut actual, &mut reference, &mut ram, &mut expected);
                        slot += 1;
                    }
                    assert!(!reference.busy);
                    // Completed states have no acceleration storage left and
                    // therefore match byte-for-byte without normalization.
                    assert_eq!(
                        bincode::serialize(&actual).unwrap(),
                        bincode::serialize(&reference).unwrap()
                    );
                }
            }
        }
    }
}

#[test]
fn overlay_elision_and_legacy_tracked_states_resume_at_every_slot() {
    for overlap in [false, true] {
        for suppress_d in [false, true] {
            let mut ram: Vec<u8> = (0..512).map(|i| (i * 17 + i / 13) as u8).collect();
            let mut actual = configured(0xF, 0, 3);
            if overlap {
                actual.bltbpt = 0xF8;
            }
            actual.set_debug_watch_addrs(&[0x100, 0x104, 0x110]);
            actual.start_scheduled_dims(3, 4, &ram);
            assert_eq!(normal(&actual).track_overlay, overlap);
            let mut reference = restore(&actual);
            force_legacy_overlay(&mut reference);
            let mut expected = ram.clone();
            let mut slot = 0;
            let mut saved_populated_overlay = false;
            let mut resumed_results = Vec::new();
            while actual.busy {
                let mut resumed = restore(&actual);
                let mut resumed_reference = restore(&reference);
                let mut resumed_ram = ram.clone();
                let mut resumed_expected = expected.clone();
                assert_eq!(normal(&resumed).track_overlay, overlap);
                assert!(normal(&resumed_reference).track_overlay);
                saved_populated_overlay |= !normal(&resumed_reference).d_overlay.is_empty();
                for future in slot..100 {
                    if !resumed.busy {
                        break;
                    }
                    live_writes(&mut resumed, &mut resumed_ram, future, suppress_d);
                    live_writes(
                        &mut resumed_reference,
                        &mut resumed_expected,
                        future,
                        suppress_d,
                    );
                    tick_pair(
                        &mut resumed,
                        &mut resumed_reference,
                        &mut resumed_ram,
                        &mut resumed_expected,
                    );
                }
                assert!(!resumed.busy);
                assert!(!resumed_reference.busy);
                resumed_results.push((resumed_ram, bincode::serialize(&resumed).unwrap()));
                live_writes(&mut actual, &mut ram, slot, suppress_d);
                live_writes(&mut reference, &mut expected, slot, suppress_d);
                tick_pair(&mut actual, &mut reference, &mut ram, &mut expected);
                slot += 1;
            }
            assert!(
                saved_populated_overlay,
                "old-style nonempty overlay was not resumed"
            );
            let final_state = bincode::serialize(&actual).unwrap();
            for (resumed_ram, resumed_state) in resumed_results {
                assert_eq!(resumed_ram, ram, "resumed versus uninterrupted RAM");
                assert_eq!(
                    resumed_state, final_state,
                    "resumed versus uninterrupted state"
                );
            }
        }
    }
}

#[test]
fn physical_alias_feedback_keeps_the_overlay() {
    let mut ram: Vec<u8> = (0..256).map(|i| (i * 19 + 7) as u8).collect();
    let mut correct = configured(0xD, 0, 0);
    correct.bltcon0 = 0x0D66; // D = A XOR B
    correct.bltapt = 0x40;
    correct.bltbpt = 0x108; // Physically the row before D, despite raw disjoint spans.
    correct.bltdpt = 0x10;
    correct.start_scheduled_dims(3, 4, &ram);
    assert!(normal(&correct).track_overlay);
    let mut missing_overlay = restore(&correct);
    let mut incorrect_ram = ram.clone();
    if let Some(PendingBlit::Normal(state)) = &mut missing_overlay.pending {
        state.track_overlay = false;
    }
    while correct.busy {
        correct.tick_scheduled_slot(&mut ram);
        missing_overlay.tick_scheduled_slot(&mut incorrect_ram);
    }
    assert_ne!(ram, incorrect_ram, "fixture must exercise D-to-B feedback");
}
