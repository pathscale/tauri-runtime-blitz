# Debug control contract

The Blitz port must be externally controllable and diagnosable before CSS and full-app work.
This is how an automated development agent observes the real renderer when the UI itself is
broken. It is a gate, not optional developer convenience.

Do not implement Chrome DevTools Protocol. CDP is a large Chromium-internal surface and Boa
does not provide a compatible remote inspector. Use the stable W3C WebDriver command model
for common automation and a small `blitz:*` extension for renderer diagnostics.

## Process boundary

```text
WebDriver client
      |
      | loopback HTTP + session token
      v
debug-control server thread
      |
      | serialized request/reply channel
      v
UI/runtime thread -> Boa -> DOM/events -> style/layout -> CPU paint
```

The server starts before application JavaScript loads and remains reachable after script
exceptions. It must never access Boa, DOM, layout, or paint state from the server thread.
Each command is sent to the UI/runtime thread and receives a structured reply with a bounded
timeout. Start with one active session so commands cannot race.

Compile the server only behind a dedicated `debug-control` Cargo feature. Enabling the feature
does not open a port by itself; startup also requires an explicit environment setting. Bind
only to `127.0.0.1` (and optionally `::1` later), never to an unspecified interface.

## Discovery and authentication

The launcher supplies:

- `TAURI_BLITZ_DRIVER=127.0.0.1:0` -- port zero asks the OS for a free port.
- `TAURI_BLITZ_DRIVER_DESCRIPTOR=<path>` -- where the app atomically writes connection data.

The descriptor is owner-readable only where the platform supports file modes:

```json
{
  "pid": 12345,
  "address": "127.0.0.1:53177",
  "token": "<random 256-bit value>",
  "protocolVersion": 1,
  "renderer": "blitz",
  "rendererRevision": "<git revision>"
}
```

Write the descriptor only after `GET /status` can succeed. Remove it on clean shutdown. A
stale descriptor is rejected when its PID or health check does not match. `POST /session`
requires the token as the `blitz:token` capability. `/status` returns liveness and protocol
version but no application data.

This endpoint can evaluate arbitrary JavaScript. Never enable it in a normal production
build, log its token, put its token in command-line arguments, or expose it beyond loopback.

## Minimum WebDriver surface

Implement W3C response envelopes and error names so ordinary WebDriver clients can use the
endpoint. Unsupported commands must return `unsupported operation`, not a successful empty
response.

Minimum commands:

- status; create and delete session
- current URL and page source
- find one or many elements by CSS selector
- element text, attribute, property, rectangle, displayed state, and enabled state
- element click, focus, and keyboard input
- pointer and keyboard actions needed by AgencyZero
- synchronous and asynchronous JavaScript execution
- screenshot
- window handles, current window, and window selection

Element references contain both the Blitz node ID and a document generation. Removing or
replacing the node makes the reference stale. Never silently retarget a stale reference to a
new node that reused the same numeric ID.

Clicks and keys must enter through Blitz's input/event path. A semantic click is addressed by
node id and dispatches pointer-down, mouse-down, pointer-up, mouse-up and click to that target;
it neither calls JavaScript `element.click()` nor asks coordinate hit-testing to choose the node
again. That preserves propagation bugs while making overflowed controls addressable.

## Blitz diagnostic extensions

The first implementation needs these vendor-prefixed commands; the HTTP routes may follow
the normal `/session/{session id}/blitz/...` shape.

| Command | Result |
|---|---|
| `blitz:waitForIdle` | Drain runnable work and commit a frame; return revision counters |
| `blitz:getDomSnapshot` | Node IDs, tree, text, attributes, and document revision |
| `blitz:getLayoutTree` | Node IDs, boxes, clipping, scroll offsets, and layout revision |
| `blitz:getComputedStyle` | Computed values for one node and style revision |
| `blitz:getConsoleEntries` | Ordered console records after a supplied sequence number |
| `blitz:getRuntimeErrors` | Ordered uncaught JS and host errors with stacks |
| `blitz:traceEvent` | Dispatch path, phases, targets, listeners, and propagation outcome |
| `blitz:getRendererMetrics` | Queue sizes, revisions, frame timing, and last paint status |

Console entries and runtime errors live in bounded ring buffers with monotonically increasing
sequence numbers. Report overflow explicitly so a client knows records were lost. JavaScript
errors include source name, line/column when available, Boa backtrace, and the DOM and paint
revisions at which the error escaped.

Polling these ordered buffers is sufficient for the first gate. Do not implement WebDriver
BiDi or a custom WebSocket until a real need for pushed events appears.

## Settled-frame barrier

`blitz:waitForIdle` is the reliability primitive. A successful response means:

1. The triggering command has completed.
2. Immediately runnable Boa promise jobs and microtasks have drained.
3. Currently due timers have run.
4. One animation-frame turn has run.
5. Resulting style and layout work has completed.
6. A CPU paint has committed.
7. No new synchronous mutation appeared during the final observation turn.

Do not wait for future timers or require repeating timers to disappear. Apply both a time
budget and work-count budget. If the document keeps mutating or a queue does not drain,
return a timeout with queue sizes and the last revisions rather than claiming the page is
idle.

Every successful response carries the relevant counters:

```json
{
  "documentRevision": 42,
  "styleRevision": 19,
  "layoutRevision": 19,
  "paintRevision": 19
}
```

A screenshot and its associated DOM/layout snapshot must be captured behind the same barrier
and identify the same committed paint revision. This prevents a client from comparing pixels
from one frame with semantic state from another.

## Stage 1.5 acceptance gate

Use the Stage 1 Solid probe as the fixture. From a separate process:

1. Launch with a temporary descriptor path and an OS-selected port.
2. Wait for the descriptor, validate its PID, and call `/status`.
3. Verify a missing or incorrect token cannot create a session.
4. Create a session and confirm advertised capabilities and protocol version.
5. Find the counter button with a CSS selector and read its initial text and rectangle.
6. Click it through the real input path, call `blitz:waitForIdle`, and verify the signal,
   effect log, DOM snapshot, and screenshot all agree.
7. Exercise `<Show>` and `<For>` and verify delegated event trace includes `document`.
8. Trigger a deliberate JavaScript exception and retrieve its message and stack.
9. Delete the session, reconnect, and repeat a read-only command.
10. Terminate the app and verify the descriptor is removed or detected as stale.

Pass only when the test is repeatable without sleeps. Timeouts wait on explicit readiness,
revision, and queue conditions; fixed delays are not an acceptable synchronization strategy.

Failure means stop and repair this channel before Stage 2. Without it, later renderer and
runtime failures cannot be investigated reliably by an external agent.

### Measured result (2026-08-09)

**PASS.** Blitz commits `80881334`, `17b2350f`, `a79b9ba7`, and `30ffb7b9` implement and
verify the transport boundary, runtime diagnostics, input tracing, standard commands, and the
complete separate-process acceptance gate:

- loopback-only HTTP, random 256-bit token, atomic 0600 descriptor, bounded request size and
  response timeout, one serialized renderer queue, and clean shutdown;
- W3C status/session envelopes, token rejection, session deletion, and reconnect;
- current URL, page source, CSS element lookup, text, rectangle, pointer-path click, DOM
  snapshot, idle/layout barrier, and CPU PNG screenshot;
- process-unique node identities so a reused slab index cannot silently retarget an old element
  reference;
- a separate-process Solid test driven entirely by descriptor readiness and protocol replies,
  with no fixed sleeps;
- synchronous and timer-backed asynchronous JavaScript execution, bounded ordered console
  records, and bounded ordered uncaught-error records with a retained Boa stack;
- pointer hit-test tracing with the full node chain through the document, plus focused text
  entry through Blitz's IME/input event path;
- attributes, properties, displayed/enabled state, focus, window selection, computed style,
  layout tree, renderer metrics, viewport pointer actions, and keyboard actions;
- full Solid transitions through signals, effect logs, `<Show>`, and `<For>`, with DOM,
  layout, style, paint, and changed PNG evidence correlated after explicit idle barriers;
- detached reference rejection, deliberate exception retrieval, session reconnect, clean
  process shutdown, and descriptor removal.

The test uses no fixed sleeps. It passed with Python unavailable, alongside all existing DOM,
Preact, Solid, transport, and doctests. This remains a headless Blitz result, not a Tauri
runtime result, but Stage 2 may now begin.

## Growth after the gate

Keep the Stage 1.5 surface small. Add network inspection, CSS rule provenance, accessibility
trees, memory/GC statistics, pushed events, or interactive JavaScript breakpoints only in
response to a demonstrated debugging need. Protocol additions are versioned capabilities so
old clients fail clearly rather than misreading new responses.
