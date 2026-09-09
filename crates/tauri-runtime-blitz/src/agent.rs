//! Inspecting, capturing and driving a document, with no window involved.
//!
//! Split out of the runtime because none of it needs one. A headless
//! inspection host wants exactly these functions: it serves a socket, answers
//! `Inspect`, and activates nodes. It opens nothing.
//!
//! While they lived beside the Tauri runtime, depending on them meant
//! compiling Tauri, and on Linux that means GTK -- system libraries pulled in
//! to build a binary that never creates a window, and a crate that would not
//! compile there at all. The dependency edge was wrong, not the platform.

use std::collections::HashMap;

#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::{
    AgentSnapshot, DebugError, DebugResponse, KeyPhase, Modifiers as ControlModifiers, SemanticNode,
};
#[cfg(all(feature = "diagnostics", unix))]
use blitz_control_protocol::{
    DebugSnapshot, FrameMetrics, FrameWindowMetrics, LayoutBounds, LayoutDiagnosticRow,
    LayoutEdges, LayoutOffset, LayoutSize, RendererMetrics, RevisionSet, ScriptMetrics,
    ScriptSource, SnapshotCost, SnapshotRequest, TimingStats,
};
#[cfg(all(feature = "agent-control", unix))]
use blitz_dom::Document;
use blitz_script::ScriptDocument;
#[cfg(all(feature = "agent-control", unix))]
use blitz_traits::events::{
    BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, DomEvent, DomEventData, KeyState,
    MouseEventButton, MouseEventButtons, Point, PointerCoords, PointerDetails, UiEvent,
};
#[cfg(all(feature = "diagnostics", unix))]
use blitz_traits::node_id::NodeId;
#[cfg(all(feature = "agent-control", unix))]
use keyboard_types::{Code, Key, Location, Modifiers as KeyboardModifiers};

/// The live inspector's reusable offscreen surface.
///
/// A capture used to construct this whole renderer for every frame. Besides
/// reallocating the viewport-sized RGBA buffer, that threw away the CPU text
/// renderer's glyph resources, so a stability assertion shaped and rasterised
/// every label four times. The surface belongs to one runtime and is resized
/// only when the window or requested scale changes.
#[cfg(all(feature = "diagnostics", unix))]
pub(crate) struct CaptureSurface {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) renderer: anyrender_vello_cpu::VelloCpuImageRenderer,
    pub(crate) rgba: Vec<u8>,
}

/// Reusable offscreen renderer for captures of one document.
///
/// A headless inspection host asks for several adjacent frames when it checks
/// visual stability. Reusing this object preserves the CPU renderer's glyph
/// resources and pixel allocation between those requests instead of rebuilding
/// an entire renderer for every sample.
#[cfg(all(feature = "diagnostics", unix))]
pub struct DocumentCapture {
    surface: Option<CaptureSurface>,
}

#[cfg(all(feature = "diagnostics", unix))]
impl DocumentCapture {
    pub fn new() -> Self {
        Self { surface: None }
    }

    pub fn capture(
        &mut self,
        document: &mut ScriptDocument,
        request: blitz_control_protocol::CaptureRequest,
    ) -> Result<blitz_control_protocol::CapturedImage, DebugError> {
        capture_document_with_surface(document, request, &mut self.surface)
    }
}

#[cfg(all(feature = "diagnostics", unix))]
impl Default for DocumentCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(feature = "diagnostics", unix))]
impl CaptureSurface {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        use anyrender::ImageRenderer as _;

        Self {
            width,
            height,
            renderer: anyrender_vello_cpu::VelloCpuImageRenderer::new(width, height),
            rgba: Vec::with_capacity((width as usize) * (height as usize) * 4),
        }
    }

    pub(crate) fn size_to(&mut self, width: u32, height: u32) {
        use anyrender::ImageRenderer as _;

        if self.width == width && self.height == height {
            return;
        }
        self.renderer.resize(width, height);
        self.width = width;
        self.height = height;
    }
}

/// Draw a standalone script document through the same CPU paint path used by
/// runtime diagnostics.
///
/// Headless QA hosts intentionally have no `RuntimeApplication`, but they must
/// not substitute a second renderer for native visual checks. Keeping the
/// capture implementation here makes a host capture and a live-app capture
/// byte-for-byte comparable.
#[cfg(all(feature = "diagnostics", unix))]
pub fn capture_document(
    script_document: &mut ScriptDocument,
    request: blitz_control_protocol::CaptureRequest,
) -> Result<blitz_control_protocol::CapturedImage, DebugError> {
    DocumentCapture::new().capture(script_document, request)
}

#[cfg(all(feature = "diagnostics", unix))]
pub(crate) fn capture_document_with_surface(
    script_document: &mut ScriptDocument,
    request: blitz_control_protocol::CaptureRequest,
    surface: &mut Option<CaptureSurface>,
) -> Result<blitz_control_protocol::CapturedImage, DebugError> {
    use anyrender::ImageRenderer;
    use base64::Engine as _;

    // Clamped rather than trusted. A scale of zero produces a zero-sized
    // buffer and a negative one panics inside the rasteriser, and neither
    // should be reachable from a debug socket.
    let scale = if request.scale.is_finite() && request.scale > 0.0 {
        request.scale.clamp(0.1, 8.0)
    } else {
        1.0
    };

    let node_id = request.node_id;

    // Style and layout first, so the capture reflects pending mutations
    // rather than the frame before them. Same call `collect_diagnostics`
    // makes, for the same reason.
    script_document.inner_mut().resolve(0.0);

    // Copied out rather than held: the guard is a `Ref` and the borrow has
    // to end before the mutable one the paint below needs.
    let (full_width, full_height) = {
        let inner = script_document.inner();
        let viewport = inner.viewport();
        (viewport.window_size.0, viewport.window_size.1)
    };
    if full_width == 0 || full_height == 0 {
        return Err(debug_error(
            "captureUnavailable",
            "the document has no viewport to draw",
        ));
    }

    // The region to keep, in unscaled document pixels.
    let (crop_x, crop_y, crop_width, crop_height) = match node_id {
        None => (
            0.0_f64,
            0.0_f64,
            f64::from(full_width),
            f64::from(full_height),
        ),
        Some(id) => {
            let inner = script_document.inner();
            let node = inner
                .get_node(NodeId::from_u64(id))
                .ok_or_else(|| debug_error("unknownNode", &format!("no node {id}")))?;
            let layout = node.final_layout();
            let position = node.absolute_position(0.0, 0.0);
            if layout.size.width <= 0.0 || layout.size.height <= 0.0 {
                return Err(debug_error(
                    "captureEmpty",
                    &format!("node {id} has a zero-sized box, so there is nothing to capture"),
                ));
            }
            let box_ = (
                f64::from(position.x),
                f64::from(position.y),
                f64::from(layout.size.width),
                f64::from(layout.size.height),
            );
            drop(inner);
            box_
        }
    };

    let full_pixel_width = ((f64::from(full_width) * f64::from(scale)).round() as u32).max(1);
    let full_pixel_height = ((f64::from(full_height) * f64::from(scale)).round() as u32).max(1);
    // Clamp before painting: a node partly offscreen yields the visible part,
    // and the regional renderer never allocates pixels that will be discarded.
    let left = ((crop_x * f64::from(scale)).round().max(0.0) as u32).min(full_pixel_width);
    let top = ((crop_y * f64::from(scale)).round().max(0.0) as u32).min(full_pixel_height);
    let width = ((crop_width * f64::from(scale)).round() as u32)
        .min(full_pixel_width.saturating_sub(left))
        .max(1);
    let height = ((crop_height * f64::from(scale)).round() as u32)
        .min(full_pixel_height.saturating_sub(top))
        .max(1);
    // Leave room for the JSON-RPC and MCP envelopes inside the transport's
    // fixed frame ceiling. The old 64-million-pixel limit allowed a 256 MiB
    // raster and a 341 MiB base64 string, only for protocol encoding to reject
    // the result against its 16 MiB frame limit after all that work was done.
    const FRAME_ENVELOPE_RESERVE: usize = 64 * 1024;
    const MAX_BASE64_BYTES: usize =
        blitz_control_protocol::MAX_DEBUG_FRAME_BYTES - FRAME_ENVELOPE_RESERVE;
    const MAX_RAW_BYTES: usize = (MAX_BASE64_BYTES / 4) * 3;
    const MAX_PIXELS: u64 = (MAX_RAW_BYTES / 4) as u64;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(debug_error(
            "captureTooLarge",
            &format!(
                "{width}x{height} cannot fit in one diagnostic frame; capture a node or lower the scale"
            ),
        ));
    }

    let surface = surface.get_or_insert_with(|| CaptureSurface::new(width, height));
    surface.size_to(width, height);
    // `ImageRenderer` retains its scene between calls. A capture is a complete
    // frame, not an incremental paint, so carrying the previous command list
    // forward duplicates every shape and makes each sample slower than the
    // last. Keep reusable renderer resources, but always begin with an empty
    // scene.
    surface.renderer.reset();
    let mut document = script_document.inner_mut();
    surface.renderer.render_to_vec(
        |scene| {
            if node_id.is_some() {
                blitz_paint::paint_scene_region(
                    scene,
                    &mut document,
                    blitz_paint::PaintRegion::crop(
                        f64::from(scale),
                        f64::from(left) / f64::from(scale),
                        f64::from(top) / f64::from(scale),
                        width,
                        height,
                    ),
                );
            } else {
                blitz_paint::paint_scene(
                    scene,
                    &mut document,
                    f64::from(scale),
                    width,
                    height,
                    0,
                    0,
                );
            }
        },
        &mut surface.rgba,
    );

    Ok(blitz_control_protocol::CapturedImage {
        width,
        height,
        rgba_base64: base64::engine::general_purpose::STANDARD.encode(&surface.rgba),
        node_id,
    })
}

/// Collect the same typed diagnostic snapshot from a standalone Blitz document
/// that the windowed runtime exposes over its control socket.
///
/// Headless component hosts own a `ScriptDocument` without a Tauri event loop.
/// Keeping snapshot collection here gives those hosts the renderer's real DOM,
/// layout and computed paint data instead of a partial or reimplemented view.
#[cfg(all(feature = "diagnostics", unix))]
pub fn snapshot_document(
    document: &mut ScriptDocument,
    request: SnapshotRequest,
    revision: u64,
) -> Result<DebugSnapshot, DebugError> {
    let started = std::time::Instant::now();
    let poll_started = std::time::Instant::now();
    let mut polls = 0u64;
    for _ in 0..100 {
        polls += 1;
        if !document.poll(None) {
            break;
        }
    }
    let poll_ms = poll_started.elapsed().as_secs_f64() * 1_000.0;
    // This forces a style and layout pass so the snapshot reports current
    // geometry. It is work the observer caused, so it is reported as snapshot
    // cost, never as the cost of a frame the application drew.
    let resolve_started = std::time::Instant::now();
    document.inner_mut().resolve(0.0);
    let snapshot_resolve_ms = resolve_started.elapsed().as_secs_f64() * 1_000.0;
    let inner = document.inner();
    let layout_node_limit = inner.tree().iter().count();
    let active_element = inner.get_focussed_node_id().map(|id| id.as_u64());
    // Once for the whole snapshot: the question a control asks is "which label
    // points at me", and answering it from the control costs a document scan
    // each time.
    let labels = LabelIndex::build(&inner);
    let nodes: Vec<SemanticNode> = inner
        .tree()
        .iter()
        .filter_map(|(id, node)| {
            if !request.node_ids.is_empty() && !request.node_ids.contains(&id.as_u64()) {
                return None;
            }
            let element = node.element_data()?;
            if !dom_chain_is_attached(&inner, id, layout_node_limit)
                || !layout_chain_is_valid(&inner, id, layout_node_limit)
            {
                return None;
            }
            let rect = inner.get_client_bounding_rect(id);
            let visible = node_is_visible(&inner, id)
                && rect
                    .as_ref()
                    .is_some_and(|rect| rect.width > 0.0 && rect.height > 0.0);
            let role = semantic_role(element);
            let value = if role == "generic" {
                Some(
                    element
                        .attrs()
                        .iter()
                        .map(|attribute| format!("{}={}", attribute.name.local, attribute.value))
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            } else {
                semantic_value(element)
            };
            Some(SemanticNode {
                dom_id: element_attr(element, "id").map(str::to_owned),
                id: id.as_u64(),
                parent: semantic_parent(&inner, id, None).map(|id| id.as_u64()),
                name: semantic_name(element, node, &role, &inner, id, &labels),
                role,
                value,
                enabled: element_attr(element, "disabled").is_none()
                    && element_attr(element, "aria-disabled") != Some("true"),
                visible,
                selected: semantic_selected(element),
                bounds: rect.and_then(|rect| {
                    let bounds = [rect.x, rect.y, rect.width, rect.height];
                    bounds
                        .iter()
                        .all(|value| value.is_finite())
                        .then_some(bounds)
                }),
                slot: element_attr(element, "data-slot").map(str::to_owned),
            })
        })
        .collect();
    let total_ms = started.elapsed().as_secs_f64() * 1_000.0;
    // The runtime keeps one counter and stamps it onto all four revision
    // fields. Style, layout and paint are not versioned independently
    // anywhere in blitz, so four copies of one number would claim a
    // resolution that does not exist. Report the counter once, as the
    // document revision, and leave the rest at zero.
    let revisions = RevisionSet {
        document: revision,
        style: 0,
        layout: 0,
        paint: 0,
    };
    // Real per-frame timings, published by blitz-shell from `View::redraw`.
    // These describe frames the application actually presented. Everything
    // measured inside this function describes the snapshot collection instead,
    // and is reported under `snapshot` so the two never get mixed up again.
    let frame_stats = blitz_shell::latest_frame_stats();
    let metrics = RendererMetrics {
        revisions: revisions.clone(),
        queue_depth: None,
        invalidations_coalesced: polls.saturating_sub(1),
        frame: frame_stats.as_ref().map(|stats| FrameMetrics {
            input_to_present_ms: None,
            style_ms: None,
            layout_ms: None,
            resolve_ms: stats.latest.resolve_ms,
            scene_ms: stats.latest.paint_ms,
            submit_ms: None,
            present_ms: None,
            renderer_ms: stats.latest.renderer_ms,
            total_ms: stats.latest.total_ms,
            age_ms: stats.latest.age_ms,
        }),
        frame_window: frame_stats.as_ref().map(|stats| FrameWindowMetrics {
            frames_total: stats.frames_total,
            window_frames: stats.window_frames,
            resolve: timing_stats(stats.resolve),
            scene: timing_stats(stats.paint),
            renderer: timing_stats(stats.renderer),
            total: timing_stats(stats.frame_total),
            interval: timing_stats(stats.interval),
            active_fps: stats.active_fps,
            missed_refreshes: stats.missed_refreshes,
            display_refresh_hz: stats.display_refresh_hz,
        }),
        snapshot: Some(SnapshotCost {
            poll_ms,
            resolve_ms: snapshot_resolve_ms,
            total_ms,
        }),
        // The other half of a frame. Everything above this line is the
        // engine; this is the language runtime the application actually
        // spends its time in.
        script: blitz_script::script_stats::latest_script_stats().map(|stats| ScriptMetrics {
            mean_ms: stats.mean_ms,
            p95_ms: stats.p95_ms,
            max_ms: stats.max_ms,
            window_polls: stats.window_polls,
            total_polls: stats.total_polls,
            productive_polls: stats.productive_polls,
            spent_ms: stats.spent_ms,
            breakdown: blitz_script::script_stats::work_breakdown()
                .into_iter()
                .take(12)
                .map(|(label, calls, total_ms, worst_ms)| ScriptSource {
                    label,
                    calls,
                    total_ms,
                    worst_ms,
                })
                .collect(),
        }),
        resident_bytes: resident_bytes(),
    };
    let dom = request
        .include_dom
        .then(|| serde_json::to_value(&nodes).unwrap_or(serde_json::Value::Null));
    let layout = request.include_layout.then(|| {
        nodes
            .iter()
            .filter_map(|node| diagnostic_layout_row(&inner, node))
            .collect()
    });
    /*
     * Resolved colours, folded into the layout rows.
     *
     * This used to answer `computedStyleUnavailable`, which left one class
     * of bug unanswerable from outside: an element whose *declared* colour
     * is correct and whose *painted* colour is not. Reading the stylesheet
     * cannot settle that - the cascade, the custom-property chain and the
     * `@supports` gating all sit between the two - and neither can a DOM
     * test environment, which has no cascade at all.
     *
     * Only the four that decide legibility, rather than a full style dump:
     * a snapshot of every longhand for 4,500 nodes is megabytes of JSON
     * nobody reads, and these are what a "why is this text invisible"
     * question actually needs.
     */
    let computed_style = request.include_computed_style.then(|| {
        serde_json::Value::Array(
            nodes
                .iter()
                .filter_map(|node| diagnostic_style_row(&inner, node))
                .collect(),
        )
    });
    Ok(DebugSnapshot {
        revisions,
        active_window: Some("blitz-main".into()),
        active_element,
        dom,
        layout,
        computed_style,
        metrics,
    })
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn element_attr<'a>(element: &'a blitz_dom::ElementData, name: &str) -> Option<&'a str> {
    element
        .attrs()
        .iter()
        .find(|attribute| attribute.name.local.as_ref() == name)
        // `as_ref`, not `as_str`. Attribute values are an interned atom as of
        // ps-blitz-dom 0.3.0-beta.11, and `str::as_str` is still unstable, so
        // `as_str` here resolved to the nightly-only inherent method and
        // failed to build on stable. `as_ref` borrows the atom as a `&str`,
        // which is what this signature returns.
        .map(|attribute| attribute.value.as_ref())
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn semantic_role(element: &blitz_dom::ElementData) -> String {
    if let Some(role) = element_attr(element, "role") {
        return role.into();
    }
    let tag = element.name.local.as_ref();
    match tag {
        "a" if element_attr(element, "href").is_some() => "link",
        "button" => "button",
        "textarea" => "textbox",
        "select" => "combobox",
        "option" => "option",
        "img" => "img",
        "nav" => "navigation",
        "main" => "main",
        // A named section is a landmark; an unnamed one is nothing.
        //
        // HTML-AAM: `<section>` maps to `region` when it has an accessible
        // name, and to `generic` otherwise. Both halves matter. A named section
        // is how a page says "this part is the connection settings", and it
        // arrived indistinguishable from the `<div>`s around it; an unnamed one
        // is a wrapper, and promoting those would put a landmark around every
        // block on a page that reaches for `<section>` as a synonym for `<div>`.
        //
        // Attributes only, because the name has not been computed yet at this
        // point and computing it here would walk the section's whole subtree for
        // every element in the document. That is the same set an accessible name
        // can come from for a container: `aria-labelledby` is included so an
        // author who names a section that way still gets the landmark, even
        // though `semantic_name` does not yet resolve that reference.
        "section"
            if ["aria-label", "aria-labelledby", "title"]
                .iter()
                .any(|name| {
                    element_attr(element, name).is_some_and(|value| !value.trim().is_empty())
                }) =>
        {
            "region"
        }
        "form" => "form",
        "ul" | "ol" => "list",
        "li" => "listitem",
        "table" => "table",
        "tr" => "row",
        "td" => "cell",
        // A header cell is not a cell.
        //
        // HTML-AAM maps `<th>` to `columnheader` or `rowheader`, and blitz-dom's
        // own accessibility tree already does exactly this, so the two trees
        // disagreed about the same document. What a header is for is saying
        // which column or row the values under it belong to, and a check that
        // wants "the Version column" has nothing to ask for while every header
        // is spelled the same as the data beneath it.
        //
        // `scope` decides. Without one this is a column header, which is the
        // common case (a `<thead>` row) and what blitz-dom falls back to.
        "th" => match element_attr(element, "scope") {
            Some("row") | Some("rowgroup") => "rowheader",
            _ => "columnheader",
        },
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "heading",
        "input" => match element_attr(element, "type").unwrap_or("text") {
            "checkbox" => "checkbox",
            "radio" => "radio",
            "button" | "submit" | "reset" => "button",
            "range" => "slider",
            _ => "textbox",
        },
        _ => "generic",
    }
    .into()
}

/// Where the labels are, so a control can be asked what names it.
///
/// # Why this exists
///
/// A name was computed from `aria-label`, `alt` and `title` and from nothing
/// else, so the ordinary way to label a form control -- a `<label for>` beside
/// it, or a `<label>` wrapped around it -- produced no name at all. Every text
/// field on every page in the fleet arrived in the semantic tree anonymous.
///
/// That is not only a reporting defect. A harness addresses a control by name,
/// so an anonymous field cannot be typed into, and a check that means "enter a
/// URL and save" cannot be written. Measured on support.cafe's connection
/// settings: three `Input.Field`s, each with a correct `<Label for>` beside it,
/// all three reported as `textbox ""`.
///
/// Built once per snapshot rather than searched per node, because the lookup is
/// "which label points at me" and answering that from the control costs a scan
/// of the document each time.
#[cfg(all(feature = "agent-control", unix))]
pub(crate) struct LabelIndex {
    /// The `for` attribute's value, to that label's text.
    by_control_id: std::collections::HashMap<String, String>,
    /// Labels by node id, so an ancestor walk can recognise one it is inside.
    labels: std::collections::HashMap<blitz_dom::NodeId, String>,
}

#[cfg(all(feature = "agent-control", unix))]
impl LabelIndex {
    pub(crate) fn build(document: &blitz_dom::BaseDocument) -> Self {
        let mut by_control_id = std::collections::HashMap::new();
        let mut labels = std::collections::HashMap::new();
        for (id, node) in document.tree().iter() {
            let Some(element) = node.element_data() else {
                continue;
            };
            if element.name.local.as_ref() != "label" {
                continue;
            }
            // Read the same way a name is, not with `textContent`. A label is a
            // name once it reaches a control, so a stylesheet inside it, or the
            // half of a responsive label that is not rendered at this width,
            // has to be left out here too.
            let text = name_text(node, document);
            if let Some(control) = element_attr(element, "for") {
                by_control_id.insert(control.to_owned(), text.clone());
            }
            labels.insert(id, text);
        }
        Self {
            by_control_id,
            labels,
        }
    }

    /// The label text for a control, by association or by containment.
    ///
    /// `for` first, matching the order a browser resolves them in: an explicit
    /// association wins over the label the control happens to sit inside.
    fn name_for(
        &self,
        document: &blitz_dom::BaseDocument,
        id: blitz_dom::NodeId,
        element: &blitz_dom::ElementData,
    ) -> Option<String> {
        if let Some(dom_id) = element_attr(element, "id")
            && let Some(text) = self.by_control_id.get(dom_id)
            && !text.trim().is_empty()
        {
            return Some(text.clone());
        }
        // A wrapping label, walked outward. Bounded rather than open, because
        // a malformed tree must not cost a traversal per node.
        //
        // An empty label does not stop the walk. Labels nested inside labels
        // are invalid markup, but they happen: a checkbox component that draws
        // its own `<label>` around a styled box, wrapped again by the page to
        // add the text beside it. The inner label has no text, and returning
        // its emptiness here made the control anonymous while a name sat one
        // level further out. Skipping it costs nothing when the markup is
        // well formed, because a real label has text.
        let mut current = document.get_node(id)?.parent;
        for _ in 0..16 {
            let ancestor = current?;
            if let Some(text) = self.labels.get(&ancestor)
                && !text.trim().is_empty()
            {
                return Some(text.clone());
            }
            current = document.get_node(ancestor)?.parent;
        }
        None
    }
}

#[cfg(all(feature = "agent-control", unix))]
/// The text a name is made of, which is not the same as `textContent`.
///
/// `textContent` is the DOM property and includes every text node under the
/// element, `<style>` and `<script>` among them. An accessible name does not:
/// those elements are not rendered, and a name computation that walks into
/// them reads out a stylesheet.
///
/// Measured on honey.id, whose header logo is an anchor wrapping an inline SVG
/// with a `<style>` in it. The site's home link arrived named
/// ".animated-logo path { fill-opacity: 0; stroke: currentColor; ... }",
/// which is unusable to a person and unaddressable to a check.
///
/// Measured on crates.vip, whose failure alert stacks two block-level lines.
/// It arrived named "This page could not loadWebSocket connection failed",
/// because every text node was concatenated with nothing between it and the
/// next. A browser puts a space there: accname appends each descendant's
/// contribution separated by a space unless the descendant is inline, which
/// is why "<span>a</span><span>b</span>" is still "ab".
fn name_text(node: &blitz_dom::Node, document: &blitz_dom::BaseDocument) -> String {
    /// Whether this node's contribution runs into its siblings' or stands
    /// apart from them. Text and inline-level elements run together; anything
    /// laid out as a block, a flex item's container, a table cell and so on
    /// is a separate run. An element with no resolved style is treated as
    /// inline, which keeps a name from gaining spaces that are not there.
    fn is_inline(node: &blitz_dom::Node) -> bool {
        use style::values::specified::box_::{DisplayInside, DisplayOutside};
        if node.element_data().is_none() {
            return true;
        }
        node.primary_styles().is_none_or(|styles| {
            let display = styles.clone_display();
            // Inline flow only. `inline-block` and `inline-flex` are atomic
            // inlines: they establish their own box and a browser separates
            // them from their siblings, which is the same rule
            // `dom-accessibility-api` applies by comparing the computed
            // display against the string "inline".
            display.outside() == DisplayOutside::Inline && display.inside() == DisplayInside::Flow
        })
    }

    fn write(node: &blitz_dom::Node, document: &blitz_dom::BaseDocument, out: &mut String) {
        if let Some(element) = node.element_data()
            && matches!(
                element.name.local.as_ref(),
                "style" | "script" | "template" | "noscript"
            )
        {
            return;
        }
        if let blitz_dom::node::NodeData::Text(text) = &node.data {
            out.push_str(&text.content);
        }
        for child in &node.children {
            let Some(child) = document.get_node(*child) else {
                continue;
            };
            // What is not rendered is not part of the name.
            //
            // A responsive control writes both labels and shows one:
            // `sm:hidden` on the short one, `hidden sm:inline` on the long one.
            // Folding both together produced "Book Book a diagnostic", a name
            // no viewer at any width can see and no check can be written
            // against. `visibility: hidden` and `aria-hidden` are excluded for
            // the same reason, which is the rule accname states directly.
            //
            // Elements only. A text node carries no display of its own, so it
            // is present exactly when the element holding it is.
            if child.element_data().is_some() && !node_is_individually_visible(child) {
                continue;
            }
            // A boundary either side, so a block between two others is
            // separated from both. `normalize_name` collapses the runs.
            let separate = !is_inline(child);
            if separate {
                out.push(' ');
            }
            write(child, document, out);
            if separate {
                out.push(' ');
            }
        }
    }
    let mut out = String::new();
    write(node, document, &mut out);
    out
}

/// Whether a role takes its accessible name from its own subtree when the
/// author wrote no explicit one.
///
/// ARIA's *nameFrom: author, contents*, and nothing else. The list is closed on
/// purpose: a role that is not on it is named only by what the author declared,
/// because a container's text content is its whole subtree and naming those
/// would give every wrapper on a page a name made of the page.
///
/// `alert` and `status` are here because they are the roles an application uses
/// to say something happened -- a refusal, a saved confirmation -- and what they
/// say is their content. Without them a live region arrives anonymous, so "the
/// reason is shown" is not a question a suite can ask, and every validation
/// outcome has to be approximated by something else that moved.
///
/// `menuitem`, `tab` and `treeitem` are the menu, tab and tree equivalents of
/// `option`. Leaving them out made every dropdown item in the fleet anonymous:
/// a `<button role="menuitem">Platform Admin</button>` came back with an empty
/// name, so nothing was announced and no check could name the option it meant
/// to press.
///
/// The table roles are here because ARIA gives all five of them
/// *nameFrom: contents*, and their absence is why whole tables of crate names,
/// versions and column types were unreadable: the cells were in the tree and
/// every one of them was anonymous, which reads from outside as a table that is
/// not in the tree at all. A row's name being the run of its cells is not an
/// accident of that rule, it is the rule: it is what a screen reader announces
/// when the caret enters the row.
///
/// `semantic_role` returns a `role` attribute verbatim, so an author who writes
/// one of these opts into the naming this list provides.
#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn names_from_contents(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "heading"
            | "option"
            | "alert"
            | "status"
            // The same class as `alert` and `status`: a tooltip exists to say
            // one thing, and what it says is its content. Anonymous, it is a
            // node reporting that some explanation is on screen without
            // reporting the explanation.
            | "tooltip"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "tab"
            | "treeitem"
            | "cell"
            | "gridcell"
            | "columnheader"
            | "rowheader"
            | "row"
    )
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn semantic_name(
    element: &blitz_dom::ElementData,
    node: &blitz_dom::Node,
    role: &str,
    document: &blitz_dom::BaseDocument,
    id: blitz_dom::NodeId,
    labels: &LabelIndex,
) -> String {
    let name = element_attr(element, "aria-label")
        .map(std::borrow::Cow::Borrowed)
        // The label a form control was given. After `aria-label`, which is the
        // author overriding the visible text on purpose, and before `title`,
        // which is a tooltip rather than a name.
        .or_else(|| {
            labels
                .name_for(document, id, element)
                .map(std::borrow::Cow::Owned)
        })
        .or_else(|| element_attr(element, "alt").map(std::borrow::Cow::Borrowed))
        .or_else(|| element_attr(element, "title").map(std::borrow::Cow::Borrowed))
        // An option's `label`, which HTML gives precedence over the option's
        // own text: `<option label="Sixty four bits">u64</option>` announces the
        // label.
        .or_else(|| {
            (role == "option")
                .then(|| element_attr(element, "label"))
                .flatten()
                .map(std::borrow::Cow::Borrowed)
        })
        // Named by their own content.
        //
        // Empty contents are not a name, and stopping here on an empty string
        // is how the fallbacks below became unreachable for the roles on this
        // list.
        .or_else(|| {
            names_from_contents(role)
                .then(|| name_text(node, document))
                .filter(|text| !text.trim().is_empty())
                .map(std::borrow::Cow::Owned)
        })
        // What is left of an option that carries no text at all: a
        // `<datalist>` entry is written `<option value="u64">`, and its value is
        // what a browser announces and what a person sees in the list.
        .or_else(|| {
            (role == "option")
                .then(|| element_attr(element, "value"))
                .flatten()
                .map(std::borrow::Cow::Borrowed)
        })
        // A placeholder is the last resort a browser falls back to, and it is
        // the only thing naming a great many search and filter fields. Last, so
        // it never displaces a real label.
        .or_else(|| {
            matches!(role, "textbox" | "combobox")
                .then(|| element_attr(element, "placeholder").map(std::borrow::Cow::Borrowed))
                .flatten()
        })
        .unwrap_or_default();
    let mut normalized = String::with_capacity(name.len().min(512));
    let mut characters = 0;
    for word in name.split_whitespace() {
        if !normalized.is_empty() && characters < 512 {
            normalized.push(' ');
            characters += 1;
        }
        for character in word.chars() {
            if characters == 512 {
                return normalized;
            }
            normalized.push(character);
            characters += 1;
        }
    }
    normalized
}

#[cfg(all(feature = "agent-control", unix))]
fn semantic_value(element: &blitz_dom::ElementData) -> Option<String> {
    element
        .text_input_data()
        .map(|input| input.editor.text().to_string())
        .or_else(|| {
            element
                .checkbox_input_checked()
                .map(|checked| checked.to_string())
        })
        .or_else(|| element_attr(element, "aria-valuenow").map(str::to_string))
        .or_else(|| element_attr(element, "value").map(str::to_string))
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn semantic_selected(element: &blitz_dom::ElementData) -> bool {
    /*
     * A real checkbox answers from its live state, not from its markup.
     *
     * `checkbox_input_checked` is the value the DOM updates when the control is
     * toggled; the attributes below are the document as it was parsed and never
     * move again. Asking the attributes about an input that has one is how a
     * toggle reports the same state for ever.
     */
    if let Some(checked) = element.checkbox_input_checked() {
        return checked;
    }

    /*
     * Presence is not truth for these two.
     *
     * `checked` and `selected` are HTML boolean attributes, so bare `checked`
     * means on. But a framework that renders a controlled value writes the
     * value out: Solid emits `checked="false"`, and `.is_some()` called that
     * selected. Every Switch, Radio and Checkbox in the QA harness reported
     * `selected: true` before anything was pressed and could never change,
     * which read as three components that ignore a click.
     *
     * Explicitly `"false"` is off; anything else present is on.
     */
    let attribute_on = |name| match element_attr(element, name) {
        Some("false") => false,
        Some(_) => true,
        None => false,
    };

    element_attr(element, "aria-selected") == Some("true")
        || element_attr(element, "aria-pressed") == Some("true")
        || element_attr(element, "aria-checked") == Some("true")
        || element_attr(element, "aria-current").is_some_and(|value| value != "false")
        || attribute_on("checked")
        || attribute_on("selected")
}

#[cfg(all(feature = "agent-control", unix))]
fn semantic_parent(
    document: &blitz_dom::BaseDocument,
    node_id: blitz_dom::NodeId,
    root: Option<blitz_dom::NodeId>,
) -> Option<blitz_dom::NodeId> {
    if Some(node_id) == root {
        return None;
    }
    let mut current = document.get_node(node_id)?.parent;
    while let Some(id) = current {
        let node = document.get_node(id)?;
        if node.element_data().is_some() {
            return Some(id);
        }
        current = node.parent;
    }
    None
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn node_is_visible(
    document: &blitz_dom::BaseDocument,
    node_id: blitz_dom::NodeId,
) -> bool {
    let mut current = Some(node_id);
    while let Some(id) = current {
        let Some(node) = document.get_node(id) else {
            return false;
        };
        if !node_is_individually_visible(node) {
            return false;
        }
        current = node.parent;
    }
    true
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn dom_chain_is_attached(
    document: &blitz_dom::BaseDocument,
    node_id: blitz_dom::NodeId,
    node_limit: usize,
) -> bool {
    let root = document.root_node().id;
    let mut current = Some(node_id);
    // Removed DOM nodes intentionally remain allocated while JavaScript may
    // still hold wrappers for them. They are not part of the document unless
    // their parent chain reaches the one document root.
    for _ in 0..=node_limit {
        let Some(id) = current else {
            return false;
        };
        if id == root {
            return true;
        }
        let Some(node) = document.get_node(id) else {
            return false;
        };
        current = node.parent;
    }
    false
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn layout_chain_is_valid(
    document: &blitz_dom::BaseDocument,
    node_id: blitz_dom::NodeId,
    node_limit: usize,
) -> bool {
    let mut current = Some(node_id);
    // A valid layout chain reaches its root in no more steps than there are
    // nodes. The bound also rejects corrupt cycles instead of hanging control.
    for _ in 0..=node_limit {
        let Some(id) = current else {
            return true;
        };
        let Some(node) = document.get_node(id) else {
            return false;
        };
        current = node.layout_parent.get();
    }
    false
}

/// Activate the node the caller selected, without asking hit-testing to select
/// it a second time from a screen coordinate.
///
/// The coordinates carried by DOM events are still the node's own geometry,
/// because handlers use offsets and text fields use them for caret placement.
/// They never choose the target. An overflowed or clipped node therefore gets
/// the same pointer, mouse and click sequence as an on-screen one.
#[cfg(all(feature = "agent-control", unix))]
/// Click a semantic node, by id, the way the runtime does.
///
/// Dispatches pointer, mouse and click events in browser order against the
/// document directly, so a headless host can drive a control without a window,
/// a pointer or a compositor. This is what makes the interaction checks
/// runnable at all: a still picture of the tree cannot answer what a control
/// does when it is pressed.
#[cfg(all(feature = "agent-control", unix))]
/// Send one key to a document, down then up.
///
/// Only keys, deliberately. The pointer and wheel arms of the runtime's input
/// handler carry pointer position and button state on the runtime itself, and a
/// headless host has no window for those to mean anything against. A key needs
/// nothing but the document.
///
/// Escape closing a menu is a real assertion in a check suite, and it is the one
/// that says a control does not trap the person using it. Without this a host
/// answers those checks with `unsupported`, which is honest but leaves the
/// suite unable to run them at all.
#[cfg(all(feature = "agent-control", unix))]
/// Move the pointer onto a semantic node, by id.
///
/// The position comes from the node's own box, so this needs no pointer
/// bookkeeping and works in a host that has no window to own a cursor. A
/// control revealed on hover cannot be reached any other way, and a defect that
/// only appears on the second entry cannot be reached at all without it.
#[cfg(all(feature = "agent-control", unix))]
pub fn hover_agent_node(
    document: &mut ScriptDocument,
    node_id: u64,
) -> Result<(f32, f32), DebugError> {
    let position = resolve_agent_node(document, node_id)?.1;
    document.handle_ui_event(UiEvent::PointerMove(pointer_event(
        position,
        MouseEventButton::Main,
        MouseEventButtons::default(),
        KeyboardModifiers::empty(),
    )));
    Ok(position)
}

pub fn press_agent_key(
    document: &mut ScriptDocument,
    key: &str,
    code: &str,
) -> Result<(), DebugError> {
    let parsed_key = key
        .parse::<Key>()
        .unwrap_or_else(|_| Key::Character(key.to_owned()));
    let parsed_code = code.parse::<Code>().unwrap_or(Code::Unidentified);
    for phase in [KeyPhase::Down, KeyPhase::Up] {
        let event = key_event(
            phase,
            parsed_key.clone(),
            parsed_code,
            keyboard_modifiers(Default::default()),
        );
        document.handle_ui_event(match phase {
            KeyPhase::Down => UiEvent::KeyDown(event),
            KeyPhase::Up => UiEvent::KeyUp(event),
        });
    }
    Ok(())
}

pub fn click_agent_node(
    document: &mut ScriptDocument,
    node_id: u64,
    count: u8,
) -> Result<(f32, f32), DebugError> {
    activate_agent_node(document, node_id, count)
}

#[cfg(all(feature = "agent-control", unix))]
pub fn focus_agent_node(
    document: &mut ScriptDocument,
    node_id: blitz_dom::NodeId,
) -> Result<(), DebugError> {
    let focusable = document
        .inner()
        .get_node(node_id)
        .and_then(|node| node.element_data())
        .is_some_and(focuses_on_click);
    if !focusable {
        return Err(debug_error(
            "notFocusable",
            "node does not accept keyboard focus",
        ));
    }
    document.inner_mut().set_focus_to(node_id);
    Ok(())
}

/// Read a document's semantic tree.
///
/// Split out of the runtime's `Inspect` handler so a host that is not this
/// runtime can answer the same request from the same code. Nothing in it is
/// window-dependent: it polls the document, resolves layout and reads the tree.
///
/// Sharing the implementation is the point. A second copy of "what is a node's
/// name" drifts from this one immediately. A QA harness that reimplemented
/// naming against `build_accessibility_tree` got a different answer than the
/// inspector for every element on the page, because that builder names only
/// text nodes and carries no geometry at all.
#[cfg(all(feature = "agent-control", unix))]
pub fn inspect_document(
    document: &mut ScriptDocument,
    root: Option<u64>,
    max_depth: u32,
    revision: u64,
) -> DebugResponse {
    /*
     * Drain immediately runnable script work, but do not force a full style and
     * layout pass for an already committed document. Agent actions resolve
     * before their Ack and the window loop resolves asynchronous frames; an
     * idle inspection is an observer, not another frame driver. Re-resolving
     * every 25ms made scoped outcome latency proportional to the entire retained
     * application even though the response contained one pane.
     */
    let mut ran_script = false;
    for _ in 0..100 {
        if !document.poll(None) {
            break;
        }
        ran_script = true;
    }
    if ran_script {
        document.inner_mut().resolve(0.0);
    }
    let inner = document.inner();
    let root = root.map(blitz_dom::NodeId::from_u64);
    if root.is_some_and(|id| inner.get_node(id).is_none()) {
        return control_error("unknownNode", "the requested root node does not exist");
    }
    let focused_node = inner.get_focussed_node_id().map(|id| id.as_u64());
    // Built over the whole document even when a subtree was asked for: a label
    // is frequently a sibling of the control rather than a descendant of the
    // node the caller rooted at.
    let labels = LabelIndex::build(&inner);
    let node_limit = inner.tree().iter().count();
    let candidates = if let Some(root) = root {
        semantic_subtree_ids(&inner, root, max_depth)
            .into_iter()
            .filter_map(|id| {
                inner.get_node(id)?;
                dom_chain_is_attached(&inner, id, node_limit).then(|| SemanticCandidate {
                    id,
                    parent: semantic_parent(&inner, id, Some(root)),
                    visible: node_is_visible(&inner, id),
                })
            })
            .collect()
    } else {
        attached_semantic_candidates(&inner, max_depth)
    };
    let layout_validity = layout_chain_validities(&inner, &candidates, node_limit);
    let nodes = candidates
        .into_iter()
        .filter_map(|candidate| {
            let id = candidate.id;
            let node = inner.get_node(id)?;
            let element = node.element_data()?;
            if layout_validity.get(&id) != Some(&true) {
                return None;
            }
            let rect = inner.get_client_bounding_rect(id);
            let visible = candidate.visible
                && rect
                    .as_ref()
                    .is_some_and(|rect| rect.width > 0.0 && rect.height > 0.0);
            let role = semantic_role(element);
            let name = semantic_name(element, node, &role, &inner, id, &labels);
            let value = semantic_value(element);
            Some(SemanticNode {
                dom_id: element_attr(element, "id").map(str::to_owned),
                id: id.as_u64(),
                parent: candidate.parent.map(|id| id.as_u64()),
                role,
                name,
                value,
                enabled: element_attr(element, "disabled").is_none()
                    && element_attr(element, "aria-disabled") != Some("true"),
                visible,
                selected: semantic_selected(element),
                bounds: rect.and_then(|rect| {
                    let bounds = [rect.x, rect.y, rect.width, rect.height];
                    bounds
                        .iter()
                        .all(|value| value.is_finite())
                        .then_some(bounds)
                }),
                slot: element_attr(element, "data-slot").map(str::to_owned),
            })
        })
        .collect();
    DebugResponse::AgentSnapshot(AgentSnapshot {
        revision,
        active_window: Some("blitz-main".into()),
        focused_node,
        nodes,
    })
}

/// Carry blitz-shell's timing summary onto the wire type.
///
/// The two structs are deliberately separate: the protocol is versioned by this
/// crate, while the shell type is free to grow fields that have no wire meaning.
#[cfg(all(feature = "diagnostics", unix))]
pub(crate) fn timing_stats(stats: blitz_shell::TimingStats) -> TimingStats {
    TimingStats {
        mean_ms: stats.mean_ms,
        p95_ms: stats.p95_ms,
        max_ms: stats.max_ms,
    }
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn debug_error(code: &str, message: &str) -> DebugError {
    DebugError {
        code: code.into(),
        message: message.into(),
    }
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn focuses_on_click(element: &blitz_dom::ElementData) -> bool {
    let tag = element.name.local.as_ref();
    matches!(tag, "button" | "input" | "select" | "textarea")
        || tag == "a" && element_attr(element, "href").is_some()
        || element_attr(element, "tabindex")
            .and_then(|value| value.parse::<i32>().ok())
            .is_some_and(|value| value >= 0)
        || element_attr(element, "contenteditable").is_some_and(|value| value != "false")
}

#[cfg(all(feature = "diagnostics", unix))]
pub(crate) fn resident_bytes() -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|kilobytes| kilobytes.checked_mul(1_024))
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn node_is_individually_visible(node: &blitz_dom::Node) -> bool {
    if !node.flags.is_in_document() || node.is_display_none() {
        return false;
    }
    /*
     * `visibility: hidden` counts too, not just `display: none`.
     *
     * The two are different in layout and identical to a viewer: a hidden
     * node keeps its box and paints nothing. Reporting it as visible made
     * an audit of the running application call it a fault, because the box
     * was there and the pixels were not. Tailwind's `invisible` is exactly
     * this, and it is how a control that is deliberately dormant - a Stop
     * button with no run to stop - is expressed.
     *
     * `Collapse` is included: on anything that is not a table row it means
     * the same as `Hidden`, and on a row it removes the row entirely, so
     * treating it as not-visible is right in both cases.
     */
    if node.primary_styles().is_some_and(|style| {
        use style::computed_values::visibility::T as Visibility;
        matches!(
            style.clone_visibility(),
            Visibility::Hidden | Visibility::Collapse
        )
    }) {
        return false;
    }
    !node.element_data().is_some_and(|element| {
        element_attr(element, "hidden").is_some()
            || element_attr(element, "aria-hidden") == Some("true")
    })
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn resolve_agent_node(
    document: &mut ScriptDocument,
    raw_node_id: u64,
) -> Result<(blitz_dom::NodeId, (f32, f32)), DebugError> {
    resolve_agent_node_inner(document, raw_node_id, Area::Required)
}

/// Whether the caller needs the node to have a box a pointer could land in.
#[cfg(all(feature = "agent-control", unix))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Area {
    /// A real pointer is going to travel to this box, so it has to have one.
    Required,
    /// The event is delivered to the node itself, and the coordinate only
    /// fills in `clientX`/`clientY` for a handler that reads them.
    Optional,
}

/// Resolve a node to press, and the point to say the press happened at.
///
/// The area requirement is a parameter because the two callers want different
/// things from it. `Hover` and the pointer moves genuinely travel to a
/// coordinate, and a box with no area has no honest one. `Click` does not:
/// [`activate_agent_node`] dispatches every phase with
/// `DomEvent::new(node_id, ..)`, so the node is the target and the position is
/// only the number the event carries.
///
/// Requiring area of both was one predicate standing for two facts, and it
/// made a whole class of control unpressable for a reason that has nothing to
/// do with whether pressing it works. A control sized entirely by its label --
/// a bare `<a>`, a trigger with no padding -- lays out at zero height on a host
/// with no font catalogue, which is what a Linux CI runner is. It is attached,
/// styled visible, enabled, and its handler runs perfectly well; only the
/// coordinate it would never use was missing.
///
/// What still refuses is everything about the node itself: it must exist, be
/// attached with a valid layout ancestry, not be `display: none`,
/// `visibility: hidden`, `hidden` or `aria-hidden`, and not be disabled. Those
/// are the reasons a press should not land, and none of them is a box size.
#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn resolve_agent_node_inner(
    document: &mut ScriptDocument,
    raw_node_id: u64,
    area: Area,
) -> Result<(blitz_dom::NodeId, (f32, f32)), DebugError> {
    let node_id = blitz_dom::NodeId::from_u64(raw_node_id);
    document.inner_mut().resolve(0.0);
    let inner = document.inner();
    let node = inner
        .get_node(node_id)
        .ok_or_else(|| debug_error("unknownNode", "node does not exist"))?;
    if !node_is_visible(&inner, node_id) {
        return Err(debug_error("notInteractable", "node is not visible"));
    }
    let node_limit = inner.tree().iter().count();
    if !dom_chain_is_attached(&inner, node_id, node_limit)
        || !layout_chain_is_valid(&inner, node_id, node_limit)
    {
        return Err(debug_error(
            "notInteractable",
            "node has a detached layout ancestor",
        ));
    }
    if node
        .element_data()
        .is_some_and(|element| element_attr(element, "disabled").is_some())
    {
        return Err(debug_error("notInteractable", "node is disabled"));
    }
    let rect = inner
        .get_client_bounding_rect(node_id)
        .filter(|rect| area == Area::Optional || (rect.width > 0.0 && rect.height > 0.0))
        .ok_or_else(|| {
            debug_error(
                "notInteractable",
                match area {
                    // The node was never laid out at all, which is a different
                    // thing from being laid out flat and is worth saying so.
                    Area::Optional => "node has no layout box",
                    Area::Required => "node has no layout box a pointer can reach",
                },
            )
        })?;
    Ok((
        node_id,
        (
            (rect.x + rect.width / 2.0) as f32,
            (rect.y + rect.height / 2.0) as f32,
        ),
    ))
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn pointer_event(
    position: (f32, f32),
    button: MouseEventButton,
    buttons: MouseEventButtons,
    modifiers: KeyboardModifiers,
) -> BlitzPointerEvent {
    BlitzPointerEvent {
        id: BlitzPointerId::Mouse,
        is_primary: true,
        coords: pointer_coords(position),
        button,
        buttons,
        mods: modifiers,
        details: PointerDetails::default(),
        element: Point::default(),
        active_pointers: Default::default(),
    }
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) struct SemanticCandidate {
    pub(crate) id: blitz_dom::NodeId,
    pub(crate) parent: Option<blitz_dom::NodeId>,
    pub(crate) visible: bool,
}

#[cfg(all(feature = "agent-control", unix))]
#[derive(Clone, Copy)]
enum LayoutChainState {
    Visiting,
    Valid,
    Invalid,
}

/// Resolve layout ancestry once for every inspected node.
///
/// DOM ancestry and layout ancestry are not interchangeable, but they share
/// the same performance trap: walking every node back to a root makes a full
/// inspection proportional to `nodes * depth`. Memoize each layout ancestor so
/// later candidates stop at the first result the traversal already proved.
#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn layout_chain_validities(
    document: &blitz_dom::BaseDocument,
    candidates: &[SemanticCandidate],
    node_limit: usize,
) -> HashMap<blitz_dom::NodeId, bool> {
    let mut states = HashMap::with_capacity(candidates.len());

    for candidate in candidates {
        if matches!(
            states.get(&candidate.id),
            Some(LayoutChainState::Valid | LayoutChainState::Invalid)
        ) {
            continue;
        }

        let mut chain = Vec::new();
        let mut current = Some(candidate.id);
        let valid = loop {
            let Some(id) = current else {
                break true;
            };
            match states.get(&id) {
                Some(LayoutChainState::Valid) => break true,
                Some(LayoutChainState::Invalid | LayoutChainState::Visiting) => break false,
                None => {}
            }
            if chain.len() > node_limit {
                break false;
            }
            let Some(node) = document.get_node(id) else {
                break false;
            };
            states.insert(id, LayoutChainState::Visiting);
            chain.push(id);
            current = node.layout_parent.get();
        };

        let resolved = if valid {
            LayoutChainState::Valid
        } else {
            LayoutChainState::Invalid
        };
        for id in chain {
            states.insert(id, resolved);
        }
    }

    states
        .into_iter()
        .filter_map(|(id, state)| match state {
            LayoutChainState::Valid => Some((id, true)),
            LayoutChainState::Invalid => Some((id, false)),
            LayoutChainState::Visiting => None,
        })
        .collect()
}

/// Collect one rooted DOM subtree in document order without visiting retained
/// panes outside it.
#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn semantic_subtree_ids(
    document: &blitz_dom::BaseDocument,
    root: blitz_dom::NodeId,
    max_depth: u32,
) -> Vec<blitz_dom::NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![(root, 0_u32)];
    while let Some((node_id, depth)) = stack.pop() {
        let Some(node) = document.get_node(node_id) else {
            continue;
        };
        if node.element_data().is_some() {
            out.push(node_id);
        }
        for &child_id in node.children.iter().rev() {
            let child_depth = depth.saturating_add(
                document
                    .get_node(child_id)
                    .is_some_and(|child| child.element_data().is_some()) as u32,
            );
            if max_depth == 0 || child_depth <= max_depth {
                stack.push((child_id, child_depth));
            }
        }
    }
    out
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn pointer_coords((x, y): (f32, f32)) -> PointerCoords {
    PointerCoords {
        page_x: x,
        page_y: y,
        screen_x: x,
        screen_y: y,
        client_x: x,
        client_y: y,
    }
}

/// The colours a node actually resolved to, as `#rrggbbaa`.
///
/// The point of reporting these rather than the stylesheet is that they are the
/// end of the chain: the cascade, every custom-property indirection and the
/// `@supports` gating have already been applied, so a disagreement between what
/// a rule declares and what an element paints shows up here and nowhere else.
/// That disagreement is exactly the shape of "this text is invisible and the CSS
/// says it should not be", which cannot be settled by reading files.
///
/// Four properties rather than a full longhand dump: a complete style for every
/// node in a real application is megabytes of JSON that nobody reads, and these
/// are the ones legibility depends on.
#[cfg(all(feature = "diagnostics", unix))]
pub(crate) fn diagnostic_style_row(
    document: &blitz_dom::BaseDocument,
    node: &SemanticNode,
) -> Option<serde_json::Value> {
    let dom_node = document.get_node(NodeId::from_u64(node.id))?;
    let styles = dom_node.primary_styles()?;

    let current = styles.clone_color();
    // The same conversion `blitz-paint` does before handing a colour to the
    // rasteriser, inlined so this crate does not need that extension trait.
    let hex = |absolute: style::color::AbsoluteColor| {
        let [r, g, b, a] = *absolute
            .to_color_space(style::color::ColorSpace::Srgb)
            .raw_components();
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            channel(r),
            channel(g),
            channel(b),
            channel(a),
        )
    };

    /*
     * Reported as a plain number of pixels rather than stylo's debug shape,
     * which nothing reading this over the wire can parse - and being able to
     * read it is the entire reason the field exists.
     */
    let radius = format!("{:?}", styles.get_border().border_top_left_radius.0.width);
    let border = styles.get_border();
    let border_width = border.border_top_width.0.to_f64_px();
    let font_size = styles.clone_font_size().computed_size().px();
    let has_text_content = !dom_node.text_content().trim().is_empty();

    Some(serde_json::json!({
        "nodeId": node.id,
        "color": hex(current),
        "backgroundColor": hex(
            styles.clone_background_color().resolve_to_absolute(&current),
        ),
        "borderColor": hex(border.border_top_color.resolve_to_absolute(&current)),
        "borderWidth": format!("{border_width}px"),
        "fontSize": format!("{font_size}px"),
        "hasTextContent": has_text_content,
        "opacity": styles.clone_opacity(),
        /*
         * The corner, as the renderer resolved it.
         *
         * Radius is set from three unrelated places in a themed application -
         * the library's own component CSS, the theme's tokens, and utility
         * classes at the call site - and which one wins is a cascade question
         * that reading any single file cannot answer. Reported repeatedly as
         * "radius is wrong" with no way to tell *which* of the three was
         * responsible; this is what settles it per element.
         */
        "borderTopLeftRadius": radius,
        "visibility": format!("{:?}", styles.clone_visibility()),
    }))
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn keyboard_modifiers(modifiers: ControlModifiers) -> KeyboardModifiers {
    let mut output = KeyboardModifiers::empty();
    output.set(KeyboardModifiers::SHIFT, modifiers.shift);
    output.set(KeyboardModifiers::CONTROL, modifiers.control);
    output.set(KeyboardModifiers::ALT, modifiers.alt);
    output.set(KeyboardModifiers::META, modifiers.meta);
    output
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn key_event(
    phase: KeyPhase,
    key: Key,
    code: Code,
    modifiers: KeyboardModifiers,
) -> BlitzKeyEvent {
    let text = match (&key, phase) {
        (Key::Character(value), KeyPhase::Down)
            if !modifiers.intersects(
                KeyboardModifiers::CONTROL | KeyboardModifiers::ALT | KeyboardModifiers::META,
            ) =>
        {
            Some(value.clone().into())
        }
        _ => None,
    };
    BlitzKeyEvent {
        key,
        code,
        modifiers,
        location: Location::Standard,
        is_auto_repeating: false,
        is_composing: false,
        state: match phase {
            KeyPhase::Down => KeyState::Pressed,
            KeyPhase::Up => KeyState::Released,
        },
        text,
    }
}

/// Collect the attached document in one traversal.
///
/// The previous full inspection asked every node to rediscover its depth,
/// semantic parent, attachment and inherited visibility by walking back to the
/// root independently. A retained application therefore paid roughly
/// `nodes * depth` before it serialized one byte. Carry those inherited facts
/// down the tree once instead.
#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn attached_semantic_candidates(
    document: &blitz_dom::BaseDocument,
    max_depth: u32,
) -> Vec<SemanticCandidate> {
    let root = document.root_node().id;
    let mut candidates = Vec::new();
    let mut stack = vec![(root, 0_u32, None, true)];

    while let Some((id, depth, semantic_parent, ancestors_visible)) = stack.pop() {
        let Some(node) = document.get_node(id) else {
            continue;
        };
        let visible = ancestors_visible && node_is_individually_visible(node);
        let is_element = node.element_data().is_some();
        if is_element && max_depth != 0 && depth > max_depth {
            continue;
        }
        if is_element {
            candidates.push(SemanticCandidate {
                id,
                parent: semantic_parent,
                visible,
            });
        }

        let child_depth = depth.saturating_add(is_element as u32);
        let child_parent = if is_element {
            Some(id)
        } else {
            semantic_parent
        };
        for &child in node.children.iter().rev() {
            stack.push((child, child_depth, child_parent, visible));
        }
    }

    candidates
}

#[cfg(all(feature = "agent-control", unix))]
pub(crate) fn control_error(code: &str, message: &str) -> DebugResponse {
    DebugResponse::Error(debug_error(code, message))
}

#[cfg(all(feature = "diagnostics", unix))]
pub(crate) fn diagnostic_layout_row(
    document: &blitz_dom::BaseDocument,
    node: &SemanticNode,
) -> Option<LayoutDiagnosticRow> {
    let bounds = node.bounds?;
    let dom_node = document.get_node(NodeId::from_u64(node.id))?;
    let layout = dom_node.final_layout();
    let unzoom = |value: f32| match dom_node.primary_styles() {
        Some(styles) => styles.effective_zoom.unzoom(value),
        None => value,
    };
    // Every field here is unzoomed, including the two that used to be raw.
    //
    // `scrollOffset` and `scrollRange` came straight off the layout while
    // `clientSize` and `scrollSize` went through `unzoom`, so a single row
    // carried two unit systems and any arithmetic across them was wrong by the
    // zoom factor. Under zoom that makes every scroller read as overscrolled,
    // and it is not only a reading error: a consumer testing
    // `scrollOffset < scrollSize - clientSize` for "is there more to scroll"
    // gets a false negative at the true end, leaving an overflow control
    // disabled while content remains off screen.
    //
    // Unzoomed is the right side to land on because it is what the DOM already
    // reports: `blitz-script`'s `scrollLeft`/`scrollTop` unzoom before
    // answering, so a raw diagnostic also disagreed with the same measurement
    // taken from script.
    //
    // Not covered by a unit test, deliberately rather than by omission. The
    // existing row test runs at zoom 1, where `unzoom` is the identity and a
    // mixed row is indistinguishable from a consistent one. Reproducing it
    // needs a scroller that is itself zoomed, and in this engine `zoom` on an
    // `overflow-y:auto` element leaves `scroll_height()` at 0 while `zoom` on
    // its child does not reach the scroller's own styles — so a test asserting
    // the relation either holds vacuously (0 == 0) or fails its own setup.
    // Verified against a zoomed live scroller where the raw offset exceeded
    // the unzoomed range by exactly the zoom factor.
    let scroll_offset = dom_node.scroll_offset();
    Some(LayoutDiagnosticRow {
        node_id: node.id,
        bounds: LayoutBounds::from(bounds),
        scroll_offset: LayoutOffset {
            x: f64::from(unzoom(scroll_offset.x as f32)),
            y: f64::from(unzoom(scroll_offset.y as f32)),
        },
        client_size: LayoutSize {
            width: f64::from(unzoom(layout.size.width)),
            height: f64::from(unzoom(layout.size.height)),
        },
        scroll_size: LayoutSize {
            width: f64::from(unzoom(layout.size.width + layout.scroll_width())),
            height: f64::from(unzoom(layout.size.height + layout.scroll_height())),
        },
        scroll_range: LayoutSize {
            width: f64::from(unzoom(layout.scroll_width())),
            height: f64::from(unzoom(layout.scroll_height())),
        },
        // Border and padding, so a box that renders taller than it was asked
        // for can be attributed instead of guessed at.
        //
        // Without these the only readable numbers are the outer bounds and
        // `clientSize`, and both are the border box: a pill declared `24px`
        // that measures 27.8 offers no way to tell a 1px border from padding
        // from a wrong height, and the difference decides which file to edit.
        // Four consecutive wrong diagnoses of one composer pill came from
        // inferring these from CSS files rather than reading what the engine
        // computed, which is exactly the guessing this replaces.
        //
        // Edge order matches CSS shorthand: top, right, bottom, left.
        border: LayoutEdges {
            top: f64::from(unzoom(layout.border.top)),
            right: f64::from(unzoom(layout.border.right)),
            bottom: f64::from(unzoom(layout.border.bottom)),
            left: f64::from(unzoom(layout.border.left)),
        },
        padding: LayoutEdges {
            top: f64::from(unzoom(layout.padding.top)),
            right: f64::from(unzoom(layout.padding.right)),
            bottom: f64::from(unzoom(layout.padding.bottom)),
            left: f64::from(unzoom(layout.padding.left)),
        },
        // The content box, which is what an author's `height` sets under the
        // default `content-box` sizing. `clientSize` above is the border box.
        content_size: LayoutSize {
            width: f64::from(unzoom(
                layout.size.width
                    - layout.border.left
                    - layout.border.right
                    - layout.padding.left
                    - layout.padding.right,
            )),
            height: f64::from(unzoom(
                layout.size.height
                    - layout.border.top
                    - layout.border.bottom
                    - layout.padding.top
                    - layout.padding.bottom,
            )),
        },
    })
}

pub(crate) fn activate_agent_node(
    document: &mut ScriptDocument,
    raw_node_id: u64,
    count: u8,
) -> Result<(f32, f32), DebugError> {
    // `Area::Optional`: every phase below is dispatched with
    // `DomEvent::new(node_id, ..)`, so the node is the target and `position`
    // only fills in the coordinate the event carries. A control sized entirely
    // by its label has no area on a host with no fonts and is still perfectly
    // pressable.
    let (node_id, position) = resolve_agent_node_inner(document, raw_node_id, Area::Optional)?;
    let focusable = document
        .inner()
        .get_node(node_id)
        .and_then(|node| node.element_data())
        .is_some_and(focuses_on_click);

    for _ in 0..count {
        let down = pointer_event(
            position,
            MouseEventButton::Main,
            MouseEventButtons::Primary,
            KeyboardModifiers::empty(),
        );
        let up = pointer_event(
            position,
            MouseEventButton::Main,
            MouseEventButtons::default(),
            KeyboardModifiers::empty(),
        );
        for data in [
            DomEventData::PointerDown(down.clone()),
            DomEventData::MouseDown(down),
            DomEventData::PointerUp(up.clone()),
            DomEventData::MouseUp(up.clone()),
            DomEventData::Click(up),
        ] {
            // A mousedown handler can deliberately replace its own control.
            // The action already happened; later phases have no surviving
            // target and must not be retargeted to whatever took its place.
            if document.inner().get_node(node_id).is_none() {
                break;
            }
            document.dispatch_dom_event(DomEvent::new(node_id, data));
        }
        if focusable && document.inner().get_node(node_id).is_some() {
            document.inner_mut().set_focus_to(node_id);
        }
    }
    Ok(position)
}

#[cfg(all(test, feature = "agent-control", unix))]
mod tests {
    use super::*;
    use blitz_dom::{Document, DocumentConfig};

    /// A control whose whole size is its label, next to one with padding.
    ///
    /// `font-size: 0` stands in for the host this exists for: a Linux CI
    /// runner with no font catalogue, where every glyph shapes to nothing and
    /// a trigger with no padding lays out flat. Written as a style rather than
    /// reproduced by removing fonts, so the test says what it is testing and
    /// does not depend on which faces the machine running it happens to have.
    fn document() -> ScriptDocument {
        let mut document = ScriptDocument::from_html(
            r#"<style>
                 button { border: 0; padding: 0; margin: 0; font-size: 0; }
                 #padded { padding: 8px 12px; }
               </style>
               <button id="flat">Open dialog</button>
               <button id="padded">Open dialog</button>"#,
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        document
    }

    fn node_id(document: &ScriptDocument, selector: &str) -> u64 {
        document
            .inner()
            .query_selector(selector)
            .unwrap()
            .expect("the fixture should contain the selector")
            .as_u64()
    }

    #[test]
    fn a_control_with_no_area_is_still_pressable() {
        let mut document = document();
        let flat = node_id(&document, "#flat");
        // The premise: this really is the degenerate case, not an accident of
        // the fixture. Without it a passing test proves nothing.
        let rect = document
            .inner()
            .get_client_bounding_rect(blitz_dom::NodeId::from_u64(flat));
        assert!(
            rect.is_some_and(|rect| rect.width == 0.0 || rect.height == 0.0),
            "the flat button should lay out with no area"
        );

        activate_agent_node(&mut document, flat, 1)
            .expect("a click is dispatched at the node, so it needs no area");
    }

    #[test]
    fn a_control_with_no_area_still_refuses_a_pointer() {
        let mut document = document();
        let flat = node_id(&document, "#flat");
        // The other half of the split: `Hover` moves a real pointer to a
        // coordinate, and a flat box has no honest one. Keeping this failing
        // is the point -- it is why the area requirement became a parameter
        // rather than being deleted.
        let error = hover_agent_node(&mut document, flat)
            .expect_err("hover needs a box a pointer can land in");
        assert!(
            error.message.contains("no layout box"),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn a_hidden_control_is_still_refused() {
        // The area gate was doing double duty. Deleting it for clicks must not
        // have opened the door to a control the author hid, which is what the
        // visibility gate above it is for.
        let mut document = ScriptDocument::from_html(
            r#"<button id="gone" style="display: none">Open dialog</button>"#,
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        let gone = node_id(&document, "#gone");
        let error = activate_agent_node(&mut document, gone, 1)
            .expect_err("a display:none control must stay unpressable");
        assert!(
            error.message.contains("not visible"),
            "unexpected error: {error:?}"
        );
    }
}

/// What the semantic tree says about one small document.
///
/// Every test here reads the tree through `inspect_document`, which is the
/// entry point a headless QA host calls, rather than through the naming
/// helpers directly. A role or a name that is right inside the crate and wrong
/// by the time it reaches the socket is the defect these were written for.
#[cfg(all(test, feature = "agent-control", unix))]
mod semantic_tests {
    use super::*;
    use blitz_dom::DocumentConfig;

    /// One document, reproducing every naming and role defect this module
    /// covers. Kept whole rather than split per test so a fix that repairs one
    /// case by breaking another is caught by the next assertion down.
    const REPRO: &str = r#"<table aria-label="named table">
  <thead><tr><th scope="col">Crate</th></tr></thead>
  <tbody><tr><td>worktable</td></tr></tbody>
</table>
<section aria-label="a named section"><p>text</p></section>
<datalist id="t"><option value="u64"></option></datalist>
<pre>plain text in a pre</pre>
<div role="tooltip">tooltip text</div>"#;

    fn tree(html: &str) -> Vec<SemanticNode> {
        let mut document = ScriptDocument::from_html(html, DocumentConfig::default());
        document.inner_mut().resolve(0.0);
        match inspect_document(&mut document, None, 0, 1) {
            DebugResponse::AgentSnapshot(snapshot) => snapshot.nodes,
            other => panic!("inspection did not answer with a tree: {other:?}"),
        }
    }

    fn roles<'a>(nodes: &'a [SemanticNode], role: &str) -> Vec<&'a SemanticNode> {
        nodes.iter().filter(|node| node.role == role).collect()
    }

    fn names(nodes: &[SemanticNode], role: &str) -> Vec<String> {
        roles(nodes, role)
            .into_iter()
            .map(|node| node.name.clone())
            .collect()
    }

    #[test]
    fn a_header_cell_is_a_header() {
        let nodes = tree(REPRO);
        assert_eq!(
            roles(&nodes, "columnheader").len(),
            1,
            "a `<th scope=\"col\">` is a column header, not an ordinary cell"
        );
        assert_eq!(roles(&nodes, "cell").len(), 1, "only the `<td>` is a cell");
    }

    #[test]
    fn a_cell_is_named_by_what_it_holds() {
        let nodes = tree(REPRO);
        assert_eq!(
            names(&nodes, "cell"),
            vec!["worktable".to_string()],
            "a data cell's text is its accessible name, so a table of values is readable"
        );
        assert_eq!(
            names(&nodes, "columnheader"),
            vec!["Crate".to_string()],
            "a header cell is named by its content too"
        );
    }

    #[test]
    fn a_row_is_named_by_its_cells() {
        let nodes = tree(REPRO);
        assert_eq!(
            names(&nodes, "row"),
            vec!["Crate".to_string(), "worktable".to_string()],
            "a row is named from its contents, which is what makes a table row addressable"
        );
    }

    #[test]
    fn a_row_scoped_header_is_a_row_header() {
        let nodes = tree(r#"<table><tr><th scope="row">Crate</th><td>worktable</td></tr></table>"#);
        assert_eq!(
            roles(&nodes, "rowheader").len(),
            1,
            "`scope=\"row\"` makes a header describe its row, which is what blitz-dom reports"
        );
    }

    #[test]
    fn a_named_section_is_a_region() {
        let nodes = tree(REPRO);
        assert_eq!(
            names(&nodes, "region"),
            vec!["a named section".to_string()],
            "a `<section>` with an accessible name is a landmark, not a wrapper"
        );
    }

    /// Both halves of a responsive label, the way Tailwind writes one.
    ///
    /// `sm:hidden` on the short one and `hidden sm:inline` on the long one is a
    /// single control that says "Book" on a phone and "Book a diagnostic" on a
    /// laptop. Exactly one of them is rendered at any width, and folding both
    /// into the name produced "Book Book a diagnostic", which matches nothing a
    /// person can see and nothing a check can be written against.
    const RESPONSIVE_LABEL: &str = r#"<button>
         <span>Book</span>
         <span style="display: none">Book a diagnostic</span>
       </button>"#;

    #[test]
    fn a_name_skips_a_subtree_that_is_not_rendered() {
        let nodes = tree(RESPONSIVE_LABEL);
        assert_eq!(
            names(&nodes, "button"),
            vec!["Book".to_string()],
            "a display:none subtree contributes nothing to a name"
        );
    }

    #[test]
    fn a_name_skips_a_subtree_that_is_hidden_or_aria_hidden() {
        let nodes = tree(
            r#"<button>Save<span style="visibility: hidden">draft</span><span aria-hidden="true">now</span></button>"#,
        );
        assert_eq!(
            names(&nodes, "button"),
            vec!["Save".to_string()],
            "an invisible box and an aria-hidden one are both outside the name"
        );
    }

    #[test]
    fn a_label_skips_what_it_does_not_show() {
        let nodes = tree(
            r#"<label for="url">Endpoint<span style="display: none"> (advanced)</span></label>
               <input id="url" type="text">"#,
        );
        assert_eq!(
            names(&nodes, "textbox"),
            vec!["Endpoint".to_string()],
            "the label a control is named by is read the same way a name is"
        );
    }

    #[test]
    fn a_value_only_option_is_named_by_its_value() {
        let nodes = tree(REPRO);
        assert_eq!(
            names(&nodes, "option"),
            vec!["u64".to_string()],
            "a `<datalist>` option carries its text in `value`, which is what a browser announces"
        );
    }

    #[test]
    fn an_options_label_attribute_wins_over_its_text() {
        let nodes = tree(r#"<select><option label="Sixty four bits">u64</option></select>"#);
        assert_eq!(
            names(&nodes, "option"),
            vec!["Sixty four bits".to_string()],
            "HTML gives `label` precedence over an option's own text"
        );
    }

    #[test]
    fn a_tooltip_says_what_it_says() {
        let nodes = tree(REPRO);
        assert_eq!(
            names(&nodes, "tooltip"),
            vec!["tooltip text".to_string()],
            "a tooltip is named by its contents, and its contents are the whole point of it"
        );
    }

    #[test]
    fn an_unnamed_section_is_not_a_region() {
        let nodes = tree("<section><p>text</p></section>");
        assert!(
            roles(&nodes, "region").is_empty(),
            "HTML-AAM gives an unnamed section no landmark role, so a page of \
             plain sections does not grow a landmark per wrapper"
        );
    }
}
