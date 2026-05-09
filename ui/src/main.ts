// Tab shell: boots the engine, owns the shared Playback, mounts both tabs.
// The visual chrome (topbar + transport bar) follows the redesign in
// kool-edit-design-docs / Kooledit.zip — phosphor palette, big timecode,
// L/R master meters at the top; status LED + filename + kbd hints at the
// bottom. Editor and arranger panel internals are still the older layout
// pending later slices.

import "./styles.css";
import { EngineClient, EngineUnavailable } from "./engine/client";
import { Playback } from "./audio/playback";
import { mountEditor } from "./editor";
import { mountArranger } from "./arranger";
import { mountDrums } from "./drums";

type TabId = "editor" | "arranger" | "drums";

async function main(): Promise<void> {
  const root = document.querySelector<HTMLDivElement>("#app");
  if (!root) throw new Error("#app element missing from index.html");

  root.className = "app";
  root.innerHTML = "";

  // ---- topbar ---------------------------------------------------------

  const topbar = document.createElement("header");
  topbar.className = "topbar";

  // Brand
  const brand = document.createElement("div");
  brand.className = "brand";
  const brandMark = document.createElement("div");
  brandMark.className = "brand-mark";
  brand.appendChild(brandMark);
  const brandText = document.createElement("div");
  brandText.className = "brand-text";
  const brandName = document.createElement("div");
  brandName.className = "brand-name";
  brandName.innerHTML = `KOOL<span class="accent">·</span>EDIT`;
  const brandMeta = document.createElement("div");
  brandMeta.className = "brand-meta";
  brandMeta.textContent = "booting…";
  brandText.appendChild(brandName);
  brandText.appendChild(brandMeta);
  brand.appendChild(brandText);
  topbar.appendChild(brand);

  // Tabs (segmented)
  const tabs = document.createElement("div");
  tabs.className = "tabs";
  const editorTab = makeTab("EDITOR");
  const arrangerTab = makeTab("ARRANGER");
  const drumsTab = makeTab("DRUMS");
  tabs.appendChild(editorTab);
  tabs.appendChild(arrangerTab);
  tabs.appendChild(drumsTab);
  topbar.appendChild(tabs);

  // Timecode
  const timecode = document.createElement("div");
  timecode.className = "timecode";
  const timecodeDisplay = document.createElement("div");
  timecodeDisplay.className = "timecode-display";
  timecodeDisplay.innerHTML = `00:00:00<span class="ms">.000</span>`;
  const timecodeLabel = document.createElement("div");
  timecodeLabel.className = "timecode-label";
  timecodeLabel.textContent = "TIMECODE";
  timecode.appendChild(timecodeDisplay);
  timecode.appendChild(timecodeLabel);
  topbar.appendChild(timecode);

  // Master meters (visual stub for now — real levels wiring is later)
  const meters = document.createElement("div");
  meters.className = "meters";
  const meterRows = [makeMeterRow("L"), makeMeterRow("R")];
  meters.appendChild(meterRows[0]!.row);
  meters.appendChild(meterRows[1]!.row);
  topbar.appendChild(meters);

  // Right-side tools
  const tools = document.createElement("div");
  tools.className = "topbar-tools";
  const saveBtn = document.createElement("button");
  saveBtn.className = "btn";
  saveBtn.type = "button";
  saveBtn.textContent = "Save .kepz";
  const loadBtn = document.createElement("button");
  loadBtn.className = "btn";
  loadBtn.type = "button";
  loadBtn.textContent = "Load .kepz";
  // Hidden <input type="file"> driven by the Load button so the topbar
  // doesn't have to host a raw file picker.
  const loadInput = document.createElement("input");
  loadInput.type = "file";
  loadInput.accept = ".kepz,application/zip";
  loadInput.style.display = "none";
  loadBtn.addEventListener("click", () => loadInput.click());
  const settingsBtn = document.createElement("button");
  settingsBtn.className = "btn icon-only";
  settingsBtn.type = "button";
  settingsBtn.title = "Settings (coming soon)";
  settingsBtn.textContent = "⚙";
  settingsBtn.disabled = true;
  tools.appendChild(saveBtn);
  tools.appendChild(loadBtn);
  tools.appendChild(loadInput);
  tools.appendChild(settingsBtn);
  topbar.appendChild(tools);

  root.appendChild(topbar);

  // ---- workspace (editor / arranger) ----------------------------------

  const workspace = document.createElement("main");
  workspace.className = "workspace";
  const editorRoot = document.createElement("div");
  const arrangerRoot = document.createElement("div");
  const drumsRoot = document.createElement("div");
  arrangerRoot.style.display = "none";
  drumsRoot.style.display = "none";
  // flex:1 + minHeight:0 lets the editor/arranger fill the workspace and
  // shrink below content height — the inner library/source list relies
  // on this cascade to scroll instead of pushing chrome off the bottom.
  editorRoot.style.flex = "1 1 auto";
  arrangerRoot.style.flex = "1 1 auto";
  drumsRoot.style.flex = "1 1 auto";
  editorRoot.style.minHeight = "0";
  arrangerRoot.style.minHeight = "0";
  drumsRoot.style.minHeight = "0";
  workspace.appendChild(editorRoot);
  workspace.appendChild(arrangerRoot);
  workspace.appendChild(drumsRoot);
  root.appendChild(workspace);

  // ---- transport bar --------------------------------------------------

  const transportBar = document.createElement("footer");
  transportBar.className = "transport-bar";
  const statusLine = document.createElement("div");
  statusLine.className = "status-line";
  const statusLed = document.createElement("span");
  statusLed.className = "led idle";
  const statusState = document.createElement("span");
  statusState.textContent = "STOPPED";
  const statusFile = document.createElement("span");
  statusFile.textContent = "no project loaded";
  const statusFmt = document.createElement("span");
  statusFmt.textContent = "format_version=1";
  const statusContext = document.createElement("span");
  statusContext.textContent = "";
  statusLine.appendChild(statusLed);
  statusLine.appendChild(statusState);
  statusLine.appendChild(span("·"));
  statusLine.appendChild(statusFile);
  statusLine.appendChild(span("·"));
  statusLine.appendChild(statusFmt);
  statusLine.appendChild(span("·"));
  statusLine.appendChild(statusContext);
  transportBar.appendChild(statusLine);

  const kbdHints = document.createElement("span");
  kbdHints.className = "hint";
  kbdHints.innerHTML = `<span class="kbd">SPACE</span> play/pause &nbsp;
    <span class="kbd">L</span> loop &nbsp;
    <span class="kbd">⌘Z</span> undo`;
  transportBar.appendChild(kbdHints);
  root.appendChild(transportBar);

  // ---- engine boot ---------------------------------------------------

  let client: EngineClient;
  try {
    client = await EngineClient.boot();
  } catch (e) {
    if (e instanceof EngineUnavailable) {
      brandMeta.textContent = `engine unavailable: ${e.message}`;
    } else {
      brandMeta.textContent = `boot failed: ${String(e)}`;
    }
    return;
  }
  // banner format: "kool-edit-engine vX.Y.Z (format_version=1)"
  const banner = await client.banner();
  brandMeta.textContent = banner.replace(/^kool-edit-engine /, "");
  const fmtMatch = banner.match(/format_version=(\d+)/);
  if (fmtMatch) statusFmt.textContent = `format_version=${fmtMatch[1]}`;

  const playback = new Playback(client);

  const editor = await mountEditor(editorRoot, client, playback, {
    onSourceImported: () => {
      void arranger.refresh();
      void drums.refresh();
    },
  });
  const arranger = await mountArranger(arrangerRoot, client, {
    onSourceImported: () => {
      void editor.refreshLibrary();
      void drums.refresh();
    },
  });
  const drums = await mountDrums(drumsRoot, client, {
    onArrangerNeedsRefresh: () => {
      void arranger.refresh();
    },
  });

  // ---- save / load wiring --------------------------------------------

  saveBtn.addEventListener("click", async () => {
    statusContext.textContent = "exporting…";
    try {
      const bytes = await client.exportKepz();
      const blob = new Blob([bytes as BlobPart], { type: "application/zip" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      const stamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
      const filename = `kool-edit-${stamp}.kepz`;
      a.download = filename;
      document.body.appendChild(a);
      a.click();
      document.body.removeChild(a);
      URL.revokeObjectURL(url);
      statusFile.textContent = filename;
      statusContext.textContent = `saved (${bytes.byteLength.toLocaleString()} bytes)`;
    } catch (err) {
      statusContext.textContent = `save failed: ${String(err)}`;
    }
  });

  loadInput.addEventListener("change", async () => {
    const file = loadInput.files?.[0];
    if (!file) return;
    statusContext.textContent = `loading ${file.name}…`;
    try {
      const buf = await file.arrayBuffer();
      await client.importKepz(new Uint8Array(buf));
      await editor.reset();
      await arranger.refresh();
      await drums.refresh();
      statusFile.textContent = file.name;
      statusContext.textContent = "loaded";
    } catch (err) {
      statusContext.textContent = `load failed: ${String(err)}`;
    } finally {
      loadInput.value = "";
    }
  });

  // ---- tab wiring ----------------------------------------------------

  const showTab = (tab: TabId): void => {
    editorRoot.style.display = tab === "editor" ? "flex" : "none";
    arrangerRoot.style.display = tab === "arranger" ? "flex" : "none";
    drumsRoot.style.display = tab === "drums" ? "flex" : "none";
    editorTab.classList.toggle("active", tab === "editor");
    arrangerTab.classList.toggle("active", tab === "arranger");
    drumsTab.classList.toggle("active", tab === "drums");
    if (tab === "arranger") void arranger.refresh();
    if (tab === "drums") void drums.refresh();
  };
  editorTab.addEventListener("click", () => showTab("editor"));
  arrangerTab.addEventListener("click", () => showTab("arranger"));
  drumsTab.addEventListener("click", () => showTab("drums"));
  showTab("editor");

  // ---- timecode + status loop ----------------------------------------

  const refreshChrome = (): void => {
    const playing = playback.isPlaying() && !playback.isPaused();
    statusLed.classList.toggle("idle", !playing);
    statusState.textContent = playing
      ? playback.isLooping()
        ? "LOOPING"
        : "PLAYING"
      : "STOPPED";

    // Time = source-frame cursor / output-sample-rate (seconds elapsed in
    // playback). When idle, snap to 0.
    const pos = playback.position();
    const outSr = playback.outputSampleRate();
    let seconds = 0;
    if (playing && pos && outSr) {
      seconds = pos.sourceFrame / outSr;
    }
    timecodeDisplay.innerHTML = formatTimecode(seconds);
  };
  // 30 fps is enough for the timecode display; cheap enough that we run
  // it always rather than gating on playback state.
  setInterval(refreshChrome, 33);
  refreshChrome();
}

function makeTab(label: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "tab";
  const dot = document.createElement("span");
  dot.className = "dot";
  b.appendChild(dot);
  const txt = document.createElement("span");
  txt.textContent = label;
  b.appendChild(txt);
  return b;
}

function makeMeterRow(label: string): {
  row: HTMLDivElement;
  fill: HTMLDivElement;
  peak: HTMLDivElement;
  db: HTMLDivElement;
} {
  const row = document.createElement("div");
  row.className = "meter-row";
  const lab = document.createElement("div");
  lab.className = "meter-label";
  lab.textContent = label;
  const bar = document.createElement("div");
  bar.className = "meter-bar";
  const fill = document.createElement("div");
  fill.className = "meter-fill";
  const peak = document.createElement("div");
  peak.className = "meter-peak";
  bar.appendChild(fill);
  bar.appendChild(peak);
  const db = document.createElement("div");
  db.className = "meter-db";
  db.textContent = "−∞";
  row.appendChild(lab);
  row.appendChild(bar);
  row.appendChild(db);
  return { row, fill, peak, db };
}

function span(text: string): HTMLSpanElement {
  const s = document.createElement("span");
  s.textContent = text;
  return s;
}

/** HH:MM:SS.mmm with the milliseconds in a dimmer span so the design's
 *  two-tone treatment lands without the caller having to do the slicing. */
function formatTimecode(totalSeconds: number): string {
  const safe = Number.isFinite(totalSeconds) ? Math.max(0, totalSeconds) : 0;
  const h = Math.floor(safe / 3600);
  const m = Math.floor((safe % 3600) / 60);
  const s = Math.floor(safe % 60);
  const ms = Math.floor((safe - Math.floor(safe)) * 1000);
  const pad2 = (n: number): string => n.toString().padStart(2, "0");
  const pad3 = (n: number): string => n.toString().padStart(3, "0");
  return `${pad2(h)}:${pad2(m)}:${pad2(s)}<span class="ms">.${pad3(ms)}</span>`;
}

main().catch((err) => {
  console.error(err);
  const app = document.querySelector<HTMLDivElement>("#app");
  if (app) app.textContent = `boot failed: ${String(err)}`;
});
