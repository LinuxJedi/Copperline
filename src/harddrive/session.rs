// SPDX-License-Identifier: GPL-3.0-or-later

//! Session disks share immutable sectors; rollback records only writes.
//! The base identifier resolves only within this process. These references
//! are for local rollback checkpoints, never a network media format.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::{Deserialize, Serialize};

type Bases = BTreeMap<[u8; 32], Weak<Vec<u8>>>;

fn bases() -> &'static Mutex<Bases> {
    static BASES: OnceLock<Mutex<Bases>> = OnceLock::new();
    BASES.get_or_init(Mutex::default)
}

#[derive(Clone)]
pub(super) struct SessionImage {
    base: Arc<Vec<u8>>,
    id: [u8; 32],
    writes: BTreeMap<u64, Vec<u8>>,
    read_only: bool,
}

#[derive(Serialize, Deserialize)]
struct State {
    id: [u8; 32],
    writes: BTreeMap<u64, Vec<u8>>,
    read_only: bool,
}

impl Serialize for SessionImage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        State {
            id: self.id,
            writes: self.writes.clone(),
            read_only: self.read_only,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SessionImage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let state = State::deserialize(deserializer)?;
        let base = bases()
            .lock()
            .unwrap()
            .get(&state.id)
            .and_then(Weak::upgrade)
            .ok_or_else(|| {
                serde::de::Error::custom("netplay disk is no longer available in this process")
            })?;
        if state.writes.iter().any(|(&sector, bytes)| {
            sector >= (base.len() / super::SECTOR_SIZE) as u64 || bytes.len() != super::SECTOR_SIZE
        }) {
            return Err(serde::de::Error::custom("invalid netplay disk sector"));
        }
        Ok(Self {
            base,
            id: state.id,
            writes: state.writes,
            read_only: state.read_only,
        })
    }
}

impl SessionImage {
    pub(super) fn read_only(&self) -> bool {
        self.read_only
    }
    pub(super) fn set_read_only(&mut self, value: bool) {
        self.read_only = value;
    }
    pub(super) fn new(bytes: Vec<u8>, read_only: bool) -> Self {
        let id = crate::netplay::digest(&bytes);
        let mut bases = bases().lock().unwrap();
        bases.retain(|_, base| base.strong_count() != 0);
        let base = bases
            .get(&id)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(bytes));
        bases.insert(id, Arc::downgrade(&base));
        Self {
            base,
            id,
            writes: BTreeMap::new(),
            read_only,
        }
    }

    pub(super) fn read(&self, sector: u64, out: &mut [u8]) -> std::io::Result<()> {
        let offset = self.offset(sector)?;
        let data = self
            .writes
            .get(&sector)
            .map(Vec::as_slice)
            .unwrap_or(&self.base[offset..offset + super::SECTOR_SIZE]);
        out[..super::SECTOR_SIZE].copy_from_slice(data);
        Ok(())
    }

    pub(super) fn write(&mut self, sector: u64, data: &[u8]) -> std::io::Result<()> {
        let offset = self.offset(sector)?;
        if self.read_only {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session disk is read-only",
            ));
        }
        let data = &data[..super::SECTOR_SIZE];
        if data == &self.base[offset..offset + super::SECTOR_SIZE] {
            self.writes.remove(&sector);
        } else {
            self.writes.insert(sector, data.to_vec());
        }
        Ok(())
    }

    fn offset(&self, sector: u64) -> std::io::Result<usize> {
        if sector >= (self.base.len() / super::SECTOR_SIZE) as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "sector outside session disk",
            ));
        }
        Ok(sector as usize * super::SECTOR_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_restores_writes_without_copying_the_disk() {
        let mut disk = SessionImage::new(vec![0; 8 * 1024 * 1024], false);
        disk.write(3, &[7; 512]).unwrap();
        let checkpoint = bincode::serialize(&disk).unwrap();
        assert!(checkpoint.len() < 1024);
        disk.write(3, &[8; 512]).unwrap();
        let restored: SessionImage = bincode::deserialize(&checkpoint).unwrap();
        assert!(Arc::ptr_eq(&disk.base, &restored.base));
        let mut data = [0; 512];
        restored.read(3, &mut data).unwrap();
        assert_eq!(data, [7; 512]);
        disk.read(3, &mut data).unwrap();
        assert_eq!(data, [8; 512]);
        disk.write(3, &[0; 512]).unwrap();
        assert!(disk.writes.is_empty());
    }

    #[test]
    fn disk_protection_and_bounds_survive_restore() {
        let disk = SessionImage::new(vec![1; 512], true);
        let mut restored: SessionImage =
            bincode::deserialize(&bincode::serialize(&disk).unwrap()).unwrap();
        assert!(restored.write(0, &[0; 512]).is_err());
        assert!(restored.read(u64::MAX, &mut [0; 512]).is_err());
        assert!(restored.write(u64::MAX, &[0; 512]).is_err());
    }
}
