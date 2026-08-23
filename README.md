# tauri-runtime-blitz

A Tauri v2 `Runtime` implementation backed by [Blitz](https://github.com/DioxusLabs/blitz)
(pure-Rust HTML/CSS engine) and [Boa](https://github.com/boa-dev/boa) (pure-Rust JS engine),
replacing `tauri-runtime-wry` and with it the OS webview.

**Goal:** run existing Tauri web frontends unchanged on a renderer with no C++ and no JIT.

```
today:   Tauri shell -> tauri-runtime-wry -> WKWebView (C++ WebKit + JSC JIT) -> Solid app
target:  Tauri shell -> tauri-runtime-blitz -> Blitz + Boa (Rust)             -> Solid app
```

Tauri keeps windowing, native menu, updater, dialog plugins, packaging, signing, and application
`#[tauri::command]` handlers. Only the webview swaps.

## Status

The runtime boots production SolidJS bundles under Boa, preserves Tauri window configuration,
strips the published trait crate's unused WebKit linkage, and forwards Boa's
`window.ipc.postMessage` into Tauri's existing IPC handler. Its webview dispatcher now queues
Tauri response scripts and callback evaluations onto Boa's document thread. A headless test passes
a real `#[tauri::command]` `greet` response through that queue and observes it in the Boa DOM.
`prepare_pending_webview` now applies Tauri initialization scripts, attaches IPC, constructs the
dispatcher, and installs queue draining and wakeups into the native Blitz document poll cycle. The
crate now implements the concrete Tauri runtime, handle, event proxy, and window dispatcher. Tauri's
main `PendingWindow` path prepares its attached webview and registers the Boa document with a native
Blitz window. Standalone child-webview creation remains unsupported.

## Docs

| Doc | Contents |
|---|---|
| `docs/06-debug-control.md` | Reliable WebDriver-compatible control and diagnostics channel |
| `docs/07-endpoint-debug-protocol.md` | MCP endpoint and semantic action contract |
| `docs/08-runtime-debug-settings.md` | Shared runtime debug-setting contract for embedders |

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
