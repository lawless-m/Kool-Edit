# Kool-Edit

Browser-based audio editor in the spirit of Cool Edit Pro 2. Destructive
waveform editor + multitrack sequencer with spectral view and editing.

Design documents live in [`kool-edit-design-docs/kool-edit-docs/`](kool-edit-design-docs/kool-edit-docs/).
Start with `CONTENTS.md`.

## Layout

```
.
├── engine/                Rust crate. Native (cargo test) and Wasm (browser) targets.
├── ui/                    TypeScript + Vite. Loads the engine in a Worker.
│   └── src/engine/        Worker, client, and protocol for the engine bridge.
├── kool-edit-design-docs/ Reference docs.
├── Cargo.toml             Workspace root.
└── Makefile               Convenience targets.
```

## Prerequisites

- Rust (stable). `rustup target add wasm32-unknown-unknown` for browser builds.
- Node 20+.
- [`wasm-pack`](https://rustwasm.github.io/wasm-pack/installer/) for browser builds.

## Common tasks

| Task                       | Command                          |
|----------------------------|----------------------------------|
| Run engine tests (native)  | `cargo test` (or `make test`)    |
| Build engine for browser   | `make engine`                    |
| Install + build UI bundle  | `make ui`                        |
| Dev server (engine + UI)   | `make engine && make dev`        |
| Full production build      | `make build`                     |
| Smoke-test the wasm bridge | `make smoke-wasm`                |
| Clean all artifacts        | `make clean`                     |

The dev server sets the COOP/COEP headers required for `SharedArrayBuffer`,
which the engine ↔ AudioWorklet path will need (see `02-architecture.md`).

## Status

First vertical slice: a user can pick a WAV file in the browser and the engine
(Rust → wasm, in a Worker) decodes it, builds a peak cache, and the UI draws
the waveform on a canvas. Playback isn't wired yet (AudioWorklet + ring
buffers come next).

Verified:
- `cargo test` — 44 tests, covers the data model and the WAV/peak/engine
  modules.
- `make smoke-wasm` — exercises the wasm-bindgen surface end-to-end from
  Node: build engine, decode a synthetic WAV, query peaks.
- `npm run build` — production bundle includes both the JS shim and the
  emitted `.wasm`.
