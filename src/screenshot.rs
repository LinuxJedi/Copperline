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

/// Save `fb` with centre-aligned bilinear vertical scaling. Used for
/// PAL presentation screenshots, where the internal overscan field
/// buffer should be viewed with non-square pixels; bilinear sampling
/// keeps line thickness even across the picture (nearest-neighbour
/// subsampling dropped source lines unevenly).
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

/// Centre-aligned bilinear vertical resample of `fb` into `scaled`
/// (cleared and resized). Shared by the presentation screenshot writer
/// and the video recorder, which scale the same field buffer per frame.
pub fn scale_y_into(fb: &[u32], width: usize, height: usize, out: usize, scaled: &mut Vec<u32>) {
    debug_assert!(fb.len() >= width * height);
    scaled.clear();
    scaled.resize(width * out, 0);
    for y in 0..out {
        // Source-row centre for this output row in 24.8 fixed point:
        // (y + 0.5) * height / out - 0.5.
        let pos = ((2 * y as i64 + 1) * height as i64 * 128 / out as i64 - 128)
            .clamp(0, ((height - 1) as i64) << 8) as usize;
        let src_y0 = pos >> 8;
        let frac = (pos & 0xFF) as u32;
        let row0 = &fb[src_y0 * width..(src_y0 + 1) * width];
        let dst = &mut scaled[y * width..(y + 1) * width];
        if frac == 0 || src_y0 + 1 >= height {
            dst.copy_from_slice(row0);
        } else {
            let row1 = &fb[(src_y0 + 1) * width..(src_y0 + 2) * width];
            for (d, (&a, &b)) in dst.iter_mut().zip(row0.iter().zip(row1.iter())) {
                *d = crate::video::blend_rgba(a, b, frac);
            }
        }
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
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    PathBuf::from(format!("copperline-screenshot-{n}.png"))
}
