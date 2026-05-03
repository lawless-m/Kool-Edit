//! Kool-Edit engine.
//!
//! See `kool-edit-design-docs/kool-edit-docs/02-architecture.md` for the
//! module layout this crate is growing into. The first slice is the data
//! model from doc 03: sources with edit lists, and the multitrack project
//! hierarchy. DSP, storage, and serialisation come in later slices.

pub mod edit_list;
pub mod effect;
pub mod engine;
pub mod envelope;
pub mod ids;
pub mod op;
pub mod peaks;
pub mod project;
pub mod range;
pub mod source;
pub mod spectral;
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
    use crate::ids::SourceId;
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

        #[wasm_bindgen(js_name = sourceFrameCount)]
        pub fn source_frame_count(&self, source_id: &str) -> Option<u64> {
            self.inner.source_frame_count(&SourceId::new(source_id))
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
