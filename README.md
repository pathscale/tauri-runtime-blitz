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

Stage 1 passed on 2026-08-09: AgencyZero-shaped Solid output runs reactively on Boa,
document-delegated events fire, and the result renders through the CPU backend. Stage 1.5
(reliable external debug control) is next. No Tauri runtime exists yet; see `docs/02-plan.md`
for the remaining gates.

## Docs

| Doc | Contents |
|---|---|
| `docs/01-architecture.md` | Repo map, crate boundaries, what we own vs consume |
| `docs/02-plan.md` | Staged plan, gates, kill criteria |
| `docs/03-gaps.md` | Measured DOM/CSS gaps between AgencyZero and Blitz+Boa |
| `docs/04-risks.md` | What makes this fail, and the honest cost |
| `docs/05-implementation.md` | Concrete commands and assertions for the first gates |
| `docs/06-debug-control.md` | Reliable WebDriver-compatible control and diagnostics channel |
