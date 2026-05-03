/// <reference lib="webworker" />
import type { EngineCommand, EngineEvent } from "./protocol";

const ctx = self as unknown as DedicatedWorkerGlobalScope;

// The wasm package is produced by `wasm-pack build engine --target web --out-dir ../ui/src/engine/pkg`.
// Until it has been built the import will fail at runtime; we surface a clear
// error message rather than a bare ModuleNotFound.
type EngineWasm = {
  default: (input?: unknown) => Promise<unknown>;
  banner: () => string;
};

async function loadWasm(): Promise<EngineWasm | { error: string }> {
  // The path is built dynamically so neither TS nor Vite try to statically
  // resolve a file that doesn't exist until `wasm-pack build` has run.
  const url = new URL("./pkg/kool_edit_engine.js", import.meta.url).href;
  try {
    const mod = (await import(/* @vite-ignore */ url)) as EngineWasm;
    await mod.default();
    return mod;
  } catch {
    return {
      error:
        "wasm not built. run `npm run build:engine` from the repo root, then reload.",
    };
  }
}

function send(ev: EngineEvent): void {
  ctx.postMessage(ev);
}

(async () => {
  const wasm = await loadWasm();
  if ("error" in wasm) {
    send({ kind: "ready" });
    ctx.onmessage = () => send({ kind: "error", reason: wasm.error });
    return;
  }

  ctx.onmessage = (ev: MessageEvent<EngineCommand>) => {
    const cmd = ev.data;
    switch (cmd.kind) {
      case "banner":
        send({ kind: "banner", banner: wasm.banner() });
        return;
    }
  };

  send({ kind: "ready" });
})();
