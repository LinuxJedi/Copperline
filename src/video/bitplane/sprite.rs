// SPDX-License-Identifier: GPL-3.0-or-later

//! Sprite rendering: captured/manual sprite line collection, the beam
//! sprite state machine, attach/priority/collision decode, and the
//! sprite drawing path. Split out of `bitplane.rs` for size; same
//! module family, full access to the parent's private items.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct SpriteLine {
    pub(super) hstart: i32,
    pub(super) hsub_70ns: bool,
    pub(super) beam_y: i32,
    pub(super) data: u16,
    pub(super) datb: u16,
    /// AGA FMODE wide-fetch words beyond the first (SPR32/SPAGEM).
    pub(super) data_ext: [u16; 3],
    pub(super) datb_ext: [u16; 3],
    /// Words per channel: 1 (16 px), 2 (32 px), or 4 (64 px).
    pub(super) width_words: u8,
    pub(super) attached: bool,
    pub(super) x_start: usize,
    pub(super) x_stop: usize,
}

impl SpriteLine {
    pub(super) fn width_words(&self) -> usize {
        (self.width_words as usize).max(1)
    }

    pub(super) fn word(&self, w: usize) -> (u16, u16) {
        if w == 0 {
            (self.data, self.datb)
        } else {
            (self.data_ext[w - 1], self.datb_ext[w - 1])
        }
    }
}

pub(super) const SPRITE_LINE_MAX_BITS: usize = 64;

pub(super) struct SpriteLineSampler<'a> {
    pub(super) line: &'a SpriteLine,
    /// Bit boundaries in 35 ns samples (two per framebuffer pixel).
    pub(super) bit_stops_subpixels: [i32; SPRITE_LINE_MAX_BITS + 1],
    pub(super) bit_values: [u8; SPRITE_LINE_MAX_BITS],
    pub(super) bit_count: usize,
}

impl<'a> SpriteLineSampler<'a> {
    pub(super) fn new(
        line: &'a SpriteLine,
        base_control: ControlState,
        control_segments: &[ControlSegment],
    ) -> Self {
        let base_x =
            sprite_base_framebuffer_x(line.hstart, line.hsub_70ns, base_control, control_segments);
        let mut bit_stops_subpixels = [0i32; SPRITE_LINE_MAX_BITS + 1];
        let mut bit_values = [0u8; SPRITE_LINE_MAX_BITS];
        let mut bit_count = 0usize;
        let mut subpixel_cursor = base_x * 2;
        bit_stops_subpixels[0] = subpixel_cursor;

        for w in 0..line.width_words() {
            let (data, datb) = line.word(w);
            for bit in (0..16).rev() {
                let sample_x = subpixel_cursor
                    .div_euclid(2)
                    .clamp(0, FB_WIDTH.saturating_sub(1) as i32)
                    as usize;
                let sprite_pixel_repeat = control_at_x(base_control, control_segments, sample_x)
                    .sprite_pixel_repeat_subpixels();
                let lo = u8::from(data & (1 << bit) != 0);
                let hi = u8::from(datb & (1 << bit) != 0);
                bit_values[bit_count] = lo | (hi << 1);
                subpixel_cursor += sprite_pixel_repeat;
                bit_count += 1;
                bit_stops_subpixels[bit_count] = subpixel_cursor;
            }
        }

        Self {
            line,
            bit_stops_subpixels,
            bit_values,
            bit_count,
        }
    }

    pub(super) fn framebuffer_range(&self) -> Option<(i32, i32)> {
        let start = self.bit_stops_subpixels[0]
            .div_euclid(2)
            .max(self.line.x_start as i32)
            .max(0);
        let stop = (self.bit_stops_subpixels[self.bit_count] + 1)
            .div_euclid(2)
            .min(self.line.x_stop as i32)
            .min(FB_WIDTH as i32);
        (start < stop).then_some((start, stop))
    }

    pub(super) fn subpixel_bits_at(&self, subpixel: i32) -> u8 {
        if subpixel < self.line.x_start as i32 * 2
            || subpixel >= self.line.x_stop as i32 * 2
            || subpixel < self.bit_stops_subpixels[0]
            || subpixel >= self.bit_stops_subpixels[self.bit_count]
        {
            return 0;
        }
        let bit_idx =
            self.bit_stops_subpixels[1..=self.bit_count].partition_point(|stop| *stop <= subpixel);
        self.bit_values[bit_idx]
    }

    pub(super) fn pixel_bits_at(&self, x: i32) -> u8 {
        self.subpixel_bits_at(x * 2) | self.subpixel_bits_at(x * 2 + 1)
    }
}

/// Sprite colour entry in the palette store. AGA bases the lookup on the
/// BPLCON4 ESPRM high nibble (even sprites) or OSPRM low nibble (odd
/// sprites, and attached pairs, which display through the odd channel);
/// pre-AGA uses the classic 16..31 block. Attached pairs use the 4-bit
/// pixel index directly; unattached sprites add the pair's 4-colour offset.
pub(super) fn sprite_color_entry(
    control: ControlState,
    sprite: usize,
    idx: u8,
    attached: bool,
) -> usize {
    let offset = if attached {
        idx as usize
    } else {
        (sprite / 2) * 4 + idx as usize
    };
    if control.aga() {
        let nibble = if attached || sprite & 1 == 1 {
            control.bplcon4 & 0x0F
        } else {
            (control.bplcon4 >> 4) & 0x0F
        } as usize;
        (nibble << 4) + offset
    } else {
        16 + offset
    }
}

#[derive(Clone, Copy)]
pub(super) struct SpriteClip {
    pub(super) x_start: usize,
    pub(super) x_stop: usize,
    pub(super) y_start: usize,
    pub(super) y_stop: usize,
}

// Scaffolding for the renderer's sprite unit-test helpers. Sprite DMA is now
// sourced from captured DMA lines (see bus.rs), so the renderer no longer reads
// pointer-refresh state; the helpers keep this shape only to drive tests.
#[cfg(test)]
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
pub(super) struct SpritePointerRefresh {
    pub(super) refreshed: bool,
    pub(super) ptr: u32,
    pub(super) beam: Option<(u32, u32)>,
}

#[derive(Clone, Copy)]
pub(super) struct BeamSpriteState {
    pub(super) sprpos: [u16; 8],
    pub(super) sprctl: [u16; 8],
    pub(super) sprdata: [u16; 8],
    pub(super) sprdatb: [u16; 8],
    pub(super) spr_armed: [bool; 8],
    pub(super) direct_data_armed: [bool; 8],
    /// Lisa only: FMODE SPR32/SPAGEM widen manual sprites too. A CPU/Copper
    /// SPRxDATA/SPRxDATB write loads the same 16-bit value into every word
    /// of the wide holding register, so a manual wide sprite repeats its
    /// 16-pixel pattern across the 32/64-pixel window (WinUAE model).
    pub(super) aga: bool,
    pub(super) bplcon0: u16,
    pub(super) fmode: u16,
    /// Sprites reused with DMA off (SPREN cleared mid-frame): the bus
    /// established the held pixel data off-screen, and the Copper repositions
    /// them via SPRxPOS. When present the sprite is armed and displays this
    /// held data (with its full wide-fetch words, unlike a manual SPRxDATA
    /// write which only replicates one word) at the current SPRxPOS, clipped
    /// per reposition interval. The held state is captured only after sprite
    /// DMA has already made the channel active; once SPREN is off, the DMA
    /// descriptor's later VSTOP no longer clears that latched display data.
    pub(super) held: [Option<HeldSpriteLine>; 8],
}

impl BeamSpriteState {
    /// `use_hardware_latch` picks the register view the replay starts from:
    /// with sprite DMA idle this frame the armed latches follow Denise's own
    /// rules across DMA history, so the hardware-true view (manual writes
    /// AND DMA fetches) is the source; with captured DMA driving the frame
    /// the replay owns only CPU/Copper writes and is calibrated against the
    /// write-shadow view.
    pub(super) fn from_render_state(
        state: &RenderState,
        held: &[Option<HeldSpriteLine>; 8],
        use_hardware_latch: bool,
    ) -> Self {
        let (mut sprpos, mut sprctl, sprdata, sprdatb, mut spr_armed) = if use_hardware_latch {
            (
                state.spr_hw_pos,
                state.spr_hw_ctl,
                state.spr_hw_data,
                state.spr_hw_datb,
                state.spr_hw_armed,
            )
        } else {
            (
                state.sprpos,
                state.sprctl,
                state.sprdata,
                state.sprdatb,
                state.spr_armed,
            )
        };
        for (i, h) in held.iter().enumerate() {
            if let Some(held) = h {
                let (pos, ctl) = sprite_control_words_from_parts(
                    held.vstart,
                    held.vstop,
                    held.line.hstart,
                    held.line.hsub_70ns,
                    held.line.attached,
                );
                sprpos[i] = pos;
                sprctl[i] = ctl;
                spr_armed[i] = true;
            }
        }
        Self {
            sprpos,
            sprctl,
            sprdata,
            sprdatb,
            spr_armed,
            direct_data_armed: [false; 8],
            aga: matches!(state.agnus_revision, AgnusRevision::AgaAlice),
            bplcon0: state.bplcon0,
            fmode: state.fmode,
            held: *held,
        }
    }

    fn disarm_nonheld_latched_output(&mut self) {
        for sprite in 0..8 {
            if self.held[sprite].is_none() {
                self.spr_armed[sprite] = false;
                self.direct_data_armed[sprite] = false;
            }
        }
    }

    pub(super) fn apply_write(&mut self, off: u16, val: u16) {
        if off == 0x100 {
            self.bplcon0 = val;
            return;
        }
        if off == 0x1FC {
            if self.aga {
                self.fmode = val & 0xC00F;
            }
            return;
        }
        let idx = ((off - 0x140) / 8) as usize;
        if idx >= 8 {
            return;
        }
        match (off - 0x140) & 0x0006 {
            0x0 => self.sprpos[idx] = val,
            0x2 => {
                self.sprctl[idx] = val;
                self.spr_armed[idx] = false;
                self.direct_data_armed[idx] = false;
            }
            0x4 => {
                self.sprdata[idx] = val;
                self.spr_armed[idx] = true;
                self.direct_data_armed[idx] = true;
            }
            0x6 => self.sprdatb[idx] = val,
            _ => {}
        }
    }

    pub(super) fn line_for_sprite(
        &self,
        sprite: usize,
        beam_y: i32,
        x_start: usize,
        x_stop: usize,
    ) -> Option<SpriteLine> {
        if x_start >= x_stop || !self.spr_armed[sprite] {
            return None;
        }
        let pos = self.sprpos[sprite];
        let ctl = self.sprctl[sprite];
        let held = self.held[sprite];
        let hstart = sprite_hstart(pos, ctl);
        let hsub_70ns = sprite_hsub_70ns(ctl);
        let base_x = sprite_nominal_base_framebuffer_x(pos, ctl, self.bplcon0, self.fmode);
        // A held sprite was already active when SPREN was cleared. With no
        // sprite DMA slot running, the DMA descriptor's stop comparator cannot
        // retire the latched data; later SPRxPOS writes simply reposition it.
        if let Some(held) = held {
            return Some(SpriteLine {
                hstart,
                hsub_70ns,
                beam_y,
                data: held.line.data,
                datb: held.line.datb,
                data_ext: held.line.data_ext,
                datb_ext: held.line.datb_ext,
                width_words: held.line.width_words,
                attached: ctl & 0x0080 != 0,
                x_start,
                x_stop,
            });
        }
        // SPRxDATA/SPRxDATB writes update Denise's data latches, but the
        // serializer only copies those latches when the horizontal sprite
        // comparator fires. A write after that compare is for a later compare,
        // not the remaining pixels of the current word.
        if x_start as i32 > base_x {
            return None;
        }
        if !self.direct_data_armed[sprite] {
            let vstart = sprite_vstart(pos, ctl);
            let vstop = sprite_vstop(ctl);
            // Normal pair: [vstart, vstop). Equal start/stop is an empty window;
            // only a strictly inverted pair wraps through the frame boundary.
            let in_window = if vstop == vstart {
                false
            } else if vstop > vstart {
                beam_y >= vstart && beam_y < vstop
            } else {
                beam_y >= vstart || beam_y < vstop
            };
            if !in_window {
                return None;
            }
        }
        let width_words = if self.aga {
            sprite_width_words_from_fmode(self.fmode)
        } else {
            1
        };
        let data = self.sprdata[sprite];
        let datb = self.sprdatb[sprite];
        let (data_ext, datb_ext) = if width_words > 1 {
            ([data; 3], [datb; 3])
        } else {
            ([0; 3], [0; 3])
        };
        Some(SpriteLine {
            hstart,
            hsub_70ns,
            beam_y,
            data,
            datb,
            data_ext,
            datb_ext,
            width_words,
            attached: ctl & 0x0080 != 0,
            x_start,
            x_stop,
        })
    }
}

/// FMODE SPR32/SPAGEM (bits 2-3): 16-bit words per sprite channel, i.e. the
/// sprite output width in words (16/32/64 pixels).
pub(super) fn sprite_width_words_from_fmode(fmode: u16) -> u8 {
    match (fmode >> 2) & 0x0003 {
        0 => 1,
        3 => 4,
        _ => 2,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn manual_sprite_lines_from_events(
    initial_state: &RenderState,
    events: &[BeamRegisterWrite],
) -> Vec<Vec<SpriteLine>> {
    manual_sprite_lines_from_events_with_visible_line0(
        initial_state,
        events,
        &[None; 8],
        PAL_VISIBLE_LINE0,
        FB_HEIGHT,
        true,
        true,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ManualSpriteFlushMode {
    ClipAtEnd,
    PreserveStartedOutput,
}

/// `sprite_dma_observed` says whether sprite DMA actually fetched data this
/// frame. The beam replay only sees CPU/Copper register writes; when DMA also
/// drives a channel, Agnus writes POS/CTL/DATA through the same Denise
/// registers without appearing here. The replay therefore starts non-held
/// channels disarmed and lets captured DMA lines own DMA-loaded data, while
/// actual beam-timed SPRxDATA writes still seed the latch for later retiming.
/// With sprite DMA idle Denise's own rules apply unmodified: SPRxDATA arms at
/// any beam position, SPRxCTL disarms, and SPRxPOS never disarms, so an armed
/// sprite serializes on every line (there is no vertical comparator in Denise).
pub(super) fn manual_sprite_lines_from_events_with_visible_line0(
    initial_state: &RenderState,
    events: &[BeamRegisterWrite],
    held: &[Option<HeldSpriteLine>; 8],
    visible_line0: i32,
    rows: usize,
    include_latched_sprite_state: bool,
    sprite_dma_observed: bool,
) -> Vec<Vec<SpriteLine>> {
    let mut regs =
        BeamSpriteState::from_render_state(initial_state, held, include_latched_sprite_state);
    if sprite_dma_observed && !include_latched_sprite_state {
        regs.disarm_nonheld_latched_output();
    }
    let visible_end = visible_line0 + rows as i32;
    let mut next_beam: [(i32, usize); 8] = std::array::from_fn(|sprite| {
        if include_latched_sprite_state || held[sprite].is_some() {
            (visible_line0, 0usize)
        } else {
            (visible_end, 0usize)
        }
    });
    let mut lines = vec![Vec::new(); 8];

    for event in events {
        let off = event.offset & 0x01FE;
        if off == 0x100 {
            // BPLCON0.SHRES controls whether SPRxCTL's 70 ns horizontal
            // sub-position bit participates in the comparator.
            regs.apply_write(off, event.value);
            continue;
        }
        if off == 0x1FC {
            // FMODE changes the manual sprite output width, so flush every
            // sprite's pending span at the old width before applying it.
            let event_beam = manual_sprite_event_beam(event.vpos, event.hpos, visible_line0, rows);
            for sprite in 0..8 {
                flush_manual_sprite_lines(
                    sprite,
                    &regs,
                    next_beam[sprite],
                    event_beam,
                    ManualSpriteFlushMode::ClipAtEnd,
                    &mut lines,
                );
                next_beam[sprite] = event_beam;
            }
            regs.apply_write(off, event.value);
            continue;
        }
        if !(0x140..=0x17F).contains(&off) {
            continue;
        }
        let sprite = ((off - 0x140) / 8) as usize;
        if sprite >= 8 {
            continue;
        }
        let event_beam = manual_sprite_event_beam_for_sprite_write(
            off,
            event.vpos,
            event.hpos,
            visible_line0,
            rows,
            matches!(event.source, BeamWriteSource::Copper),
        );
        flush_manual_sprite_lines(
            sprite,
            &regs,
            next_beam[sprite],
            event_beam,
            ManualSpriteFlushMode::PreserveStartedOutput,
            &mut lines,
        );
        if sprite_dma_observed
            && !include_latched_sprite_state
            && held[sprite].is_none()
            && (off - 0x140) & 0x0006 == 0
            && event.hpos < SPRITE_DMA_PAIR_CAPTURE_HPOS[sprite / 2]
        {
            if regs.direct_data_armed[sprite] {
                regs.spr_armed[sprite] = false;
            }
            regs.direct_data_armed[sprite] = false;
        }
        regs.apply_write(off, event.value);
        if sprite_dma_observed
            && (event.vpos as i32) < visible_line0
            && matches!((off - 0x140) & 0x0006, 0x4 | 0x6)
        {
            regs.direct_data_armed[sprite] = false;
        }
        next_beam[sprite] = event_beam;
    }

    for sprite in 0..8 {
        flush_manual_sprite_lines(
            sprite,
            &regs,
            next_beam[sprite],
            (visible_end, 0),
            ManualSpriteFlushMode::ClipAtEnd,
            &mut lines,
        );
    }

    lines
}

pub(super) fn manual_sprite_lines_from_captured_dma_reuse(
    initial_state: &RenderState,
    events: &[BeamRegisterWrite],
    captured_sprite_lines: &[CapturedSpriteLine],
    visible_line0: i32,
    rows: usize,
) -> Vec<Vec<SpriteLine>> {
    let mut lines = vec![Vec::new(); 8];
    if captured_sprite_lines.is_empty() || events.is_empty() {
        return lines;
    }

    let visible_end = visible_line0 + rows as i32;
    let mut events_by_sprite: [Vec<BeamRegisterWrite>; 8] = std::array::from_fn(|_| Vec::new());
    for event in events {
        let off = event.offset & 0x01FE;
        if !(0x140..=0x17F).contains(&off) {
            continue;
        }
        let sprite = ((off - 0x140) / 8) as usize;
        if sprite < events_by_sprite.len() {
            events_by_sprite[sprite].push(*event);
        }
    }

    for captured in captured_sprite_lines {
        let sprite = captured.sprite;
        if sprite >= 8 || events_by_sprite[sprite].is_empty() {
            continue;
        }
        let beam_y = captured.beam_y;
        if beam_y < visible_line0 || beam_y >= visible_end {
            continue;
        }

        let mut held = [None; 8];
        held[sprite] = Some(HeldSpriteLine {
            line: *captured,
            vstart: beam_y,
            vstop: beam_y + 1,
        });
        let mut regs = BeamSpriteState::from_render_state(initial_state, &held, false);
        let mut next_beam = (visible_end, 0usize);
        let dma_hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[sprite / 2];

        for event in events_by_sprite[sprite]
            .iter()
            .filter(|event| event.vpos as i32 == beam_y && event.hpos >= dma_hpos)
        {
            let off = event.offset & 0x01FE;
            let event_beam = manual_sprite_event_beam_for_sprite_write(
                off,
                event.vpos,
                event.hpos,
                visible_line0,
                rows,
                matches!(event.source, BeamWriteSource::Copper),
            );

            match (off - 0x140) & 0x0006 {
                // SPRxPOS re-arms the horizontal comparator. If sprite DMA has
                // already loaded this scanline's data, a later POS write can
                // reuse that data without another SPRxDATA write.
                0x0 => {
                    flush_manual_sprite_lines(
                        sprite,
                        &regs,
                        next_beam,
                        event_beam,
                        ManualSpriteFlushMode::PreserveStartedOutput,
                        &mut lines,
                    );
                    regs.apply_write(off, event.value);
                    next_beam = event_beam;
                }
                // DATA/CTL writes leave the DMA-seeded reuse model. The normal
                // beam-timed manual replay handles explicitly written data;
                // CTL disarms output until DATA arms it again. The disarm has
                // to stick in this replay too: it clears Denise's armed latch,
                // so a later SPRxPOS write on the same line cannot serialize
                // the DMA-loaded words either.
                _ => {
                    flush_manual_sprite_lines(
                        sprite,
                        &regs,
                        next_beam,
                        event_beam,
                        ManualSpriteFlushMode::ClipAtEnd,
                        &mut lines,
                    );
                    if (off - 0x140) & 0x0006 == 0x2 {
                        regs.apply_write(off, event.value);
                    }
                    next_beam = (visible_end, 0);
                }
            }
        }

        flush_manual_sprite_lines(
            sprite,
            &regs,
            next_beam,
            (beam_y + 1, 0),
            ManualSpriteFlushMode::ClipAtEnd,
            &mut lines,
        );
    }

    lines
}

/// Sprite DMA lands its fetched words in Denise's data latches and arms the
/// channel, but the serializer only copies those latches when the horizontal
/// comparator fires at HSTART. A SPRxCTL write clears the armed bit, so a CTL
/// write that lands between the fetch slot and the comparator match cancels
/// the fetched line outright: nothing is ever loaded for it. That is how a
/// Copper-multiplexed sprite panel retires its channels on the line below the
/// panel, where sprite DMA is still fetching against a descriptor whose
/// vertical stop never matches. A CTL write *after* the match cannot recall
/// pixels the serializer has already shifted out, and a SPRxDATA write arms
/// the channel again.
///
/// Returns the captured lines that are still armed when their comparator
/// fires; a cancelled fetch reaches neither the renderer nor the collision
/// accumulator. HSTART is in lo-res pixels and the beam position in colour
/// clocks, hence the halving.
pub(super) fn retain_armed_captured_sprite_lines<'a>(
    captured_sprite_lines: &'a [CapturedSpriteLine],
    events: &[BeamRegisterWrite],
) -> Cow<'a, [CapturedSpriteLine]> {
    let is_sprite_ctl_write = |event: &BeamRegisterWrite| {
        let off = event.offset & 0x01FE;
        (0x140..=0x17F).contains(&off) && (off - 0x140) & 0x0006 == 0x2
    };
    if captured_sprite_lines.is_empty() || !events.iter().any(is_sprite_ctl_write) {
        return Cow::Borrowed(captured_sprite_lines);
    }

    // Per channel, the writes that move the armed latch as (line, beam, arms),
    // sorted so each captured line can binary-search its own line's block.
    let mut arming_writes: [Vec<(u32, u32, bool)>; 8] = std::array::from_fn(|_| Vec::new());
    for event in events {
        let off = event.offset & 0x01FE;
        if !(0x140..=0x17F).contains(&off) {
            continue;
        }
        // Only SPRxCTL (disarm) and SPRxDATA (arm) touch the armed latch.
        let arms = match (off - 0x140) & 0x0006 {
            0x2 => false,
            0x4 => true,
            _ => continue,
        };
        let sprite = ((off - 0x140) / 8) as usize;
        arming_writes[sprite].push((event.vpos, event.hpos, arms));
    }
    for writes in &mut arming_writes {
        writes.sort_unstable();
    }

    let armed_at_hstart = |line: &CapturedSpriteLine| {
        if line.sprite >= 8 || line.beam_y < 0 {
            return true;
        }
        let writes = &arming_writes[line.sprite];
        let dma_hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[line.sprite / 2];
        let match_hpos = (line.hstart.max(0) / 2) as u32;
        let beam_y = line.beam_y as u32;
        let first = writes.partition_point(|&(vpos, hpos, _)| (vpos, hpos) < (beam_y, dma_hpos));
        let mut armed = true;
        for &(vpos, hpos, arms) in &writes[first..] {
            if vpos != beam_y || hpos > match_hpos {
                break;
            }
            armed = arms;
        }
        armed
    };

    if captured_sprite_lines.iter().all(armed_at_hstart) {
        return Cow::Borrowed(captured_sprite_lines);
    }
    Cow::Owned(
        captured_sprite_lines
            .iter()
            .filter(|line| armed_at_hstart(line))
            .copied()
            .collect(),
    )
}

pub(super) fn merge_dma_seeded_manual_sprite_lines(
    manual_lines: &mut [Vec<SpriteLine>],
    mut dma_seeded_lines: Vec<Vec<SpriteLine>>,
) {
    for (sprite, seeded) in dma_seeded_lines.iter_mut().enumerate() {
        if seeded.is_empty() {
            continue;
        }
        let target = &mut manual_lines[sprite];
        clip_sprite_lines_around_register_lines(seeded, target);
        target.append(seeded);
        target.sort_by_key(|line| (line.beam_y, line.x_start, line.x_stop));
    }
}

pub(super) fn manual_sprite_event_beam_for_sprite_write(
    off: u16,
    vpos: u32,
    hpos: u32,
    visible_line0: i32,
    rows: usize,
    copper: bool,
) -> (i32, usize) {
    match (off - 0x140) & 0x0006 {
        // SPRxPOS re-arms the sprite horizontal comparator. When the
        // write happens before the newly programmed HSTART, the sprite can
        // still begin at HSTART; clipping in the later colour-output register
        // domain delays attached pairs whose even/odd position writes are
        // staggered by the Copper.
        0x0 => manual_sprite_position_event_beam(vpos, hpos, visible_line0, rows, copper),
        // SPRxDATA/SPRxDATB update the latches copied by Denise's horizontal
        // sprite comparator. If the write reaches that path before the
        // comparator fires, the new data belongs to the current scanline.
        0x4 | 0x6 => manual_sprite_data_event_beam(vpos, hpos, visible_line0, rows, copper),
        _ => manual_sprite_event_beam(vpos, hpos, visible_line0, rows),
    }
}

pub(super) fn manual_sprite_event_beam(
    vpos: u32,
    hpos: u32,
    visible_line0: i32,
    rows: usize,
) -> (i32, usize) {
    let visible_end = visible_line0 + rows as i32;
    let vpos = vpos as i32;
    if vpos < visible_line0 {
        return (visible_line0, 0);
    }
    if vpos >= visible_end {
        return (visible_end, 0);
    }
    let (_, x) = beam_to_framebuffer_pos_with_visible_line0(vpos as u32, hpos, visible_line0, rows);
    (vpos, x)
}

pub(super) fn manual_sprite_position_event_beam(
    vpos: u32,
    hpos: u32,
    visible_line0: i32,
    rows: usize,
    copper: bool,
) -> (i32, usize) {
    let visible_end = visible_line0 + rows as i32;
    let vpos = vpos as i32;
    if vpos < visible_line0 {
        return (visible_line0, 0);
    }
    if vpos >= visible_end {
        return (visible_end, 0);
    }
    let x = sprite_position_write_framebuffer_x_from(hpos, copper);
    (vpos, x)
}

pub(super) fn manual_sprite_data_event_beam(
    vpos: u32,
    hpos: u32,
    visible_line0: i32,
    rows: usize,
    copper: bool,
) -> (i32, usize) {
    let visible_end = visible_line0 + rows as i32;
    let vpos = vpos as i32;
    if vpos < visible_line0 {
        return (visible_line0, 0);
    }
    if vpos >= visible_end {
        return (visible_end, 0);
    }
    let x = sprite_data_write_framebuffer_x_from(hpos, copper);
    (vpos, x)
}

/// Reposition pipeline for the writer: the Copper's bus landings carry the
/// WAIT-comparator lookahead, so copper-sourced sprite writes use the shorter
/// [`COPPER_SPRITE_REGISTER_WRITE_PIPELINE_CCK`] to reposition where the demo
/// author (and vAmiga) place them; CPU writes keep the calibrated pipeline.
fn sprite_register_write_pipeline_cck(copper: bool) -> u32 {
    if copper {
        COPPER_SPRITE_REGISTER_WRITE_PIPELINE_CCK
    } else {
        SPRITE_REGISTER_WRITE_PIPELINE_CCK
    }
}

pub(super) fn sprite_position_write_framebuffer_x_from(hpos: u32, copper: bool) -> usize {
    let hpos = hpos.saturating_sub(sprite_register_write_pipeline_cck(copper));
    ((hpos as i32 * 2 - DIW_HSTART_FB0) * 2).clamp(0, FB_WIDTH as i32) as usize
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn sprite_position_write_framebuffer_x(hpos: u32) -> usize {
    sprite_position_write_framebuffer_x_from(hpos, false)
}

pub(super) fn sprite_data_write_framebuffer_x_from(hpos: u32, copper: bool) -> usize {
    sprite_position_write_framebuffer_x_from(hpos, copper)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn sprite_data_write_framebuffer_x(hpos: u32) -> usize {
    sprite_position_write_framebuffer_x_from(hpos, false)
}

pub(super) fn flush_manual_sprite_lines(
    sprite: usize,
    regs: &BeamSpriteState,
    start_beam: (i32, usize),
    end_beam: (i32, usize),
    mode: ManualSpriteFlushMode,
    lines: &mut [Vec<SpriteLine>],
) {
    let (start_line, start_x) = start_beam;
    let (end_line, end_x) = end_beam;
    if start_line > end_line || (start_line == end_line && start_x >= end_x) {
        return;
    }
    let end_exclusive = if end_x == 0 { end_line } else { end_line + 1 };
    for beam_y in start_line..end_exclusive {
        let x_start = if beam_y == start_line { start_x } else { 0 };
        let mut x_stop = if beam_y == end_line { end_x } else { FB_WIDTH };
        if mode == ManualSpriteFlushMode::PreserveStartedOutput && beam_y == end_line {
            let pos = regs.sprpos[sprite];
            let ctl = regs.sprctl[sprite];
            let base_x = sprite_nominal_base_framebuffer_x(pos, ctl, regs.bplcon0, regs.fmode);
            if x_stop as i32 >= base_x {
                x_stop = FB_WIDTH;
            }
        }
        if let Some(line) = regs.line_for_sprite(sprite, beam_y, x_start, x_stop) {
            lines[sprite].push(line);
        }
    }
}

#[cfg(test)]
pub(super) fn render_sprites(
    state: &RenderState,
    ram: &[u8],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    playfield_mask: &[u8],
    collision_pixels: &mut [CollisionPixel],
    sprite_ptr_refreshed: [bool; 8],
    captured_sprite_lines: &[CapturedSpriteLine],
    sprite_dma_observed: bool,
) -> u16 {
    #[cfg(feature = "internal-diagnostics")]
    if crate::envcfg::flag("COPPERLINE_EXP_NO_SPRITE_RENDER") {
        return 0;
    }
    render_sprites_with_manual_lines(
        state,
        ram,
        fb,
        clip,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        playfield_mask,
        collision_pixels,
        sprite_pointer_refreshes_from_mask(sprite_ptr_refreshed),
        captured_sprite_lines,
        sprite_dma_observed,
        None,
    )
}

#[cfg(test)]
pub(super) fn render_sprites_with_manual_lines(
    state: &RenderState,
    ram: &[u8],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    playfield_mask: &[u8],
    collision_pixels: &mut [CollisionPixel],
    // Sprite pointer refreshes are no longer consumed by the renderer (captured
    // sprite DMA is authoritative); kept so existing renderer tests compile.
    _sprite_ptr_refreshes: [SpritePointerRefresh; 8],
    captured_sprite_lines: &[CapturedSpriteLine],
    sprite_dma_observed: bool,
    manual_sprite_lines: Option<&[Vec<SpriteLine>]>,
) -> u16 {
    let sprite_display_enable_x_by_y = sprite_display_enabled_from_line_start();
    render_sprites_with_manual_lines_and_writes(
        state,
        ram,
        fb,
        clip,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        &sprite_display_enable_x_by_y,
        playfield_mask,
        collision_pixels,
        captured_sprite_lines,
        sprite_dma_observed,
        manual_sprite_lines,
        PAL_VISIBLE_LINE0,
    )
}

#[cfg(test)]
pub(super) fn render_sprites_with_manual_lines_and_writes(
    state: &RenderState,
    ram: &[u8],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    collision_pixels: &mut [CollisionPixel],
    captured_sprite_lines: &[CapturedSpriteLine],
    sprite_dma_observed: bool,
    manual_sprite_lines: Option<&[Vec<SpriteLine>]>,
    visible_line0: i32,
) -> u16 {
    let mut sprite_group_mask = Vec::new();
    let mut sprite_lines = std::array::from_fn(|_| Vec::new());
    let mut attached_beams = std::array::from_fn(|_| Vec::new());
    let mut sprite_subpixels = SpriteSubpixelState::from_collapsed(fb, playfield_mask);
    render_sprites_with_manual_lines_and_writes_reusing_mask(
        state,
        ram,
        fb,
        clip,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        sprite_display_enable_x_by_y,
        playfield_mask,
        &mut sprite_subpixels,
        collision_pixels,
        &mut sprite_group_mask,
        &mut sprite_lines,
        &mut attached_beams,
        captured_sprite_lines,
        sprite_dma_observed,
        manual_sprite_lines,
        visible_line0,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_sprites_with_manual_lines_and_writes_reusing_mask(
    state: &RenderState,
    ram: &[u8],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    sprite_subpixels: &mut SpriteSubpixelState,
    collision_pixels: &mut [CollisionPixel],
    sprite_group_mask: &mut Vec<u8>,
    sprite_lines: &mut [Vec<SpriteLine>; 8],
    attached_beams: &mut [Vec<i32>; 4],
    captured_sprite_lines: &[CapturedSpriteLine],
    sprite_dma_observed: bool,
    manual_sprite_lines: Option<&[Vec<SpriteLine>]>,
    visible_line0: i32,
) -> u16 {
    #[cfg(feature = "internal-diagnostics")]
    if crate::envcfg::flag("COPPERLINE_EXP_NO_SPRITE_RENDER") {
        return 0;
    }

    if ram.is_empty() && !sprite_dma_observed {
        return 0;
    }

    let mut clxdat = 0u16;
    sprite_group_mask.resize(fb.len(), 0);
    sprite_group_mask.fill(0);
    let use_captured_sprite_dma = sprite_dma_observed;
    for (sprite, lines) in sprite_lines.iter_mut().enumerate() {
        collect_sprite_lines_into(
            sprite,
            state,
            captured_sprite_lines,
            use_captured_sprite_dma,
            manual_sprite_lines,
            lines,
        );
    }

    // Draw low-priority sprite pairs first so lower-numbered pairs
    // overwrite higher-numbered pairs, matching Denise's fixed sprite
    // group priority.
    for pair in (0..4).rev() {
        let even_sprite = pair * 2;
        let odd_sprite = even_sprite + 1;
        let even_lines = &sprite_lines[even_sprite];
        let odd_lines = &sprite_lines[odd_sprite];
        pair_attached_beams_into(even_lines, odd_lines, &mut attached_beams[pair]);
        let pair_attached_beams = &attached_beams[pair];

        clxdat |= render_attached_sprite_pair_lines(
            even_sprite,
            even_lines,
            odd_lines,
            pair_attached_beams,
            fb,
            clip,
            base_palettes,
            palette_segments,
            base_controls,
            control_segments,
            sprite_display_enable_x_by_y,
            playfield_mask,
            sprite_subpixels,
            collision_pixels,
            sprite_group_mask,
            visible_line0,
        );
        clxdat |= render_unattached_sprite_pair_lines(
            even_sprite,
            even_lines,
            odd_lines,
            pair_attached_beams,
            fb,
            clip,
            base_palettes,
            palette_segments,
            base_controls,
            control_segments,
            sprite_display_enable_x_by_y,
            playfield_mask,
            sprite_subpixels,
            collision_pixels,
            sprite_group_mask,
            visible_line0,
        );
    }
    clxdat
}

/// The sorted, deduplicated beam lines on which this sprite pair has an
/// attached line. Computed once per pair: the per-line attach test is a
/// binary search here instead of a rescan of every line of the pair
/// (quadratic on sprite-multiplexing games, a measured render hot spot).
fn pair_attached_beams_into(
    even_lines: &[SpriteLine],
    odd_lines: &[SpriteLine],
    beams: &mut Vec<i32>,
) {
    beams.clear();
    beams.extend(
        even_lines
            .iter()
            .chain(odd_lines.iter())
            .filter(|line| line.attached)
            .map(|line| line.beam_y),
    );
    beams.sort_unstable();
    beams.dedup();
}

pub(super) fn render_unattached_sprite_pair_lines(
    even_sprite: usize,
    even_lines: &[SpriteLine],
    odd_lines: &[SpriteLine],
    attached_beams: &[i32],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    sprite_subpixels: &mut SpriteSubpixelState,
    collision_pixels: &mut [CollisionPixel],
    sprite_group_mask: &mut [u8],
    visible_line0: i32,
) -> u16 {
    let mut clxdat = 0u16;
    let odd_sprite = even_sprite + 1;
    clxdat |= render_collected_sprite_lines(
        odd_sprite,
        odd_lines,
        |line| attached_beams.binary_search(&line.beam_y).is_err(),
        fb,
        clip,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        sprite_display_enable_x_by_y,
        playfield_mask,
        sprite_subpixels,
        collision_pixels,
        sprite_group_mask,
        visible_line0,
    );
    for even in even_lines {
        if attached_beams.binary_search(&even.beam_y).is_ok() {
            continue;
        }
        clxdat |= draw_sprite_line(
            even_sprite,
            even,
            fb,
            clip,
            base_palettes,
            palette_segments,
            base_controls,
            control_segments,
            sprite_display_enable_x_by_y,
            playfield_mask,
            sprite_subpixels,
            collision_pixels,
            sprite_group_mask,
            visible_line0,
        );
    }
    clxdat
}

pub(super) fn render_collected_sprite_lines<F>(
    sprite: usize,
    lines: &[SpriteLine],
    mut include_line: F,
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    sprite_subpixels: &mut SpriteSubpixelState,
    collision_pixels: &mut [CollisionPixel],
    sprite_group_mask: &mut [u8],
    visible_line0: i32,
) -> u16
where
    F: FnMut(&SpriteLine) -> bool,
{
    let mut clxdat = 0u16;
    for line in lines {
        if !include_line(line) {
            continue;
        }
        clxdat |= draw_sprite_line(
            sprite,
            line,
            fb,
            clip,
            base_palettes,
            palette_segments,
            base_controls,
            control_segments,
            sprite_display_enable_x_by_y,
            playfield_mask,
            sprite_subpixels,
            collision_pixels,
            sprite_group_mask,
            visible_line0,
        );
    }
    clxdat
}

pub(super) fn render_attached_sprite_pair_lines(
    even_sprite: usize,
    even_lines: &[SpriteLine],
    odd_lines: &[SpriteLine],
    attached_beams: &[i32],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    sprite_subpixels: &mut SpriteSubpixelState,
    collision_pixels: &mut [CollisionPixel],
    sprite_group_mask: &mut [u8],
    visible_line0: i32,
) -> u16 {
    let mut clxdat = 0u16;
    for &beam_y in attached_beams {
        let y = beam_y - visible_line0;
        if y < 0 || y >= base_controls.len() as i32 {
            continue;
        }
        let y = y as usize;
        if y < clip.y_start || y >= clip.y_stop {
            continue;
        }

        let even_beam_lines: Vec<SpriteLineSampler<'_>> = even_lines
            .iter()
            .filter(|line| line.beam_y == beam_y)
            .map(|line| SpriteLineSampler::new(line, base_controls[y], &control_segments[y]))
            .collect();
        let odd_beam_lines: Vec<SpriteLineSampler<'_>> = odd_lines
            .iter()
            .filter(|line| line.beam_y == beam_y)
            .map(|line| SpriteLineSampler::new(line, base_controls[y], &control_segments[y]))
            .collect();

        let mut x_start = FB_WIDTH as i32;
        let mut x_stop = 0i32;
        for line in even_beam_lines.iter().chain(odd_beam_lines.iter()) {
            if let Some((start, stop)) = line.framebuffer_range() {
                x_start = x_start.min(start);
                x_stop = x_stop.max(stop);
            }
        }
        x_start = x_start.max(clip.x_start as i32);
        x_stop = x_stop.min(clip.x_stop as i32);
        if x_start >= x_stop {
            continue;
        }

        for x in x_start..x_stop {
            let x_usize = x as usize;
            let even_left = sprite_line_samplers_subpixel_bits_at(&even_beam_lines, x * 2);
            let even_right = sprite_line_samplers_subpixel_bits_at(&even_beam_lines, x * 2 + 1);
            let odd_left = sprite_line_samplers_subpixel_bits_at(&odd_beam_lines, x * 2);
            let odd_right = sprite_line_samplers_subpixel_bits_at(&odd_beam_lines, x * 2 + 1);
            let left_idx = even_left | (odd_left << 2);
            let right_idx = even_right | (odd_right << 2);
            if left_idx == 0 && right_idx == 0 {
                continue;
            }
            let control = control_at_x(base_controls[y], &control_segments[y], x_usize);
            if !sprite_pixel_inside_display_window(
                control,
                y,
                x_usize,
                visible_line0,
                sprite_display_enable_x_for_y(sprite_display_enable_x_by_y, y),
            ) {
                continue;
            }
            let fb_idx = y * FB_WIDTH + x_usize;
            clxdat |= generated_sprite_pair_collision_bits(
                even_sprite,
                fb_idx,
                control.clxcon,
                even_left != 0 || even_right != 0,
                odd_left != 0 || odd_right != 0,
                sprite_group_mask,
                collision_pixels,
                playfield_mask,
            );
            let subpixel_masks = sprite_subpixels.playfield_masks[fb_idx];
            let left_visible =
                left_idx != 0 && sprite_has_priority(even_sprite, subpixel_masks[0], control);
            let right_visible =
                right_idx != 0 && sprite_has_priority(even_sprite, subpixel_masks[1], control);
            if !left_visible && !right_visible {
                continue;
            }
            // Debugger layer isolation: an attached pair's pixels use both
            // channels, so hiding either sprite hides the pair's output.
            // Collisions above are already accumulated from the true data.
            let sprite_mask = super::active_debug_sprite_mask();
            if sprite_mask & (0b11 << even_sprite) != 0b11 << even_sprite {
                continue;
            }
            let resolve = |idx| {
                let color_idx = sprite_color_entry(control, even_sprite, idx, true);
                let entry =
                    palette_entry_at_x(&base_palettes[y], &palette_segments[y], x_usize, color_idx);
                sprite_pixel_rgba(control, entry)
            };
            let (left, right) = if left_visible && right_visible && left_idx == right_idx {
                let pixel = resolve(left_idx);
                (Some(pixel), Some(pixel))
            } else {
                (
                    left_visible.then(|| resolve(left_idx)),
                    right_visible.then(|| resolve(right_idx)),
                )
            };
            let canvas_scale = super::active_canvas_scale();
            let out_base = y * FB_WIDTH * canvas_scale + x_usize * canvas_scale;
            paint_sprite_subpixel_pair(
                fb,
                out_base,
                canvas_scale,
                &mut sprite_subpixels.pixels[fb_idx],
                sprite_subpixels.sources.get_mut(fb_idx),
                even_sprite,
                left,
                right,
            );
        }
    }
    clxdat
}

pub(super) fn sprite_pair_attach_active_for_beam(
    even_lines: &[SpriteLine],
    odd_lines: &[SpriteLine],
    beam_y: i32,
) -> bool {
    even_lines
        .iter()
        .chain(odd_lines.iter())
        .any(|line| line.beam_y == beam_y && line.attached)
}

pub(super) fn sprite_lines_pixel_bits_at(
    lines: &[SpriteLine],
    beam_y: i32,
    y: usize,
    x: i32,
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
) -> u8 {
    lines
        .iter()
        .filter(|line| line.beam_y == beam_y)
        .find_map(|line| {
            let idx = sprite_line_pixel_bits_at(line, x, base_controls[y], &control_segments[y]);
            (idx != 0).then_some(idx)
        })
        .unwrap_or(0)
}

pub(super) fn sprite_line_samplers_subpixel_bits_at(
    lines: &[SpriteLineSampler<'_>],
    subpixel: i32,
) -> u8 {
    lines
        .iter()
        .find_map(|line| {
            let idx = line.subpixel_bits_at(subpixel);
            (idx != 0).then_some(idx)
        })
        .unwrap_or(0)
}

pub(super) fn sprite_line_pixel_bits_at(
    line: &SpriteLine,
    x: i32,
    base_control: ControlState,
    control_segments: &[ControlSegment],
) -> u8 {
    SpriteLineSampler::new(line, base_control, control_segments).pixel_bits_at(x)
}

pub(super) fn clip_sprite_lines_around_register_lines(
    lines: &mut Vec<SpriteLine>,
    register_lines: &[SpriteLine],
) {
    if lines.is_empty() || register_lines.is_empty() {
        return;
    }

    let mut clipped = Vec::with_capacity(lines.len());
    for line in lines.drain(..) {
        let mut segments = vec![(line.x_start, line.x_stop)];
        for register_line in register_lines
            .iter()
            .filter(|register_line| register_line.beam_y == line.beam_y)
        {
            let mask_start = register_line.x_start.max(line.x_start);
            let mask_stop = register_line.x_stop.min(line.x_stop);
            if mask_start >= mask_stop {
                continue;
            }
            let mut next_segments = Vec::new();
            for (start, stop) in segments {
                if start < mask_start {
                    next_segments.push((start, mask_start));
                }
                if mask_stop < stop {
                    next_segments.push((mask_stop, stop));
                }
            }
            segments = next_segments;
            if segments.is_empty() {
                break;
            }
        }
        for (x_start, x_stop) in segments {
            let mut segment = line;
            segment.x_start = x_start;
            segment.x_stop = x_stop;
            clipped.push(segment);
        }
    }
    *lines = clipped;
}

#[inline]
fn paint_sprite_subpixel_pair(
    fb: &mut [u32],
    out_base: usize,
    canvas_scale: usize,
    composite: &mut [u32; 2],
    sources: Option<&mut [u8; 2]>,
    sprite: usize,
    left: Option<u32>,
    right: Option<u32>,
) {
    if let Some(pixel) = left {
        composite[0] = pixel;
    }
    if let Some(pixel) = right {
        composite[1] = pixel;
    }
    if let Some(sources) = sources {
        let source = super::PIXEL_SOURCE_SPRITE0 + sprite.min(7) as u8;
        if left.is_some() {
            sources[0] = source;
        }
        if right.is_some() {
            sources[1] = source;
        }
    }
    if left.is_none() && right.is_none() {
        return;
    }

    if canvas_scale == 2 {
        fb[out_base..out_base + 2].copy_from_slice(composite);
    } else {
        fb[out_base] = rgba8_blend_halves(composite[0], composite[1]);
    }
}

#[inline]
fn sprite_pixel_rgba(control: ControlState, entry: crate::chipset::denise::PaletteEntry) -> u32 {
    let color_latch = entry.latch();
    let transparent = control.genlock_transparent(color_latch, None, false);
    let color = if control.aga() {
        entry.rgb24() & 0x00FF_FFFF
    } else {
        rgb12_to_rgb24(color_rgb12(color_latch))
    };
    rgb24_to_rgba8_alpha(color, !transparent)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_sprite_line(
    sprite: usize,
    line: &SpriteLine,
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    sprite_subpixels: &mut SpriteSubpixelState,
    collision_pixels: &mut [CollisionPixel],
    sprite_group_mask: &mut [u8],
    visible_line0: i32,
) -> u16 {
    let y = line.beam_y - visible_line0;
    if y < 0 || y >= base_controls.len() as i32 {
        return 0;
    }
    let y = y as usize;
    if y < clip.y_start || y >= clip.y_stop {
        return 0;
    }
    let sampler = SpriteLineSampler::new(line, base_controls[y], &control_segments[y]);
    let Some((mut x_start, mut x_stop)) = sampler.framebuffer_range() else {
        return 0;
    };
    x_start = x_start.max(clip.x_start as i32);
    x_stop = x_stop.min(clip.x_stop as i32);
    let mut clxdat = 0u16;

    for x in x_start..x_stop {
        let left_idx = sampler.subpixel_bits_at(x * 2);
        let right_idx = sampler.subpixel_bits_at(x * 2 + 1);
        if left_idx == 0 && right_idx == 0 {
            continue;
        }
        let x = x as usize;
        let fb_idx = y * FB_WIDTH + x;
        let control = control_at_x(base_controls[y], &control_segments[y], x);
        if !sprite_pixel_inside_display_window(
            control,
            y,
            x,
            visible_line0,
            sprite_display_enable_x_for_y(sprite_display_enable_x_by_y, y),
        ) {
            continue;
        }
        clxdat |= generated_sprite_collision_bits(
            sprite,
            fb_idx,
            control.clxcon,
            sprite_group_mask,
            collision_pixels,
            playfield_mask,
        );
        let subpixel_masks = sprite_subpixels.playfield_masks[fb_idx];
        let left_visible = left_idx != 0 && sprite_has_priority(sprite, subpixel_masks[0], control);
        let right_visible =
            right_idx != 0 && sprite_has_priority(sprite, subpixel_masks[1], control);
        if !left_visible && !right_visible {
            continue;
        }
        // Debugger layer isolation: output only, after the collision bits
        // above are accumulated from the true 35 ns samples.
        if super::active_debug_sprite_mask() & (1 << sprite) == 0 {
            continue;
        }
        let resolve = |idx| {
            let color_idx = sprite_color_entry(control, sprite, idx, false);
            let entry = palette_entry_at_x(&base_palettes[y], &palette_segments[y], x, color_idx);
            sprite_pixel_rgba(control, entry)
        };
        let (left, right) = if left_visible && right_visible && left_idx == right_idx {
            let pixel = resolve(left_idx);
            (Some(pixel), Some(pixel))
        } else {
            (
                left_visible.then(|| resolve(left_idx)),
                right_visible.then(|| resolve(right_idx)),
            )
        };
        let canvas_scale = super::active_canvas_scale();
        let out_base = y * FB_WIDTH * canvas_scale + x * canvas_scale;
        paint_sprite_subpixel_pair(
            fb,
            out_base,
            canvas_scale,
            &mut sprite_subpixels.pixels[fb_idx],
            sprite_subpixels.sources.get_mut(fb_idx),
            sprite,
            left,
            right,
        );
    }
    clxdat
}

pub(super) fn collect_sprite_lines(
    sprite: usize,
    state: &RenderState,
    captured_sprite_lines: &[CapturedSpriteLine],
    use_captured_sprite_dma: bool,
    manual_sprite_lines: Option<&[Vec<SpriteLine>]>,
) -> Vec<SpriteLine> {
    let mut lines = Vec::new();
    collect_sprite_lines_into(
        sprite,
        state,
        captured_sprite_lines,
        use_captured_sprite_dma,
        manual_sprite_lines,
        &mut lines,
    );
    lines
}

fn collect_sprite_lines_into(
    sprite: usize,
    state: &RenderState,
    captured_sprite_lines: &[CapturedSpriteLine],
    use_captured_sprite_dma: bool,
    manual_sprite_lines: Option<&[Vec<SpriteLine>]>,
    lines: &mut Vec<SpriteLine>,
) {
    let sprite_dma_blocked_by_ddf = sprite_dma_disabled_by_bitplane_ddf(
        sprite,
        state.agnus_revision,
        state.bplcon0,
        state.fmode,
        state.dmacon,
        state.ddfstrt,
        state.ddfstop,
        state.harddis,
    );
    lines.clear();

    if use_captured_sprite_dma && !sprite_dma_blocked_by_ddf {
        lines.extend(
            captured_sprite_lines
                .iter()
                .filter(|line| line.sprite == sprite)
                .map(|line| SpriteLine {
                    hstart: line.hstart,
                    hsub_70ns: line.hsub_70ns,
                    beam_y: line.beam_y,
                    data: line.data,
                    datb: line.datb,
                    data_ext: line.data_ext,
                    datb_ext: line.datb_ext,
                    width_words: line.width_words,
                    attached: line.attached,
                    x_start: 0,
                    x_stop: FB_WIDTH,
                }),
        );
    }

    if let Some(register_lines) = manual_sprite_lines.and_then(|lines| lines.get(sprite)) {
        clip_sprite_lines_around_register_lines(lines, register_lines);
        lines.extend_from_slice(register_lines);
        return;
    }

    // With no captured sprite DMA for this frame, render any armed sprites
    // from their latched registers (CPU-driven sprites); captured DMA is the
    // source whenever Agnus actually fetched sprite data.
    if !use_captured_sprite_dma && state.spr_hw_armed[sprite] {
        let regs = BeamSpriteState::from_render_state(state, &[None; 8], true);
        let pos = regs.sprpos[sprite];
        let ctl = regs.sprctl[sprite];
        lines.extend(
            (sprite_vstart(pos, ctl)..sprite_vstop(ctl))
                .filter_map(|beam_y| regs.line_for_sprite(sprite, beam_y, 0, FB_WIDTH)),
        );
    }
}

pub(super) fn sprite_has_priority(sprite: usize, playfield: u8, control: ControlState) -> bool {
    // Denise resolves the two playfields against each other first (opacity,
    // then PF2PRI where both are opaque) and holds only the winning field's
    // BPLCON2 code in the depth comparison, so a sprite pixel is tested
    // against that single code: e.g. PF1P=0 with PF2P=4 keeps an opaque PF1
    // in front of a sprite that itself covers PF2 (vAmiga keeps one zPF value
    // per pixel in its z-buffer; cross-checked band by band by
    // timing-test/sprprobe-dpfpri, including the circular programmings where
    // PF2PRI and the sprite codes disagree about the field order).
    let winner = match playfield {
        0 => return true,
        3 if control.pf2_priority() => 2,
        3 => 1,
        pf => pf,
    };
    let group = (sprite / 2) as u8;
    group < control.playfield_priority_code(winner).min(4)
}

pub(super) fn sprite_base_framebuffer_x(
    hstart: i32,
    hsub_70ns: bool,
    base_control: ControlState,
    control_segments: &[ControlSegment],
) -> i32 {
    // The serializer's first pixel appears one lo-res pixel after the
    // horizontal comparator match (crate::bus::SPRITE_OUTPUT_DELAY_LORES,
    // ruler-probed against FS-UAE and vAmiga). The anchor carries the
    // active canvas shift like every comparator mapping.
    let hstart = crate::bus::sprite_hstart_for_fmode(hstart, base_control.fmode);
    let base_x = (hstart + crate::bus::SPRITE_OUTPUT_DELAY_LORES - DIW_HSTART_FB0
        + active_canvas_shift_h())
        * 2;
    let sample_x = base_x.clamp(0, FB_WIDTH.saturating_sub(1) as i32) as usize;
    let control = control_at_x(base_control, control_segments, sample_x);
    base_x + i32::from(hsub_70ns && control.shres())
}

pub(super) fn sprite_nominal_base_framebuffer_x(
    pos: u16,
    ctl: u16,
    bplcon0: u16,
    fmode: u16,
) -> i32 {
    let hstart = crate::bus::sprite_hstart_for_fmode(sprite_hstart(pos, ctl), fmode);
    (hstart + crate::bus::SPRITE_OUTPUT_DELAY_LORES - DIW_HSTART_FB0 + active_canvas_shift_h()) * 2
        + i32::from(sprite_hsub_70ns(ctl) && bplcon0 & BPLCON0_SHRES != 0)
}

pub(super) fn sprite_display_enable_x_for_y(
    sprite_display_enable_x_by_y: &[Option<usize>],
    y: usize,
) -> Option<usize> {
    if y < sprite_display_enable_x_by_y.len() {
        sprite_display_enable_x_by_y[y]
    } else {
        Some(0)
    }
}

pub(super) fn sprite_pixel_inside_display_window(
    control: ControlState,
    _y: usize,
    x: usize,
    _visible_line0: i32,
    display_enable_x: Option<usize>,
) -> bool {
    if control.border_sprite_enabled() {
        return true;
    }
    let Some(enable_x) = display_enable_x else {
        return false;
    };
    if x < enable_x {
        return false;
    }
    // OCS/ECS Denise clips normal sprites to the horizontal display window.
    // Bitplane DMA opens that gate at DIW's left edge even when DDFSTRT delays
    // the first playfield word; a manual BPL1DAT write can still open it on a
    // scanline where the vertical bitplane window is closed.
    let (x_start, x_stop) = control.display_window_x();
    x >= x_start && x < x_stop
}

pub(super) fn generated_sprite_collision_bits(
    sprite: usize,
    fb_idx: usize,
    clxcon: u16,
    sprite_group_mask: &mut [u8],
    collision_pixels: &mut [CollisionPixel],
    _playfield_mask: &[u8],
) -> u16 {
    let group = sprite / 2;
    if sprite & 1 != 0 && clxcon & (1 << (12 + group)) == 0 {
        return 0;
    }
    let bit = 1u8 << group;
    let mut clxdat = 0u16;
    let prior_sprites = sprite_group_mask[fb_idx];
    if prior_sprites != 0 {
        for other in 0..4 {
            if prior_sprites & (1 << other) != 0 && other != group {
                clxdat |= sprite_sprite_clx_bit(group, other);
            }
        }
    }
    let collision = collision_pixels[fb_idx];
    if collision.pf1_match() {
        clxdat |= 1 << (group + 1);
    }
    if collision.pf2_match() {
        clxdat |= 1 << (group + 5);
    }
    sprite_group_mask[fb_idx] |= bit;
    clxdat
}

pub(super) fn generated_sprite_pair_collision_bits(
    even_sprite: usize,
    fb_idx: usize,
    clxcon: u16,
    even_opaque: bool,
    odd_opaque: bool,
    sprite_group_mask: &mut [u8],
    collision_pixels: &mut [CollisionPixel],
    _playfield_mask: &[u8],
) -> u16 {
    let group = even_sprite / 2;
    // CLXCON bits 12..15 (ENSP1/3/5/7) gate the odd sprite of each pair
    // into collision detection.
    let odd_collides = odd_opaque && clxcon & (1 << (12 + group)) != 0;
    if !even_opaque && !odd_collides {
        return 0;
    }
    let bit = 1u8 << group;
    let mut clxdat = 0u16;
    let prior_sprites = sprite_group_mask[fb_idx];
    if prior_sprites != 0 {
        for other in 0..4 {
            if prior_sprites & (1 << other) != 0 && other != group {
                clxdat |= sprite_sprite_clx_bit(group, other);
            }
        }
    }
    let collision = collision_pixels[fb_idx];
    if collision.pf1_match() {
        clxdat |= 1 << (group + 1);
    }
    if collision.pf2_match() {
        clxdat |= 1 << (group + 5);
    }
    sprite_group_mask[fb_idx] |= bit;
    clxdat
}

pub(super) fn sprite_sprite_clx_bit(a: usize, b: usize) -> u16 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    match (lo, hi) {
        (0, 1) => 1 << 9,
        (0, 2) => 1 << 10,
        (0, 3) => 1 << 11,
        (1, 2) => 1 << 12,
        (1, 3) => 1 << 13,
        (2, 3) => 1 << 14,
        _ => 0,
    }
}

pub(super) fn sprite_vstart(pos: u16, ctl: u16) -> i32 {
    (((pos >> 8) & 0x00FF) | ((ctl & 0x0004) << 6)) as i32
}

pub(super) fn sprite_vstop(ctl: u16) -> i32 {
    (((ctl >> 8) & 0x00FF) | ((ctl & 0x0002) << 7)) as i32
}

pub(super) fn sprite_hstart(pos: u16, ctl: u16) -> i32 {
    (((pos & 0x00FF) << 1) | (ctl & 0x0001)) as i32
}

pub(super) fn sprite_hsub_70ns(ctl: u16) -> bool {
    ctl & 0x0010 != 0
}

pub(super) fn sprite_control_words_from_parts(
    vstart: i32,
    vstop: i32,
    hstart: i32,
    hsub_70ns: bool,
    attached: bool,
) -> (u16, u16) {
    let vstart = vstart as u16;
    let vstop = vstop as u16;
    let hstart = hstart as u16;
    let pos = ((vstart & 0x00FF) << 8) | ((hstart >> 1) & 0x00FF);
    let mut ctl = ((vstop & 0x00FF) << 8)
        | ((vstart & 0x0100) >> 6)
        | ((vstop & 0x0100) >> 7)
        | (hstart & 0x0001);
    if hsub_70ns {
        ctl |= 0x0010;
    }
    if attached {
        ctl |= 0x0080;
    }
    (pos, ctl)
}

#[cfg(test)]
mod performance_tests {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;

    /// Run this same test on both revisions to include each renderer's row
    /// selection and sampler allocations in the measurement. Frame storage
    /// and captured lines are prepared outside the timed region. Compare the
    /// output hashes as well as timings; these are isolated render costs, not
    /// whole-emulator speedups.
    #[test]
    #[ignore = "isolated performance measurement; run release builds with --nocapture"]
    fn bench_attached_sprite_rows() {
        const ROWS: usize = 256;
        const ITERATIONS: usize = 1_000;
        for (name, merged, width_words, odd_only_rows, attached_rows) in [
            ("sorted-16", false, 1, false, ROWS),
            ("merged-16", true, 1, false, ROWS),
            ("odd-only-16", false, 1, true, ROWS),
            ("merged-64", true, 4, false, ROWS),
            ("sparse-one-merged-16", true, 1, false, 1),
            ("sparse-two-merged-16", true, 1, false, 2),
        ] {
            // Sparse calls are much shorter; give them enough repetitions
            // to measure without changing the established dense workloads.
            let iterations = if attached_rows <= 2 {
                100_000
            } else {
                ITERATIONS
            };
            let control = ControlState {
                agnus_revision: if width_words == 4 {
                    AgnusRevision::AgaAlice
                } else {
                    AgnusRevision::Ocs
                },
                bplcon2: 0x24,
                bplcon4: 0x0011,
                clxcon: 1 << 13,
                ..ControlState::default()
            };
            let controls = vec![control; ROWS];
            let control_segments = vec![Vec::new(); ROWS];
            let mut palette = Palette::new();
            for idx in 16..32 {
                palette.write_ocs(idx, (idx as u16 - 15) * 0x111);
            }
            let palettes = vec![palette; ROWS];
            let palette_segments = vec![Vec::new(); ROWS];
            let enable_x = vec![Some(0); ROWS];
            let mut even_lines = Vec::new();
            let mut odd_lines = Vec::new();
            for row in 0..ROWS {
                let line = SpriteLine {
                    hstart: DIW_HSTART_FB0 - crate::bus::SPRITE_OUTPUT_DELAY_LORES,
                    hsub_70ns: false,
                    beam_y: PAL_VISIBLE_LINE0 + row as i32,
                    data: 0xA55A,
                    datb: 0x3333,
                    data_ext: [0xA55A; 3],
                    datb_ext: [0x3333; 3],
                    width_words,
                    attached: row % (ROWS / attached_rows) == 0,
                    x_start: 0,
                    x_stop: FB_WIDTH,
                };
                odd_lines.push(line);
                if !odd_only_rows || row % 2 == 0 {
                    even_lines.push(SpriteLine {
                        data: 0x5AA5,
                        datb: 0x5555,
                        ..line
                    });
                }
            }
            if merged {
                // Register spans append after captured DMA rows. This keeps
                // the same row order/overlap contract as the real merge.
                for lines in [&mut even_lines, &mut odd_lines] {
                    let captured_count = lines.len();
                    for idx in 0..captured_count {
                        lines.push(SpriteLine {
                            hstart: lines[idx].hstart + 24,
                            x_start: 32,
                            x_stop: 160,
                            ..lines[idx]
                        });
                    }
                }
            }
            let beams: Vec<_> = (0..attached_rows)
                .map(|row| PAL_VISIBLE_LINE0 + (row * ROWS / attached_rows) as i32)
                .collect();
            let len = FB_WIDTH * ROWS;
            let mut fb = vec![rgb12_to_rgba8(0x000F); len];
            let playfield_mask = vec![0; len];
            let mut subpixels = SpriteSubpixelState::from_collapsed(&fb, &playfield_mask);
            let mut collisions = vec![CollisionPixel::new(true, true, true, true); len];
            let mut groups = vec![0b0101; len];
            let mut render = || {
                render_attached_sprite_pair_lines(
                    2,
                    black_box(&even_lines),
                    black_box(&odd_lines),
                    black_box(&beams),
                    black_box(&mut fb),
                    SpriteClip {
                        x_start: 0,
                        x_stop: FB_WIDTH,
                        y_start: 0,
                        y_stop: ROWS,
                    },
                    &palettes,
                    &palette_segments,
                    &controls,
                    &control_segments,
                    &enable_x,
                    &playfield_mask,
                    &mut subpixels,
                    &mut collisions,
                    &mut groups,
                    PAL_VISIBLE_LINE0,
                )
            };
            let mut clxdat = 0;
            for _ in 0..10 {
                clxdat |= black_box(render());
            }
            let start = Instant::now();
            for _ in 0..iterations {
                clxdat |= black_box(render());
            }
            let elapsed = start.elapsed();
            assert_ne!(clxdat, 0);
            assert!(fb.iter().any(|&pixel| pixel != rgb12_to_rgba8(0x000F)));
            let mut hash = 0xcbf29ce484222325u64;
            let mut hash_byte = |byte: u8| {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
            };
            for pixel in fb.iter().chain(subpixels.pixels.iter().flatten()) {
                for byte in pixel.to_le_bytes() {
                    hash_byte(byte);
                }
            }
            for &mask in &groups {
                hash_byte(mask);
            }
            for collision in &collisions {
                hash_byte(collision.0);
            }
            for byte in clxdat.to_le_bytes() {
                hash_byte(byte);
            }
            println!(
                "{name}: iterations={iterations} ns_per_render={:.1} output_hash={hash:016x}",
                elapsed.as_nanos() as f64 / iterations as f64,
            );
        }
    }
}
