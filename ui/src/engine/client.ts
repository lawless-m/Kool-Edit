import type { EngineCommand, EngineEvent } from "./protocol";

export type BootResult =
  | { kind: "ok"; banner: string }
  | { kind: "error"; reason: string };

export async function bootEngine(): Promise<BootResult> {
  const worker = new Worker(new URL("./worker.ts", import.meta.url), {
    type: "module",
    name: "kool-edit-engine",
  });

  return new Promise<BootResult>((resolve) => {
    let settled = false;
    const settle = (r: BootResult) => {
      if (settled) return;
      settled = true;
      resolve(r);
    };

    worker.onerror = (e) => {
      settle({ kind: "error", reason: e.message || "worker error" });
      worker.terminate();
    };

    worker.onmessage = (ev: MessageEvent<EngineEvent>) => {
      const msg = ev.data;
      switch (msg.kind) {
        case "ready": {
          const cmd: EngineCommand = { kind: "banner" };
          worker.postMessage(cmd);
          return;
        }
        case "banner":
          settle({ kind: "ok", banner: msg.banner });
          return;
        case "error":
          settle({ kind: "error", reason: msg.reason });
          return;
      }
    };
  });
}
