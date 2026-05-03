// Wire protocol between the UI (main thread) and the engine Worker.
// Every command carries a numeric request id; the matching event echoes it
// so the client can correlate replies even when commands are in flight.

export type RequestId = number;

export type EngineCommand =
  | { kind: "banner"; req: RequestId }
  | { kind: "import_wav"; req: RequestId; name: string; bytes: Uint8Array; nowIso: string }
  | { kind: "peak_summary"; req: RequestId; sourceId: string; columns: number };

export type EngineEvent =
  | { kind: "ready" }
  | { kind: "fatal"; reason: string }
  | { kind: "banner"; req: RequestId; banner: string }
  | { kind: "import_wav_ok"; req: RequestId; sourceId: string; frames: number }
  | { kind: "peak_summary_ok"; req: RequestId; peaks: Float32Array }
  | { kind: "error"; req: RequestId; reason: string };
