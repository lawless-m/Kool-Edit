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
    use crate::ids::{ProfileId, SourceId};
    use crate::op::Op;
    use crate::project::Project;
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

        #[wasm_bindgen(js_name = sourceFrameCount)]
        pub fn source_frame_count(&self, source_id: &str) -> Option<u64> {
            self.inner.source_frame_count(&SourceId::new(source_id))
        }

        #[wasm_bindgen(js_name = sourceSampleRate)]
        pub fn source_sample_rate(&self, source_id: &str) -> Option<u32> {
            self.inner.source_sample_rate(&SourceId::new(source_id))
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
