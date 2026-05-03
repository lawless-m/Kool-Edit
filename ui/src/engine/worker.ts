/// <reference lib="webworker" />
import type { EngineCommand, EngineEvent } from "./protocol";

const ctx = self as unknown as DedicatedWorkerGlobalScope;

// `kool_edit_engine.js` is produced by `wasm-pack build engine --target web
// --out-dir ../ui/src/engine/pkg --features wasm`. Until it has been built
// the dynamic import fails; we surface a clear message rather than a bare
// ModuleNotFound. The path is built dynamically so neither TS nor Vite try
// to statically resolve a file that doesn't exist yet.
type WasmEngine = {
  importWav: (name: string, bytes: Uint8Array, nowIso: string) => string;
  peakSummary: (sourceId: string, columns: number) => Float32Array | undefined;
  sourceFrameCount: (sourceId: string) => bigint | undefined;
};

type EngineModule = {
  default: (input?: unknown) => Promise<unknown>;
  banner: () => string;
  WasmEngine: new (sampleRate: number) => WasmEngine;
};

async function loadWasm(): Promise<EngineModule | { error: string }> {
  // Both URLs use string literals so Vite recognises them as asset references
  // and emits both files in the production bundle. The dynamic `import()` is
  // wrapped in a try/catch because the JS bundle only exists once
  // `wasm-pack build` has run.
  const jsUrl = new URL("./pkg/kool_edit_engine.js", import.meta.url).href;
  const wasmUrl = new URL("./pkg/kool_edit_engine_bg.wasm", import.meta.url).href;
  try {
    const mod = (await import(/* @vite-ignore */ jsUrl)) as EngineModule;
    await mod.default(wasmUrl);
    return mod;
  } catch {
    return {
      error:
        "wasm not built. run `make engine` (or `npm run build:engine` from ui/), then reload.",
    };
  }
}

function send(ev: EngineEvent): void {
  ctx.postMessage(ev);
}

(async () => {
  const loaded = await loadWasm();
  if ("error" in loaded) {
    send({ kind: "fatal", reason: loaded.error });
    return;
  }

  const wasm = loaded;
  const engine = new wasm.WasmEngine(96_000);

  ctx.onmessage = (ev: MessageEvent<EngineCommand>) => {
    const cmd = ev.data;
    try {
      switch (cmd.kind) {
        case "banner":
          send({ kind: "banner", req: cmd.req, banner: wasm.banner() });
          return;

        case "import_wav": {
          const sourceId = engine.importWav(cmd.name, cmd.bytes, cmd.nowIso);
          const framesBig = engine.sourceFrameCount(sourceId);
          const frames = framesBig === undefined ? 0 : Number(framesBig);
          send({ kind: "import_wav_ok", req: cmd.req, sourceId, frames });
          return;
        }

        case "peak_summary": {
          const peaks = engine.peakSummary(cmd.sourceId, cmd.columns);
          if (!peaks) {
            send({ kind: "error", req: cmd.req, reason: `unknown source ${cmd.sourceId}` });
            return;
          }
          send({ kind: "peak_summary_ok", req: cmd.req, peaks });
          return;
        }
      }
    } catch (e) {
      send({ kind: "error", req: cmd.req, reason: String(e) });
    }
  };

  send({ kind: "ready" });
})();
