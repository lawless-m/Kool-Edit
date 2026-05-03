//! In-memory sample storage. Works on every target. Used by the browser path
//! today (samples in heap memory) and by native tests that don't want to
//! touch the filesystem.

use std::collections::HashMap;
use std::ops::Range;

use super::{SampleStorage, StorageError};

#[derive(Default)]
pub struct MemoryStorage {
    files: HashMap<String, Vec<f32>>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SampleStorage for MemoryStorage {
    fn write_all(&mut self, path: &str, samples: &[f32]) -> Result<(), StorageError> {
        self.files.insert(path.to_owned(), samples.to_vec());
        Ok(())
    }

    fn append(&mut self, path: &str, samples: &[f32]) -> Result<(), StorageError> {
        self.files
            .entry(path.to_owned())
            .or_default()
            .extend_from_slice(samples);
        Ok(())
    }

    fn read(&self, path: &str, range: Range<u64>) -> Result<Vec<f32>, StorageError> {
        let buf = self
            .files
            .get(path)
            .ok_or_else(|| StorageError::NotFound(path.to_owned()))?;
        let len = buf.len() as u64;
        if range.end > len || range.start > range.end {
            return Err(StorageError::OutOfRange {
                path: path.to_owned(),
                requested: range,
                len,
            });
        }
        Ok(buf[range.start as usize..range.end as usize].to_vec())
    }

    fn length(&self, path: &str) -> Result<u64, StorageError> {
        self.files
            .get(path)
            .map(|v| v.len() as u64)
            .ok_or_else(|| StorageError::NotFound(path.to_owned()))
    }

    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    fn delete(&mut self, path: &str) -> Result<(), StorageError> {
        self.files
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| StorageError::NotFound(path.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests_shared as shared;

    fn fresh() -> MemoryStorage {
        MemoryStorage::new()
    }

    #[test]
    fn write_then_read() {
        shared::write_then_read_round_trips(&mut fresh());
    }
    #[test]
    fn append() {
        shared::append_extends_length(&mut fresh());
    }
    #[test]
    fn subrange() {
        shared::read_returns_subrange(&mut fresh());
    }
    #[test]
    fn past_end() {
        shared::read_past_end_errors(&mut fresh());
    }
    #[test]
    fn unknown() {
        shared::read_unknown_path_errors(&mut fresh());
    }
    #[test]
    fn delete() {
        shared::delete_removes(&mut fresh());
    }
    #[test]
    fn first_append() {
        shared::append_to_missing_path_creates(&mut fresh());
    }
}
