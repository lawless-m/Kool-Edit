//! Kool-Edit engine.
//!
//! See `kool-edit-design-docs/kool-edit-docs/02-architecture.md` for the
//! module layout this crate is growing into. The first slice is the data
//! model from doc 03: sources with edit lists, and the multitrack project
//! hierarchy. DSP, storage, and serialisation come in later slices.

pub mod dsl;
pub mod dsp;
pub mod edit_list;
pub mod effect;
pub mod engine;
pub mod envelope;
pub mod ids;
pub mod kepz;
pub mod mixdown;
pub mod nr;
pub mod op;
pub mod peaks;
pub mod project;
pub mod range;
pub mod source;
pub mod spectral;
pub mod stft;
pub mod storage;
pub mod tempo;
pub mod wav;

pub const FORMAT_VERSION: u32 = 1;
pub const DEFAULT_PROJECT_SAMPLE_RATE: u32 = 96_000;

/// Returns a banner string used by both the native test harness and the wasm
/// surface, so the round-trip from UI → engine can be smoke-tested end to end.
pub fn banner() -> String {
    format!(
        "kool-edit-engine v{} (format_version={})",
        env!("CARGO_PKG_VERSION"),
        FORMAT_VERSION
    )
}

#[cfg(feature = "wasm")]
mod wasm_api {
    use wasm_bindgen::prelude::*;

    use crate::engine::Engine;
    use crate::ids::{ClipId, ProfileId, SourceId, TrackId};
    use crate::op::Op;
    use crate::project::{Clip, Fade, Project, TempoSettings, Track};
    use crate::range::SampleRange;
    use crate::source::Timestamp;

    #[wasm_bindgen]
    pub fn banner() -> String {
        super::banner()
    }

    #[wasm_bindgen]
    pub fn format_version() -> u32 {
        super::FORMAT_VERSION
    }

    /// JS-callable wrapper around [`Engine`]. The browser instantiates one
    /// of these inside the engine Worker and drives it through postMessage
    /// commands; the Worker translates those into method calls here.
    #[wasm_bindgen]
    pub struct WasmEngine {
        inner: Engine,
    }

    #[wasm_bindgen]
    impl WasmEngine {
        #[wasm_bindgen(constructor)]
        pub fn new(sample_rate: u32) -> Self {
            Self {
                inner: Engine::new(sample_rate),
            }
        }

        #[wasm_bindgen(js_name = importWav)]
        pub fn import_wav(
            &mut self,
            name: &str,
            bytes: &[u8],
            now_iso8601: &str,
        ) -> Result<String, JsError> {
            self.inner
                .import_wav(name, bytes, Timestamp(now_iso8601.to_string()))
                .map(|id| id.as_str().to_owned())
                .map_err(|e| JsError::new(&e.to_string()))
        }

        #[wasm_bindgen(js_name = peakSummary)]
        pub fn peak_summary(
            &self,
            source_id: &str,
            columns: u32,
        ) -> Option<Box<[f32]>> {
            let id = SourceId::new(source_id);
            let pairs = self.inner.peak_summary(&id, columns as usize)?;
            // Flatten to [min, max, min, max, ...] for cheap transfer to JS.
            let mut flat = Vec::with_capacity(pairs.len() * 2);
            for p in pairs {
                flat.push(p.min);
                flat.push(p.max);
            }
            Some(flat.into_boxed_slice())
        }

        #[wasm_bindgen(js_name = peakSummaryRange)]
        pub fn peak_summary_range(
            &self,
            source_id: &str,
            start_frame: u64,
            end_frame: u64,
            columns: u32,
        ) -> Option<Box<[f32]>> {
            let id = SourceId::new(source_id);
            let pairs = self
                .inner
                .peak_summary_range(&id, start_frame, end_frame, columns as usize)?;
            let mut flat = Vec::with_capacity(pairs.len() * 2);
            for p in pairs {
                flat.push(p.min);
                flat.push(p.max);
            }
            Some(flat.into_boxed_slice())
        }

        /// Per-channel range summary. Returns `channels * columns * 2`
        /// floats laid out as channel-major: all of channel 0's
        /// `[min, max, min, max, ...]` first, then channel 1, etc.
        /// Caller knows the channel count from `listSources` and uses
        /// the matching stride. Used by the stereo waveform renderer.
        #[wasm_bindgen(js_name = peakSummaryRangeChannels)]
        pub fn peak_summary_range_channels(
            &self,
            source_id: &str,
            start_frame: u64,
            end_frame: u64,
            columns: u32,
        ) -> Option<Box<[f32]>> {
            let id = SourceId::new(source_id);
            let per_channel = self
                .inner
                .peak_summary_range_channels(&id, start_frame, end_frame, columns as usize)?;
            let cols = columns as usize;
            let mut flat = Vec::with_capacity(per_channel.len() * cols * 2);
            for channel in &per_channel {
                for p in channel {
                    flat.push(p.min);
                    flat.push(p.max);
                }
            }
            Some(flat.into_boxed_slice())
        }

        /// Compute STFT magnitudes for a source range, returning a flat
        /// row-major buffer of `frame_count * bin_count` values, where
        /// `bin_count = fft_size/2 + 1` (positive bins only) and
        /// `frame_count = (end - start) / hop_size + 1`. Multichannel
        /// sources are mixed to mono before analysis. Magnitudes are linear
        /// (not log); the UI applies its own dB / colour mapping.
        #[wasm_bindgen(js_name = spectrogramTile)]
        pub fn spectrogram_tile(
            &self,
            source_id: &str,
            start_frame: u64,
            end_frame: u64,
            fft_size: u32,
            hop_size: u32,
        ) -> Result<Box<[f32]>, JsError> {
            use crate::stft::Stft;
            if fft_size == 0 || hop_size == 0 || hop_size > fft_size {
                return Err(JsError::new("invalid stft params"));
            }
            let id = SourceId::new(source_id);
            let range = SampleRange::new(start_frame, end_frame)
                .map_err(|e| JsError::new(&e.to_string()))?;
            let samples = self
                .inner
                .query_samples(&id, range)
                .map_err(|e| JsError::new(&e.to_string()))?;
            let channels = self
                .inner
                .source_channel_count(&id)
                .ok_or_else(|| JsError::new("unknown source"))?;
            let mono: Vec<f32> = if channels == 1 {
                samples
            } else {
                let ch = channels as usize;
                let frames = samples.len() / ch;
                (0..frames)
                    .map(|f| {
                        let mut s = 0.0_f32;
                        for c in 0..ch {
                            s += samples[f * ch + c];
                        }
                        s / ch as f32
                    })
                    .collect()
            };
            let stft = Stft::new_hann(fft_size as usize, hop_size as usize);
            let frames = stft.analyze(&mono);
            let bin_count = fft_size as usize / 2 + 1;
            let mut out = Vec::with_capacity(frames.len() * bin_count);
            for frame in &frames {
                for k in 0..bin_count {
                    out.push(frame[k].norm());
                }
            }
            Ok(out.into_boxed_slice())
        }

        #[wasm_bindgen(js_name = sourceFrameCount)]
        pub fn source_frame_count(&self, source_id: &str) -> Option<u64> {
            self.inner.source_frame_count(&SourceId::new(source_id))
        }

        /// Run YIN pitch detection over the given source range and return
        /// one Hz value per `hop_samples` step (window centred on each hop;
        /// 0.0 = unvoiced). Used by Autotune's Reference mode in the UI.
        #[wasm_bindgen(js_name = detectPitchContour)]
        pub fn detect_pitch_contour(
            &self,
            source_id: &str,
            start_frame: u64,
            end_frame: u64,
            hop_samples: u32,
            window_samples: u32,
        ) -> Result<Box<[f32]>, JsError> {
            let id = SourceId::new(source_id);
            let range = SampleRange::new(start_frame, end_frame)
                .map_err(|e| JsError::new(&e.to_string()))?;
            let samples = self
                .inner
                .query_samples(&id, range)
                .map_err(|e| JsError::new(&e.to_string()))?;
            let channels = self
                .inner
                .source_channel_count(&id)
                .ok_or_else(|| JsError::new("unknown source"))?;
            // YIN is monophonic: collapse multichannel input by averaging.
            let mono: Vec<f32> = if channels == 1 {
                samples
            } else {
                let ch = channels as usize;
                let frames = samples.len() / ch;
                (0..frames)
                    .map(|f| {
                        let mut s = 0.0_f32;
                        for c in 0..ch {
                            s += samples[f * ch + c];
                        }
                        s / ch as f32
                    })
                    .collect()
            };
            let sample_rate = self
                .inner
                .source_sample_rate(&id)
                .ok_or_else(|| JsError::new("unknown source"))?;
            let contour = crate::dsp::pitch_contour(
                &mono,
                sample_rate,
                hop_samples as usize,
                window_samples as usize,
            );
            Ok(contour.into_boxed_slice())
        }

        #[wasm_bindgen(js_name = sourceSampleRate)]
        pub fn source_sample_rate(&self, source_id: &str) -> Option<u32> {
            self.inner.source_sample_rate(&SourceId::new(source_id))
        }

        /// Estimate the dominant tempo of a source. Returns JSON
        /// `{bpm, confidence, candidates: [[bpm, normScore], ...]}` —
        /// candidates are top-3, ordered best-first, with scores
        /// normalised so the top is always 1.0.
        #[wasm_bindgen(js_name = detectBpm)]
        pub fn detect_bpm(&self, source_id: &str) -> Result<String, JsError> {
            let id = SourceId::new(source_id);
            let frames = self
                .inner
                .source_frame_count(&id)
                .ok_or_else(|| JsError::new("unknown source"))?;
            let range = SampleRange::new(0, frames)
                .map_err(|e| JsError::new(&e.to_string()))?;
            let samples = self
                .inner
                .query_samples(&id, range)
                .map_err(|e| JsError::new(&e.to_string()))?;
            let channels = self
                .inner
                .source_channel_count(&id)
                .ok_or_else(|| JsError::new("unknown source"))?;
            // Sum to mono: BPM cues are equally distributed across channels
            // for typical material; the cheap mean is plenty.
            let mono: Vec<f32> = if channels == 1 {
                samples
            } else {
                let ch = channels as usize;
                let frames = samples.len() / ch;
                (0..frames)
                    .map(|f| {
                        let mut s = 0.0_f32;
                        for c in 0..ch {
                            s += samples[f * ch + c];
                        }
                        s / ch as f32
                    })
                    .collect()
            };
            let sample_rate = self
                .inner
                .source_sample_rate(&id)
                .ok_or_else(|| JsError::new("unknown source"))?;
            let est = crate::tempo::estimate_bpm(&mono, sample_rate)
                .map_err(|e| JsError::new(&e.to_string()))?;
            let candidates: Vec<serde_json::Value> = est
                .candidates
                .iter()
                .map(|(b, s)| serde_json::json!([b, s]))
                .collect();
            Ok(serde_json::json!({
                "bpm": est.bpm,
                "confidence": est.confidence,
                "candidates": candidates,
            })
            .to_string())
        }

        #[wasm_bindgen(js_name = sourceChannelCount)]
        pub fn source_channel_count(&self, source_id: &str) -> Option<u32> {
            self.inner
                .source_channel_count(&SourceId::new(source_id))
                .map(|c| c as u32)
        }

        /// Apply a destructive op to a source. The op is passed as JSON to
        /// keep the bridge surface small; `op_json` matches the same shape
        /// produced by `Project::to_json`.
        #[wasm_bindgen(js_name = applyOp)]
        pub fn apply_op(
            &mut self,
            source_id: &str,
            op_json: &str,
            now_iso8601: &str,
        ) -> Result<(), JsError> {
            let op: Op = serde_json::from_str(op_json)
                .map_err(|e| JsError::new(&format!("op parse: {e}")))?;
            self.inner
                .apply_op(&SourceId::new(source_id), op, Timestamp(now_iso8601.into()))
                .map_err(|e| JsError::new(&e.to_string()))
        }

        #[wasm_bindgen(js_name = undo)]
        pub fn undo(&mut self, source_id: &str) -> Result<bool, JsError> {
            self.inner
                .undo(&SourceId::new(source_id))
                .map_err(|e| JsError::new(&e.to_string()))
        }

        #[wasm_bindgen(js_name = redo)]
        pub fn redo(&mut self, source_id: &str) -> Result<bool, JsError> {
            self.inner
                .redo(&SourceId::new(source_id))
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Render samples for the given frame range, replaying the active
        /// edit list. Returned as a flat Float32Array of interleaved samples.
        #[wasm_bindgen(js_name = querySamples)]
        pub fn query_samples(
            &self,
            source_id: &str,
            start_frame: u64,
            end_frame: u64,
        ) -> Result<Box<[f32]>, JsError> {
            let range = SampleRange::new(start_frame, end_frame)
                .map_err(|e| JsError::new(&e.to_string()))?;
            let samples = self
                .inner
                .query_samples(&SourceId::new(source_id), range)
                .map_err(|e| JsError::new(&e.to_string()))?;
            Ok(samples.into_boxed_slice())
        }

        #[wasm_bindgen(js_name = flatten)]
        pub fn flatten(&mut self, source_id: &str, now_iso8601: &str) -> Result<(), JsError> {
            self.inner
                .flatten(&SourceId::new(source_id), Timestamp(now_iso8601.into()))
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Make an independent copy of `source_id`. The new source captures
        /// the current rendered state (edits flattened) and gets a unique
        /// id and auto-suffixed name. Returns the new source id.
        #[wasm_bindgen(js_name = duplicateSource)]
        pub fn duplicate_source(
            &mut self,
            source_id: &str,
            now_iso8601: &str,
        ) -> Result<String, JsError> {
            self.inner
                .duplicate_source(&SourceId::new(source_id), Timestamp(now_iso8601.into()))
                .map(|id| id.as_str().to_owned())
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Create a new zero-filled source under `desired_name` at the
        /// project's sample rate. Useful as a blank canvas the Generate
        /// op can splice tone or noise into.
        #[wasm_bindgen(js_name = createEmptySource)]
        pub fn create_empty_source(
            &mut self,
            length_frames: u64,
            channels: u32,
            desired_name: &str,
            now_iso8601: &str,
        ) -> Result<String, JsError> {
            self.inner
                .create_empty_source(
                    length_frames,
                    channels as u16,
                    desired_name,
                    Timestamp(now_iso8601.into()),
                )
                .map(|id| id.as_str().to_owned())
                .map_err(|e| JsError::new(&e.to_string()))
        }

        #[wasm_bindgen(js_name = renameSource)]
        pub fn rename_source(
            &mut self,
            source_id: &str,
            new_name: &str,
        ) -> Result<(), JsError> {
            self.inner
                .rename_source(&SourceId::new(source_id), new_name)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Remove a source. Also drops every clip referencing it on every
        /// track (lossy — the user is expected to confirm). Returns true
        /// if the source existed.
        #[wasm_bindgen(js_name = removeSource)]
        pub fn remove_source(&mut self, source_id: &str) -> bool {
            self.inner.remove_source(&SourceId::new(source_id))
        }

        /// Move a source to a library folder (or to root when the folder
        /// argument is empty / null-ish). Folders are a UI grouping
        /// label; the engine treats them as opaque.
        #[wasm_bindgen(js_name = setSourceFolder)]
        pub fn set_source_folder(
            &mut self,
            source_id: &str,
            folder: Option<String>,
        ) -> Result<(), JsError> {
            self.inner
                .set_source_folder(&SourceId::new(source_id), folder.as_deref())
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Render the arrangement over `[start_frame, end_frame)` (project
        /// frames at the project sample rate) and store the result as a
        /// new stereo source. Returns the new source id.
        #[wasm_bindgen(js_name = renderRangeToSource)]
        pub fn render_range_to_source(
            &mut self,
            start_frame: u64,
            end_frame: u64,
            desired_name: &str,
            now_iso8601: &str,
        ) -> Result<String, JsError> {
            self.inner
                .render_range_to_source(
                    start_frame,
                    end_frame,
                    desired_name,
                    Timestamp(now_iso8601.into()),
                )
                .map(|id| id.as_str().to_owned())
                .map_err(|e| JsError::new(&e.to_string()))
        }

        #[wasm_bindgen(js_name = projectJson)]
        pub fn project_json(&self) -> Result<String, JsError> {
            self.inner
                .project()
                .to_json()
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Render the project as DSL text per `04-dsl-grammar.md`. Returns
        /// an error for projects that use features the emitter doesn't yet
        /// cover (effect param blocks, clipboard ops, spectral edits, etc.).
        #[wasm_bindgen(js_name = projectDsl)]
        pub fn project_dsl(&self) -> Result<String, JsError> {
            crate::dsl::project_to_dsl(self.inner.project())
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Parse `.keds` text and replace the engine's project with the
        /// result. The supported subset matches the emitter; anything else
        /// returns a typed `Unsupported` error.
        #[wasm_bindgen(js_name = loadProjectDsl)]
        pub fn load_project_dsl(&mut self, dsl: &str) -> Result<(), JsError> {
            let project = crate::dsl::parse_project(dsl)
                .map_err(|e| JsError::new(&e.to_string()))?;
            self.inner.replace_project(project);
            Ok(())
        }

        /// Mix the project down to a 32-bit float WAV, returned as bytes
        /// suitable for download. Errors describe which feature blocked the
        /// render (envelopes, automation, time stretch, sample-rate
        /// mismatch, etc.).
        #[wasm_bindgen(js_name = mixdownWav)]
        pub fn mixdown_wav(&self) -> Result<Box<[u8]>, JsError> {
            self.inner
                .mixdown_wav()
                .map(|v| v.into_boxed_slice())
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Capture a noise profile from a region of a source. The averaged
        /// magnitude spectrum is stored under `profile_id` and can be
        /// referenced by subsequent NoiseReduce ops.
        #[wasm_bindgen(js_name = captureNoiseProfile)]
        pub fn capture_noise_profile(
            &mut self,
            source_id: &str,
            start_frame: u64,
            end_frame: u64,
            name: &str,
            profile_id: &str,
            fft_size: u32,
        ) -> Result<(), JsError> {
            let range = SampleRange::new(start_frame, end_frame)
                .map_err(|e| JsError::new(&e.to_string()))?;
            self.inner
                .capture_noise_profile(
                    &SourceId::new(source_id),
                    range,
                    name.to_string(),
                    ProfileId::new(profile_id),
                    fft_size,
                )
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// List every captured noise profile in the project as JSON. Each
        /// entry is `{id, name, sourceId, start, end, fftSize}` so the UI
        /// can populate a profile picker for NoiseReduce.
        #[wasm_bindgen(js_name = listNoiseProfiles)]
        pub fn list_noise_profiles(&self) -> String {
            let arr: Vec<serde_json::Value> = self
                .inner
                .project()
                .noise_profiles
                .values()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id.as_str(),
                        "name": p.name,
                        "sourceId": p.captured_from.as_str(),
                        "start": p.range.start(),
                        "end": p.range.end(),
                        "magnitudeBins": p.magnitudes.len(),
                    })
                })
                .collect();
            serde_json::Value::Array(arr).to_string()
        }

        /// Build a `.kepz` portable archive (project JSON + every source's
        /// base file in one zip). Returns the raw bytes.
        #[wasm_bindgen(js_name = exportKepz)]
        pub fn export_kepz(&self) -> Result<Box<[u8]>, JsError> {
            self.inner
                .export_kepz()
                .map(|v| v.into_boxed_slice())
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Replace the engine's project + sources with the contents of a
        /// `.kepz` archive. The samples are restored into a fresh in-memory
        /// storage; peak caches are not serialised so any peak-rendering UI
        /// will fetch them lazily.
        #[wasm_bindgen(js_name = importKepz)]
        pub fn import_kepz(&mut self, bytes: &[u8]) -> Result<(), JsError> {
            let new_engine = Engine::import_kepz(bytes)
                .map_err(|e| JsError::new(&e.to_string()))?;
            self.inner = new_engine;
            Ok(())
        }

        #[wasm_bindgen(js_name = loadProjectJson)]
        pub fn load_project_json(&mut self, json: &str) -> Result<(), JsError> {
            let project = Project::from_json(json).map_err(|e| JsError::new(&e.to_string()))?;
            self.inner.replace_project(project);
            Ok(())
        }

        // ---- multitrack: sources / tracks / clips ------------------------

        #[wasm_bindgen(js_name = projectSampleRate)]
        pub fn project_sample_rate(&self) -> u32 {
            self.inner.project().sample_rate()
        }

        /// Returns the project's tempo settings as JSON:
        /// `{bpm, beatsPerBar, beatUnit}`.
        #[wasm_bindgen(js_name = getTempo)]
        pub fn get_tempo(&self) -> String {
            let t = self.inner.project().tempo;
            serde_json::json!({
                "bpm": t.bpm,
                "beatsPerBar": t.beats_per_bar,
                "beatUnit": t.beat_unit,
            })
            .to_string()
        }

        #[wasm_bindgen(js_name = setTempo)]
        pub fn set_tempo(&mut self, bpm: f32, beats_per_bar: u32, beat_unit: u32) {
            self.inner.project_mut().tempo = TempoSettings {
                bpm,
                beats_per_bar,
                beat_unit,
            };
        }

        /// List all imported sources as a JSON array. Each entry is
        /// `{id, name, frames, sampleRate, channels}` where `frames` is the
        /// *effective* length after applying the source's active edit list
        /// — UI callers (selection bounds, clip placement) need the
        /// playable length, not the immutable base file length, otherwise
        /// length-changing ops (Trim, Cut, Generate) leave them off by the
        /// difference.
        #[wasm_bindgen(js_name = listSources)]
        pub fn list_sources(&self) -> String {
            let arr: Vec<serde_json::Value> = self
                .inner
                .project()
                .sources
                .values()
                .map(|s| {
                    let effective = self
                        .inner
                        .effective_frame_count(&s.id)
                        .unwrap_or(s.base_length);
                    serde_json::json!({
                        "id": s.id.as_str(),
                        "name": s.name,
                        "frames": effective,
                        "sampleRate": s.sample_rate,
                        "channels": s.channel_count,
                        "folder": s.folder,
                    })
                })
                .collect();
            serde_json::Value::Array(arr).to_string()
        }

        #[wasm_bindgen(js_name = addTrack)]
        pub fn add_track(&mut self, name: &str) -> u64 {
            let next = self
                .inner
                .project()
                .tracks
                .iter()
                .map(|t| t.id.0)
                .max()
                .map(|m| m + 1)
                .unwrap_or(1);
            let track = Track {
                id: TrackId(next),
                name: name.to_string(),
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
            self.inner.project_mut().tracks.push(track);
            next
        }

        /// JSON: `[{id, name, mute, solo, gainDb, pan, clipCount}]`.
        #[wasm_bindgen(js_name = listTracks)]
        pub fn list_tracks(&self) -> String {
            let arr: Vec<serde_json::Value> = self
                .inner
                .project()
                .tracks
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "id": t.id.0,
                        "name": t.name,
                        "mute": t.mute,
                        "solo": t.solo,
                        "gainDb": t.gain_db,
                        "pan": t.pan,
                        "clipCount": t.clips.len(),
                    })
                })
                .collect();
            serde_json::Value::Array(arr).to_string()
        }

        #[wasm_bindgen(js_name = setTrackGain)]
        pub fn set_track_gain(&mut self, track_id: u64, gain_db: f32) -> Result<(), JsError> {
            let track = self
                .inner
                .project_mut()
                .track_mut(TrackId(track_id))
                .ok_or_else(|| JsError::new(&format!("unknown track {track_id}")))?;
            track.gain_db = gain_db;
            Ok(())
        }

        /// Set the static stereo pan for a track. `pan` is clamped to
        /// [-1.0, +1.0] (L → R). Track pan automation lanes (set
        /// elsewhere) still take precedence at frames where the lane is
        /// defined; this scalar is the fallback used by mixdown when no
        /// automation is present.
        #[wasm_bindgen(js_name = setTrackPan)]
        pub fn set_track_pan(&mut self, track_id: u64, pan: f32) -> Result<(), JsError> {
            let track = self
                .inner
                .project_mut()
                .track_mut(TrackId(track_id))
                .ok_or_else(|| JsError::new(&format!("unknown track {track_id}")))?;
            track.pan = pan.clamp(-1.0, 1.0);
            Ok(())
        }

        #[wasm_bindgen(js_name = setTrackMute)]
        pub fn set_track_mute(&mut self, track_id: u64, mute: bool) -> Result<(), JsError> {
            let track = self
                .inner
                .project_mut()
                .track_mut(TrackId(track_id))
                .ok_or_else(|| JsError::new(&format!("unknown track {track_id}")))?;
            track.mute = mute;
            Ok(())
        }

        #[wasm_bindgen(js_name = setTrackName)]
        pub fn set_track_name(&mut self, track_id: u64, name: &str) -> Result<(), JsError> {
            let track = self
                .inner
                .project_mut()
                .track_mut(TrackId(track_id))
                .ok_or_else(|| JsError::new(&format!("unknown track {track_id}")))?;
            track.name = name.to_string();
            Ok(())
        }

        #[wasm_bindgen(js_name = setTrackSolo)]
        pub fn set_track_solo(&mut self, track_id: u64, solo: bool) -> Result<(), JsError> {
            let track = self
                .inner
                .project_mut()
                .track_mut(TrackId(track_id))
                .ok_or_else(|| JsError::new(&format!("unknown track {track_id}")))?;
            track.solo = solo;
            Ok(())
        }

        #[wasm_bindgen(js_name = removeTrack)]
        pub fn remove_track(&mut self, track_id: u64) -> bool {
            let tracks = &mut self.inner.project_mut().tracks;
            let before = tracks.len();
            tracks.retain(|t| t.id.0 != track_id);
            tracks.len() != before
        }

        /// Add a clip to `track_id`. The clip places `[source_in, source_out)`
        /// of `source_id` at `position_frame` on the track's timeline. Returns
        /// the new clip id.
        #[wasm_bindgen(js_name = addClip)]
        pub fn add_clip(
            &mut self,
            track_id: u64,
            source_id: &str,
            position_frame: u64,
            source_in: u64,
            source_out: u64,
        ) -> Result<u64, JsError> {
            if source_out < source_in {
                return Err(JsError::new("source_out < source_in"));
            }
            let len = source_out - source_in;
            let track_position = SampleRange::new(position_frame, position_frame + len)
                .map_err(|e| JsError::new(&e.to_string()))?;
            // Verify the source exists so callers get a typed error instead of
            // a silent no-op.
            let source_id_typed = SourceId::new(source_id);
            if !self
                .inner
                .project()
                .sources
                .contains_key(&source_id_typed)
            {
                return Err(JsError::new(&format!("unknown source {source_id}")));
            }
            // Mint a clip ID unique across the whole project.
            let next = self
                .inner
                .project()
                .tracks
                .iter()
                .flat_map(|t| t.clips.iter().map(|c| c.id.0))
                .max()
                .map(|m| m + 1)
                .unwrap_or(1);
            let clip = Clip {
                id: ClipId(next),
                source_id: source_id_typed,
                name: "Clip".to_string(),
                track_position,
                source_in,
                source_out,
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
            let track = self
                .inner
                .project_mut()
                .track_mut(TrackId(track_id))
                .ok_or_else(|| JsError::new(&format!("unknown track {track_id}")))?;
            track.clips.push(clip);
            Ok(next)
        }

        /// JSON: `[{id, sourceId, position, sourceIn, sourceOut, gainDb, pan,
        /// name, volumeEnvelope, panEnvelope}]`. Envelopes are arrays of
        /// `{time, value, curve}` (omitted when there are no breakpoints).
        #[wasm_bindgen(js_name = listClips)]
        pub fn list_clips(&self, track_id: u64) -> Option<String> {
            use crate::envelope::EnvelopeParam;
            let track = self.inner.project().track(TrackId(track_id))?;
            let arr: Vec<serde_json::Value> = track
                .clips
                .iter()
                .map(|c| {
                    let env_to_json = |param: EnvelopeParam| -> serde_json::Value {
                        let env = c.envelopes.iter().find(|e| e.parameter == param);
                        match env {
                            Some(e) => serde_json::Value::Array(
                                e.breakpoints
                                    .points()
                                    .iter()
                                    .map(|bp| {
                                        serde_json::json!({
                                            "time": bp.time,
                                            "value": bp.value,
                                            "curve": format!("{:?}", bp.curve),
                                        })
                                    })
                                    .collect(),
                            ),
                            None => serde_json::Value::Array(Vec::new()),
                        }
                    };
                    serde_json::json!({
                        "id": c.id.0,
                        "sourceId": c.source_id.as_str(),
                        "name": c.name,
                        "position": c.track_position.start(),
                        "endPosition": c.track_position.end(),
                        "sourceIn": c.source_in,
                        "sourceOut": c.source_out,
                        "gainDb": c.gain_db,
                        "pan": c.pan,
                        "volumeEnvelope": env_to_json(EnvelopeParam::Volume),
                        "panEnvelope": env_to_json(EnvelopeParam::Pan),
                        // 0 means ungrouped — matches the wire convention used
                        // by setClipGroup so callers don't have to handle null.
                        "group": c.group.map(|g| g.0).unwrap_or(0),
                    })
                })
                .collect();
            Some(serde_json::Value::Array(arr).to_string())
        }

        /// Replace a clip's envelope for `parameter` ("volume" or "pan").
        /// `breakpoints_json` is `[{time, value, curve?}]`; an empty array
        /// removes the envelope. `curve` defaults to "Linear".
        #[wasm_bindgen(js_name = setClipEnvelope)]
        pub fn set_clip_envelope(
            &mut self,
            track_id: u64,
            clip_id: u64,
            parameter: &str,
            breakpoints_json: &str,
        ) -> Result<(), JsError> {
            use crate::envelope::{
                Breakpoint, BreakpointSeq, ClipEnvelope, CurveKind, EnvelopeParam,
            };
            let param = match parameter {
                "volume" | "Volume" => EnvelopeParam::Volume,
                "pan" | "Pan" => EnvelopeParam::Pan,
                other => {
                    return Err(JsError::new(&format!("unknown envelope parameter `{other}`")));
                }
            };
            #[derive(serde::Deserialize)]
            struct InBp {
                time: u64,
                value: f32,
                #[serde(default)]
                curve: Option<String>,
            }
            let raw: Vec<InBp> = serde_json::from_str(breakpoints_json)
                .map_err(|e| JsError::new(&format!("breakpoints parse: {e}")))?;
            let parse_curve = |s: &str| match s {
                "Linear" | "linear" => CurveKind::Linear,
                "Exponential" | "exponential" => CurveKind::Exponential,
                "Logarithmic" | "logarithmic" => CurveKind::Logarithmic,
                "Hold" | "hold" => CurveKind::Hold,
                "SCurve" | "scurve" | "s_curve" => CurveKind::SCurve,
                _ => CurveKind::Linear,
            };
            let bps: Vec<Breakpoint> = raw
                .into_iter()
                .map(|b| Breakpoint {
                    time: b.time,
                    value: b.value,
                    curve: b
                        .curve
                        .as_deref()
                        .map(parse_curve)
                        .unwrap_or(CurveKind::Linear),
                })
                .collect();
            let track = self
                .inner
                .project_mut()
                .track_mut(TrackId(track_id))
                .ok_or_else(|| JsError::new(&format!("unknown track {track_id}")))?;
            let clip = track
                .clips
                .iter_mut()
                .find(|c| c.id.0 == clip_id)
                .ok_or_else(|| JsError::new(&format!("unknown clip {clip_id}")))?;
            clip.envelopes.retain(|e| e.parameter != param);
            if !bps.is_empty() {
                let seq = BreakpointSeq::new(bps).map_err(|e| JsError::new(&e.to_string()))?;
                clip.envelopes.push(ClipEnvelope {
                    parameter: param,
                    breakpoints: seq,
                });
            }
            Ok(())
        }

        #[wasm_bindgen(js_name = moveClip)]
        pub fn move_clip(
            &mut self,
            track_id: u64,
            clip_id: u64,
            new_position_frame: u64,
        ) -> Result<(), JsError> {
            let track = self
                .inner
                .project_mut()
                .track_mut(TrackId(track_id))
                .ok_or_else(|| JsError::new(&format!("unknown track {track_id}")))?;
            let clip = track
                .clips
                .iter_mut()
                .find(|c| c.id.0 == clip_id)
                .ok_or_else(|| JsError::new(&format!("unknown clip {clip_id}")))?;
            let len = clip.track_position.len();
            clip.track_position = SampleRange::new(new_position_frame, new_position_frame + len)
                .map_err(|e| JsError::new(&e.to_string()))?;
            Ok(())
        }

        /// Update a clip's source-frame window. The clip's `track_position` is
        /// resized so its length stays equal to `source_out - source_in` (the
        /// invariant established at addClip time). Used by the arranger's
        /// Choke action and any future trim-handle drags.
        #[wasm_bindgen(js_name = setClipSourceRange)]
        pub fn set_clip_source_range(
            &mut self,
            track_id: u64,
            clip_id: u64,
            source_in: u64,
            source_out: u64,
        ) -> Result<(), JsError> {
            if source_out <= source_in {
                return Err(JsError::new("source_out must be > source_in"));
            }
            let track = self
                .inner
                .project_mut()
                .track_mut(TrackId(track_id))
                .ok_or_else(|| JsError::new(&format!("unknown track {track_id}")))?;
            let clip = track
                .clips
                .iter_mut()
                .find(|c| c.id.0 == clip_id)
                .ok_or_else(|| JsError::new(&format!("unknown clip {clip_id}")))?;
            let new_len = source_out - source_in;
            let pos_start = clip.track_position.start();
            clip.source_in = source_in;
            clip.source_out = source_out;
            clip.track_position = SampleRange::new(pos_start, pos_start + new_len)
                .map_err(|e| JsError::new(&e.to_string()))?;
            Ok(())
        }

        #[wasm_bindgen(js_name = removeClip)]
        pub fn remove_clip(&mut self, track_id: u64, clip_id: u64) -> bool {
            let Some(track) = self.inner.project_mut().track_mut(TrackId(track_id)) else {
                return false;
            };
            let before = track.clips.len();
            track.clips.retain(|c| c.id.0 != clip_id);
            track.clips.len() != before
        }

        /// Set the group id of a clip. `group_id == 0` clears the group
        /// (clip becomes ungrouped). Other values are stored verbatim — the
        /// caller is responsible for minting fresh ids that don't collide.
        #[wasm_bindgen(js_name = setClipGroup)]
        pub fn set_clip_group(
            &mut self,
            track_id: u64,
            clip_id: u64,
            group_id: u64,
        ) -> Result<(), JsError> {
            use crate::ids::GroupId;
            let track = self
                .inner
                .project_mut()
                .track_mut(TrackId(track_id))
                .ok_or_else(|| JsError::new(&format!("unknown track {track_id}")))?;
            let clip = track
                .clips
                .iter_mut()
                .find(|c| c.id.0 == clip_id)
                .ok_or_else(|| JsError::new(&format!("unknown clip {clip_id}")))?;
            clip.group = if group_id == 0 {
                None
            } else {
                Some(GroupId(group_id))
            };
            Ok(())
        }

        // ---- groups ----------------------------------------------------

        /// JSON: `[{id, name}]`. Only includes groups that have been
        /// explicitly named via setGroupName — clips referencing an
        /// id that has no entry are still valid (the UI can fall back to
        /// "Group N" for those).
        #[wasm_bindgen(js_name = listGroups)]
        pub fn list_groups(&self) -> String {
            let arr: Vec<serde_json::Value> = self
                .inner
                .project()
                .groups
                .iter()
                .map(|g| serde_json::json!({ "id": g.id.0, "name": g.name }))
                .collect();
            serde_json::Value::Array(arr).to_string()
        }

        /// Set or update the human-readable name of a group. Inserts a new
        /// entry if the id isn't already named. Empty names are accepted —
        /// the UI is responsible for any "must not be empty" rule.
        #[wasm_bindgen(js_name = setGroupName)]
        pub fn set_group_name(&mut self, group_id: u64, name: String) {
            use crate::ids::GroupId;
            use crate::project::Group;
            let id = GroupId(group_id);
            let groups = &mut self.inner.project_mut().groups;
            if let Some(g) = groups.iter_mut().find(|g| g.id == id) {
                g.name = name;
            } else {
                groups.push(Group { id, name });
            }
        }

        /// Remove a group's entry from `Project::groups`. Member clips keep
        /// their `group` id — call setClipGroup with 0 to actually free
        /// them. Returns true if an entry was removed.
        #[wasm_bindgen(js_name = removeGroup)]
        pub fn remove_group(&mut self, group_id: u64) -> bool {
            use crate::ids::GroupId;
            let id = GroupId(group_id);
            let groups = &mut self.inner.project_mut().groups;
            let before = groups.len();
            groups.retain(|g| g.id != id);
            groups.len() != before
        }

        // ---- patterns --------------------------------------------------

        /// JSON: `[{name, gridJson}]`. Patterns are stored opaquely — the
        /// engine doesn't parse `gridJson`.
        #[wasm_bindgen(js_name = listPatterns)]
        pub fn list_patterns(&self) -> String {
            let arr: Vec<serde_json::Value> = self
                .inner
                .project()
                .patterns
                .iter()
                .map(|p| serde_json::json!({ "name": p.name, "gridJson": p.grid_json }))
                .collect();
            serde_json::Value::Array(arr).to_string()
        }

        /// Insert or replace a saved pattern. `name` is the unique key.
        #[wasm_bindgen(js_name = savePattern)]
        pub fn save_pattern(&mut self, name: String, grid_json: String) {
            use crate::project::Pattern;
            let patterns = &mut self.inner.project_mut().patterns;
            if let Some(p) = patterns.iter_mut().find(|p| p.name == name) {
                p.grid_json = grid_json;
            } else {
                patterns.push(Pattern { name, grid_json });
            }
        }

        /// Look up a saved pattern's grid JSON by name. Returns `None` when
        /// no pattern with that name exists.
        #[wasm_bindgen(js_name = loadPattern)]
        pub fn load_pattern(&self, name: String) -> Option<String> {
            self.inner
                .project()
                .patterns
                .iter()
                .find(|p| p.name == name)
                .map(|p| p.grid_json.clone())
        }

        /// Remove a saved pattern by name. Returns true if anything was
        /// removed.
        #[wasm_bindgen(js_name = removePattern)]
        pub fn remove_pattern(&mut self, name: String) -> bool {
            let patterns = &mut self.inner.project_mut().patterns;
            let before = patterns.len();
            patterns.retain(|p| p.name != name);
            patterns.len() != before
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_mentions_format_version() {
        let b = banner();
        assert!(b.contains("format_version=1"), "got: {b}");
    }

    #[test]
    fn defaults_match_design_docs() {
        assert_eq!(DEFAULT_PROJECT_SAMPLE_RATE, 96_000);
        assert_eq!(FORMAT_VERSION, 1);
    }
}
