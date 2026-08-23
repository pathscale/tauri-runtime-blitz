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
a real `#[tauri::command]` `greet` response through that queue and observes it in the Boa DOM.
`prepare_pending_webview` now applies Tauri initialization scripts, attaches IPC, constructs the
dispatcher, and installs queue draining and wakeups into the native Blitz document poll cycle. The
crate now implements the concrete Tauri runtime, handle, event proxy, and window dispatcher. Tauri's
main `PendingWindow` path prepares its attached webview and registers the Boa document with a native
Blitz window. A signed AgencyZero preview builds through `tauri::Builder<BlitzRuntime>` with a
visible real-command IPC probe. Standalone child-webview creation remains unsupported.

## Docs

| Doc | Contents |
|---|---|
| `docs/01-architecture.md` | Repo map, crate boundaries, what we own vs consume |
| `docs/02-plan.md` | Staged plan, gates, kill criteria |
| `docs/03-gaps.md` | Measured DOM/CSS gaps between AgencyZero and Blitz+Boa |
| `docs/04-risks.md` | What makes this fail, and the honest cost |
| `docs/05-implementation.md` | Concrete commands and assertions for the first gates |
| `docs/06-debug-control.md` | Reliable WebDriver-compatible control and diagnostics channel |
| `docs/08-runtime-debug-settings.md` | Shared two-setting runtime gate for AgencyZero, Chuzz, and other embedders |
| `docs/07-css-conformance.md` | Chromium/Blitz screenshot diffs and bounded visual gaps |

## CI and releases

Every pull request runs formatting, strict Clippy, the workspace's all-feature
tests serially, and protocol packaging. Runtime debug tests are serial because
their enablement and sampling state is process-global by design.

Crates publish only from a `v<workspace-version>` tag or a manually dispatched
`Publish crates` workflow with the exact workspace version. The workflow uses
the repository's `CARGO_REGISTRY_TOKEN`, publishes `blitz-control-protocol`
first, waits until that version resolves from the registry index, then packages
and publishes `tauri-runtime-blitz`. A Cargo.toml version edit alone never
publishes anything.
