# Kool-Edit Architecture

## Principles

1. **Engine in Rust, UI in TypeScript.** The engine compiles to both native (for tests, fast iteration, real debugger) and Wasm (for the browser). Same code, two targets.
2. **Engine is headless.** No DOM, no Web Audio, no rendering. It owns project state, edit list, source storage abstraction, DSP, and serialisation. Inputs are commands, outputs are events and rendered audio buffers.
3. **UI is thin.** TypeScript layer owns DOM, WebGL, AudioWorklet glue, file dialogs, keyboard handling. It calls into the engine for everything that touches audio or project state.
4. **Engine runs in a Worker.** UI thread stays responsive even during heavy DSP. Communication via `postMessage` with structured cloning, or `SharedArrayBuffer` where ring-buffer semantics matter (metering, parameter updates).
5. **OPFS is the storage substrate.** All sample data and undo log lives in OPFS. Project JSON also lives there for autosave; explicit save uses File System Access API for user-chosen locations.
6. **Effects are shaped to be WAM-compatible.** Same parameter model, same audio I/O contract. No WAM host in v1, but the option is preserved.

## Top-level module map

```
+------------------------------------------------------------------+
|  UI (TypeScript, main thread)                                    |
|  - React or Solid (TBD)                                          |
|  - WebGL2 renderers (waveform, spectrogram, meters)              |
|  - Keyboard, mouse, file dialogs                                 |
|  - Effect dialogs, transport, mixer, project browser             |
+----------------------------+-------------------------------------+
                             | postMessage (commands / events)
                             | SharedArrayBuffer (metering, params)
+----------------------------v-------------------------------------+
|  Engine (Rust → Wasm, Worker thread)                             |
|                                                                  |
|  +---------------+  +---------------+  +---------------------+   |
|  | Project state |  | Edit list     |  | Source registry     |   |
|  | (tracks,      |  | (operations,  |  | (file refs, OPFS    |   |
|  |  clips, env.) |  |  history,     |  |  paths, peak cache) |   |
|  |               |  |  flatten)     |  |                     |   |
|  +---------------+  +---------------+  +---------------------+   |
|                                                                  |
|  +---------------+  +---------------+  +---------------------+   |
|  | DSP           |  | Render        |  | Serialisation       |   |
|  | (effects,     |  | (offline      |  | (JSON canonical,    |   |
|  |  STFT, time/  |  |  mixdown,     |  |  DSL parser/        |   |
|  |  pitch, NR)   |  |  preview)     |  |  emitter)           |   |
|  +---------------+  +---------------+  +---------------------+   |
+----------------------------+-------------------------------------+
                             | OPFS (sync access handle in Worker)
                             | getUserMedia (via UI proxy)
+----------------------------v-------------------------------------+
|  Audio I/O (TypeScript, AudioWorklet)                            |
|  - Playback worklet: pulls from engine via SAB ring buffer       |
|  - Recording worklet: pushes to engine via SAB ring buffer       |
|  - AudioContext at hardware rate, engine resamples to 96k        |
+------------------------------------------------------------------+
```

## Module: Engine core (Rust)

### Project state

The in-memory model. Owns:

- Project metadata: name, sample rate, created/modified timestamps, version.
- Source registry: map from source ID to source descriptor (path in OPFS, channel count, sample rate, length, peak cache reference).
- Edit lists: one per source, plus operations on multitrack clips.
- Track list: ordered, each with name, mute, solo, arm, height, insert chain, automation lanes.
- Clip list: each clip references a source by ID, with in-point, out-point, position on track, per-clip parameters, per-clip envelopes.
- Markers, transport state, view state (zoom, scroll).
- Noise profiles (named, stored as magnitude spectra).

### Source registry

A source is an immutable reference to a chunk of audio in OPFS. Sources are identified by content-addressed IDs (hash of initial content) so duplicate imports deduplicate. After destructive edits, the source's working version diverges; the registry tracks both the original and the current edited form.

Each source has a peak cache: pre-computed min/max pairs at multiple zoom levels, also stored in OPFS. Generated on import, updated on edit-list flatten.

### Edit list

See data model document for full semantics. Engine exposes:

- `apply_op(source_id, op)` — append operation to source's edit list, advance current state.
- `undo(source_id)` / `redo(source_id)` — move pointer in edit list.
- `flatten(source_id)` — render current state to a new base file in OPFS, truncate edit list.
- `query_samples(source_id, range)` — return float32 buffer for the requested sample range, computed by replaying ops from the most recent flatten.

### Serialisation

JSON ↔ in-memory model is the canonical pair. DSL emit and parse go through the JSON form (in-memory → JSON → DSL on emit; DSL → JSON → in-memory on parse). DSL parsing uses a hand-written recursive descent parser for clear error messages.

Versioning: every JSON document has `format_version`. Migration functions handle upgrades. Downgrade is not supported.

## Module: DSP (Rust)

All audio processing. Pure functions over float32 buffers where possible. Stateful processors (compressor, reverb, delay) implement a common trait:

```rust
trait Processor {
    fn reset(&mut self);
    fn set_param(&mut self, id: ParamId, value: f32);
    fn process(&mut self, input: &[f32], output: &mut [f32]);
    fn latency_samples(&self) -> usize;
}
```

This trait is what makes the WAM future possible — the shape matches AudioWorklet semantics.

### STFT machinery

Centralised. Used by spectral view, noise reduction, time stretch, pitch shift. Built on RustFFT.

```rust
struct Stft {
    fft_size: usize,
    hop_size: usize,
    window: Vec<f32>,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
}
```

Frame size and hop are configurable per call but a project-wide default is set at project creation (2048 / 512 with Hann is the default). Spectral edits are only invertible if analysis parameters are consistent across the edit, so the project default is what spectral selection edits use.

### Effect catalogue

Each effect is a Rust struct implementing `Processor`. Compiled into the engine. No dynamic loading in v1.

### Offline render

Mixdown and preview. Walks the multitrack timeline, queries source samples, applies clip parameters and envelopes, sums into bus buffers, applies track inserts and automation, outputs to a destination buffer. Used for both file export and preview rendering when realtime processing can't keep up.

## Module: Storage

OPFS abstraction. Engine sees a typed API:

```rust
trait Storage {
    fn create_source(&mut self, channel_count: u16) -> SourceHandle;
    fn append_samples(&mut self, h: &SourceHandle, samples: &[f32]) -> Result<()>;
    fn read_samples(&self, h: &SourceHandle, range: Range<usize>) -> Result<Vec<f32>>;
    fn truncate(&mut self, h: &SourceHandle, len: usize) -> Result<()>;
    // ...
}
```

Two implementations:

- **OpfsStorage** for the browser. Uses `FileSystemSyncAccessHandle` (only available in Workers). Stores sources as raw float32 blobs, peak caches as separate files, undo log as append-only journal.
- **NativeStorage** for tests. Uses ordinary filesystem. Same API.

Sources are stored chunked (e.g. 1 MB chunks) so reads of a sub-range don't pull the whole file. Peak caches are stored at multiple zoom levels (1:1, 1:64, 1:4096) for fast waveform rendering.

Undo log is append-only. Each entry is a serialised operation. Periodic flatten writes a new base and truncates the log.

OPFS quota management: engine tracks total usage, exposes it to UI for the status bar, and handles quota-exceeded errors by suggesting flatten or project export.

## Module: Audio I/O

Lives in TypeScript because AudioWorklet must. Two worklets:

### Playback worklet

Receives float32 frames from the engine via a SharedArrayBuffer ring buffer. The engine fills the buffer ahead of playback; the worklet consumes from it at the AudioContext's rate. If the engine and project sample rates differ from the AudioContext rate, the engine resamples before writing.

The worklet itself does no DSP — it only pulls samples and outputs them. All effect processing happens in the engine. This keeps the worklet's render quantum (128 samples, no allocation) simple and reliable.

Underrun handling: if the engine can't fill the buffer in time, the worklet outputs zeros and reports the underrun via a separate event channel. UI shows a glitch indicator in the status bar.

Metering: engine writes peak/RMS values for the master bus (and any soloed track) into a separate small SAB. UI reads on every frame for meter display.

### Recording worklet

Mirror of playback. Receives float32 from the AudioContext (input device), writes to SAB. Engine reads from SAB, writes to OPFS source file. Levels written to a metering SAB for the input meter.

Sample rate conversion happens in the engine after reading from SAB, so the worklet stays simple.

## Module: Rendering

TypeScript, main thread, WebGL2.

### Waveform renderer

Reads peak data from the engine (via message-passing on viewport changes, cached on the UI side per-source). Renders min/max bars per pixel column using a single draw call with an instanced quad. Selection overlay is a separate translucent quad.

Zoom levels match the engine's peak cache levels. Sub-pixel zoom interpolates between cached levels.

### Spectrogram renderer

Tile-based. Each tile is a fixed-size texture (e.g. 256×256) covering a fixed time-frequency region. Tiles are computed on demand by the engine (STFT magnitude → log-mag → colour-mapped to a single-channel texture or RGBA texture), uploaded to GPU, cached LRU on the UI side.

A fragment shader applies the user's selected colour map and dB range at draw time, so changing the visualisation parameters is free (no recomputation, just a uniform update).

### Meters and other widgets

Plain Canvas2D for level meters, phase scope, spectrum analyzer overlay. WebGL only where data volume demands it.

## Module: UI

TypeScript. Framework choice TBD (React, Solid, or Svelte all viable). Owns:

- Window layout, panel docking, floating dialogs.
- Transport controls, mixer view, track headers.
- Effect dialogs (each effect has a custom UI definition; common patterns abstracted).
- Keyboard shortcut handling (Cool Edit's `[` and `]` for selection, `F-keys` for transport, etc. — definitive list in feature spec).
- File dialogs (File System Access API on Chromium).
- Project browser.
- Settings.

UI never touches audio data directly. Every audio operation goes through engine commands.

## Threading and concurrency

- **Main thread:** UI, rendering, input handling.
- **Worker thread:** engine. All DSP, all project state, all OPFS access.
- **AudioWorklet thread:** playback and recording worklets. Communicate with engine via SAB ring buffers.
- **Render thread (browser-managed):** WebGL command buffer.

The engine being in a Worker means the engine API is asynchronous from the UI's perspective. Commands go in via `postMessage`, events come out the same way. SAB is reserved for tight-loop data (samples, meters, parameter automation during playback).

## Build and tooling

- **Engine:** Rust crate, `wasm-bindgen` + `wasm-pack` for the Wasm target, plain `cargo test` for native tests.
- **UI:** Vite + TypeScript. Engine imported as a Wasm module.
- **Testing:** unit tests in Rust for engine and DSP; Playwright for UI integration tests on Chromium.
- **CI:** build engine for both targets, run native tests, run Playwright suite, produce a deployable web bundle.

## Performance budget

Indicative numbers, refined in implementation:

- **Waveform render:** 60 fps at any zoom, including drag-scroll. Achieved by peak cache.
- **Spectrogram render:** 60 fps when scrolling existing tiles; new tiles computed in engine asynchronously, displayed as ready.
- **Playback:** zero underruns at typical project complexity (8 tracks, 2 inserts each, on 4070-class hardware running Vivaldi). AudioWorklet buffer of ~50 ms.
- **Edit responsiveness:** sample-region operations on 10-minute selections complete in under 200 ms.
- **Spectral edit:** time-frequency selection edit on 10-second region completes in under 500 ms.

These are targets, not contracts. Profile early and often.

## Risks and mitigations

1. **Long-session OPFS pressure.** Mitigation: chunked sources, automatic flatten, quota monitoring, clear UI for cleanup.
2. **AudioWorklet underruns under load.** Mitigation: large enough ring buffer, engine pre-renders ahead of playhead, glitch indicator, ability to fall back to offline render if realtime can't keep up.
3. **Spectral edit data model complexity.** Mitigation: edits bake on apply for v1 (no re-editable spectral layer). Re-editable deferred to v2.
4. **Time/pitch quality.** Mitigation: phase vocoder for v1, accept its limitations, document them. Rubber Band as a future option.
5. **Browser API drift.** Mitigation: Chromium-only commitment, periodic verification on the user's actual Vivaldi version.
