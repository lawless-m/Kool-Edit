# Kool-Edit Design Documents

A browser-based audio editor in the spirit of Cool Edit Pro 2. Destructive waveform editor + multitrack sequencer with spectral view and editing.

## Where to start

If you're handing this to Claude Code (or anyone else picking up the project), read in this order:

1. **`01-feature-spec.md`** — what we're building, scope, what's in and out of v1, acceptance criteria. Read first to know the shape.
2. **`02-architecture.md`** — module structure, language and library choices, threading model, module responsibilities. Read second to know how it fits together.
3. **`03-data-model.md`** — sources, edit lists, multitrack project structure, persistence, invariants. Read third because the data model is the foundation everything else sits on.
4. **`04-dsl-grammar.md`** — the textual surface for projects and scripts. Read fourth; it depends on the data model.

## Document summaries

### 01-feature-spec.md
Product vision, target browser (Chromium-only), internal audio format (96 kHz float32), the two editing surfaces (destructive editor and multitrack sequencer), the bridging workflow (Cool Edit's double-click-to-edit-source model), spectral view, noise reduction, recording, project file format, visual identity, full effects list, explicit non-goals, acceptance test for v1.

### 02-architecture.md
Engine-in-Rust, UI-in-TypeScript split. Engine compiles to native (for tests) and Wasm (for the browser). Engine runs in a Worker, communicates with UI via postMessage and SharedArrayBuffer. Six top-level modules: Engine core, DSP, Storage (OPFS), Audio I/O (AudioWorklet), Rendering (WebGL2), UI. Performance budget, threading model, build tooling, risks.

### 03-data-model.md
Two layers: sources (audio data, edited via edit lists) and the multitrack project (declarative composition). Edit list semantics: append-only operations, history pointer for undo/redo, periodic flatten when the list grows. Source registry, peak caches, sample queries. Multitrack model: tracks, clips, envelopes, automation, "Make Unique" semantics. Constraints, invariants, JSON persistence.

### 04-dsl-grammar.md
Textual surface for projects (`.keds`, declarative) and scripts (`.keda`, imperative). Shared operation vocabulary. Lexical structure, time literals, dB literals, full reference of operations. Versioning tied to JSON `format_version`. Parser implementation notes (hand-written recursive descent in Rust). Future syntactic extensions kept in mind.

## Settled decisions (quick reference)

| Decision               | Choice                                                            |
|------------------------|-------------------------------------------------------------------|
| Working name           | Kool-Edit (public name TBD)                                       |
| Target browser         | Chromium-only (Vivaldi as primary dev browser)                    |
| Internal sample rate   | 96 kHz, float32                                                   |
| Engine language        | Rust → Wasm                                                       |
| UI language            | TypeScript                                                        |
| UI framework           | TBD (React, Solid, or Svelte all viable)                          |
| Storage                | OPFS (sync access handle in Worker)                               |
| Project file format    | JSON canonical, DSL as surface                                    |
| Project file extension | `.kep` (single), `.kepz` (portable zip)                           |
| Edit history model     | Edit list with periodic flatten                                   |
| Destructive/multitrack | Cool Edit model: clips reference sources, "Make Unique" splits    |
| Spectral edits         | Bake on apply (not RX-style re-editable)                          |
| Time/pitch algorithm   | Phase vocoder (v1); Rubber Band as future option                  |
| Plugin format          | Internal effects only; shaped to be WAM-compatible later          |
| Channels               | Mono and stereo only (no surround in v1)                          |
| MIDI                   | Out of scope for v1                                               |

## What's not in this bundle

- UI mockups or wireframes — not produced. Visual direction is described verbally in the feature spec.
- Detailed effect algorithms — described at the parameter level in the feature spec; algorithm choice is implementation work for the DSP module.
- Build configuration files (Cargo.toml, package.json, etc.) — produced when implementation starts.
- Specific UI framework choice — flagged as TBD; decide at start of UI implementation.
- Keyboard shortcut map — flagged in the feature spec as "Cool Edit's familiar shortcuts" but the definitive list is implementation work.
- Test plan — Playwright and Rust unit tests are mentioned in the architecture; specific test cases are implementation work.

## Open questions to resolve at implementation time

1. **UI framework.** React (largest ecosystem, biggest hire pool), Solid (better perf, smaller bundles), Svelte (compiler-based, less runtime). Worth a small spike before committing.
2. **Audio format support on import.** WAV and FLAC are easy via existing Rust crates. MP3 requires a decoder (symphonia handles it). Decide what import formats v1 supports beyond WAV.
3. **Default colour map for spectrogram.** The black-blue-magenta-yellow-white described in the spec is a starting point; worth side-by-siding alternatives (viridis, inferno, magma) on real audio before committing.
4. **WebGPU vs WebGL2.** Architecture says WebGL2 for v1. WebGPU is more capable and now widely available in Chromium; worth re-evaluating at implementation start.
5. **OPFS quota strategy.** What's a reasonable default? When do we warn the user? When do we refuse new operations? Empirical work needed once real session sizes are observable.
