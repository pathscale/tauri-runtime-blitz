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
  `05-implementation.md`, `06-debug-control.md`, `07-css-conformance.md`.

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

## Current task: Stage 3

Stage 1 passed on 2026-08-09. Do not rerun or reinterpret it as Tauri runtime success. The
measured result and commands are in `docs/05-implementation.md`. The relevant local Blitz
commits are:

- `8402481a` -- pin `pathscale/stylo-less-py@8f39d56b`, removing Python from normal builds.
- `c8363173` -- document event propagation, template content, system fonts, Solid probe, and
  CPU PNG evidence.
- `80881334` -- loopback debug-control transport, UI-thread DOM adapter, CPU screenshot, and a
  passing separate-process Solid control test.
- `17b2350f` -- synchronous/asynchronous remote JavaScript and ordered console/error capture,
  including external retrieval of a deliberate Boa exception and stack.
- `a79b9ba7` -- pointer-path traces through the document, remote text input through the real
  IME path, and the empty-input initialization fix exposed by that test.
- `30ffb7b9` -- complete standard inspection/actions surface and a passing full Stage 1.5
  separate-process acceptance test.

The pushed Stylo fork is <https://github.com/pathscale/stylo-less-py>. It is based on the exact
Stylo revision Blitz previously pinned, so the change does not include unrelated CSS updates.

Stages 1.5 and 2 passed on 2026-08-09; measured results are in `docs/06-debug-control.md` and
`docs/07-css-conformance.md`. Begin the full-bundle headless Stage 3 gate from
`docs/05-implementation.md`. Do not begin the Tauri runtime crate before Stage 3 passes.

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
