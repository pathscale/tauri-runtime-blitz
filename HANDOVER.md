# Handover prompt

Copy everything below the line into a new session.

---

I am evaluating whether AgencyZero (a Tauri v2 desktop app) can render its existing SolidJS UI
on Blitz (pure-Rust HTML/CSS engine) plus Boa (pure-Rust JS engine), replacing the macOS
WKWebView. Driver is a zero-C++ policy on memory-safety grounds: WKWebView is C++ WebKit with
a JIT.

## Repos on disk

- `~/code/agencyzero` -- the app. Rust backend (27k LOC) + SolidJS frontend (21k LOC) at
  `apps/gui/frontend`. Tauri 2.11.5, tauri-runtime 2.11.3, tauri-runtime-wry 2.11.4.
- `~/code/blitz-rust` -- fork of DioxusLabs/blitz, **on branch `js-engine`** (head of PR #491,
  which adds `packages/blitz-script`: Boa-based JS execution). Workspace version
  0.3.0-beta.1, MSRV 1.91.
- `~/code/tauri-runtime-blitz` -- planning repo. **Read all of `docs/` before doing
  anything.** `01-architecture.md`, `02-plan.md`, `03-gaps.md`, `04-risks.md`,
  `05-implementation.md`.

## Decided, do not relitigate

- **Blitz + Boa**, not Blitz-native. Blitz-native means Dioxus RSX (Rust components), which is
  not Solid and would fork the UI into two codebases. The UI must work across web and desktop
  from one source.
- **Solid to Rust transpilation was considered and set aside.** The reactivity model maps well
  to Leptos, but Leptos has no Blitz renderer, and faithful TS-to-Rust needs a dynamic runtime
  for `createStore`'s Proxy usage -- which is Boa again.
- **No Tauri fork.** `tauri-runtime` is a published crate defining the traits;
  `tauri-runtime-wry` is one implementation and we write a sibling.
- **CPU rasterisation** (`anyrender_vello_cpu`), not wgpu -- wgpu dlopens C++ GPU drivers at
  runtime, which defeats the policy.
- Repos needed: 1 new (`tauri-runtime-blitz`), 1 fork (`blitz-rust`). Plus changes to
  `agencyzero` and `@pathscale/ui`, both owned by us.

## Current task: Stage 1

Follow `docs/05-implementation.md` section "Stage 1". In short:

1. `cargo build -p blitz-script` in `~/code/blitz-rust` (long first compile).
2. Run the **existing** examples under `packages/blitz-script/examples/` and
   `examples/preact/` first. Read `examples/preact/*.md` -- the maintainer's own DOM API
   inventory.
3. Build a ~50-line Solid probe (signal, effect, `<Show>`, `<For>`, click handler) with
   rsbuild + the Solid babel plugin, so it compiles to the same template form AgencyZero uses.
4. Run it under `blitz-script`, render to PNG via `anyrender_vello_cpu`.

**Gate:** does Solid's fine-grained reactivity drive `blitz-dom`, and do delegated events
bubble? Solid attaches listeners at the document root, so **event delegation is the highest
risk item** -- test it explicitly and separately from direct listeners.

Pass -> Stage 2 (CSS conformance, section in the same doc).
Fail on reactivity or delegation -> **stop and report.** That is architectural; it would mean
rewriting `blitz-script` rather than extending it.

## Constraints

- Ask before installing anything or downloading beyond `cargo build` of the existing
  workspaces.
- No `git worktree`. Local branches only.
- Commit messages: no em-dashes (a hook rejects them), no `Co-Authored-By: Claude` trailer.
- Do not modify `~/code/agencyzero` during Stages 1-2. It is a shipping app; this is a spike.

## Known gaps to expect (already measured, see `03-gaps.md`)

DOM APIs AgencyZero needs that `blitz-script` lacks: `setPointerCapture`
(`features/tabs/reorder.ts`), `document.getSelection()` (`lib/clipboard.ts`),
`scrollIntoView`, `matchMedia`. `ResizeObserver` is missing but already `typeof`-guarded in
the app.

CSS at risk, all in code we own: 335 `color-mix()`, 43 `oklch()`, 14 `rgb(from ...)`, 71
`@property`, 228 `mask-image` icons from `@iconify/tailwind4` inside `@pathscale/ui`.

## Honesty requirements

- `blitz-script` is an **unreviewed AI-generated draft**; its own author says it "probably
  isn't mergeable in this form" and JS is in Blitz's backlog. Assume we maintain the fork
  forever. Do not present it as a supported upstream feature.
- Report what actually happens, including failures, with the real output. Do not smooth over a
  partial result.
- If a stage fails its gate, say so plainly and stop rather than working around it. The point
  of the gates is to kill this cheaply if it does not work.
