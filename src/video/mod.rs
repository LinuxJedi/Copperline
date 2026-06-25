// SPDX-License-Identifier: GPL-3.0-or-later

pub mod beam;
pub mod bitplane;
pub mod deinterlace;
pub mod font;
pub mod launcher;
pub mod ui;
pub mod window;

#[cfg(target_os = "macos")]
pub const HOST_SHORTCUT_MODIFIER_LABEL: &str = "Cmd";
#[cfg(not(target_os = "macos"))]
pub const HOST_SHORTCUT_MODIFIER_LABEL: &str = "Alt";

/// Native hi-res Amiga PAL overscan field = 716x285 emulated pixels.
/// DiagROM's main menu copperlist sets BPLCON0 to hi-res (bit 15)
/// with 3 bitplanes, so we size the framebuffer for hi-res by
/// default. Lo-res content is rendered with each pixel doubled
/// horizontally inside `render`.
///
/// The width and horizontal origin match vAmiga's 716-wide regression
/// cutout: 716 hi-res pixels = 179 colour clocks, anchored 8 colour
/// clocks (16 lo-res pixels) further left than the standard display so
/// the deep-left overscan a real Denise can fetch/display is captured
/// rather than clipped. See the origin anchors in `bitplane.rs`
/// (`DIW_HSTART_FB0` / `COPPER_WAIT_HPOS_FB0`).
pub const FB_WIDTH: usize = 716;
pub const FB_HEIGHT: usize = 285;
pub const FB_PIXELS: usize = FB_WIDTH * FB_HEIGHT;

/// Tallest scan the capture/present pipeline supports: a full ECS/AGA
/// programmable 31 kHz frame (VTOTAL+1 = 626 half-length lines).
/// Standard PAL/NTSC frames keep using FB_HEIGHT rows; programmable
/// geometry is clamped here, matching a multisync monitor that simply
/// cannot scan an arbitrarily tall frame.
pub const MAX_VISIBLE_LINES: usize = 626;
pub const MAX_FB_PIXELS: usize = FB_WIDTH * MAX_VISIBLE_LINES;

/// Per-frame display geometry, latched at the frame wrap (like the
/// interlace long-field flag). Standard PAL/NTSC frames report exactly
/// the fixed-canvas values (FB_HEIGHT rows, 227-cck lines) so the
/// classic path stays byte-identical; ECS/AGA VARBEAMEN frames derive
/// their window from HTOTAL/VTOTAL and the programmable vertical
/// blank. The presentation scales whatever scan this describes onto
/// the fixed 4:3 output, like a multisync monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FrameGeometry {
    /// BEAMCON0.VARBEAMEN geometry was active at the frame start.
    pub programmable: bool,
    /// Beam line mapped to framebuffer row 0.
    pub visible_start_vpos: u32,
    /// Rows captured/rendered/presented this frame (<= MAX_VISIBLE_LINES).
    pub visible_lines: usize,
    /// Line length in colour clocks (HTOTAL+1 under VARBEAMEN, else 227).
    pub line_cck: u32,
    /// BPLCON0.LACE at the frame start (field weaving).
    pub lace: bool,
}

impl FrameGeometry {
    pub fn standard(visible_start_vpos: u32, lace: bool) -> Self {
        Self {
            programmable: false,
            visible_start_vpos,
            visible_lines: FB_HEIGHT,
            line_cck: 227,
            lace,
        }
    }
}

/// 4:3 presentation height for screenshots/window scaling. This keeps
/// the internal 716x285 overscan field buffer but presents it with the
/// non-square pixel aspect of a standard Amiga display.
pub const PRESENT_HEIGHT: usize = FB_WIDTH * 3 / 4;

/// Blend two RGBA pixels channel-wise: frac=0 returns a, frac=256
/// returns b. Used by the bilinear vertical resamplers in the window
/// presentation path and the screenshot writer.
#[inline]
pub fn blend_rgba(a: u32, b: u32, frac: u32) -> u32 {
    let inv = 256 - frac;
    let rb = ((a & 0x00FF_00FF) * inv + (b & 0x00FF_00FF) * frac) >> 8;
    let ag = (((a >> 8) & 0x00FF_00FF) * inv + ((b >> 8) & 0x00FF_00FF) * frac) >> 8;
    (rb & 0x00FF_00FF) | ((ag & 0x00FF_00FF) << 8)
}
