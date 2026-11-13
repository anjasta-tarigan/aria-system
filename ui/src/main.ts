/// <reference types="vite/client" />
import * as initialApp from "./app";
import "katex/dist/katex.min.css";

// `?mock=1` runs the same UI against an in-browser fake of the Rust side, for
// demos and screenshots without a model. The real app always uses IPC.
const mock = new URLSearchParams(location.search).has("mock");

type AppModule = typeof import("./app");

// Keep the live app reachable across HMR re-executions of this entry module.
const store = globalThis as {
  __ariaApp?: AppModule;
  __ariaBoot?: number;
};

// Coalesce the duplicate update events Vite can emit for a single save so the
// app re-renders once, not twice.
let hotBoot: ReturnType<typeof setTimeout> | null = null;

const appWindow = window as unknown as { aria: unknown };

async function boot(load: () => Promise<AppModule>) {
  const seq = (store.__ariaBoot = (store.__ariaBoot ?? 0) + 1);
  const previous = store.__ariaApp;
  // Keep the reader on the same chat across a re-boot.
  const selected = previous?.testHooks.state().detail?.conversation.id ?? null;
  previous?.stop();

  const app = await load();
  // A newer boot (e.g. two quick saves) wins; drop this stale one.
  if (seq !== store.__ariaBoot) return;
  store.__ariaApp = app;

  if (mock) {
    app.useMock();
    appWindow.aria = app.testHooks;
  }
  await app.start(selected ?? undefined);
}

void boot(async () => initialApp);

if (import.meta.hot) {
  // Re-boot in place instead of letting Vite fall back to a full page reload.
  // Only app.ts is accepted as a dependency here: when it changes, Vite passes
  // the fresh module into the callback (mods[0]); we re-boot from it. Using the
  // callback's module avoids re-importing by URL, which would hit the browser's
  // HTTP cache for a stale ?t= token and serve old code. main.ts itself rarely
  // changes and falls back to a normal reload, which is fine.
  import.meta.hot.accept(["./app"], (mods) => {
    const next = mods[0] as AppModule | undefined;
    if (!next) return;
    if (hotBoot) clearTimeout(hotBoot);
    hotBoot = setTimeout(() => {
      hotBoot = null;
      void boot(() => Promise.resolve(next));
    }, 40);
  });
}
