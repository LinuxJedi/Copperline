// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

fn next_word(state: &mut u64) -> u16 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state >> 16) as u16
}

fn populated_palette(state: &mut u64) -> Palette {
    let mut palette = Palette::new();
    for idx in 0..palette.len() {
        palette.write_entry(idx, false, next_word(state));
        palette.write_entry(idx, true, next_word(state));
    }
    palette
}

fn assert_cached_outputs(cache: &mut IndexedOutputCache, control: ControlState, palette: &Palette) {
    let outputs = cache.outputs(control, palette);
    for idx in u8::MIN..=u8::MAX {
        let mut expected_history = 0x0012_3456;
        let expected = denise_playfield_output(control, palette, idx, &mut expected_history);
        let mut cached_history = 0x0065_4321;
        let actual = cached_indexed_output(outputs, idx, &mut cached_history);
        assert_eq!(actual, expected, "index {idx:#04x}, control {control:?}");
        assert_eq!(cached_history, expected_history);
    }
}

#[test]
fn indexed_cache_reuses_tables_across_sample_and_composition_controls() {
    let mut random = 0x1732_29ab_aade_921d;
    let palette = populated_palette(&mut random);
    for _ in 0..4096 {
        // Name every control field: a new field must be considered here as
        // well as in the cache key. All omitted fields vary independently.
        let control = ControlState {
            agnus_revision: match next_word(&mut random) & 3 {
                0 => AgnusRevision::Ocs,
                1 => AgnusRevision::Ecs8372Rev4,
                2 => AgnusRevision::Ecs8375,
                _ => AgnusRevision::AgaAlice,
            },
            harddis: next_word(&mut random) & 1 != 0,
            dmacon: next_word(&mut random),
            bplcon0: next_word(&mut random),
            bplcon1: next_word(&mut random),
            bplcon2: next_word(&mut random),
            bplcon3: next_word(&mut random),
            bplcon4: next_word(&mut random),
            fmode: next_word(&mut random),
            clxcon: next_word(&mut random),
            clxcon2: next_word(&mut random),
            diwstrt: next_word(&mut random),
            diwstop: next_word(&mut random),
            diwhigh: DiwHigh::EcsExplicit(next_word(&mut random)),
            ddfstrt: next_word(&mut random),
            ddfstop: next_word(&mut random),
            bpl1mod: next_word(&mut random) as i16,
            bpl2mod: next_word(&mut random) as i16,
        };
        if control.ham() {
            continue;
        }
        let canonical = ControlState {
            agnus_revision: if control.aga() {
                AgnusRevision::AgaAlice
            } else {
                AgnusRevision::Ocs
            },
            bplcon0: control.bplcon0,
            bplcon2: control.bplcon2,
            bplcon3: control.bplcon3,
            bplcon4: control.bplcon4,
            ..ControlState::default()
        };
        let mut cache = IndexedOutputCache::default();
        cache.outputs(canonical, &palette);
        assert_cached_outputs(&mut cache, control, &palette);
        assert_eq!(cache.entries.len(), 1, "control {control:?}");
    }
}

#[test]
fn indexed_cache_invalidates_each_colour_control_register_bit() {
    let mut random = 0x3729_912d_eee4_aabb;
    let palette = populated_palette(&mut random);
    for agnus_revision in [AgnusRevision::Ocs, AgnusRevision::AgaAlice] {
        let base = ControlState {
            agnus_revision,
            ..ControlState::default()
        };
        for register in 0..4 {
            for bit in 0..16 {
                let mut changed = base;
                match register {
                    0 => changed.bplcon0 ^= 1 << bit,
                    1 => changed.bplcon2 ^= 1 << bit,
                    2 => changed.bplcon3 ^= 1 << bit,
                    _ => changed.bplcon4 ^= 1 << bit,
                }
                assert!(!changed.ham());
                let mut cache = IndexedOutputCache::default();
                cache.outputs(base, &palette);
                assert_cached_outputs(&mut cache, changed, &palette);
                assert_eq!(cache.entries.len(), 2, "control {changed:?}");
            }
        }
        let mut cache = IndexedOutputCache::default();
        cache.outputs(base, &palette);
        let changed = ControlState {
            agnus_revision: if base.aga() {
                AgnusRevision::Ocs
            } else {
                AgnusRevision::AgaAlice
            },
            ..base
        };
        assert_cached_outputs(&mut cache, changed, &palette);
        assert_eq!(cache.entries.len(), 2);
    }
}

#[test]
fn indexed_cache_invalidates_palette_high_and_low_writes_in_every_bank() {
    let palette = Palette::new();
    let control = ControlState {
        agnus_revision: AgnusRevision::AgaAlice,
        bplcon0: 0x0010, // Eight planes.
        ..ControlState::default()
    };
    for idx in 0..palette.len() {
        for loct in [false, true] {
            let mut cache = IndexedOutputCache::default();
            cache.outputs(control, &palette);
            let mut changed = palette;
            changed.write_entry(idx, loct, 1);
            assert_cached_outputs(&mut cache, control, &changed);
            assert_eq!(cache.entries.len(), 2, "entry {idx}, LOCT {loct}");
        }
    }
}

#[test]
fn indexed_cache_reuse_preserves_held_colour_for_ham6_and_ham8_transitions() {
    let mut random = 0x4281_923c_a837_231e;
    let palette = populated_palette(&mut random);
    for (agnus_revision, indexed_bplcon0, ham_bplcon0) in [
        (AgnusRevision::Ocs, 0x6000, 0x6800),
        (AgnusRevision::Ocs, 0x2400, 0x6c00),
        (AgnusRevision::AgaAlice, 0x6000, 0x6800),
        (AgnusRevision::AgaAlice, 0x0010, 0x0810),
    ] {
        let indexed = ControlState {
            agnus_revision,
            bplcon0: indexed_bplcon0,
            bplcon3: BPLCON3_PF2OF_DEFAULT,
            bplcon4: 0x5a00,
            ..ControlState::default()
        };
        let ham = ControlState {
            bplcon0: ham_bplcon0,
            ..indexed
        };
        assert!(!indexed.ham());
        assert!(ham.ham());
        let mut cache = IndexedOutputCache::default();
        cache.outputs(indexed, &palette);
        let scrolled = ControlState {
            bplcon1: 0xffff,
            bpl1mod: -128,
            ..indexed
        };
        for seed_idx in u8::MIN..=u8::MAX {
            let mut expected_history = 0x0012_3456;
            let expected =
                denise_playfield_output(scrolled, &palette, seed_idx, &mut expected_history);
            let mut cached_history = 0x0065_4321;
            let actual = cached_indexed_output(
                cache.outputs(scrolled, &palette),
                seed_idx,
                &mut cached_history,
            );
            assert_eq!(actual, expected);
            assert_eq!(cached_history, expected_history);
            for ham_idx in u8::MIN..=u8::MAX {
                assert_eq!(
                    denise_playfield_output(ham, &palette, ham_idx, &mut cached_history),
                    denise_playfield_output(ham, &palette, ham_idx, &mut expected_history),
                );
                assert_eq!(cached_history, expected_history);
            }
        }
        assert_eq!(cache.entries.len(), 1);
    }
}

// This module is self-contained so the same benchmark can be compiled
// against the parent revision without changing either production cache.
mod performance {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;

    const HASH_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

    fn hash_output(hash: u64, output: DenisePlayfieldOutput) -> u64 {
        (hash
            ^ u64::from(output.color)
            ^ (u64::from(output.color_latch) << 32)
            ^ (u64::from(output.pf_mask) << 48))
            .wrapping_mul(0x0000_0100_0000_01b3)
    }

    /// Run with `cargo test --release --lib indexed_cache_lookup_benchmark
    /// -- --ignored --nocapture --test-threads=1`. Compare the same module,
    /// compiler and release settings on each revision; this times cache
    /// work, including frame-local allocation, rather than whole frames.
    #[test]
    #[ignore = "host performance benchmark"]
    fn indexed_cache_lookup_benchmark() {
        const FRAMES: usize = 2_000;
        const ROWS: usize = 256;
        const SAMPLES: usize = 5;
        let mut palette = Palette::new();
        for idx in 0..palette.len() {
            palette.write_entry(idx, false, (idx as u16).wrapping_mul(73) ^ 0xA593);
            palette.write_entry(idx, true, (idx as u16).wrapping_mul(139) ^ 0x0862);
        }
        for (chipset, agnus_revision, bplcon0) in [
            ("ocs", AgnusRevision::Ocs, 0x6000),
            ("aga", AgnusRevision::AgaAlice, 0x0010),
        ] {
            let base = ControlState {
                agnus_revision,
                bplcon0,
                bplcon3: BPLCON3_PF2OF_DEFAULT,
                bplcon4: 0x5A00,
                ..ControlState::default()
            };
            let scalar: [DenisePlayfieldOutput; 256] = std::array::from_fn(|idx| {
                denise_playfield_output(base, &palette, idx as u8, &mut 0)
            });
            let mut expected_hash = HASH_OFFSET;
            for frame in 0..FRAMES {
                for row in 0..ROWS {
                    expected_hash = hash_output(expected_hash, scalar[(row + frame * 17) & 255]);
                }
            }
            for workload in ["constant", "scroll16"] {
                let controls: [ControlState; 16] = std::array::from_fn(|scroll| ControlState {
                    bplcon1: if workload == "scroll16" {
                        (scroll as u16) * 0x11
                    } else {
                        0
                    },
                    ..base
                });
                let mut validation_cache = IndexedOutputCache::default();
                for control in controls {
                    assert_eq!(
                        validation_cache.outputs(control, &palette),
                        &scalar,
                        "{chipset} {workload}, BPLCON1={:#06x}",
                        control.bplcon1,
                    );
                }
                for sample in 0..SAMPLES {
                    let mut hash = HASH_OFFSET;
                    let started = Instant::now();
                    for frame in 0..FRAMES {
                        let mut cache = IndexedOutputCache::default();
                        for row in 0..ROWS {
                            let outputs = black_box(
                                cache.outputs(black_box(controls[row & 15]), black_box(&palette)),
                            );
                            hash = hash_output(hash, outputs[(row + frame * 17) & 255]);
                        }
                    }
                    let elapsed = started.elapsed();
                    assert_eq!(hash, expected_hash, "{chipset} {workload}, sample {sample}");
                    println!(
                        "indexed_cache_bench chipset={chipset} workload={workload} sample={sample} \
                         lookups={} ns_per_lookup={:.2} hash={hash:016x}",
                        FRAMES * ROWS,
                        elapsed.as_nanos() as f64 / (FRAMES * ROWS) as f64,
                    );
                }
            }
        }
    }
}
