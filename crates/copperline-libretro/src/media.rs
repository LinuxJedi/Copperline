// SPDX-License-Identifier: GPL-3.0-or-later

use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const MAX_DISKS: usize = 16;
pub const MAX_ADF: usize = 1_802_240;

pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "{} exceeds {limit} bytes",
        path.display()
    );
    Ok(bytes)
}

pub fn validate_adf(data: &[u8]) -> Result<()> {
    ensure!(
        matches!(data.len(), 901_120 | 1_802_240),
        "this core supports standard 880 KiB and 1760 KiB ADF images"
    );
    Ok(())
}

pub fn playlist(path: &Path) -> Result<Vec<PathBuf>> {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("adf"))
    {
        return Ok(vec![path.to_path_buf()]);
    }
    ensure!(
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u")),
        "expected an ADF or M3U file"
    );
    let bytes = read_bounded(path, 64 * 1024)?;
    let text = std::str::from_utf8(&bytes)?.trim_start_matches('\u{feff}');
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut disks = Vec::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        ensure!(
            disks.len() < MAX_DISKS,
            "a playlist can contain at most {MAX_DISKS} disks"
        );
        let disk = parent.join(line);
        ensure!(
            disk.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("adf")),
            "playlist entries must be ADF files"
        );
        disks.push(disk);
    }
    ensure!(!disks.is_empty(), "the playlist is empty");
    Ok(disks)
}

pub struct Disk {
    pub label: PathBuf,
    pub bytes: Vec<u8>,
    pub source_hash: [u8; 32],
    save_path: PathBuf,
    saved_hash: [u8; 32],
}

impl Disk {
    pub fn open(path: &Path, save_dir: &Path) -> Result<Self> {
        let original = read_bounded(path, MAX_ADF)?;
        validate_adf(&original)?;
        let source_hash: [u8; 32] = Sha256::digest(&original).into();
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        let name: String = name
            .chars()
            .take(60)
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let suffix: String = source_hash[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let save_path = save_dir
            .join("copperline")
            .join(format!("{name}-{suffix}.adf"));
        let bytes = if save_path.exists() {
            read_bounded(&save_path, MAX_ADF)?
        } else {
            original
        };
        validate_adf(&bytes)?;
        let saved_hash = Sha256::digest(&bytes).into();
        Ok(Self {
            label: path.file_name().unwrap_or_default().into(),
            bytes,
            source_hash,
            save_path,
            saved_hash,
        })
    }

    pub fn persist(&mut self) -> Result<()> {
        let hash: [u8; 32] = Sha256::digest(&self.bytes).into();
        if hash == self.saved_hash {
            return Ok(());
        }
        std::fs::create_dir_all(self.save_path.parent().context("missing save directory")?)?;
        // Write alongside the destination so rename stays on the same volume.
        let temporary = self.save_path.with_extension("adf.tmp");
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&self.bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &self.save_path)
            .with_context(|| format!("saving {}", self.save_path.display()))?;
        self.saved_hash = hash;
        Ok(())
    }
}
