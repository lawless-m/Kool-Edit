// Wire protocol between the UI (main thread) and the engine Worker.
// Kept narrow on purpose; commands grow here as the engine surface grows.

export type EngineCommand = { kind: "banner" };

export type EngineEvent =
  | { kind: "ready" }
  | { kind: "banner"; banner: string }
  | { kind: "error"; reason: string };
