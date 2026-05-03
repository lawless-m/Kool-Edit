//! Engine facade: the single entry point a UI sits in front of.
//!
//! Owns a [`Project`](crate::project::Project), a sample store keyed by
//! `SourceId`, and a peak cache per source. The browser path will eventually
//! offload sample storage to OPFS via the storage trait described in
//! `02-architecture.md`; for the first vertical slice we just keep buffers in
//! memory and let the engine resolve every query directly.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

use crate::ids::SourceId;
use crate::peaks::{DEFAULT_DECIMATION, MinMax, PeakCache};
use crate::project::Project;
use crate::source::{Source, StoragePath, Timestamp};
use crate::wav::{self, WavError};

#[derive(Debug)]
pub enum ImportError {
    Wav(WavError),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Wav(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<WavError> for ImportError {
    fn from(e: WavError) -> Self {
        ImportError::Wav(e)
    }
}

pub struct Engine {
    project: Project,
    samples: BTreeMap<SourceId, Vec<f32>>,
    peaks: BTreeMap<SourceId, PeakCache>,
}

impl Engine {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            project: Project::new(sample_rate),
            samples: BTreeMap::new(),
            peaks: BTreeMap::new(),
        }
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Decode WAV `bytes`, register a new source, and build its peak cache.
    /// `now` is supplied by the caller so the engine doesn't depend on a
    /// clock — keeps it usable from native tests and from the browser.
    pub fn import_wav(
        &mut self,
        name: &str,
        bytes: &[u8],
        now: Timestamp,
    ) -> Result<SourceId, ImportError> {
        let decoded = wav::decode(bytes)?;
        let id = content_derived_id(bytes);

        let source = Source::new(
            id.clone(),
            name,
            decoded.channel_count,
            decoded.sample_rate,
            StoragePath::new(format!("sources/{id}/base.f32")),
            decoded.frames,
            now,
        );

        let peaks = PeakCache::from_decoded(&decoded, DEFAULT_DECIMATION);

        self.project.sources.insert(id.clone(), source);
        self.samples.insert(id.clone(), decoded.samples);
        self.peaks.insert(id.clone(), peaks);
        Ok(id)
    }

    pub fn source_frame_count(&self, id: &SourceId) -> Option<u64> {
        self.project.sources.get(id).map(|s| s.base_length)
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
    fn import_wav_registers_source_and_returns_content_derived_id() {
        let bytes = synth_mono_wav(&[0.0, 0.5, -0.5, 1.0, -1.0, 0.0, 0.0, 0.0], 48_000);
        let mut engine = Engine::new(96_000);
        let id = engine.import_wav("test.wav", &bytes, now()).unwrap();
        assert!(id.as_str().starts_with("src_"));
        assert_eq!(id.as_str().len(), 8);
        assert!(engine.project.sources.contains_key(&id));
        assert_eq!(engine.source_frame_count(&id), Some(8));
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
        // The ramp goes 0..1, so the last column's max should be near 1.0.
        assert!(summary.last().unwrap().max > 0.9);
    }

    #[test]
    fn peak_summary_for_unknown_source_returns_none() {
        let engine = Engine::new(96_000);
        assert!(engine.peak_summary(&SourceId::new("src_nope"), 16).is_none());
    }

    #[test]
    fn rejects_bad_wav_bytes() {
        let mut engine = Engine::new(96_000);
        let err = engine.import_wav("bad.wav", b"not a wav", now()).unwrap_err();
        assert!(matches!(err, ImportError::Wav(_)));
    }
}
