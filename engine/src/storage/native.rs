//! Native filesystem sample storage. Files live under a root directory; on
//! disk each file is a raw little-endian f32 sequence (per doc 02 §Storage).
//!
//! This is the implementation the engine's native tests use. The browser
//! path is served by [`MemoryStorage`](super::MemoryStorage) until the OPFS
//! adapter lands.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use super::{SampleStorage, StorageError};

pub struct NativeStorage {
    root: PathBuf,
}

impl NativeStorage {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn resolve(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    fn ensure_parent(p: &Path) -> std::io::Result<()> {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}

fn samples_to_bytes(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 4);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn bytes_to_samples(bytes: &[u8]) -> Vec<f32> {
    debug_assert!(bytes.len().is_multiple_of(4));
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

impl SampleStorage for NativeStorage {
    fn write_all(&mut self, path: &str, samples: &[f32]) -> Result<(), StorageError> {
        let p = self.resolve(path);
        Self::ensure_parent(&p)?;
        let mut f = File::create(&p)?;
        f.write_all(&samples_to_bytes(samples))?;
        Ok(())
    }

    fn append(&mut self, path: &str, samples: &[f32]) -> Result<(), StorageError> {
        let p = self.resolve(path);
        Self::ensure_parent(&p)?;
        let mut f = OpenOptions::new().create(true).append(true).open(&p)?;
        f.write_all(&samples_to_bytes(samples))?;
        Ok(())
    }

    fn read(&self, path: &str, range: Range<u64>) -> Result<Vec<f32>, StorageError> {
        let p = self.resolve(path);
        if !p.exists() {
            return Err(StorageError::NotFound(path.to_owned()));
        }
        let bytes_len = fs::metadata(&p)?.len();
        let total = bytes_len / 4;
        if range.end > total || range.start > range.end {
            return Err(StorageError::OutOfRange {
                path: path.to_owned(),
                requested: range,
                len: total,
            });
        }
        let mut f = File::open(&p)?;
        f.seek(SeekFrom::Start(range.start * 4))?;
        let want = (range.end - range.start) as usize * 4;
        let mut buf = vec![0u8; want];
        f.read_exact(&mut buf)?;
        Ok(bytes_to_samples(&buf))
    }

    fn length(&self, path: &str) -> Result<u64, StorageError> {
        let p = self.resolve(path);
        if !p.exists() {
            return Err(StorageError::NotFound(path.to_owned()));
        }
        Ok(fs::metadata(&p)?.len() / 4)
    }

    fn exists(&self, path: &str) -> bool {
        self.resolve(path).exists()
    }

    fn delete(&mut self, path: &str) -> Result<(), StorageError> {
        let p = self.resolve(path);
        if !p.exists() {
            return Err(StorageError::NotFound(path.to_owned()));
        }
        fs::remove_file(&p)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests_shared as shared;

    fn fresh() -> (NativeStorage, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let storage = NativeStorage::new(dir.path()).unwrap();
        (storage, dir)
    }

    #[test]
    fn write_then_read() {
        let (mut s, _g) = fresh();
        shared::write_then_read_round_trips(&mut s);
    }
    #[test]
    fn append() {
        let (mut s, _g) = fresh();
        shared::append_extends_length(&mut s);
    }
    #[test]
    fn subrange() {
        let (mut s, _g) = fresh();
        shared::read_returns_subrange(&mut s);
    }
    #[test]
    fn past_end() {
        let (mut s, _g) = fresh();
        shared::read_past_end_errors(&mut s);
    }
    #[test]
    fn unknown() {
        let (mut s, _g) = fresh();
        shared::read_unknown_path_errors(&mut s);
    }
    #[test]
    fn delete() {
        let (mut s, _g) = fresh();
        shared::delete_removes(&mut s);
    }
    #[test]
    fn first_append() {
        let (mut s, _g) = fresh();
        shared::append_to_missing_path_creates(&mut s);
    }
    #[test]
    fn nested_paths_are_created() {
        let (mut s, _g) = fresh();
        s.write_all("sources/abcd/base.f32", &[1.0, 2.0]).unwrap();
        assert_eq!(s.length("sources/abcd/base.f32").unwrap(), 2);
    }
}
