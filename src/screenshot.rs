// SPDX-License-Identifier: GPL-3.0-or-later

//! Save the current framebuffer as a PNG. Useful for debugging the
//! video pipeline from a headless run (--screenshot-after) and for
//! capturing snapshots interactively (host screenshot shortcut in the window).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Encode `fb` (RGBA8 packed in memory order R,G,B,A per pixel, as
/// produced by `video::bitplane::render`) into a PNG at `path`.
pub fn save(path: &Path, fb: &[u32], width: u32, height: u32) -> Result<()> {
    let expected = (width as usize) * (height as usize);
    if fb.len() != expected {
        anyhow::bail!(
            "framebuffer size mismatch: got {} pixels, expected {}x{}={}",
            fb.len(),
            width,
            height,
            expected
        );
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    let file =
        std::fs::File::create(path).with_context(|| format!("opening {}", path.display()))?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut wr = encoder
        .write_header()
        .with_context(|| format!("writing PNG header to {}", path.display()))?;
    let bytes = unsafe { std::slice::from_raw_parts(fb.as_ptr() as *const u8, fb.len() * 4) };
    wr.write_image_data(bytes)
        .with_context(|| format!("writing PNG data to {}", path.display()))?;
    Ok(())
}

/// Save `fb` with centre-aligned vertical scaling. Used for
/// PAL presentation screenshots, where the internal overscan field
/// buffer should be viewed with non-square pixels. The normal
/// presentation path preserves source-row colours instead of blending
/// adjacent scanlines; filtered output belongs behind an explicit
/// display filter.
pub fn save_scaled_y(
    path: &Path,
    fb: &[u32],
    width: u32,
    height: u32,
    out_height: u32,
) -> Result<()> {
    let expected = (width as usize) * (height as usize);
    // The source may be a maximum-sized presentation buffer with only the
    // leading `height` rows active.
    if fb.len() < expected {
        anyhow::bail!(
            "framebuffer size mismatch: got {} pixels, expected at least {}x{}={}",
            fb.len(),
            width,
            height,
            expected
        );
    }
    let fb = &fb[..expected];
    if out_height == height {
        return save(path, fb, width, height);
    }

    let mut scaled = Vec::new();
    scale_y_into(
        fb,
        width as usize,
        height as usize,
        out_height as usize,
        &mut scaled,
    );
    save(path, &scaled, width, out_height)
}

/// Save a rectangular viewport from `fb`, clamping source coordinates at the
/// source edges. The clamp lets a presentation aperture extend slightly beyond
/// the emulated capture buffer while preserving the captured border colour at
/// the edge.
pub fn save_cropped_clamped(
    path: &Path,
    fb: &[u32],
    src_width: usize,
    src_height: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<()> {
    let cropped = crop_clamped(fb, src_width, src_height, x, y, width, height)?;
    save(path, &cropped, width as u32, height as u32)
}

fn crop_clamped(
    fb: &[u32],
    src_width: usize,
    src_height: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<Vec<u32>> {
    let expected = src_width * src_height;
    if fb.len() < expected {
        anyhow::bail!(
            "framebuffer size mismatch: got {} pixels, expected at least {}x{}={}",
            fb.len(),
            src_width,
            src_height,
            expected
        );
    }
    if src_width == 0 || src_height == 0 || width == 0 || height == 0 {
        anyhow::bail!("invalid crop dimensions");
    }

    let mut cropped = vec![0; width * height];
    for dst_y in 0..height {
        let src_y = (y + dst_y).min(src_height - 1);
        let dst = &mut cropped[dst_y * width..(dst_y + 1) * width];
        for (dst_x, pixel) in dst.iter_mut().enumerate() {
            let src_x = (x + dst_x).min(src_width - 1);
            *pixel = fb[src_y * src_width + src_x];
        }
    }
    Ok(cropped)
}

/// Centre-aligned source row for presentation row `y`.
#[inline]
pub fn scaled_source_row(y: usize, src_rows: usize, dst_rows: usize) -> usize {
    (((2 * y + 1) * src_rows) / (2 * dst_rows)).min(src_rows.saturating_sub(1))
}

/// Centre-aligned vertical resample of `fb` into `scaled` (cleared and
/// resized). Shared by the presentation screenshot writer and the video
/// recorder, which scale the same field buffer per frame.
pub fn scale_y_into(fb: &[u32], width: usize, height: usize, out: usize, scaled: &mut Vec<u32>) {
    debug_assert!(fb.len() >= width * height);
    scaled.clear();
    scaled.resize(width * out, 0);
    for y in 0..out {
        let src_y = scaled_source_row(y, height, out);
        let row = &fb[src_y * width..(src_y + 1) * width];
        let dst = &mut scaled[y * width..(y + 1) * width];
        dst.copy_from_slice(row);
    }
}

/// Centre-aligned bilinear horizontal resample, in place, of the leading
/// `rows` rows of the `width`-pixel-wide `fb`: output pixel x samples
/// source position x * src_num / src_den. The presentation uses this to
/// map a programmable scan's line onto the fixed glass width - a
/// multisync monitor's horizontal deflection is time-linear, so a colour
/// clock of a short (e.g. 31 kHz, ~130-cck) line covers proportionally
/// more of the screen than one of a 227-cck standard line
/// (src_num = line_cck, src_den = 227). Source pixels pushed past the
/// right edge by a factor > 1 are cut; a factor < 1 leaves black on the
/// right.
pub fn stretch_rows_x(fb: &mut [u32], width: usize, rows: usize, src_num: u32, src_den: u32) {
    debug_assert!(fb.len() >= width * rows);
    if src_num == src_den || width == 0 {
        return;
    }
    let mut scratch = vec![0u32; width];
    for y in 0..rows {
        let row = &mut fb[y * width..(y + 1) * width];
        scratch.copy_from_slice(row);
        for (x, out) in row.iter_mut().enumerate() {
            // Source-pixel centre in 24.8 fixed point:
            // (x + 0.5) * src_num / src_den - 0.5.
            let pos = ((2 * x as i64 + 1) * src_num as i64 * 128 / src_den as i64 - 128)
                .clamp(0, ((width - 1) as i64) << 8) as usize;
            let src_x0 = pos >> 8;
            let frac = (pos & 0xFF) as u32;
            *out = if frac == 0 || src_x0 + 1 >= width {
                scratch[src_x0]
            } else {
                crate::video::blend_rgba(scratch[src_x0], scratch[src_x0 + 1], frac)
            };
        }
    }
}

/// Pick a default filename for an interactive screenshot grab.
pub fn auto_filename() -> PathBuf {
    let ts = crate::timestamp::compact_now();
    PathBuf::from(format!("copperline-screenshot-{ts}.png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_presentation_scale_preserves_source_pixel_values() {
        let a = 0x1122_3344;
        let b = 0x5566_7788;
        let fb = [a, b, a, b, a, b];
        let mut scaled = Vec::new();

        scale_y_into(&fb, 1, fb.len(), 5, &mut scaled);

        assert_eq!(scaled.len(), 5);
        assert!(scaled.iter().all(|px| *px == a || *px == b));
    }

    #[test]
    fn cropped_clamped_view_extends_edge_pixels() -> Result<()> {
        let fb = vec![1, 2, 3, 4, 5, 6];

        let cropped = crop_clamped(&fb, 3, 2, 1, 0, 4, 2)?;

        assert_eq!(cropped, vec![2, 3, 3, 3, 5, 6, 6, 6]);
        Ok(())
    }
}
