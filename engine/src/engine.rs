//! Engine facade: the single entry point a UI sits in front of.
//!
//! Owns a [`Project`], a sample-storage backend (per
//! `02-architecture.md` §Storage), and a peak cache per source. The engine
//! never holds raw sample data itself; it always goes through the storage
//! trait so the same code paths work for native tests (filesystem) and the
//! browser (in-memory today, OPFS later).

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

use crate::ids::SourceId;
use crate::peaks::{DEFAULT_DECIMATION, MinMax, PeakCache};
use crate::project::Project;
use crate::range::SampleRange;
use crate::source::{Source, StoragePath, Timestamp};
use crate::storage::{MemoryStorage, SampleStorage, StorageError};
use crate::wav::{self, WavError};

#[derive(Debug)]
pub enum ImportError {
    Wav(WavError),
    Storage(StorageError),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wav(e) => write!(f, "{e}"),
            Self::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<WavError> for ImportError {
    fn from(e: WavError) -> Self {
        Self::Wav(e)
    }
}

impl From<StorageError> for ImportError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

#[derive(Debug)]
pub enum QueryError {
    UnknownSource(SourceId),
    Storage(StorageError),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSource(id) => write!(f, "unknown source: {id}"),
            Self::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<StorageError> for QueryError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

pub struct Engine {
    project: Project,
    storage: Box<dyn SampleStorage>,
    peaks: BTreeMap<SourceId, PeakCache>,
}

impl Engine {
    /// Construct an engine with the default in-memory storage. Suitable for
    /// the browser path (samples live in heap memory) and for native tests
    /// that don't need to touch the filesystem.
    pub fn new(sample_rate: u32) -> Self {
        Self::with_storage(sample_rate, Box::new(MemoryStorage::new()))
    }

    pub fn with_storage(sample_rate: u32, storage: Box<dyn SampleStorage>) -> Self {
        Self {
            project: Project::new(sample_rate),
            storage,
            peaks: BTreeMap::new(),
        }
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Decode WAV `bytes`, write samples to storage, register a source, and
    /// build its peak cache. `now` is supplied by the caller so the engine
    /// doesn't depend on a clock — keeps it usable from native tests and
    /// from the browser worker.
    pub fn import_wav(
        &mut self,
        name: &str,
        bytes: &[u8],
        now: Timestamp,
    ) -> Result<SourceId, ImportError> {
        let decoded = wav::decode(bytes)?;
        let id = content_derived_id(bytes);
        let path = format!("sources/{id}/base.f32");
        self.storage.write_all(&path, &decoded.samples)?;

        let source = Source::new(
            id.clone(),
            name,
            decoded.channel_count,
            decoded.sample_rate,
            StoragePath::new(path),
            decoded.frames,
            now,
        );
        let peaks = PeakCache::from_decoded(&decoded, DEFAULT_DECIMATION);

        self.project.sources.insert(id.clone(), source);
        self.peaks.insert(id.clone(), peaks);
        Ok(id)
    }

    pub fn source_frame_count(&self, id: &SourceId) -> Option<u64> {
        self.project.sources.get(id).map(|s| s.base_length)
    }

    /// Read interleaved samples for the given frame range from the source's
    /// current base file. This is the "no edits to apply" version of
    /// [`Self::query_samples`]; once DSP lands, query_samples will replay the
    /// active edit list on top of these base samples.
    pub fn read_base_samples(
        &self,
        id: &SourceId,
        range: SampleRange,
    ) -> Result<Vec<f32>, QueryError> {
        let source = self
            .project
            .sources
            .get(id)
            .ok_or_else(|| QueryError::UnknownSource(id.clone()))?;
        let channels = source.channel_count as u64;
        let sample_range =
            range.start() * channels..range.end() * channels;
        Ok(self.storage.read(source.base_file.as_str(), sample_range)?)
    }

    /// Render `columns` min/max pairs spanning the source's full length.
    /// Renderer-friendly: one pair per pixel column.
    pub fn peak_summary(&self, id: &SourceId, columns: usize) -> Option<Vec<MinMax>> {
        self.peaks.get(id).map(|c| c.summarize(columns))
    }
}

/// Doc 04 §"Identifier conventions": `src_xxxx` where xxxx is the first 4 hex
/// digits of the content hash. We use `DefaultHasher` (FNV-style); collision
/// risk inside a single project is fine for v1, and content-addressing is
/// what makes duplicate-import deduplication possible later.
fn content_derived_id(bytes: &[u8]) -> SourceId {
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    let h = hasher.finish();
    SourceId::new(format!("src_{:04x}", h & 0xffff))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_mono_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut buf = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut writer = hound::WavWriter::new(&mut buf, spec).unwrap();
            for &s in samples {
                writer.write_sample(s).unwrap();
            }
            writer.finalize().unwrap();
        }
        buf.into_inner()
    }

    fn now() -> Timestamp {
        Timestamp("2026-05-03T12:00:00Z".into())
    }

    #[test]
    fn import_wav_writes_samples_through_storage() {
        let bytes = synth_mono_wav(&[0.0, 0.5, -0.5, 1.0, -1.0, 0.0, 0.0, 0.0], 48_000);
        let mut engine = Engine::new(96_000);
        let id = engine.import_wav("test.wav", &bytes, now()).unwrap();
        assert_eq!(engine.source_frame_count(&id), Some(8));
        let samples = engine
            .read_base_samples(&id, SampleRange::new(0, 8).unwrap())
            .unwrap();
        assert_eq!(samples.len(), 8);
        assert!((samples[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn read_base_samples_handles_subrange_for_mono() {
        let bytes = synth_mono_wav(&[10.0, 20.0, 30.0, 40.0, 50.0], 48_000);
        let mut engine = Engine::new(96_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();
        let buf = engine
            .read_base_samples(&id, SampleRange::new(1, 4).unwrap())
            .unwrap();
        assert_eq!(buf, vec![20.0, 30.0, 40.0]);
    }

    #[test]
    fn read_base_samples_returns_interleaved_for_stereo() {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut buf = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
            // 4 frames: (1, -1) (2, -2) (3, -3) (4, -4)
            for i in 1..=4 {
                w.write_sample(i as f32).unwrap();
                w.write_sample(-(i as f32)).unwrap();
            }
            w.finalize().unwrap();
        }
        let bytes = buf.into_inner();

        let mut engine = Engine::new(96_000);
        let id = engine.import_wav("stereo.wav", &bytes, now()).unwrap();
        let samples = engine
            .read_base_samples(&id, SampleRange::new(1, 3).unwrap())
            .unwrap();
        // 2 frames × 2 channels = 4 samples, frames 1 and 2
        assert_eq!(samples, vec![2.0, -2.0, 3.0, -3.0]);
    }

    #[test]
    fn unknown_source_yields_typed_error() {
        let engine = Engine::new(96_000);
        let err = engine
            .read_base_samples(&SourceId::new("src_nope"), SampleRange::new(0, 1).unwrap())
            .unwrap_err();
        assert!(matches!(err, QueryError::UnknownSource(_)));
    }

    #[test]
    fn duplicate_import_returns_same_id() {
        let bytes = synth_mono_wav(&[0.1, 0.2, 0.3, 0.4], 48_000);
        let mut engine = Engine::new(96_000);
        let a = engine.import_wav("a.wav", &bytes, now()).unwrap();
        let b = engine.import_wav("b.wav", &bytes, now()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn peak_summary_returns_requested_column_count() {
        let samples: Vec<f32> = (0..1024).map(|i| i as f32 / 1024.0).collect();
        let bytes = synth_mono_wav(&samples, 48_000);
        let mut engine = Engine::new(96_000);
        let id = engine.import_wav("ramp.wav", &bytes, now()).unwrap();
        let summary = engine.peak_summary(&id, 16).unwrap();
        assert_eq!(summary.len(), 16);
        assert!(summary.last().unwrap().max > 0.9);
    }

    #[test]
    fn engine_with_native_storage_reads_back_what_it_wrote() {
        use crate::storage::NativeStorage;
        let dir = tempfile::tempdir().unwrap();
        let storage = Box::new(NativeStorage::new(dir.path()).unwrap());
        let mut engine = Engine::with_storage(96_000, storage);

        let bytes = synth_mono_wav(&[0.25, -0.25, 0.5, -0.5], 48_000);
        let id = engine.import_wav("n.wav", &bytes, now()).unwrap();
        let read = engine
            .read_base_samples(&id, SampleRange::new(0, 4).unwrap())
            .unwrap();
        assert_eq!(read.len(), 4);
        assert!((read[2] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_wav_bytes() {
        let mut engine = Engine::new(96_000);
        let err = engine.import_wav("bad.wav", b"not a wav", now()).unwrap_err();
        assert!(matches!(err, ImportError::Wav(_)));
    }
}
