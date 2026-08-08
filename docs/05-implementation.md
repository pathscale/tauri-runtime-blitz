# Implementation guide

Concrete steps for Stages 1, 1.5, and 2. Stage 1 was run on 2026-08-09; later-stage API names
are still from reading source and must be verified against the actual crates.

## Prerequisites

```sh
rustc --version    # must be >= 1.91.0 (blitz-rust workspace rust-version)
```

Repos, already in place:

- `~/code/blitz-rust` -- fork of DioxusLabs/blitz, on branch `js-engine` (PR #491 head)
- `~/code/agencyzero` -- the app
- `~/code/tauri-runtime-blitz` -- this repo

## Stage 1 -- Solid on Boa

### 1.1 Build the workspace

```sh
cd ~/code/blitz-rust
cargo build -p blitz-script 2>&1 | tail -40
```

Expect a long first compile: Stylo, Taffy, Parley, ICU4X, Boa. If Boa's `icu_normalizer`
conflicts with parley's, the branch's `Cargo.toml` already git-pins Boa to rev `8a1e8fe0` to
resolve it -- do not "fix" that pin.

### 1.2 Run the existing examples first

Do not write new code until the shipped examples work.

```sh
cd ~/code/blitz-rust
cargo test -p blitz-script --test dom
cargo test -p blitz-script --test preact
```

There is no `packages/blitz-script/examples/` directory at the checked-out PR head. The
executable Preact baseline is `packages/blitz-script/tests/preact.rs`; it loads the standalone
assets under `examples/preact/`.

Read `examples/preact/preact_dom_apis.md`, `react_dom_apis.md`, and `wpt_dom_apis.md` before
writing probes. They are the maintainer's own inventory of what is and is not covered.

Also read `packages/blitz-script/tests/` for the intended usage shape.

### 1.3 The Solid probe

Build a minimal Solid app the same way AgencyZero builds its frontend (rsbuild + Solid babel
plugin), so the output uses the same compiled-template form:

```tsx
import { render } from "solid-js/web";
import { createSignal, createEffect, For, Show } from "solid-js";

function App() {
  const [n, setN] = createSignal(0);
  const [items, setItems] = createSignal(["a", "b"]);
  createEffect(() => console.log("effect:", n()));
  return (
    <div class="root">
      <button onClick={() => setN(n() + 1)}>inc</button>
      <span>{n()}</span>
      <Show when={n() > 2}><p>over</p></Show>
      <For each={items()}>{(i) => <li>{i}</li>}</For>
      <button onClick={() => setItems([...items(), "c"])}>add</button>
    </div>
  );
}
render(() => <App />, document.getElementById("app")!);
```

Wrap the bundle in an HTML shell with `<div id="app">` and a `<script>` tag, then load it
through `blitz-script`'s document API (from the crate docs: `ScriptDocument::from_html(html,
config)` then `execute_scripts()` -- verify the real signature).

### 1.4 What to assert

Each of these is a separate signal, and they can fail independently:

| Check | Exercises |
|---|---|
| Initial render shows `0`, `a`, `b` | `template()` + `cloneNode` + `insert` |
| Clicking `inc` updates the span | signal -> DOM write path |
| `console.log` fires from the effect | `createEffect` scheduling / microtasks |
| `<Show>` appears past 2 | conditional insert/remove |
| Clicking `add` appends `c` | `<For>` reconciliation, `insertBefore` |
| Click handler fires at all | **event delegation** -- Solid attaches at the document root and relies on bubbling |

Event delegation is the one most likely to break and the most important. If direct listeners
work but delegated ones do not, check `bubbles` / `target` / `currentTarget` propagation in
`packages/blitz-script/src/dom/event.rs`.

Render to PNG with `anyrender_vello_cpu` to inspect output without a window.

### 1.5 Gate

Pass -> Stage 1.5. Fail on reactivity or delegation -> stop and report; that is
architectural, not a gap to fill.

### 1.6 Measured result (2026-08-09)

**PASS.** The initial source-level risks were real but bounded:

- `blitz-script` built successfully.
- Existing DOM tests passed (17 initially, 19 after the new regressions) and both Preact tests
  passed.
- Document delegation initially failed because `BaseDocument::node_chain` omitted the
  document node. Appending it made Solid's root `click` listener reachable without changing
  element bubbling behavior.
- Solid's compiled template path initially failed because `HTMLTemplateElement.content` was
  absent. Blitz already stored parsed template children on the template node; exposing that
  container unblocked clone-based templates.
- The AgencyZero-shaped Rsbuild/Babel/Solid probe passed initial render, signals,
  `createEffect`, `<Show>`, `<For>`, and delegated clicks.
- CPU rendering produced a 640x480 PNG with the final state (`3`, `effect:3`, `over`, and
  `a/b/c`). System fonts must be enabled; without `blitz-dom/system-fonts`, only the built-in
  bullet font renders.
- The full `blitz-script` suite passed with Python deliberately unavailable: 19 DOM tests, 2
  Preact tests, 1 Solid test, and 1 doctest.

Blitz commits: `8402481a` (Python-free Stylo pin) and `c8363173` (Solid DOM support and probe).
This is a headless renderer result, not a `tauri-runtime-blitz` result.

## Stage 1.5 -- Reliable debug control

Implement and pass the minimum control-plane gate in `06-debug-control.md` before Stage 2.
Do not substitute shell logs, one-off JavaScript hooks, or screenshots without correlated DOM
and render revisions. The endpoint must remain usable when application JavaScript throws.

## Stage 2 -- CSS conformance

### 2.1 Produce the corpus

```sh
cd ~/code/agencyzero/apps/gui/frontend
bun install && bun run build
ls ../dist/static/css/     # index.<hash>.css, ~144 KB
```

Two corpora:

1. `~/code/agencyzero/design/workspace.html` (214 KB) -- standalone design mockup, no build
   needed, easiest first target.
2. The real compiled CSS plus a DOM snapshot (dump `document.documentElement.outerHTML` from
   the running app in a browser).

### 2.2 Render and diff

Render each with `blitz-dom` + `anyrender_vello_cpu` to PNG. Screenshot the same input in
Chrome at identical viewport size. Diff.

**Do not eyeball this.** 150 rules are wrapped in
`@supports (color: color-mix(in lab, red, red))`, so an engine lacking `color-mix` silently
takes the fallback branch -- the page renders, in the wrong colours.

### 2.3 Report, per feature

Produce a table: feature, occurrences, renders correctly yes/no, owner. Priorities are
`color-mix()` (335), `oklch()` (43), `rgb(from ...)` relative colour (14), `@property` (71),
`mask-image` icons (228). See `03-gaps.md`.

The output is a work estimate for `@pathscale/ui` and `theme.css`, not a pass/fail. Both are
ours to change.

## Stage 3 notes

Run the real 542 KB bundle against `apps/gui/frontend/src/api/mock.ts` (1,037 lines of
fixtures). No Tauri, no IPC -- the UI already runs headless against these.

Profile `features/project/MessageBody.tsx` specifically. It parses markdown with regex on
every message render, and regex is Boa's single weakest operation (47 vs 3941 against jitless
V8 on the V8 benchmark suite).

## Stage 4 notes

Do not start until Stage 3 renders the real app. Order within Stage 4:

1. IPC bridge (unblocks everything else)
2. Windowing (winit vs tao)
3. Overlay title bar hit-testing
4. Menu passthrough

Reference implementation for the trait surface: `versotile-org/tauri-runtime-verso` (archived,
but it is the only prior art for a non-wry Tauri runtime). Pin an exact Tauri version;
`tauri-runtime`'s traits move between minor releases.
