# Endpoint agent-control and diagnostics protocol

One endpoint-libs transport carries two deliberately separate protocol planes.
Each length-delimited payload is JSON-RPC 2.0 using the small MCP wire layer in
endpoint-libs 2.1.5. WebDriver remains a temporary adapter until the existing
acceptance coverage runs through the new agent-control plane.

## Agent control plane

This plane is for an AI agent or automation client operating the product:

- a semantic tree keyed by stable node ids, roles, accessible names, values,
  state, visibility, and bounds;
- click, set-value, scroll-into-view, and native key/pointer/wheel actions;
- window/app lifecycle including quit and restart with automatic reconnect;
- pushed tree/lifecycle changes for efficient observation.

Agent control is the runtime's default `agent-control` feature. On macOS it uses
a Unix-domain socket rather than localhost HTTP, so the app can leave it
available without a browser-reachable port, token ceremony, or exclusive
automation session. Calls use the standard MCP `tools/call` method with the
`blitz.agent.control` tool; pushed state uses JSON-RPC notifications.

Use W3C WebDriver/BiDi input semantics and WAI-ARIA/accessibility-tree semantics
where they fit. MCP may expose discovery and coarse application tools, while
Prompt Syntax remains the authored intent/execution layer. Neither MCP nor
Prompt Syntax replaces native input or semantic UI state.

## Diagnostics plane

This plane is for debugging the implementation rather than operating the app:

- DOM/layout/computed-style snapshots behind settled revision barriers;
- console and runtime-error streams;
- renderer queue, invalidation, frame-stage, memory, and revision metrics;
- debug screenshots tied to the same committed revision as their snapshots.

Diagnostics collection and handlers are compiled only with the `diagnostics`
feature and exposed as the separate `blitz.diagnostics` tool. Production agent
control does not retain debug logs, DOM snapshots, or renderer telemetry.

## Contract

- The local agent-control listener is part of the default runtime feature.
- Diagnostics are absent unless the `diagnostics` feature is compiled and
  explicitly enabled at launch.
- Local transport only. There is no authentication or exclusive session.
- A mode-0600 descriptor publishes the address, PID, instance id, protocol
  version, renderer, and renderer revision.
- Set `TAURI_BLITZ_CONTROL_DESCRIPTOR` to a stable path when a launcher wants to
  watch one descriptor across relaunches. Without it, descriptors are published
  under the per-user temporary `tauri-blitz-agent` discovery directory.
- Multiple clients may observe concurrently. Mutating requests are serialized
  on the UI thread.
- Relaunch emits the replacement instance id before shutdown. Clients watch the
  descriptor and reconnect without owner involvement.
- Native input carries logical key, physical code, modifiers, pointer phases,
  and wheel phases; it enters the same runtime path as device input.
- Snapshots and screenshots are tied to one settled revision set.
- Metrics name real stages: input-to-present, style, layout, scene, submit,
  present, total, queue depth, coalesced invalidations, and resident bytes.

## Delivery stages

1. Typed MCP-compatible calls, results, notifications, and endpoint-libs
   framing, with no token or session handshake. Implemented.
2. Local Unix-socket listener, mode-0600 descriptor lifecycle, MCP initialize
   and tool discovery, and multi-client connections. Implemented.
3. UI-thread agent and diagnostics handler boundaries. Implemented. The agent
   plane now extracts a semantic tree with ancestor-aware visibility and drives
   click, set-value, scroll-into-view, physical key, pointer, wheel, and modifier
   input through `ScriptDocument`'s native event path. Relaunch uses LaunchServices
   for a macOS app bundle (and the current executable elsewhere), then exits the
   old instance cleanly. An embedder that registers an agent-control handler owns
   the relaunch lifecycle instead; this lets AgencyZero drain state and delegate
   replacement to its restart Angel. Diagnostic idle/snapshot handlers remain to
   be connected.
4. Renderer instrumentation and pushed metrics/errors.
5. WebDriver compatibility adapter backed by the endpoint protocol; delete the
   old bespoke server after its acceptance suite passes unchanged.

The live AgencyZero audit that defined this contract found the old layer could
not inject Command-modified keys or wheel input, treated descendants of
`display:none` retained views as displayed, exposed revision counters without
real timings, allowed only one session, and lost its environment during an
AppKit/LaunchServices relaunch. Each is an acceptance case for this protocol.
