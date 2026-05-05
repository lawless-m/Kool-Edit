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

use crate::dsp::{self, DspError};
use crate::ids::{ProfileId, SourceId};
use crate::kepz::{self, KepzError, KepzSource};
use crate::nr::{self, NrSettings};
use crate::op::Op;
use crate::peaks::{DEFAULT_DECIMATION, MinMax, PeakCache};
use crate::project::{NoiseProfile, Project};
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
    UnknownProfile(ProfileId),
    Storage(StorageError),
    Dsp(DspError),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSource(id) => write!(f, "unknown source: {id}"),
            Self::UnknownProfile(id) => write!(f, "unknown noise profile: {id}"),
            Self::Storage(e) => write!(f, "{e}"),
            Self::Dsp(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<StorageError> for QueryError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

impl From<DspError> for QueryError {
    fn from(e: DspError) -> Self {
        Self::Dsp(e)
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

    /// Mutable project access. Used by callers that build the project
    /// structure directly (e.g. tests, the multitrack UI). Destructive
    /// edits on a source still go through [`Self::apply_op`] so the edit
    /// list and timestamps stay consistent.
    pub fn project_mut(&mut self) -> &mut Project {
        &mut self.project
    }

    /// Replace the in-memory project, e.g. after loading from JSON. The
    /// caller is responsible for ensuring the storage backend already
    /// contains every base file the new project references.
    pub fn replace_project(&mut self, project: Project) {
        self.project = project;
        self.peaks.clear();
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

        // Resample to the project's rate at import time so mixdown and any
        // other downstream consumer can treat every source uniformly. Linear
        // interpolation is good enough for v1; a windowed-sinc upgrade slots
        // into this same call site later.
        let project_rate = self.project.sample_rate();
        let (samples, frames, stored_rate) = if decoded.sample_rate == project_rate {
            (decoded.samples, decoded.frames, decoded.sample_rate)
        } else {
            let (resampled, new_frames) = resample_linear(
                &decoded.samples,
                decoded.channel_count,
                decoded.sample_rate,
                project_rate,
            );
            (resampled, new_frames, project_rate)
        };

        self.storage.write_all(&path, &samples)?;

        let source = Source::new(
            id.clone(),
            name,
            decoded.channel_count,
            stored_rate,
            StoragePath::new(path),
            frames,
            now,
        );
        let peaks = PeakCache::from_samples(&samples, decoded.channel_count, DEFAULT_DECIMATION);

        self.project.sources.insert(id.clone(), source);
        self.peaks.insert(id.clone(), peaks);
        Ok(id)
    }

    pub fn source_frame_count(&self, id: &SourceId) -> Option<u64> {
        self.project.sources.get(id).map(|s| s.base_length)
    }

    pub fn source_sample_rate(&self, id: &SourceId) -> Option<u32> {
        self.project.sources.get(id).map(|s| s.sample_rate)
    }

    pub fn source_channel_count(&self, id: &SourceId) -> Option<u16> {
        self.project.sources.get(id).map(|s| s.channel_count)
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

    /// Range-aware peak summary: bin the cache covering `[start_frame, end_frame)`
    /// into `columns` columns. Used by the zoomed waveform renderer.
    /// When the requested resolution is finer than the cache's decimation
    /// (i.e. fewer than `samples_per_pair` frames per column), reads raw
    /// samples for the range and bins them directly so high-zoom views show
    /// real detail rather than stretched cache pairs.
    pub fn peak_summary_range(
        &self,
        id: &SourceId,
        start_frame: u64,
        end_frame: u64,
        columns: usize,
    ) -> Option<Vec<MinMax>> {
        let cache = self.peaks.get(id)?;
        if columns == 0 || end_frame <= start_frame {
            return Some(vec![MinMax::default(); columns]);
        }
        let range_frames = end_frame - start_frame;
        let frames_per_column = range_frames / columns as u64;
        if frames_per_column >= cache.samples_per_pair as u64 {
            return Some(cache.summarize_range(start_frame, end_frame, columns));
        }
        // Raw-sample path: pull interleaved samples for the range and bin
        // them. Errors degrade gracefully to a zero summary so the renderer
        // doesn't blank out.
        let source = self.project.sources.get(id)?;
        // Use query_samples so the render reflects the active edit list. For
        // sources with no edits this is the same as read_base_samples; for
        // edited sources it includes silence/gain/etc.
        let effective_len = self.effective_frame_count(id).ok()?;
        let clamped_end = end_frame.min(effective_len);
        if clamped_end <= start_frame {
            return Some(vec![MinMax::default(); columns]);
        }
        let range = SampleRange::new(start_frame, clamped_end).ok()?;
        let samples = self.query_samples(id, range).ok()?;
        Some(crate::peaks::bin_raw_samples(
            &samples,
            source.channel_count,
            columns,
        ))
    }

    /// Append an op to the source's edit list. Truncates any redo branch.
    /// Regenerates the peak cache so renderers (which read peaks) reflect
    /// the post-op state.
    pub fn apply_op(&mut self, id: &SourceId, op: Op, now: Timestamp) -> Result<(), QueryError> {
        {
            let source = self
                .project
                .sources
                .get_mut(id)
                .ok_or_else(|| QueryError::UnknownSource(id.clone()))?;
            source.apply(op, now);
        }
        self.regenerate_peaks(id)
    }

    pub fn undo(&mut self, id: &SourceId) -> Result<bool, QueryError> {
        let did = {
            let source = self
                .project
                .sources
                .get_mut(id)
                .ok_or_else(|| QueryError::UnknownSource(id.clone()))?;
            source.edits.undo().is_some()
        };
        if did {
            self.regenerate_peaks(id)?;
        }
        Ok(did)
    }

    pub fn redo(&mut self, id: &SourceId) -> Result<bool, QueryError> {
        let did = {
            let source = self
                .project
                .sources
                .get_mut(id)
                .ok_or_else(|| QueryError::UnknownSource(id.clone()))?;
            source.edits.redo().is_some()
        };
        if did {
            self.regenerate_peaks(id)?;
        }
        Ok(did)
    }

    /// Render the source's current state and rebuild its peak cache from the
    /// result. Called after any op or undo/redo so the waveform display
    /// reflects the active edit list. Cheap on short sources; on long ones
    /// the cost is one render_full per op.
    fn regenerate_peaks(&mut self, id: &SourceId) -> Result<(), QueryError> {
        let rendered = self.render_full(id)?;
        let channels = self
            .project
            .sources
            .get(id)
            .expect("verified by render_full")
            .channel_count;
        let cache = PeakCache::from_samples(&rendered, channels, DEFAULT_DECIMATION);
        self.peaks.insert(id.clone(), cache);
        Ok(())
    }

    /// Render the source's full effective buffer (base + all active ops),
    /// then return the requested frame range. The render-then-slice approach
    /// stays correct under length-changing ops (Cut, Generate) because the
    /// final buffer's length already reflects them; it's just expensive on
    /// long sources. Optimised range-aware replay can land later.
    pub fn query_samples(
        &self,
        id: &SourceId,
        range: SampleRange,
    ) -> Result<Vec<f32>, QueryError> {
        let buf = self.render_full(id)?;
        let source = self
            .project
            .sources
            .get(id)
            .expect("verified by render_full");
        let ch = source.channel_count as u64;
        let start = ((range.start() * ch) as usize).min(buf.len());
        let end = ((range.end() * ch) as usize).min(buf.len());
        Ok(buf[start..end].to_vec())
    }

    /// Effective length of a source's current rendered buffer in frames.
    /// Equals base_length when no length-changing ops are active.
    pub fn effective_frame_count(&self, id: &SourceId) -> Result<u64, QueryError> {
        let buf = self.render_full(id)?;
        let source = self.project.sources.get(id).expect("verified by render_full");
        Ok(buf.len() as u64 / source.channel_count as u64)
    }

    fn render_full(&self, id: &SourceId) -> Result<Vec<f32>, QueryError> {
        let source = self
            .project
            .sources
            .get(id)
            .ok_or_else(|| QueryError::UnknownSource(id.clone()))?;
        let full = SampleRange::new(0, source.base_length).expect("base_length valid range");
        let mut buf = self.read_base_samples(id, full)?;
        for op in source.edits.active() {
            // NoiseReduce needs the profile data, which lives outside the
            // Op record; route it through a dedicated path that pulls the
            // profile from the project. Everything else goes through the
            // standard dsp::apply.
            if let Op::NoiseReduce {
                range,
                profile,
                params,
            } = op
            {
                let prof = self
                    .project
                    .noise_profiles
                    .get(profile)
                    .ok_or_else(|| QueryError::UnknownProfile(profile.clone()))?;
                apply_noise_reduce(
                    &mut buf,
                    source.channel_count,
                    source.sample_rate,
                    *range,
                    prof,
                    params,
                )?;
            } else {
                dsp::apply(op, &mut buf, source.channel_count, source.sample_rate)?;
            }
        }
        Ok(buf)
    }

    /// Capture a noise profile from a region of a source (post any active
    /// edits) and store it under `profile_id`. The captured spectrum is the
    /// average magnitude across STFT frames at `fft_size`/`fft_size/4` hop.
    pub fn capture_noise_profile(
        &mut self,
        source_id: &SourceId,
        range: SampleRange,
        name: impl Into<String>,
        profile_id: ProfileId,
        fft_size: u32,
    ) -> Result<(), QueryError> {
        let source_channels = self
            .project
            .sources
            .get(source_id)
            .ok_or_else(|| QueryError::UnknownSource(source_id.clone()))?
            .channel_count as usize;
        let interleaved = self.query_samples(source_id, range)?;
        let mono = mono_sum(&interleaved, source_channels);
        let hop = (fft_size as usize / 4).max(1);
        let magnitudes = nr::estimate_profile(&mono, fft_size as usize, hop);
        let profile = NoiseProfile {
            id: profile_id.clone(),
            name: name.into(),
            captured_from: source_id.clone(),
            range,
            magnitudes,
        };
        self.project.noise_profiles.insert(profile_id, profile);
        Ok(())
    }

    /// Doc 03 §Flattening: render the current state to a new base file and
    /// clear the edit list. Updates `base_length` if length-changing ops
    /// (Cut, Generate) made the buffer larger or smaller. Regenerates the
    /// peak cache from the freshly rendered samples.
    pub fn flatten(&mut self, id: &SourceId, now: Timestamp) -> Result<(), QueryError> {
        let rendered = self.render_full(id)?;
        let (path, channel_count) = {
            let source = self
                .project
                .sources
                .get(id)
                .ok_or_else(|| QueryError::UnknownSource(id.clone()))?;
            (
                source.base_file.as_str().to_owned(),
                source.channel_count,
            )
        };
        self.storage.write_all(&path, &rendered)?;

        let new_peaks = PeakCache::from_samples(&rendered, channel_count, DEFAULT_DECIMATION);
        self.peaks.insert(id.clone(), new_peaks);

        let new_length = rendered.len() as u64 / channel_count as u64;
        let source = self.project.sources.get_mut(id).expect("checked above");
        source.base_length = new_length;
        source.edits.truncate_history();
        source.modified_at = now;
        Ok(())
    }

    /// Render the project as interleaved stereo at the project's sample rate.
    /// See [`crate::mixdown`] for what's currently supported.
    pub fn mixdown_stereo(&self) -> Result<Vec<f32>, crate::mixdown::MixdownError> {
        crate::mixdown::mixdown_stereo(self)
    }

    /// Convenience: render the project and encode the result as a 32-bit
    /// float WAV. The output is suitable for download or for writing to disk.
    pub fn mixdown_wav(&self) -> Result<Vec<u8>, crate::mixdown::MixdownError> {
        let stereo = self.mixdown_stereo()?;
        Ok(crate::wav::encode_f32(&stereo, 2, self.project.sample_rate()))
    }

    /// Build a `.kepz` portable archive: project JSON plus every source's
    /// base file, zipped. The result is shareable across machines without
    /// any external file dependencies.
    pub fn export_kepz(&self) -> Result<Vec<u8>, KepzError> {
        let mut sources = Vec::with_capacity(self.project.sources.len());
        for source in self.project.sources.values() {
            let path = source.base_file.as_str().to_owned();
            let len = self
                .storage
                .length(&path)
                .map_err(|e| KepzError::Io(std::io::Error::other(e.to_string())))?;
            let samples = self
                .storage
                .read(&path, 0..len)
                .map_err(|e| KepzError::Io(std::io::Error::other(e.to_string())))?;
            sources.push(KepzSource { path, samples });
        }
        kepz::write_archive(&self.project, sources)
    }

    /// Read a `.kepz` archive and load it into a fresh engine. The returned
    /// engine owns a [`MemoryStorage`] populated with every source's base
    /// file. Pass an explicit storage backend with [`Self::import_kepz_into`]
    /// if you want the samples on disk.
    pub fn import_kepz(bytes: &[u8]) -> Result<Self, KepzError> {
        Self::import_kepz_into(bytes, Box::new(MemoryStorage::new()))
    }

    pub fn import_kepz_into(
        bytes: &[u8],
        mut storage: Box<dyn SampleStorage>,
    ) -> Result<Self, KepzError> {
        let archive = kepz::read_archive(bytes)?;
        for source in &archive.sources {
            storage
                .write_all(&source.path, &source.samples)
                .map_err(|e| KepzError::Io(std::io::Error::other(e.to_string())))?;
        }
        let mut engine = Self {
            project: archive.project,
            storage,
            peaks: std::collections::BTreeMap::new(),
        };
        // Peak caches aren't part of the archive; rebuild them now so the
        // arranger's first peakSummary call after load returns proper data
        // (otherwise clips render as flat rectangles until something else
        // dirties the source).
        let ids: Vec<SourceId> = engine.project.sources.keys().cloned().collect();
        for id in ids {
            // Best-effort: a per-source failure shouldn't block the load.
            let _ = engine.regenerate_peaks(&id);
        }
        Ok(engine)
    }
}

/// Run noise reduction on a single channel-interleaved range. Builds a mono
/// view, processes through the spectral subtractor, and overlays the result
/// onto every channel of the original buffer.
fn apply_noise_reduce(
    buffer: &mut [f32],
    channels: u16,
    sample_rate: u32,
    range: SampleRange,
    profile: &NoiseProfile,
    params: &crate::effect::NrParams,
) -> Result<(), QueryError> {
    let _ = sample_rate; // smoothing parameters use sample_rate; unused for now
    let ch = channels as u64;
    let total_frames = buffer.len() as u64 / ch;
    if range.end() > total_frames {
        return Err(QueryError::Dsp(DspError::RangeOutsideBuffer {
            range,
            frames: total_frames,
        }));
    }
    let frame_lo = (range.start() * ch) as usize;
    let frame_hi = (range.end() * ch) as usize;
    let region = &mut buffer[frame_lo..frame_hi];
    let ch_us = channels as usize;
    let frames = region.len() / ch_us;
    if frames == 0 {
        return Ok(());
    }
    // Mono-sum the region, run NR, then write the processed signal back to
    // every channel. A per-channel pass would keep stereo width but doubles
    // CPU; this matches the "single profile applied to the whole region"
    // model the design doc describes.
    let mono = mono_sum(region, ch_us);
    let hop = (params.fft_size as usize / 4).max(1);
    let processed = nr::apply(
        &mono,
        &profile.magnitudes,
        params.fft_size as usize,
        hop,
        NrSettings {
            amount_db: params.amount_db,
            floor_db: params.floor_db,
            oversubtraction: params.oversubtraction,
        },
    );
    for f in 0..frames {
        for c in 0..ch_us {
            region[f * ch_us + c] = processed[f];
        }
    }
    Ok(())
}

fn mono_sum(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels == 1 {
        return interleaved.to_vec();
    }
    let frames = interleaved.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut s = 0.0_f32;
        for c in 0..channels {
            s += interleaved[f * channels + c];
        }
        out.push(s / channels as f32);
    }
    out
}

/// Linear-interpolation resampler. Returns `(interleaved_samples, frame_count)`
/// at `dst_rate`. Pass-through when src_rate == dst_rate; empty input yields
/// empty output. Channel order is preserved.
fn resample_linear(
    interleaved: &[f32],
    channels: u16,
    src_rate: u32,
    dst_rate: u32,
) -> (Vec<f32>, u64) {
    if src_rate == dst_rate || interleaved.is_empty() {
        let frames = interleaved.len() as u64 / channels.max(1) as u64;
        return (interleaved.to_vec(), frames);
    }
    let ch = channels as usize;
    let src_frames = interleaved.len() / ch;
    if src_frames == 0 {
        return (Vec::new(), 0);
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let dst_frames = ((src_frames as f64) / ratio).floor() as usize;
    let mut out = vec![0.0_f32; dst_frames * ch];
    let last = src_frames - 1;
    for i in 0..dst_frames {
        let src_pos = i as f64 * ratio;
        let i0 = src_pos.floor() as usize;
        let frac = (src_pos - i0 as f64) as f32;
        let i1 = (i0 + 1).min(last);
        for c in 0..ch {
            let a = interleaved[i0 * ch + c];
            let b = interleaved[i1 * ch + c];
            out[i * ch + c] = a + (b - a) * frac;
        }
    }
    (out, dst_frames as u64)
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
        let mut engine = Engine::new(48_000);
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
        let mut engine = Engine::new(48_000);
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

        let mut engine = Engine::new(48_000);
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
        let mut engine = Engine::with_storage(48_000, storage);

        let bytes = synth_mono_wav(&[0.25, -0.25, 0.5, -0.5], 48_000);
        let id = engine.import_wav("n.wav", &bytes, now()).unwrap();
        let read = engine
            .read_base_samples(&id, SampleRange::new(0, 4).unwrap())
            .unwrap();
        assert_eq!(read.len(), 4);
        assert!((read[2] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn query_samples_with_no_ops_matches_base() {
        let bytes = synth_mono_wav(&[0.1, 0.2, 0.3, 0.4, 0.5], 48_000);
        let mut engine = Engine::new(48_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();
        let q = engine
            .query_samples(&id, SampleRange::new(1, 4).unwrap())
            .unwrap();
        assert_eq!(q.len(), 3);
        assert!((q[0] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn query_samples_replays_silence_op() {
        let bytes = synth_mono_wav(&[0.5, 0.5, 0.5, 0.5, 0.5], 48_000);
        let mut engine = Engine::new(96_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();
        engine
            .apply_op(
                &id,
                Op::Silence {
                    range: SampleRange::new(1, 4).unwrap(),
                },
                now(),
            )
            .unwrap();
        let q = engine
            .query_samples(&id, SampleRange::new(0, 5).unwrap())
            .unwrap();
        assert_eq!(q, vec![0.5, 0.0, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn query_samples_chains_ops_in_order() {
        // gain -6dB then silence the middle: middle should still be zero, edges
        // should be halved (within tolerance).
        let bytes = synth_mono_wav(&[1.0, 1.0, 1.0, 1.0, 1.0], 48_000);
        let mut engine = Engine::new(96_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();
        engine
            .apply_op(
                &id,
                Op::Gain {
                    range: SampleRange::new(0, 5).unwrap(),
                    db: -6.0206,
                },
                now(),
            )
            .unwrap();
        engine
            .apply_op(
                &id,
                Op::Silence {
                    range: SampleRange::new(1, 4).unwrap(),
                },
                now(),
            )
            .unwrap();
        let q = engine
            .query_samples(&id, SampleRange::new(0, 5).unwrap())
            .unwrap();
        assert!((q[0] - 0.5).abs() < 1e-3);
        assert_eq!(q[1], 0.0);
        assert_eq!(q[3], 0.0);
        assert!((q[4] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn undo_then_query_returns_pre_op_samples() {
        let bytes = synth_mono_wav(&[0.5; 4], 48_000);
        let mut engine = Engine::new(96_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();
        engine
            .apply_op(
                &id,
                Op::Silence {
                    range: SampleRange::new(0, 4).unwrap(),
                },
                now(),
            )
            .unwrap();
        assert_eq!(
            engine
                .query_samples(&id, SampleRange::new(0, 4).unwrap())
                .unwrap(),
            vec![0.0; 4]
        );
        assert!(engine.undo(&id).unwrap());
        let q = engine
            .query_samples(&id, SampleRange::new(0, 4).unwrap())
            .unwrap();
        for s in &q {
            assert!((s - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn flatten_clears_edit_list_and_writes_new_base() {
        let bytes = synth_mono_wav(&[1.0, 1.0, 1.0, 1.0], 48_000);
        let mut engine = Engine::new(96_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();
        engine
            .apply_op(
                &id,
                Op::Silence {
                    range: SampleRange::new(0, 2).unwrap(),
                },
                now(),
            )
            .unwrap();
        engine.flatten(&id, now()).unwrap();

        let source = engine.project().sources.get(&id).unwrap();
        assert_eq!(source.edits.len(), 0);
        assert!(!source.edits.can_undo());

        // Reading the base directly should now reflect the silenced range.
        let base = engine
            .read_base_samples(&id, SampleRange::new(0, 4).unwrap())
            .unwrap();
        assert_eq!(base, vec![0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn flatten_regenerates_peak_cache() {
        let bytes = synth_mono_wav(&[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0], 48_000);
        let mut engine = Engine::new(48_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();
        engine
            .apply_op(
                &id,
                Op::Silence {
                    range: SampleRange::new(0, 8).unwrap(),
                },
                now(),
            )
            .unwrap();
        engine.flatten(&id, now()).unwrap();

        let summary = engine.peak_summary(&id, 4).unwrap();
        for p in summary {
            assert_eq!(p.min, 0.0);
            assert_eq!(p.max, 0.0);
        }
    }

    #[test]
    fn noise_reduction_lowers_floor_on_a_real_signal() {
        use crate::effect::NrParams;
        use crate::ids::ProfileId;

        let sample_rate = 48_000;
        // Build a source: 0.25 s of pure noise, followed by 0.5 s of
        // sine + the same noise. The first half is the profile capture
        // region; the second half is what NR processes.
        let noise_samples: Vec<f32> = (0..sample_rate / 4)
            .map(|n| {
                let mut state = (n as u32).wrapping_mul(0x9e37_79b1) | 1;
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                ((state as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.1
            })
            .collect();
        let signal_with_noise: Vec<f32> = (0..sample_rate / 2)
            .map(|n: usize| {
                let sine =
                    0.5 * (n as f32 / 48.0 * std::f32::consts::TAU).sin();
                let mut state = (n as u32).wrapping_add(99_999).wrapping_mul(0x9e37_79b1) | 1;
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let noise = ((state as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.1;
                sine + noise
            })
            .collect();
        let mut full = noise_samples;
        full.extend(signal_with_noise);
        let bytes = wav::encode_f32(&full, 1, sample_rate as u32);

        let mut engine = Engine::new(sample_rate as u32);
        let id = engine.import_wav("noisy.wav", &bytes, now()).unwrap();

        // Capture profile from the noise-only region.
        engine
            .capture_noise_profile(
                &id,
                SampleRange::new(0, sample_rate as u64 / 4).unwrap(),
                "AC",
                ProfileId::new("np_001"),
                512,
            )
            .unwrap();

        // Measure RMS of the (silent) profile region BEFORE NR — this is
        // pure noise and gives us our baseline noise level.
        let pre_noise_rms = {
            let pre = engine
                .query_samples(&id, SampleRange::new(0, sample_rate as u64 / 4).unwrap())
                .unwrap();
            rms(&pre)
        };

        // Apply NR across the entire source.
        engine
            .apply_op(
                &id,
                Op::NoiseReduce {
                    range: SampleRange::new(0, full_len(&engine, &id)).unwrap(),
                    profile: ProfileId::new("np_001"),
                    params: NrParams {
                        amount_db: 24.0,
                        floor_db: -30.0,
                        oversubtraction: 1.5,
                        attack_ms: 5.0,
                        release_ms: 50.0,
                        freq_smoothing: 0.0,
                        fft_size: 512,
                    },
                },
                now(),
            )
            .unwrap();

        // The noise-only region should now be much quieter.
        let post = engine
            .query_samples(&id, SampleRange::new(0, sample_rate as u64 / 4).unwrap())
            .unwrap();
        let post_rms = rms(&post);
        let reduction_db = 20.0 * (post_rms / pre_noise_rms).log10();
        assert!(
            reduction_db < -3.0,
            "expected real noise reduction, got {reduction_db} dB"
        );

        // The signal-plus-noise region should retain a strong tonal level —
        // peak energy stays in the same ballpark as the dry signal.
        let signal_region = engine
            .query_samples(
                &id,
                SampleRange::new(sample_rate as u64 / 4, sample_rate as u64 * 3 / 4)
                    .unwrap(),
            )
            .unwrap();
        let signal_rms = rms(&signal_region);
        // Loose lower bound — 0.5 amplitude sine has RMS ~0.354.
        assert!(signal_rms > 0.2, "tonal region too quiet after NR: {signal_rms}");
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let s: f64 = samples.iter().map(|&x| (x as f64).powi(2)).sum();
        (s / samples.len() as f64).sqrt() as f32
    }

    fn full_len(engine: &Engine, id: &SourceId) -> u64 {
        engine.effective_frame_count(id).unwrap()
    }

    #[test]
    fn kepz_round_trip_preserves_sources_and_destructive_edits() {
        // Source A: 0.5 amplitude tone; apply a Silence op over the middle
        // half. The reloaded engine should show the same edit list and the
        // same query_samples output.
        let mut engine = Engine::new(48_000);
        let src_bytes = synth_mono_wav(&[0.5_f32; 100], 48_000);
        let id = engine.import_wav("a.wav", &src_bytes, now()).unwrap();
        engine
            .apply_op(
                &id,
                Op::Silence {
                    range: SampleRange::new(25, 75).unwrap(),
                },
                now(),
            )
            .unwrap();
        let pre_query = engine
            .query_samples(&id, SampleRange::new(0, 100).unwrap())
            .unwrap();

        // Export, then reload into a fresh engine.
        let archive_bytes = engine.export_kepz().unwrap();
        let restored = Engine::import_kepz(&archive_bytes).unwrap();

        // Project structure preserved (sources + edit list).
        assert_eq!(restored.project().sources.len(), 1);
        let restored_source = restored.project().sources.get(&id).unwrap();
        assert_eq!(restored_source.edits.len(), 1);

        // Sample data preserved through the storage round-trip.
        let post_query = restored
            .query_samples(&id, SampleRange::new(0, 100).unwrap())
            .unwrap();
        assert_eq!(pre_query, post_query);
    }

    #[test]
    fn import_resamples_to_project_rate_when_rates_differ() {
        // 8 frames at 48 kHz imported into a 96 kHz project should land as
        // ~16 frames at 96 kHz, with the source's recorded rate now 96 kHz.
        let bytes = synth_mono_wav(&[0.0, 0.5, 1.0, 0.5, 0.0, -0.5, -1.0, -0.5], 48_000);
        let mut engine = Engine::new(96_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();
        let frames = engine.source_frame_count(&id).unwrap();
        assert!(frames >= 15 && frames <= 17, "expected ~16 frames, got {frames}");
        assert_eq!(engine.source_sample_rate(&id), Some(96_000));
        // The first resampled frame should still be the first original sample
        // (linear interp at t=0 hits the original point exactly).
        let read = engine
            .read_base_samples(&id, SampleRange::new(0, 1).unwrap())
            .unwrap();
        assert_eq!(read[0], 0.0);
    }

    #[test]
    fn rejects_bad_wav_bytes() {
        let mut engine = Engine::new(96_000);
        let err = engine.import_wav("bad.wav", b"not a wav", now()).unwrap_err();
        assert!(matches!(err, ImportError::Wav(_)));
    }
}
