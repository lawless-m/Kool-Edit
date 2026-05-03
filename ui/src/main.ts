import { bootEngine } from "./engine/client";

async function main(): Promise<void> {
  const app = document.querySelector<HTMLDivElement>("#app");
  if (!app) throw new Error("#app element missing from index.html");

  app.textContent = "Kool-Edit booting…";

  const result = await bootEngine();
  if (result.kind === "ok") {
    app.textContent = result.banner;
  } else {
    app.textContent = `engine unavailable: ${result.reason}`;
  }
}

main().catch((err) => {
  console.error(err);
  const app = document.querySelector<HTMLDivElement>("#app");
  if (app) app.textContent = `boot failed: ${String(err)}`;
});
