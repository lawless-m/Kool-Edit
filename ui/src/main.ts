import { EngineClient, EngineUnavailable } from "./engine/client";
import { drawWaveform } from "./waveform";

async function main(): Promise<void> {
  const root = document.querySelector<HTMLDivElement>("#app");
  if (!root) throw new Error("#app element missing from index.html");

  const ui = buildUi(root);

  let client: EngineClient;
  try {
    client = await EngineClient.boot();
  } catch (e) {
    if (e instanceof EngineUnavailable) {
      ui.status.textContent = `engine unavailable: ${e.message}`;
    } else {
      ui.status.textContent = `boot failed: ${String(e)}`;
    }
    ui.fileInput.disabled = true;
    return;
  }

  ui.status.textContent = await client.banner();

  ui.fileInput.addEventListener("change", async () => {
    const file = ui.fileInput.files?.[0];
    if (!file) return;
    ui.status.textContent = `decoding ${file.name}…`;
    try {
      const buf = await file.arrayBuffer();
      const { sourceId, frames } = await client.importWav(
        file.name,
        new Uint8Array(buf),
      );
      const peaks = await client.peakSummary(sourceId, ui.canvas.width);
      drawWaveform(ui.canvas, peaks);
      ui.status.textContent = `${file.name} · ${sourceId} · ${frames.toLocaleString()} frames`;
    } catch (err) {
      ui.status.textContent = `import failed: ${String(err)}`;
    }
  });
}

function buildUi(root: HTMLElement): {
  fileInput: HTMLInputElement;
  canvas: HTMLCanvasElement;
  status: HTMLElement;
} {
  root.innerHTML = "";
  Object.assign(root.style, {
    display: "flex",
    flexDirection: "column",
    gap: "12px",
    fontFamily: "system-ui, sans-serif",
    background: "#1a1a1a",
    color: "#d8d8d8",
    padding: "16px",
    minHeight: "100vh",
    boxSizing: "border-box",
  } satisfies Partial<CSSStyleDeclaration>);

  const header = document.createElement("div");
  header.textContent = "Kool-Edit";
  header.style.fontSize = "18px";
  header.style.fontWeight = "600";
  root.appendChild(header);

  const fileRow = document.createElement("label");
  fileRow.style.display = "flex";
  fileRow.style.alignItems = "center";
  fileRow.style.gap = "8px";
  fileRow.textContent = "Open WAV: ";
  const fileInput = document.createElement("input");
  fileInput.type = "file";
  fileInput.accept = ".wav,audio/wav,audio/x-wav";
  fileRow.appendChild(fileInput);
  root.appendChild(fileRow);

  const canvas = document.createElement("canvas");
  canvas.width = 1024;
  canvas.height = 200;
  Object.assign(canvas.style, {
    width: "100%",
    height: "200px",
    background: "#0c0c0c",
    border: "1px solid #2a2a2a",
  } satisfies Partial<CSSStyleDeclaration>);
  root.appendChild(canvas);

  const status = document.createElement("div");
  status.textContent = "booting…";
  status.style.fontSize = "12px";
  status.style.color = "#9a9a9a";
  status.style.fontFamily = "ui-monospace, monospace";
  root.appendChild(status);

  return { fileInput, canvas, status };
}

main().catch((err) => {
  console.error(err);
  const app = document.querySelector<HTMLDivElement>("#app");
  if (app) app.textContent = `boot failed: ${String(err)}`;
});
