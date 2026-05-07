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
    EmptyName,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSource(id) => write!(f, "unknown source: {id}"),
            Self::UnknownProfile(id) => write!(f, "unknown noise profile: {id}"),
            Self::Storage(e) => write!(f, "{e}"),
            Self::Dsp(e) => write!(f, "{e}"),
            Self::EmptyName => write!(f, "name must not be empty"),
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

    /// Effective playable length of a source in frames, after applying its
    /// active edit list. Length-changing ops (Trim, Cut, Generate) make
    /// this differ from the immutable base file length — UI callers
    /// always want the effective value.
    pub fn source_frame_count(&self, id: &SourceId) -> Option<u64> {
        self.effective_frame_count(id).ok()
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
        // Snapshot any range information we need *before* mutating the
        // source — Trim needs the trim window to remap referencing clips.
        let trim_range = match &op {
            Op::Trim { range } => Some(*range),
            _ => None,
        };
        {
            let source = self
                .project
                .sources
                .get_mut(id)
                .ok_or_else(|| QueryError::UnknownSource(id.clone()))?;
            source.apply(op, now);
        }
        self.regenerate_peaks(id)?;
        if let Some(range) = trim_range {
            self.reconcile_clips_for_trim(id, range);
        }
        Ok(())
    }

    /// Trim shortens a source to its `[a, b)` window. Walk every clip
    /// referencing the source and remap its source range and timeline
    /// length so the projection still makes sense:
    ///   new_in  = max(0, old_in  - a)   clamped to [0, b-a]
    ///   new_out = max(0, old_out - a)   clamped to [0, b-a]
    /// Clips whose remapped range is empty (their content fell entirely
    /// outside the trim window) are removed. The clip's timeline start
    /// stays where it was; the timeline length shrinks/grows to match
    /// the new source span.
    fn reconcile_clips_for_trim(&mut self, source_id: &SourceId, trim: SampleRange) {
        let new_len = trim.end() - trim.start();
        for track in &mut self.project.tracks {
            track.clips.retain_mut(|clip| {
                if clip.source_id != *source_id {
                    return true;
                }
                let new_in = clip.source_in.saturating_sub(trim.start()).min(new_len);
                let new_out = clip.source_out.saturating_sub(trim.start()).min(new_len);
                if new_out <= new_in {
                    return false;
                }
                clip.source_in = new_in;
                clip.source_out = new_out;
                let span = new_out - new_in;
                let stretched = (span as f64 * clip.time_stretch as f64).round() as u64;
                let start = clip.track_position.start();
                if let Ok(new_pos) =
                    SampleRange::new(start, start + stretched.max(1))
                {
                    clip.track_position = new_pos;
                }
                true
            });
        }
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

    /// Make an independent copy of `id`. The copy captures the *current
    /// rendered* state (base + active edits flattened into a fresh base
    /// file) with an empty edit list, so subsequent destructive edits to
    /// either the original or the copy don't affect the other. The new
    /// source gets an auto-suffixed name (`foo.wav` → `foo(1).wav`) and a
    /// unique id derived from the original.
    pub fn duplicate_source(
        &mut self,
        id: &SourceId,
        now: Timestamp,
    ) -> Result<SourceId, QueryError> {
        let rendered = self.render_full(id)?;
        let (channel_count, sample_rate, name) = {
            let s = self
                .project
                .sources
                .get(id)
                .expect("verified by render_full");
            (s.channel_count, s.sample_rate, s.name.clone())
        };
        let new_id = self.next_duplicate_id(id);
        let new_name = self.next_duplicate_name(&name);
        let path = format!("sources/{new_id}/base.f32");
        self.storage.write_all(&path, &rendered)?;

        let new_length = rendered.len() as u64 / channel_count as u64;
        let source = Source::new(
            new_id.clone(),
            new_name,
            channel_count,
            sample_rate,
            StoragePath::new(path),
            new_length,
            now,
        );
        let peaks = PeakCache::from_samples(&rendered, channel_count, DEFAULT_DECIMATION);
        self.project.sources.insert(new_id.clone(), source);
        self.peaks.insert(new_id.clone(), peaks);
        Ok(new_id)
    }

    /// Create a new source of zeros with the given length and channel
    /// count, registered under `desired_name` (auto-suffixed on collision)
    /// at the project's sample rate. Useful as a blank canvas the
    /// Generate op can splice content into.
    pub fn create_empty_source(
        &mut self,
        length_frames: u64,
        channels: u16,
        desired_name: &str,
        now: Timestamp,
    ) -> Result<SourceId, QueryError> {
        let channels = channels.max(1);
        let total_samples = length_frames as usize * channels as usize;
        let zeros = vec![0.0_f32; total_samples];
        let new_id = self.next_id_with_prefix("src_blank");
        let new_name = self.ensure_unique_name(desired_name);
        let path = format!("sources/{new_id}/base.f32");
        self.storage.write_all(&path, &zeros)?;

        let source = Source::new(
            new_id.clone(),
            new_name,
            channels,
            self.project.sample_rate(),
            StoragePath::new(path),
            length_frames,
            now,
        );
        let peaks = PeakCache::from_samples(&zeros, channels, DEFAULT_DECIMATION);
        self.project.sources.insert(new_id.clone(), source);
        self.peaks.insert(new_id.clone(), peaks);
        Ok(new_id)
    }

    /// Remove a source from the project, evicting it from the peak cache
    /// and dropping every clip that referenced it on every track. Returns
    /// `true` if the source existed (and was removed), `false` otherwise.
    /// The on-disk base file is not deleted from storage — kepz exports
    /// after a remove will skip it because the project no longer lists it.
    pub fn remove_source(&mut self, id: &SourceId) -> bool {
        let removed = self.project.sources.remove(id).is_some();
        if !removed {
            return false;
        }
        for track in &mut self.project.tracks {
            track.clips.retain(|c| c.source_id != *id);
        }
        self.peaks.remove(id);
        true
    }

    /// Set the display name of a source. Empty/whitespace names are rejected.
    pub fn rename_source(&mut self, id: &SourceId, new_name: &str) -> Result<(), QueryError> {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            return Err(QueryError::EmptyName);
        }
        let source = self
            .project
            .sources
            .get_mut(id)
            .ok_or_else(|| QueryError::UnknownSource(id.clone()))?;
        source.name = trimmed.to_owned();
        Ok(())
    }

    /// Set (or clear, when `None`) the library folder a source lives in.
    /// Folders are a UI grouping construct — the engine treats them as
    /// opaque labels.
    pub fn set_source_folder(
        &mut self,
        id: &SourceId,
        folder: Option<&str>,
    ) -> Result<(), QueryError> {
        let source = self
            .project
            .sources
            .get_mut(id)
            .ok_or_else(|| QueryError::UnknownSource(id.clone()))?;
        source.folder = folder.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        Ok(())
    }

    fn next_duplicate_id(&self, original: &SourceId) -> SourceId {
        let stem = original.as_str();
        for n in 1u32.. {
            let candidate = SourceId::new(format!("{stem}_d{n}"));
            if !self.project.sources.contains_key(&candidate) {
                return candidate;
            }
        }
        unreachable!("u32 exhausted before unique id found")
    }

    fn next_duplicate_name(&self, original: &str) -> String {
        let existing: std::collections::HashSet<&str> = self
            .project
            .sources
            .values()
            .map(|s| s.name.as_str())
            .collect();
        let (stem, ext) = split_name_ext(original);
        let (base_stem, start_n) = strip_paren_suffix(stem);
        for n in start_n.. {
            let candidate = if ext.is_empty() {
                format!("{base_stem}({n})")
            } else {
                format!("{base_stem}({n}).{ext}")
            };
            if !existing.contains(candidate.as_str()) {
                return candidate;
            }
        }
        unreachable!("u32 exhausted before unique name found")
    }

    /// Convenience: render the project and encode the result as a 32-bit
    /// float WAV. The output is suitable for download or for writing to disk.
    pub fn mixdown_wav(&self) -> Result<Vec<u8>, crate::mixdown::MixdownError> {
        let stereo = self.mixdown_stereo()?;
        Ok(crate::wav::encode_f32(&stereo, 2, self.project.sample_rate()))
    }

    /// Render the arrangement over `[start_frame, end_frame)` (project
    /// frames, project sample rate) and store the result as a new stereo
    /// source named `desired_name`. Returns the new source's id. The name
    /// is auto-suffixed if it collides with an existing source.
    ///
    /// Implementation note: the v1 path renders the full project and slices
    /// — wasteful for short selections in long arrangements, but correct
    /// across every existing mixdown feature. A range-aware mixdown can
    /// land later behind the same API.
    pub fn render_range_to_source(
        &mut self,
        start_frame: u64,
        end_frame: u64,
        desired_name: &str,
        now: Timestamp,
    ) -> Result<SourceId, crate::mixdown::MixdownError> {
        if end_frame <= start_frame {
            return Err(crate::mixdown::MixdownError::Unsupported("empty range"));
        }
        let stereo = self.mixdown_stereo()?;
        let total_frames = (stereo.len() / 2) as u64;
        if start_frame >= total_frames {
            return Err(crate::mixdown::MixdownError::Unsupported("range past end"));
        }
        let end = end_frame.min(total_frames);
        let lo = (start_frame * 2) as usize;
        let hi = (end * 2) as usize;
        let slice: Vec<f32> = stereo[lo..hi].to_vec();

        let new_id = self.next_id_with_prefix("src_render");
        let new_name = self.ensure_unique_name(desired_name);
        let path = format!("sources/{new_id}/base.f32");
        self.storage
            .write_all(&path, &slice)
            .map_err(|e| crate::mixdown::MixdownError::Query(QueryError::Storage(e)))?;

        let frame_count = end - start_frame;
        let source = Source::new(
            new_id.clone(),
            new_name,
            2,
            self.project.sample_rate(),
            StoragePath::new(path),
            frame_count,
            now,
        );
        let peaks = PeakCache::from_samples(&slice, 2, DEFAULT_DECIMATION);
        self.project.sources.insert(new_id.clone(), source);
        self.peaks.insert(new_id.clone(), peaks);
        Ok(new_id)
    }

    fn next_id_with_prefix(&self, prefix: &str) -> SourceId {
        for n in 1u32.. {
            let candidate = SourceId::new(format!("{prefix}_{n:04}"));
            if !self.project.sources.contains_key(&candidate) {
                return candidate;
            }
        }
        unreachable!("u32 exhausted before unique id found")
    }

    fn ensure_unique_name(&self, desired: &str) -> String {
        let existing: std::collections::HashSet<&str> = self
            .project
            .sources
            .values()
            .map(|s| s.name.as_str())
            .collect();
        if !existing.contains(desired) {
            return desired.to_owned();
        }
        self.next_duplicate_name(desired)
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

fn split_name_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

fn strip_paren_suffix(stem: &str) -> (&str, u32) {
    if stem.ends_with(')') {
        if let Some(open) = stem.rfind('(') {
            if open > 0 {
                let inside = &stem[open + 1..stem.len() - 1];
                if let Ok(n) = inside.parse::<u32>() {
                    return (&stem[..open], n + 1);
                }
            }
        }
    }
    (stem, 1)
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

    #[test]
    fn duplicate_source_creates_independent_copy_with_suffixed_name() {
        let bytes = synth_mono_wav(&[0.1, 0.2, 0.3, 0.4], 48_000);
        let mut engine = Engine::new(48_000);
        let a = engine.import_wav("loop.wav", &bytes, now()).unwrap();
        let b = engine.duplicate_source(&a, now()).unwrap();
        assert_ne!(a, b);
        let names: Vec<String> = engine
            .project()
            .sources
            .values()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"loop.wav".to_string()));
        assert!(names.contains(&"loop(1).wav".to_string()));

        // Editing the original must not change the duplicate's samples.
        engine
            .apply_op(
                &a,
                Op::Silence {
                    range: SampleRange::new(0, 4).unwrap(),
                },
                now(),
            )
            .unwrap();
        let dup = engine
            .read_base_samples(&b, SampleRange::new(0, 4).unwrap())
            .unwrap();
        assert!((dup[0] - 0.1).abs() < 1e-6);
        assert!((dup[1] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn duplicate_source_increments_existing_paren_suffix() {
        let bytes = synth_mono_wav(&[0.0; 4], 48_000);
        let mut engine = Engine::new(48_000);
        let a = engine.import_wav("loop.wav", &bytes, now()).unwrap();
        let _b = engine.duplicate_source(&a, now()).unwrap(); // loop(1).wav
        let c = engine.duplicate_source(&a, now()).unwrap(); // loop(2).wav (loop(1) taken)
        let name_c = engine.project().sources.get(&c).unwrap().name.clone();
        assert_eq!(name_c, "loop(2).wav");

        // Duplicating the (1) variant should produce (2) — but (2) is taken,
        // so it should land on (3).
        let b_id = engine
            .project()
            .sources
            .iter()
            .find(|(_, s)| s.name == "loop(1).wav")
            .map(|(id, _)| id.clone())
            .unwrap();
        let d = engine.duplicate_source(&b_id, now()).unwrap();
        let name_d = engine.project().sources.get(&d).unwrap().name.clone();
        assert_eq!(name_d, "loop(3).wav");
    }

    #[test]
    fn duplicate_source_handles_extensionless_name() {
        let bytes = synth_mono_wav(&[0.0; 4], 48_000);
        let mut engine = Engine::new(48_000);
        let a = engine.import_wav("kick", &bytes, now()).unwrap();
        let b = engine.duplicate_source(&a, now()).unwrap();
        let name = engine.project().sources.get(&b).unwrap().name.clone();
        assert_eq!(name, "kick(1)");
    }

    #[test]
    fn trim_reconciles_clips_referencing_the_trimmed_source() {
        use crate::ids::{ClipId, TrackId};
        use crate::project::{Clip, Fade, Track};

        let bytes = synth_mono_wav(&[0.0_f32; 100], 48_000);
        let mut engine = Engine::new(48_000);
        let src = engine.import_wav("a.wav", &bytes, now()).unwrap();

        let mut track = Track {
            id: TrackId(1),
            name: "T".into(),
            height: 80.0,
            mute: false,
            solo: false,
            arm: false,
            gain_db: 0.0,
            pan: 0.0,
            inserts: Vec::new(),
            automation: Vec::new(),
            clips: Vec::new(),
        };
        let mk_clip = |id: u64, src: SourceId, in_: u64, out_: u64, at: u64| Clip {
            id: ClipId(id),
            source_id: src,
            name: "clip".into(),
            track_position: SampleRange::new(at, at + (out_ - in_)).unwrap(),
            source_in: in_,
            source_out: out_,
            gain_db: 0.0,
            pan: 0.0,
            fade_in: Fade::none(),
            fade_out: Fade::none(),
            time_stretch: 1.0,
            pitch_shift_cents: 0.0,
            envelopes: Vec::new(),
            locked: false,
            group: None,
        };
        // Clip 1: fully inside the trim window [10, 60) — should keep.
        track.clips.push(mk_clip(1, src.clone(), 0, 100, 0));
        // Clip 2: entirely past the trim window — should be removed.
        track.clips.push(mk_clip(2, src.clone(), 70, 90, 200));
        engine.project_mut().tracks.push(track);

        engine
            .apply_op(
                &src,
                Op::Trim {
                    range: SampleRange::new(10, 60).unwrap(),
                },
                now(),
            )
            .unwrap();

        let clips = &engine.project().tracks[0].clips;
        assert_eq!(clips.len(), 1, "clip 2 should have been removed");
        let c = &clips[0];
        assert_eq!(c.id, ClipId(1));
        // Old [0, 100) → after Trim(10, 60), shifted by -10 and clamped to [0, 50): [0, 50).
        assert_eq!(c.source_in, 0);
        assert_eq!(c.source_out, 50);
        assert_eq!(c.track_position.len(), 50);
    }

    #[test]
    fn create_empty_source_yields_a_zeroed_buffer_of_the_requested_length() {
        let mut engine = Engine::new(48_000);
        let id = engine
            .create_empty_source(1000, 2, "blank.wav", now())
            .unwrap();
        let s = engine.project().sources.get(&id).unwrap();
        assert_eq!(s.base_length, 1000);
        assert_eq!(s.channel_count, 2);
        assert_eq!(s.name, "blank.wav");
        let buf = engine
            .read_base_samples(&id, SampleRange::new(0, 1000).unwrap())
            .unwrap();
        assert_eq!(buf.len(), 2000);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn rename_source_sets_name_and_rejects_empty() {
        let bytes = synth_mono_wav(&[0.0; 2], 48_000);
        let mut engine = Engine::new(48_000);
        let a = engine.import_wav("orig.wav", &bytes, now()).unwrap();
        engine.rename_source(&a, "  bass loop  ").unwrap();
        let n = engine.project().sources.get(&a).unwrap().name.clone();
        assert_eq!(n, "bass loop");

        let err = engine.rename_source(&a, "   ").unwrap_err();
        assert!(matches!(err, QueryError::EmptyName));
    }

    #[test]
    fn render_range_to_source_captures_arrangement_slice_as_new_source() {
        use crate::ids::{ClipId, TrackId};
        use crate::project::{Clip, Fade, Track};

        let bytes = synth_mono_wav(&[0.5_f32; 100], 48_000);
        let mut engine = Engine::new(48_000);
        let src = engine.import_wav("a.wav", &bytes, now()).unwrap();

        let mut track = Track {
            id: TrackId(1),
            name: "T".into(),
            height: 80.0,
            mute: false,
            solo: false,
            arm: false,
            gain_db: 0.0,
            pan: 0.0,
            inserts: Vec::new(),
            automation: Vec::new(),
            clips: Vec::new(),
        };
        track.clips.push(Clip {
            id: ClipId(1),
            source_id: src.clone(),
            name: "clip".into(),
            track_position: SampleRange::new(0, 100).unwrap(),
            source_in: 0,
            source_out: 100,
            gain_db: 0.0,
            pan: 0.0,
            fade_in: Fade::none(),
            fade_out: Fade::none(),
            time_stretch: 1.0,
            pitch_shift_cents: 0.0,
            envelopes: Vec::new(),
            locked: false,
            group: None,
        });
        engine.project_mut().tracks.push(track);

        let new_id = engine
            .render_range_to_source(20, 60, "Render.wav", now())
            .unwrap();
        let new_src = engine.project().sources.get(&new_id).unwrap();
        assert_eq!(new_src.channel_count, 2);
        assert_eq!(new_src.base_length, 40);
        assert_eq!(new_src.name, "Render.wav");

        // A second render with the same desired name should auto-suffix.
        let new_id2 = engine
            .render_range_to_source(20, 60, "Render.wav", now())
            .unwrap();
        assert_ne!(new_id, new_id2);
        assert_eq!(
            engine.project().sources.get(&new_id2).unwrap().name,
            "Render(1).wav"
        );

        // Empty range rejected.
        let err = engine
            .render_range_to_source(50, 50, "x.wav", now())
            .unwrap_err();
        assert!(matches!(err, crate::mixdown::MixdownError::Unsupported(_)));
    }
}
