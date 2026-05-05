// Tab shell: boots the engine, owns the shared Playback, mounts both tabs.
// Each tab's DOM is built once and toggled with `display:none` so state
// persists across switches.

import { EngineClient, EngineUnavailable } from "./engine/client";
import { Playback } from "./audio/playback";
import { mountEditor, btnStyle } from "./editor";
import { mountArranger } from "./arranger";

type TabId = "editor" | "arranger";

async function main(): Promise<void> {
  const root = document.querySelector<HTMLDivElement>("#app");
  if (!root) throw new Error("#app element missing from index.html");

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

  const banner = document.createElement("div");
  banner.textContent = "booting…";
  banner.style.fontSize = "12px";
  banner.style.color = "#9a9a9a";
  banner.style.fontFamily = "ui-monospace, monospace";
  root.appendChild(banner);

  let client: EngineClient;
  try {
    client = await EngineClient.boot();
  } catch (e) {
    if (e instanceof EngineUnavailable) {
      banner.textContent = `engine unavailable: ${e.message}`;
    } else {
      banner.textContent = `boot failed: ${String(e)}`;
    }
    return;
  }
  banner.textContent = await client.banner();

  const playback = new Playback(client);

  // ---- tab bar ----
  const tabBar = document.createElement("div");
  Object.assign(tabBar.style, {
    display: "flex",
    gap: "4px",
    borderBottom: "1px solid #2a2a2a",
  } satisfies Partial<CSSStyleDeclaration>);
  const editorTab = makeTabBtn("Editor");
  const arrangerTab = makeTabBtn("Arranger");
  tabBar.appendChild(editorTab);
  tabBar.appendChild(arrangerTab);
  root.appendChild(tabBar);

  // ---- content hosts ----
  const editorRoot = document.createElement("div");
  const arrangerRoot = document.createElement("div");
  arrangerRoot.style.display = "none";
  root.appendChild(editorRoot);
  root.appendChild(arrangerRoot);

  await mountEditor(editorRoot, client, playback, {
    onSourceImported: () => arranger.refresh(),
  });
  const arranger = await mountArranger(arrangerRoot, client);

  const showTab = (tab: TabId): void => {
    editorRoot.style.display = tab === "editor" ? "" : "none";
    arrangerRoot.style.display = tab === "arranger" ? "" : "none";
    setActiveStyle(editorTab, tab === "editor");
    setActiveStyle(arrangerTab, tab === "arranger");
    if (tab === "arranger") arranger.refresh();
  };

  editorTab.addEventListener("click", () => showTab("editor"));
  arrangerTab.addEventListener("click", () => showTab("arranger"));
  showTab("editor");
}

function makeTabBtn(label: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  Object.assign(b.style, btnStyle(), {
    borderBottom: "none",
    borderRadius: "0",
  } satisfies Partial<CSSStyleDeclaration>);
  return b;
}

function setActiveStyle(b: HTMLButtonElement, active: boolean): void {
  b.style.background = active ? "#3a3a3a" : "#2a2a2a";
  b.style.color = active ? "#ffffff" : "#d8d8d8";
}

main().catch((err) => {
  console.error(err);
  const app = document.querySelector<HTMLDivElement>("#app");
  if (app) app.textContent = `boot failed: ${String(err)}`;
});
