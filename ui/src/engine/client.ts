import type { EngineCommand, EngineEvent, RequestId } from "./protocol";

type Pending = (ev: EngineEvent) => void;

export class EngineUnavailable extends Error {}

/**
 * Promise-returning wrapper around the engine Worker. A single client owns
 * one worker; concurrent calls are correlated by request id.
 */
export class EngineClient {
  private worker: Worker;
  private nextReq: RequestId = 1;
  private pending = new Map<RequestId, Pending>();

  private constructor(worker: Worker) {
    this.worker = worker;
    worker.onmessage = (ev: MessageEvent<EngineEvent>) => this.dispatch(ev.data);
  }

  /** Boot a new engine worker, resolving once it has signalled `ready`. */
  static async boot(): Promise<EngineClient> {
    const worker = new Worker(new URL("./worker.ts", import.meta.url), {
      type: "module",
      name: "kool-edit-engine",
    });

    return new Promise<EngineClient>((resolve, reject) => {
      const onReady = (ev: MessageEvent<EngineEvent>) => {
        const msg = ev.data;
        if (msg.kind === "ready") {
          worker.removeEventListener("message", onReady);
          worker.removeEventListener("error", onError);
          resolve(new EngineClient(worker));
        } else if (msg.kind === "fatal") {
          worker.removeEventListener("message", onReady);
          worker.removeEventListener("error", onError);
          worker.terminate();
          reject(new EngineUnavailable(msg.reason));
        }
      };
      const onError = (e: ErrorEvent) => {
        worker.removeEventListener("message", onReady);
        worker.removeEventListener("error", onError);
        worker.terminate();
        reject(new EngineUnavailable(e.message || "worker error"));
      };
      worker.addEventListener("message", onReady);
      worker.addEventListener("error", onError);
    });
  }

  async banner(): Promise<string> {
    const ev = await this.request<EngineCommand & { kind: "banner" }>(
      (req) => ({ kind: "banner", req }),
    );
    if (ev.kind !== "banner") throw new Error("unexpected event for banner");
    return ev.banner;
  }

  async importWav(
    name: string,
    bytes: Uint8Array,
  ): Promise<{ sourceId: string; frames: number }> {
    const nowIso = new Date().toISOString();
    const ev = await this.request<EngineCommand & { kind: "import_wav" }>(
      (req) => ({ kind: "import_wav", req, name, bytes, nowIso }),
    );
    if (ev.kind !== "import_wav_ok") throw new Error("unexpected event");
    return { sourceId: ev.sourceId, frames: ev.frames };
  }

  async peakSummary(sourceId: string, columns: number): Promise<Float32Array> {
    const ev = await this.request<EngineCommand & { kind: "peak_summary" }>(
      (req) => ({ kind: "peak_summary", req, sourceId, columns }),
    );
    if (ev.kind !== "peak_summary_ok") throw new Error("unexpected event");
    return ev.peaks;
  }

  terminate(): void {
    this.worker.terminate();
    this.pending.clear();
  }

  private request<C extends EngineCommand>(build: (req: RequestId) => C): Promise<EngineEvent> {
    const req = this.nextReq++;
    const cmd = build(req);
    return new Promise<EngineEvent>((resolve, reject) => {
      this.pending.set(req, (ev) => {
        if (ev.kind === "error") reject(new Error(ev.reason));
        else resolve(ev);
      });
      this.worker.postMessage(cmd);
    });
  }

  private dispatch(msg: EngineEvent): void {
    if (msg.kind === "ready" || msg.kind === "fatal") return; // boot-time only
    const handler = this.pending.get(msg.req);
    if (!handler) {
      console.warn("engine: orphan reply", msg);
      return;
    }
    this.pending.delete(msg.req);
    handler(msg);
  }
}
