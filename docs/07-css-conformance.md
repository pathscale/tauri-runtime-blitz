# CSS conformance results

Stage 2 started on 2026-08-09 after the debug-control gate passed.

## Current corpus inventory

The existing AgencyZero build at
`apps/gui/dist/static/css/index.3981681f.css` contains:

| Feature | Compiled CSS occurrences | Design artifact occurrences |
|---|---:|---:|
| `color-mix()` | 407 | 18 |
| `oklch()` | 43 | 334 |
| relative `rgb(from …)` | 21 | 0 |
| `@property` | 71 | 0 |
| `mask-image` | 168 | 0 |

These are literal occurrence counts, not unique declarations. They supersede the older counts
in `03-gaps.md` for the current build artifact.

## Probe status

`tools/css-probe` builds and renders with `PYTHON3` deliberately invalid. Its representative
fixture covers all five high-risk features plus the `@supports (color: color-mix(...))` branch.
The first 640×240 CPU render produced the expected layout and visible results for `oklch`,
`color-mix`, relative color, and masking. Its self-diff reported zero changed pixels and zero
mean absolute error.

This is preliminary engine evidence, not a conformance pass. The connected browser surface was
unavailable, so the settled `design/workspace.html` DOM and Chrome reference screenshot could
not yet be captured. Once available, capture the browser DOM/styles and 1344×900 screenshot,
render that settled input with the probe, then retain the actual and heatmap with the numerical
diff report.
