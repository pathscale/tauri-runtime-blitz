# tauri-runtime-blitz

A Tauri v2 `Runtime` implementation backed by [Blitz](https://github.com/DioxusLabs/blitz)
(pure-Rust HTML/CSS engine) and [Boa](https://github.com/boa-dev/boa) (pure-Rust JS engine),
replacing `tauri-runtime-wry` and with it the OS webview.

**Goal:** run AgencyZero's existing SolidJS UI, unchanged, on a renderer with no C++ and no JIT.

```
today:   Tauri shell -> tauri-runtime-wry -> WKWebView (C++ WebKit + JSC JIT) -> Solid app
target:  Tauri shell -> tauri-runtime-blitz -> Blitz + Boa (Rust)             -> Solid app
```

Tauri keeps windowing, native menu, updater, dialog plugin, packaging, signing, and all 85
`#[tauri::command]`s. Only the webview swaps.

## Status

Stages 1 through 3 passed on 2026-08-09. The real AgencyZero production bundle boots and is
interactive under Boa, and a signed 32 MB native CPU-rendered preview launches from the isolated
`agencyzero-blitz` fork. Stage 4 is active: the runtime crate preserves AgencyZero's window
configuration, strips the published trait crate's unused WebKit linkage, and forwards Boa's
`window.ipc.postMessage` into Tauri's existing IPC handler. Its webview dispatcher now queues
Tauri response scripts and callback evaluations onto Boa's document thread. A headless test passes
a real `#[tauri::command]` `greet` response through that queue and observes it in the Boa DOM. The
next gate is constructing this dispatcher from Tauri's pending webview and draining it from the
native event loop.

## Docs

| Doc | Contents |
|---|---|
| `docs/01-architecture.md` | Repo map, crate boundaries, what we own vs consume |
| `docs/02-plan.md` | Staged plan, gates, kill criteria |
| `docs/03-gaps.md` | Measured DOM/CSS gaps between AgencyZero and Blitz+Boa |
| `docs/04-risks.md` | What makes this fail, and the honest cost |
| `docs/05-implementation.md` | Concrete commands and assertions for the first gates |
| `docs/06-debug-control.md` | Reliable WebDriver-compatible control and diagnostics channel |
| `docs/07-css-conformance.md` | Chromium/Blitz screenshot diffs and bounded visual gaps |
