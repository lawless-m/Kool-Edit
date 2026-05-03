//! Storage abstraction for raw sample data.
//!
//! Doc 02 §Storage: the engine sees a typed API; two implementations exist —
//! one for the browser (OPFS) and one for tests (native filesystem). For now
//! only [`MemoryStorage`] (works on every target) and [`NativeStorage`]
//! (native only) are provided; the OPFS adapter lands when the browser path
//! grows beyond keeping samples in memory.
//!
//! Paths are opaque strings shaped to match `StoragePath` from
//! [`crate::source`]. Sample data is a flat sequence of `f32`; range
//! arithmetic and channel layout are the caller's concern.

use std::ops::Range;

mod memory;
#[cfg(not(target_arch = "wasm32"))]
mod native;

pub use memory::MemoryStorage;
#[cfg(not(target_arch = "wasm32"))]
pub use native::NativeStorage;

pub trait SampleStorage {
    fn write_all(&mut self, path: &str, samples: &[f32]) -> Result<(), StorageError>;

    fn append(&mut self, path: &str, samples: &[f32]) -> Result<(), StorageError>;

    fn read(&self, path: &str, range: Range<u64>) -> Result<Vec<f32>, StorageError>;

    fn length(&self, path: &str) -> Result<u64, StorageError>;

    fn exists(&self, path: &str) -> bool;

    fn delete(&mut self, path: &str) -> Result<(), StorageError>;
}

#[derive(Debug)]
pub enum StorageError {
    NotFound(String),
    OutOfRange {
        path: String,
        requested: Range<u64>,
        len: u64,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(p) => write!(f, "no such storage path: {p}"),
            Self::OutOfRange { path, requested, len } => write!(
                f,
                "{path}: requested {}..{} but length is {len}",
                requested.start, requested.end
            ),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Shared test suite for any `SampleStorage` implementation. Each backend
/// runs this against a freshly-constructed instance so the contract stays
/// uniform.
#[cfg(test)]
pub mod tests_shared {
    use super::*;

    pub fn write_then_read_round_trips(storage: &mut dyn SampleStorage) {
        let samples = [0.0_f32, 0.5, -0.5, 1.0, -1.0];
        storage.write_all("a", &samples).unwrap();
        assert_eq!(storage.length("a").unwrap(), 5);
        let read = storage.read("a", 0..5).unwrap();
        assert_eq!(read, samples);
    }

    pub fn append_extends_length(storage: &mut dyn SampleStorage) {
        storage.write_all("b", &[1.0, 2.0]).unwrap();
        storage.append("b", &[3.0, 4.0, 5.0]).unwrap();
        assert_eq!(storage.length("b").unwrap(), 5);
        assert_eq!(storage.read("b", 0..5).unwrap(), vec![1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    pub fn read_returns_subrange(storage: &mut dyn SampleStorage) {
        storage
            .write_all("c", &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0])
            .unwrap();
        assert_eq!(storage.read("c", 2..5).unwrap(), vec![2.0, 3.0, 4.0]);
    }

    pub fn read_past_end_errors(storage: &mut dyn SampleStorage) {
        storage.write_all("d", &[0.0, 1.0]).unwrap();
        let err = storage.read("d", 0..10).unwrap_err();
        assert!(matches!(err, StorageError::OutOfRange { .. }));
    }

    pub fn read_unknown_path_errors(storage: &mut dyn SampleStorage) {
        let err = storage.read("missing", 0..1).unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    pub fn delete_removes(storage: &mut dyn SampleStorage) {
        storage.write_all("e", &[0.0, 1.0]).unwrap();
        assert!(storage.exists("e"));
        storage.delete("e").unwrap();
        assert!(!storage.exists("e"));
    }

    pub fn append_to_missing_path_creates(storage: &mut dyn SampleStorage) {
        // append on a non-existent path is treated as a fresh write so callers
        // don't need to special-case the first append after create.
        storage.append("fresh", &[7.0, 8.0, 9.0]).unwrap();
        assert_eq!(storage.read("fresh", 0..3).unwrap(), vec![7.0, 8.0, 9.0]);
    }
}
