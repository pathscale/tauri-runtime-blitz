use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::thread::{ThreadId, current};

#[cfg(not(feature = "hybrid-renderer"))]
use anyrender_vello::VelloWindowRenderer;
#[cfg(feature = "hybrid-renderer")]
use anyrender_vello_hybrid::VelloHybridWindowRenderer as VelloWindowRenderer;
use blitz_script::ScriptDocument;
use blitz_shell::{
    BlitzApplication, BlitzShellEvent, BlitzShellProxy, WindowConfig as BlitzShellWindowConfig,
    create_default_event_loop,
};
use tauri_runtime::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use tauri_runtime::monitor::Monitor;
use tauri_runtime::webview::{DetachedWebview, PendingWebview};
use tauri_runtime::window::{
    DetachedWindow, DetachedWindowWebview, PendingWindow, RawWindow, WindowId,
};
use tauri_runtime::{
    DeviceEventFilter, Error, EventLoopProxy, RunEvent, Runtime, RuntimeHandle, RuntimeInitArgs,
    UserEvent,
};
use tauri_utils::Theme;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent as WinitWindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::monitor::Fullscreen;
use winit::window::{WindowAttributes, WindowButtons, WindowLevel};

#[cfg(target_os = "macos")]
use winit::platform::macos::{ApplicationHandlerExtMacOS, WindowAttributesMacOS};

#[cfg(all(feature = "agent-control", unix, test))]
use crate::agent_control_server::CONTROL_TEST_LOCK;
#[cfg(all(feature = "diagnostics", unix))]
use crate::control_protocol::{
    DebugSnapshot, DiagnosticsRequest, FrameMetrics, FrameWindowMetrics, RendererMetrics,
    RevisionSet, ScriptMetrics, ScriptSource, SnapshotCost, SnapshotRequest, TimingStats,
};
use crate::window_dispatch::{BlitzWindowDispatcher, NativeWindowState};
use crate::{
    BlitzWebviewDispatcher, BlitzWindowBuilder, PreparedBlitzWebview, prepare_pending_webview,
};
#[cfg(all(feature = "agent-control", unix))]
use crate::{
    agent_control_server::{AgentControlServer, ControlBridge, ControlBridgeRequest},
    control_protocol::{
        AgentAction, AgentControlRequest, AgentSnapshot, DebugError, DebugResponse, InputCommand,
        KeyPhase, Modifiers as ControlModifiers, PointerPhase, SemanticNode,
    },
};
#[cfg(all(feature = "agent-control", unix))]
use blitz_dom::Document;
#[cfg(all(feature = "agent-control", unix))]
use blitz_traits::events::{
    BlitzImeEvent, BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, BlitzWheelDelta,
    BlitzWheelEvent, DomEvent, DomEventData, KeyState, MouseEventButton, MouseEventButtons, Point,
    PointerCoords, PointerDetails, UiEvent,
};
#[cfg(all(feature = "diagnostics", unix))]
use blitz_traits::node_id::NodeId;
#[cfg(all(feature = "agent-control", unix))]
use keyboard_types::{Code, Key, Location, Modifiers as KeyboardModifiers};

type DocumentFactory = dyn Fn(&str) -> Result<ScriptDocument, String> + Send + Sync + 'static;
type RuntimeTrace = dyn Fn(&str) + Send + Sync + 'static;
#[cfg(all(feature = "agent-control", unix))]
type AgentControlHandler = dyn Fn(AgentControlRequest) -> DebugResponse + Send + Sync + 'static;
#[cfg(all(feature = "diagnostics", unix))]
type DiagnosticsHandler = dyn Fn(DiagnosticsRequest) -> DebugResponse + Send + Sync + 'static;

static DOCUMENT_FACTORY: OnceLock<RwLock<Option<Arc<DocumentFactory>>>> = OnceLock::new();
static RUNTIME_TRACE: OnceLock<RwLock<Option<Arc<RuntimeTrace>>>> = OnceLock::new();
#[cfg(all(feature = "agent-control", unix))]
static AGENT_CONTROL_HANDLER: OnceLock<RwLock<Option<Arc<AgentControlHandler>>>> = OnceLock::new();
#[cfg(all(feature = "agent-control", unix))]
static AGENT_CONTROL_RUNTIME: OnceLock<Mutex<Option<Weak<Mutex<AgentControlRuntime>>>>> =
    OnceLock::new();
#[cfg(all(feature = "diagnostics", unix))]
static DIAGNOSTICS_HANDLER: OnceLock<RwLock<Option<Arc<DiagnosticsHandler>>>> = OnceLock::new();

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
fn diagnostic_style_row(
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

    Some(serde_json::json!({
        "nodeId": node.id,
        "color": hex(current),
        "backgroundColor": hex(
            styles.clone_background_color().resolve_to_absolute(&current),
        ),
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

#[cfg(all(feature = "diagnostics", unix))]
fn diagnostic_layout_row(
    document: &blitz_dom::BaseDocument,
    node: &SemanticNode,
) -> Option<serde_json::Value> {
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
    Some(serde_json::json!({
        "nodeId": node.id,
        "bounds": bounds,
        "scrollOffset": [
            unzoom(scroll_offset.x as f32),
            unzoom(scroll_offset.y as f32)
        ],
        "clientSize": [
            unzoom(layout.size.width),
            unzoom(layout.size.height)
        ],
        "scrollSize": [
            unzoom(layout.size.width + layout.scroll_width()),
            unzoom(layout.size.height + layout.scroll_height())
        ],
        "scrollRange": [
            unzoom(layout.scroll_width()),
            unzoom(layout.scroll_height())
        ]
    }))
}

thread_local! {
    static CURRENT_BLITZ_APPLICATION: std::cell::Cell<*const ()> = const {
        std::cell::Cell::new(std::ptr::null())
    };
}

struct CurrentApplicationGuard;

impl Drop for CurrentApplicationGuard {
    fn drop(&mut self) {
        CURRENT_BLITZ_APPLICATION.with(|current| current.set(std::ptr::null()));
    }
}

/// Set the document source used when Tauri asks the runtime to create a webview.
///
/// The first native preview uses an embedded production bundle. Protocol-backed document loading
/// can replace this factory without changing the runtime or dispatcher path.
pub fn set_document_factory(
    factory: impl Fn(&str) -> Result<ScriptDocument, String> + Send + Sync + 'static,
) {
    *DOCUMENT_FACTORY
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap() = Some(Arc::new(factory));
}

/// Install lifecycle tracing for embedders diagnosing native runtime startup.
pub fn set_runtime_trace(trace: impl Fn(&str) + Send + Sync + 'static) {
    *RUNTIME_TRACE
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap() = Some(Arc::new(trace));
}

/// Install the UI-thread handler used by the local agent-control socket.
#[cfg(all(feature = "agent-control", unix))]
pub fn set_agent_control_handler(
    handler: impl Fn(AgentControlRequest) -> DebugResponse + Send + Sync + 'static,
) {
    *AGENT_CONTROL_HANDLER
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap() = Some(Arc::new(handler));
}

#[cfg(all(feature = "agent-control", unix))]
struct AgentControlRuntime {
    bridge: ControlBridge,
    server: Option<AgentControlServer>,
}

/// Enable or disable the complete local agent-control interface at runtime.
///
/// Disabled means no listener and no discovery descriptor. The runtime starts
/// disabled; embedders opt in after loading their owner-controlled setting.
#[cfg(all(feature = "agent-control", unix))]
pub fn set_agent_control_enabled(enabled: bool) -> std::io::Result<()> {
    let runtime = AGENT_CONTROL_RUNTIME
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .as_ref()
        .and_then(Weak::upgrade)
        .ok_or_else(|| std::io::Error::other("the Blitz runtime is not initialized"))?;
    let mut runtime = runtime.lock().unwrap();
    match (enabled, runtime.server.is_some()) {
        (true, false) => runtime.server = Some(AgentControlServer::start(runtime.bridge.clone())?),
        (false, true) => runtime.server = None,
        _ => {}
    }
    Ok(())
}

/// Whether the complete local agent-control interface is currently listening.
#[cfg(all(feature = "agent-control", unix))]
pub fn agent_control_enabled() -> bool {
    AGENT_CONTROL_RUNTIME
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .as_ref()
        .and_then(Weak::upgrade)
        .is_some_and(|runtime| runtime.lock().unwrap().server.is_some())
}

/// Apply both debug capabilities as one runtime category decision.
///
/// Deep profiling is always stopped when inspection/control is off, even if an
/// embedder passes an inconsistent pair of booleans. Disabling it also clears
/// retained samples so a later trace starts at its activation boundary.
#[cfg(all(feature = "agent-control", unix))]
pub fn apply_runtime_debug_options(
    options: blitz_traits::profiling::DebugOptions,
) -> std::io::Result<()> {
    // Permission first, then the server. The server takes a profiling session
    // as it starts listening, and that session is only available once sampling
    // is permitted: starting it first meant the consumer asked for samples
    // before the answer could be yes, and collection never began.
    set_deep_profiling_permitted(options.effective_deep_profiling());
    set_agent_control_enabled(options.inspection_and_agent_control)?;
    Ok(())
}

/// Permit or forbid the intrusive performance collectors shipped with the
/// runtime. Inspection/control remains independently selectable.
///
/// This grants permission and starts nothing. Collection runs only while a
/// consumer holds a session from [`begin_deep_profiling`], because samples with
/// no reader have no value and are not free: the readers here are the inspector
/// and `blitz-bench`, and both are separate processes, so the ordinary case for
/// an embedder that merely had the setting on was to collect for nobody.
///
/// Forbidding also releases retained samples, so a capability that is off holds
/// nothing.
#[cfg(feature = "agent-control")]
pub fn set_deep_profiling_permitted(permitted: bool) {
    blitz_shell::set_deep_profiling_permitted(permitted);
}

/// Ask for samples for as long as the returned session is held.
///
/// `None` when the profile does not permit sampling. An out-of-process consumer
/// cannot hold this directly, so the request handler serving it holds one for
/// the life of the request.
#[cfg(feature = "agent-control")]
#[must_use = "sampling stops as soon as the session is dropped"]
pub fn begin_deep_profiling() -> Option<blitz_shell::DeepProfilingSession> {
    blitz_shell::begin_deep_profiling()
}

/// Whether the owner has permitted sampling, regardless of any consumer.
#[cfg(feature = "agent-control")]
pub fn deep_profiling_permitted() -> bool {
    blitz_traits::profiling::deep_profiling_permitted()
}

/// Whether collection is actually running: permitted, and a consumer attached.
#[cfg(feature = "agent-control")]
pub fn deep_profiling_enabled() -> bool {
    blitz_traits::profiling::deep_profiling_enabled()
}

/// Install the UI-thread handler used by the feature-gated diagnostics tool.
#[cfg(all(feature = "diagnostics", unix))]
pub fn set_diagnostics_handler(
    handler: impl Fn(DiagnosticsRequest) -> DebugResponse + Send + Sync + 'static,
) {
    *DIAGNOSTICS_HANDLER
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap() = Some(Arc::new(handler));
}

fn runtime_trace(message: &str) {
    let callback = RUNTIME_TRACE
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap()
        .clone();
    if let Some(callback) = callback {
        callback(message);
    }
}

fn create_document(url: &str) -> tauri_runtime::Result<ScriptDocument> {
    let factory = DOCUMENT_FACTORY
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap()
        .clone()
        .ok_or_else(|| {
            Error::CreateWebview(
                std::io::Error::other("Blitz document factory is not configured").into(),
            )
        })?;
    factory(url).map_err(|error| Error::CreateWebview(std::io::Error::other(error).into()))
}

type Task = Box<dyn FnOnce() + Send + 'static>;

enum RuntimeMessage<T: UserEvent> {
    Task(Task),
    UserEvent(T),
    RequestExit(i32),
    #[cfg(all(feature = "agent-control", unix))]
    Control {
        request: ControlBridgeRequest,
        response: tokio::sync::oneshot::Sender<DebugResponse>,
    },
}

pub(crate) struct BlitzRuntimeContext<T: UserEvent> {
    sender: Sender<RuntimeMessage<T>>,
    proxy: BlitzShellProxy,
    next_window_id: Arc<AtomicU32>,
    next_window_event_id: Arc<AtomicU32>,
    windows: Arc<Mutex<HashMap<WindowId, Arc<NativeWindowState>>>>,
    main_thread_id: ThreadId,
}

impl<T: UserEvent> Clone for BlitzRuntimeContext<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            proxy: self.proxy.clone(),
            next_window_id: Arc::clone(&self.next_window_id),
            next_window_event_id: Arc::clone(&self.next_window_event_id),
            windows: Arc::clone(&self.windows),
            main_thread_id: self.main_thread_id,
        }
    }
}

impl<T: UserEvent> fmt::Debug for BlitzRuntimeContext<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlitzRuntimeContext")
            .field("window_count", &self.windows.lock().unwrap().len())
            .finish()
    }
}

impl<T: UserEvent> BlitzRuntimeContext<T> {
    pub(crate) fn next_window_event_id(&self) -> tauri_runtime::WindowEventId {
        self.next_window_event_id.fetch_add(1, Ordering::Relaxed)
    }

    fn next_window_id(&self) -> WindowId {
        self.next_window_id.fetch_add(1, Ordering::Relaxed).into()
    }

    fn send(&self, message: RuntimeMessage<T>) -> tauri_runtime::Result<()> {
        self.sender
            .send(message)
            .map_err(|_| Error::FailedToSendMessage)?;
        self.proxy.wake_up();
        Ok(())
    }

    /// Queued tasks only run when the event loop next drains them, so a caller
    /// already on the main thread that waits for its task would wait forever:
    /// the drain is that thread's own job. Tauri does exactly that. Menu
    /// construction posts the work here and blocks on the reply, and `setup`
    /// runs on the main thread inside `can_create_surfaces`, so queuing there
    /// hangs the app before the first window. Running inline is what
    /// tauri-runtime-wry does, for the same reason.
    pub(crate) fn run_on_main_thread(
        &self,
        task: impl FnOnce() + Send + 'static,
    ) -> tauri_runtime::Result<()> {
        if current().id() == self.main_thread_id {
            task();
            return Ok(());
        }
        self.send(RuntimeMessage::Task(Box::new(task)))
    }

    pub(crate) fn close_window(&self, id: WindowId) -> tauri_runtime::Result<()> {
        let native_id = self
            .windows
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|state| {
                state
                    .native
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|window| window.id())
            })
            .ok_or(Error::WindowNotFound)?;
        self.proxy.send_event(BlitzShellEvent::CloseWindow {
            window_id: native_id,
        });
        Ok(())
    }

    pub(crate) fn create_window<F: Fn(RawWindow) + Send + 'static>(
        &self,
        pending: PendingWindow<T, BlitzRuntime<T>>,
        after_window_creation: Option<F>,
    ) -> tauri_runtime::Result<DetachedWindow<T, BlitzRuntime<T>>> {
        if current().id() != self.main_thread_id {
            return Err(Error::FailedToSendMessage);
        }
        CURRENT_BLITZ_APPLICATION.with(|current| {
            let application = current.get();
            if application.is_null() {
                return Err(Error::EventLoopClosed);
            }
            // The pointer is installed only while Tauri's main-thread callback is running. The
            // callback does not otherwise borrow the Blitz application, and this runtime clears
            // the pointer before returning to the event loop.
            let application =
                unsafe { &*(application as *const RefCell<BlitzApplication<VelloWindowRenderer>>) };
            let mut application = application
                .try_borrow_mut()
                .map_err(|_| Error::FailedToSendMessage)?;
            register_window(self, &mut application, pending, after_window_creation)
        })
    }

    pub(crate) fn create_webview(
        &self,
        _window_id: WindowId,
        _pending: PendingWebview<T, BlitzRuntime<T>>,
    ) -> tauri_runtime::Result<DetachedWebview<T, BlitzRuntime<T>>> {
        Err(Error::CreateWindow)
    }
}

#[derive(Debug, Clone)]
pub struct BlitzEventLoopProxy<T: UserEvent> {
    context: BlitzRuntimeContext<T>,
}

impl<T: UserEvent> EventLoopProxy<T> for BlitzEventLoopProxy<T> {
    fn send_event(&self, event: T) -> tauri_runtime::Result<()> {
        self.context.send(RuntimeMessage::UserEvent(event))
    }
}

#[derive(Debug, Clone)]
pub struct BlitzRuntimeHandle<T: UserEvent> {
    context: BlitzRuntimeContext<T>,
}

impl<T: UserEvent> RuntimeHandle<T> for BlitzRuntimeHandle<T> {
    type Runtime = BlitzRuntime<T>;

    fn create_proxy(&self) -> BlitzEventLoopProxy<T> {
        BlitzEventLoopProxy {
            context: self.context.clone(),
        }
    }

    #[cfg(target_os = "macos")]
    fn set_activation_policy(
        &self,
        _activation_policy: tauri_runtime::ActivationPolicy,
    ) -> tauri_runtime::Result<()> {
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn set_dock_visibility(&self, _visible: bool) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn request_exit(&self, code: i32) -> tauri_runtime::Result<()> {
        runtime_trace("runtime exit requested");
        let result = self.context.send(RuntimeMessage::RequestExit(code));
        runtime_trace("runtime exit request enqueued");
        result
    }

    fn create_window<F: Fn(RawWindow) + Send + 'static>(
        &self,
        pending: PendingWindow<T, Self::Runtime>,
        after_window_creation: Option<F>,
    ) -> tauri_runtime::Result<DetachedWindow<T, Self::Runtime>> {
        self.context.create_window(pending, after_window_creation)
    }

    fn create_webview(
        &self,
        window_id: WindowId,
        pending: PendingWebview<T, Self::Runtime>,
    ) -> tauri_runtime::Result<DetachedWebview<T, Self::Runtime>> {
        self.context.create_webview(window_id, pending)
    }

    fn run_on_main_thread<F: FnOnce() + Send + 'static>(
        &self,
        task: F,
    ) -> tauri_runtime::Result<()> {
        self.context.run_on_main_thread(task)
    }

    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        Err(raw_window_handle::HandleError::NotSupported)
    }

    fn primary_monitor(&self) -> Option<Monitor> {
        None
    }

    fn monitor_from_point(&self, _x: f64, _y: f64) -> Option<Monitor> {
        None
    }

    fn available_monitors(&self) -> Vec<Monitor> {
        Vec::new()
    }

    fn cursor_position(&self) -> tauri_runtime::Result<PhysicalPosition<f64>> {
        Err(Error::FailedToGetCursorPosition)
    }

    fn set_theme(&self, _theme: Option<Theme>) {}

    #[cfg(target_os = "macos")]
    fn show(&self) -> tauri_runtime::Result<()> {
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn hide(&self) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_device_event_filter(&self, _filter: DeviceEventFilter) {}

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn fetch_data_store_identifiers<F: FnOnce(Vec<[u8; 16]>) + Send + 'static>(
        &self,
        _callback: F,
    ) -> tauri_runtime::Result<()> {
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn remove_data_store<F: FnOnce(tauri_runtime::Result<()>) + Send + 'static>(
        &self,
        _uuid: [u8; 16],
        _callback: F,
    ) -> tauri_runtime::Result<()> {
        Ok(())
    }
}

struct RuntimeApplication<T: UserEvent> {
    blitz: RefCell<BlitzApplication<VelloWindowRenderer>>,
    receiver: Receiver<RuntimeMessage<T>>,
    callback: Option<Box<dyn FnMut(RunEvent<T>)>>,
    ready: bool,
    #[cfg(all(feature = "agent-control", unix))]
    _agent_control: Arc<Mutex<AgentControlRuntime>>,
    #[cfg(all(feature = "agent-control", unix))]
    agent_revision: u64,
    #[cfg(all(feature = "agent-control", unix))]
    agent_pointer: (f32, f32),
    #[cfg(all(feature = "agent-control", unix))]
    agent_buttons: MouseEventButtons,
}

impl<T: UserEvent> RuntimeApplication<T> {
    fn emit(&mut self, event: RunEvent<T>) {
        if let Some(callback) = &mut self.callback {
            callback(event);
        }
    }

    #[cfg(all(feature = "agent-control", unix))]
    fn agent_document(&mut self) -> Option<&mut ScriptDocument> {
        self.blitz
            .get_mut()
            .windows
            .values_mut()
            .find_map(|view| view.try_downcast_doc_mut::<ScriptDocument>())
    }

    #[cfg(all(feature = "agent-control", unix))]
    fn handle_builtin_agent(&mut self, request: AgentControlRequest) -> DebugResponse {
        match request {
            AgentControlRequest::Inspect { root, max_depth } => {
                self.agent_revision += 1;
                let revision = self.agent_revision;
                let Some(document) = self.agent_document() else {
                    return control_error("documentUnavailable", "no active script document");
                };
                inspect_document(document, root, max_depth, revision)
            }
            AgentControlRequest::Act(action) => match self.perform_agent_action(action) {
                Ok(()) => {
                    self.agent_revision += 1;
                    DebugResponse::Ack
                }
                Err(error) => DebugResponse::Error(error),
            },
            AgentControlRequest::Relaunch => AGENT_CONTROL_HANDLER
                .get_or_init(|| RwLock::new(None))
                .read()
                .unwrap()
                .clone()
                .map(|handler| handler(AgentControlRequest::Relaunch))
                .unwrap_or_else(|| match relaunch_current_process() {
                    Ok(()) => DebugResponse::Ack,
                    Err(error) => DebugResponse::Error(DebugError {
                        code: "relaunchFailed".into(),
                        message: error.to_string(),
                    }),
                }),
            AgentControlRequest::Quit => DebugResponse::Ack,
        }
    }

    #[cfg(all(feature = "diagnostics", unix))]
    fn handle_builtin_diagnostics(&mut self, request: DiagnosticsRequest) -> DebugResponse {
        match request {
            DiagnosticsRequest::Snapshot(request) => self
                .collect_diagnostics(request)
                .map(DebugResponse::Snapshot)
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::Metrics => self
                .collect_diagnostics(SnapshotRequest {
                    include_dom: false,
                    include_layout: false,
                    include_computed_style: false,
                })
                .map(|snapshot| DebugResponse::Metrics(snapshot.metrics))
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::WaitForIdle => self
                .collect_diagnostics(SnapshotRequest {
                    include_dom: false,
                    include_layout: false,
                    include_computed_style: false,
                })
                .map(|snapshot| DebugResponse::Idle(snapshot.revisions))
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::Capture(request) => self
                .capture_image(request)
                .map(DebugResponse::Captured)
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::Observe { .. } => control_error(
                "streamingUnavailable",
                "diagnostic subscriptions are not implemented; request snapshots or metrics",
            ),
        }
    }

    /// Draw the document offscreen and hand back the pixels.
    ///
    /// The point of this is that it is the *same* paint. `blitz_paint::paint_scene`
    /// is the entry point the window renderer uses, so a capture cannot agree
    /// with a broken frame or disagree with a good one: whatever the window
    /// would show is what lands in this buffer. Anything less than that is a
    /// second implementation of the renderer, and a test against a second
    /// implementation only proves the two implementations match.
    ///
    /// A node capture crops rather than re-laying-out. Laying the subtree out in
    /// isolation would answer a different question - "can this element draw on
    /// its own" - when the one worth asking is whether it drew *here*, inside
    /// the page that actually exists, with the styles it actually inherited.
    #[cfg(all(feature = "diagnostics", unix))]
    fn capture_image(
        &mut self,
        request: blitz_control_protocol::CaptureRequest,
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
        let script_document = self
            .agent_document()
            .ok_or_else(|| debug_error("documentUnavailable", "no active script document"))?;

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

        let width = ((crop_width * f64::from(scale)).round() as u32).max(1);
        let height = ((crop_height * f64::from(scale)).round() as u32).max(1);
        // A whole window at 8x is gigabytes; refuse rather than exhaust memory
        // on a machine that is probably already running the app under test.
        const MAX_PIXELS: u64 = 64 * 1024 * 1024;
        if u64::from(width) * u64::from(height) > MAX_PIXELS {
            return Err(debug_error(
                "captureTooLarge",
                &format!("{width}x{height} exceeds the capture ceiling; lower the scale"),
            ));
        }

        /*
         * Paint the whole document, then cut the region out of the buffer.
         *
         * `paint_scene` takes offsets, but they move the scene inside a surface
         * that is still sized to the full viewport, so drawing a 66x64 button
         * into a 66x64 target put every pixel of it outside the surface and
         * returned solid black. Cropping afterwards depends on no such
         * semantics: whatever the renderer drew for the real window is what
         * gets cut, which is the property this whole call exists to have.
         */
        let full_pixel_width = ((f64::from(full_width) * f64::from(scale)).round() as u32).max(1);
        let full_pixel_height = ((f64::from(full_height) * f64::from(scale)).round() as u32).max(1);
        let mut document = script_document.inner_mut();
        let mut renderer =
            anyrender_vello_cpu::VelloCpuImageRenderer::new(full_pixel_width, full_pixel_height);
        let mut full_rgba =
            Vec::with_capacity((full_pixel_width as usize) * (full_pixel_height as usize) * 4);
        renderer.render_to_vec(
            |scene| {
                blitz_paint::paint_scene(
                    scene,
                    &mut document,
                    f64::from(scale),
                    full_pixel_width,
                    full_pixel_height,
                    0,
                    0,
                );
            },
            &mut full_rgba,
        );

        // The crop, clamped to the surface: a node partly offscreen yields the
        // part that exists rather than an error or a panic.
        let left = ((crop_x * f64::from(scale)).round().max(0.0) as u32).min(full_pixel_width);
        let top = ((crop_y * f64::from(scale)).round().max(0.0) as u32).min(full_pixel_height);
        let width = width.min(full_pixel_width.saturating_sub(left)).max(1);
        let height = height.min(full_pixel_height.saturating_sub(top)).max(1);

        let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for row in 0..height {
            let source = (((top + row) as usize) * (full_pixel_width as usize) + left as usize) * 4;
            let take = (width as usize) * 4;
            if source + take <= full_rgba.len() {
                rgba.extend_from_slice(&full_rgba[source..source + take]);
            } else {
                rgba.resize(rgba.len() + take, 0);
            }
        }

        Ok(blitz_control_protocol::CapturedImage {
            width,
            height,
            rgba_base64: base64::engine::general_purpose::STANDARD.encode(&rgba),
            node_id,
        })
    }

    #[cfg(all(feature = "diagnostics", unix))]
    fn collect_diagnostics(
        &mut self,
        request: SnapshotRequest,
    ) -> Result<DebugSnapshot, DebugError> {
        self.agent_revision += 1;
        let revision = self.agent_revision;
        let started = std::time::Instant::now();
        let document = self
            .agent_document()
            .ok_or_else(|| debug_error("documentUnavailable", "no active script document"))?;
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
        let nodes: Vec<SemanticNode> = inner
            .tree()
            .iter()
            .filter_map(|(id, node)| {
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
                            .map(|attribute| {
                                format!("{}={}", attribute.name.local, attribute.value)
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                    )
                } else {
                    semantic_value(element)
                };
                Some(SemanticNode {
                    id: id.as_u64(),
                    parent: semantic_parent(&inner, id, None).map(|id| id.as_u64()),
                    name: semantic_name(element, node, &role),
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
            serde_json::Value::Array(
                nodes
                    .iter()
                    .filter_map(|node| diagnostic_layout_row(&inner, node))
                    .collect(),
            )
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
    fn perform_agent_action(&mut self, action: AgentAction) -> Result<(), DebugError> {
        match action {
            AgentAction::Click { node_id } => {
                let document = self
                    .agent_document()
                    .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                self.agent_pointer = activate_agent_node(document, node_id, 1)?;
            }
            AgentAction::DoubleClick { node_id } => {
                let document = self
                    .agent_document()
                    .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                self.agent_pointer = activate_agent_node(document, node_id, 2)?;
            }
            AgentAction::Hover { node_id } => {
                let position = {
                    let document = self
                        .agent_document()
                        .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                    resolve_agent_node(document, node_id)?.1
                };
                self.agent_pointer = position;
                let document = self.agent_document().unwrap();
                document.handle_ui_event(UiEvent::PointerMove(pointer_event(
                    position,
                    MouseEventButton::Main,
                    MouseEventButtons::default(),
                    KeyboardModifiers::empty(),
                )));
            }
            AgentAction::SetValue { node_id, value } => {
                let node_id = blitz_dom::NodeId::from_u64(node_id);
                let document = self
                    .agent_document()
                    .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                set_agent_node_value(document, node_id, value)?;
            }
            AgentAction::ScrollIntoView { node_id } => {
                let node_id = blitz_dom::NodeId::from_u64(node_id);
                let document = self
                    .agent_document()
                    .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                if document.inner().get_node(node_id).is_none() {
                    return Err(debug_error("unknownNode", "node does not exist"));
                }
                document.inner_mut().scroll_to_node(node_id);
            }
            AgentAction::ScrollBy {
                node_id,
                delta_x,
                delta_y,
            } => {
                let node_id = blitz_dom::NodeId::from_u64(node_id);
                let document = self
                    .agent_document()
                    .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                if document.inner().get_node(node_id).is_none() {
                    return Err(debug_error("unknownNode", "node does not exist"));
                }
                document
                    .inner_mut()
                    .scroll_nearest_container_by(node_id, delta_x, delta_y);
            }
            AgentAction::Input(input) => self.perform_agent_input(input)?,
        }
        Ok(())
    }

    #[cfg(all(feature = "agent-control", unix))]
    fn perform_agent_input(&mut self, input: InputCommand) -> Result<(), DebugError> {
        match input {
            InputCommand::Key {
                phase,
                key,
                code,
                modifiers,
            } => {
                let parsed_key = key.parse::<Key>().unwrap_or(Key::Character(key));
                let parsed_code = code.parse::<Code>().unwrap_or(Code::Unidentified);
                let event = key_event(
                    phase,
                    parsed_key,
                    parsed_code,
                    keyboard_modifiers(modifiers),
                );
                let document = self
                    .agent_document()
                    .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                document.handle_ui_event(match phase {
                    KeyPhase::Down => UiEvent::KeyDown(event),
                    KeyPhase::Up => UiEvent::KeyUp(event),
                });
            }
            InputCommand::Pointer {
                phase,
                x,
                y,
                button,
                modifiers,
            } => {
                let button = mouse_button(button)?;
                self.agent_pointer = (x as f32, y as f32);
                match phase {
                    PointerPhase::Down => self.agent_buttons.insert(button.into()),
                    PointerPhase::Up | PointerPhase::Cancel => {
                        self.agent_buttons.remove(button.into())
                    }
                    PointerPhase::Move => {}
                }
                let event = pointer_event(
                    self.agent_pointer,
                    button,
                    self.agent_buttons,
                    keyboard_modifiers(modifiers),
                );
                let document = self
                    .agent_document()
                    .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                document.handle_ui_event(match phase {
                    PointerPhase::Move => UiEvent::PointerMove(event),
                    PointerPhase::Down => UiEvent::PointerDown(event),
                    PointerPhase::Up => UiEvent::PointerUp(event),
                    PointerPhase::Cancel => UiEvent::PointerCancel(event),
                });
            }
            InputCommand::Wheel {
                delta_x,
                delta_y,
                modifiers,
                ..
            } => {
                let coords = pointer_coords(self.agent_pointer);
                let hover_at = self.agent_pointer;
                let event = BlitzWheelEvent {
                    delta: BlitzWheelDelta::Pixels(delta_x, delta_y),
                    coords,
                    buttons: self.agent_buttons,
                    mods: keyboard_modifiers(modifiers),
                    element: Point::default(),
                };
                let document = self
                    .agent_document()
                    .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                // A wheel event targets the hovered node, and hover is resolved
                // by the shell from real cursor movement. An injected pointer
                // move never touches it, so an injected wheel had no target and
                // scrolled nothing, making remote wheel input look accepted
                // while the document remained unchanged.
                document.inner_mut().set_hover_to(hover_at.0, hover_at.1);
                document.handle_ui_event(UiEvent::Wheel(event));
            }
        }
        Ok(())
    }

    fn drain_runtime_messages(&mut self, event_loop: &dyn ActiveEventLoop) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                RuntimeMessage::Task(task) => task(),
                RuntimeMessage::UserEvent(event) => self.emit(RunEvent::UserEvent(event)),
                RuntimeMessage::RequestExit(code) => {
                    runtime_trace("runtime exit request draining");
                    self.emit(RunEvent::ExitRequested {
                        code: Some(code),
                        tx: channel().0,
                    });
                    self.emit(RunEvent::Exit);
                    event_loop.exit();
                    runtime_trace("native event loop exit flagged");
                }
                #[cfg(all(feature = "agent-control", unix))]
                RuntimeMessage::Control { request, response } => {
                    let result = match request {
                        ControlBridgeRequest::Agent(AgentControlRequest::Quit) => {
                            self.emit(RunEvent::ExitRequested {
                                code: Some(0),
                                tx: channel().0,
                            });
                            self.emit(RunEvent::Exit);
                            event_loop.exit();
                            DebugResponse::Ack
                        }
                        ControlBridgeRequest::Agent(AgentControlRequest::Relaunch) => {
                            let delegated = AGENT_CONTROL_HANDLER
                                .get_or_init(|| RwLock::new(None))
                                .read()
                                .unwrap()
                                .is_some();
                            let result = self.handle_builtin_agent(AgentControlRequest::Relaunch);
                            if !delegated && matches!(result, DebugResponse::Ack) {
                                self.emit(RunEvent::ExitRequested {
                                    code: Some(0),
                                    tx: channel().0,
                                });
                                self.emit(RunEvent::Exit);
                                event_loop.exit();
                            }
                            result
                        }
                        ControlBridgeRequest::Agent(request) => self.handle_builtin_agent(request),
                        #[cfg(feature = "diagnostics")]
                        ControlBridgeRequest::Diagnostics(request) => DIAGNOSTICS_HANDLER
                            .get_or_init(|| RwLock::new(None))
                            .read()
                            .unwrap()
                            .clone()
                            .map(|handler| handler(request.clone()))
                            .unwrap_or_else(|| self.handle_builtin_diagnostics(request)),
                    };
                    let _ = response.send(result);
                }
            }
        }
    }
}

#[cfg(all(feature = "diagnostics", unix))]
fn resident_bytes() -> Option<u64> {
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

/// Carry blitz-shell's timing summary onto the wire type.
///
/// The two structs are deliberately separate: the protocol is versioned by this
/// crate, while the shell type is free to grow fields that have no wire meaning.
#[cfg(all(feature = "diagnostics", unix))]
fn timing_stats(stats: blitz_shell::TimingStats) -> TimingStats {
    TimingStats {
        mean_ms: stats.mean_ms,
        p95_ms: stats.p95_ms,
        max_ms: stats.max_ms,
    }
}

impl<T: UserEvent> ApplicationHandler for RuntimeApplication<T> {
    #[cfg(target_os = "macos")]
    fn macos_handler(&mut self) -> Option<&mut dyn ApplicationHandlerExtMacOS> {
        Some(self)
    }

    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        runtime_trace("runtime can_create_surfaces entered");
        if !self.ready {
            self.ready = true;
            CURRENT_BLITZ_APPLICATION.with(|current| {
                current.set(&self.blitz as *const _ as *const ());
                let _guard = CurrentApplicationGuard;
                runtime_trace("Tauri Ready emission started");
                self.emit(RunEvent::Ready);
                runtime_trace("Tauri Ready emission completed");
            });
        }
        self.blitz.get_mut().can_create_surfaces(event_loop);
        runtime_trace("runtime can_create_surfaces completed");
    }

    fn destroy_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.blitz.get_mut().destroy_surfaces(event_loop);
    }

    fn resumed(&mut self, event_loop: &dyn ActiveEventLoop) {
        runtime_trace("runtime resumed entered");
        self.blitz.get_mut().resumed(event_loop);
        self.emit(RunEvent::Resumed);
        runtime_trace("runtime resumed completed");
    }

    fn suspended(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.blitz.get_mut().suspended(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: WinitWindowEvent,
    ) {
        self.blitz
            .get_mut()
            .window_event(event_loop, window_id, event);
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        // Embedder messages own the runtime lifecycle, so they run before
        // renderer wake work. In particular an exit request must not return to
        // AppKit's wait loop after a renderer callback consumed the wake.
        self.drain_runtime_messages(event_loop);
        if event_loop.exiting() {
            return;
        }
        self.blitz.get_mut().proxy_wake_up(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.drain_runtime_messages(event_loop);
        self.emit(RunEvent::MainEventsCleared);
        self.blitz.get_mut().about_to_wait(event_loop);
    }
}

#[cfg(target_os = "macos")]
impl<T: UserEvent> ApplicationHandlerExtMacOS for RuntimeApplication<T> {
    fn standard_key_binding(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: winit::window::WindowId,
        action: &str,
    ) {
        self.blitz
            .get_mut()
            .standard_key_binding(event_loop, window_id, action);
    }
}

pub struct BlitzRuntime<T: UserEvent = tauri::EventLoopMessage> {
    event_loop: winit::event_loop::EventLoop,
    application: RefCell<Option<RuntimeApplication<T>>>,
    context: BlitzRuntimeContext<T>,
}

impl<T: UserEvent> fmt::Debug for BlitzRuntime<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlitzRuntime")
            .field("context", &self.context)
            .finish()
    }
}

impl<T: UserEvent> BlitzRuntime<T> {
    fn prepared_webview(
        pending: PendingWebview<T, Self>,
    ) -> tauri_runtime::Result<PreparedBlitzWebview<T, Self>> {
        let document = create_document(&pending.url)?;
        Ok(prepare_pending_webview(pending, document))
    }
}

impl<T: UserEvent> Runtime<T> for BlitzRuntime<T> {
    type WindowDispatcher = BlitzWindowDispatcher<T>;
    type WebviewDispatcher = BlitzWebviewDispatcher<T, Self>;
    type Handle = BlitzRuntimeHandle<T>;
    type EventLoopProxy = BlitzEventLoopProxy<T>;

    fn new(_args: RuntimeInitArgs) -> tauri_runtime::Result<Self> {
        runtime_trace("BlitzRuntime::new entered");
        runtime_trace("native event-loop construction started");
        let event_loop = create_default_event_loop();
        runtime_trace("native event-loop construction completed");
        let (proxy, blitz_receiver) = BlitzShellProxy::new(event_loop.create_proxy());
        let (sender, receiver) = channel();
        let context = BlitzRuntimeContext {
            sender,
            proxy: proxy.clone(),
            next_window_id: Arc::new(AtomicU32::new(1)),
            next_window_event_id: Arc::new(AtomicU32::new(1)),
            windows: Arc::new(Mutex::new(HashMap::new())),
            main_thread_id: current().id(),
        };
        #[cfg(all(feature = "agent-control", unix))]
        let agent_control = {
            let control_context = context.clone();
            let bridge: ControlBridge = Arc::new(move |request| {
                let (response, receiver) = tokio::sync::oneshot::channel();
                let _ = control_context.send(RuntimeMessage::Control { request, response });
                receiver
            });
            // The embedder's persisted control state is applied later during Tauri
            // setup. This enable-only rescue must start earlier: when control
            // was switched off, setup is unreachable to the very automation
            // needed to switch it back on. It also lets QA attach while a large
            // application is still booting.
            let server = if std::env::args().any(|arg| arg == "--blitz-control") {
                Some(
                    AgentControlServer::start(bridge.clone())
                        .map_err(|error| Error::CreateWebview(Box::new(error)))?,
                )
            } else {
                None
            };
            let runtime = Arc::new(Mutex::new(AgentControlRuntime { bridge, server }));
            *AGENT_CONTROL_RUNTIME
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap() = Some(Arc::downgrade(&runtime));
            runtime
        };
        let blitz = BlitzApplication::new(proxy, blitz_receiver);
        #[cfg(feature = "debug-control")]
        let mut blitz = blitz;
        #[cfg(feature = "debug-control")]
        if let Some(mut controller) =
            blitz_script::DebugController::start_from_env(env!("CARGO_PKG_VERSION"))
                .map_err(|error| Error::CreateWebview(Box::new(error)))?
        {
            let (width, height) = debug_screenshot_size_from_env().unwrap_or((1344, 932));
            controller = controller.with_cpu_screenshot(width, height);
            blitz.set_debug_controller(controller);
        }
        let application = RuntimeApplication {
            blitz: RefCell::new(blitz),
            receiver,
            callback: None,
            ready: false,
            #[cfg(all(feature = "agent-control", unix))]
            _agent_control: agent_control,
            #[cfg(all(feature = "agent-control", unix))]
            agent_revision: 0,
            #[cfg(all(feature = "agent-control", unix))]
            agent_pointer: (0.0, 0.0),
            #[cfg(all(feature = "agent-control", unix))]
            agent_buttons: MouseEventButtons::default(),
        };
        runtime_trace("BlitzRuntime::new completed");
        Ok(Self {
            event_loop,
            application: RefCell::new(Some(application)),
            context,
        })
    }

    fn create_proxy(&self) -> Self::EventLoopProxy {
        BlitzEventLoopProxy {
            context: self.context.clone(),
        }
    }

    fn handle(&self) -> Self::Handle {
        BlitzRuntimeHandle {
            context: self.context.clone(),
        }
    }

    fn create_window<F: Fn(RawWindow) + Send + 'static>(
        &self,
        pending: PendingWindow<T, Self>,
        after_window_creation: Option<F>,
    ) -> tauri_runtime::Result<DetachedWindow<T, Self>> {
        let mut application = self.application.borrow_mut();
        let application = application.as_mut().ok_or(Error::EventLoopClosed)?;
        register_window(
            &self.context,
            application.blitz.get_mut(),
            pending,
            after_window_creation,
        )
    }

    fn create_webview(
        &self,
        _window_id: WindowId,
        _pending: PendingWebview<T, Self>,
    ) -> tauri_runtime::Result<DetachedWebview<T, Self>> {
        Err(Error::CreateWindow)
    }

    fn primary_monitor(&self) -> Option<Monitor> {
        None
    }

    fn monitor_from_point(&self, _x: f64, _y: f64) -> Option<Monitor> {
        None
    }

    fn available_monitors(&self) -> Vec<Monitor> {
        Vec::new()
    }

    fn cursor_position(&self) -> tauri_runtime::Result<PhysicalPosition<f64>> {
        Err(Error::FailedToGetCursorPosition)
    }

    fn set_theme(&self, _theme: Option<Theme>) {}

    #[cfg(target_os = "macos")]
    fn set_activation_policy(&mut self, _activation_policy: tauri_runtime::ActivationPolicy) {}

    #[cfg(target_os = "macos")]
    fn set_dock_visibility(&mut self, _visible: bool) {}

    #[cfg(target_os = "macos")]
    fn show(&self) {}

    #[cfg(target_os = "macos")]
    fn hide(&self) {}

    fn set_device_event_filter(&mut self, _filter: DeviceEventFilter) {}

    #[cfg(any(
        target_os = "macos",
        windows,
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    fn run_iteration<F: FnMut(RunEvent<T>) + 'static>(&mut self, _callback: F) {}

    fn run_return<F: FnMut(RunEvent<T>) + 'static>(self, callback: F) -> i32 {
        self.run(callback);
        0
    }

    fn run<F: FnMut(RunEvent<T>) + 'static>(self, callback: F) {
        runtime_trace("BlitzRuntime::run entered");
        let mut application = self
            .application
            .into_inner()
            .expect("Blitz runtime application already consumed");
        application.callback = Some(Box::new(callback));
        runtime_trace("native event loop run_app started");
        if let Err(error) = self.event_loop.run_app(application) {
            eprintln!("tauri-runtime-blitz: event loop failed: {error}");
        }
        runtime_trace("native event loop run_app returned");
    }
}

#[cfg(all(feature = "agent-control", unix))]
fn element_attr<'a>(element: &'a blitz_dom::ElementData, name: &str) -> Option<&'a str> {
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
fn focuses_on_click(element: &blitz_dom::ElementData) -> bool {
    let tag = element.name.local.as_ref();
    matches!(tag, "button" | "input" | "select" | "textarea")
        || tag == "a" && element_attr(element, "href").is_some()
        || element_attr(element, "tabindex")
            .and_then(|value| value.parse::<i32>().ok())
            .is_some_and(|value| value >= 0)
        || element_attr(element, "contenteditable").is_some_and(|value| value != "false")
}

#[cfg(all(feature = "agent-control", unix))]
fn semantic_role(element: &blitz_dom::ElementData) -> String {
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
        "form" => "form",
        "ul" | "ol" => "list",
        "li" => "listitem",
        "table" => "table",
        "tr" => "row",
        "td" | "th" => "cell",
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

#[cfg(all(feature = "agent-control", unix))]
fn semantic_name(element: &blitz_dom::ElementData, node: &blitz_dom::Node, role: &str) -> String {
    let name = element_attr(element, "aria-label")
        .or_else(|| element_attr(element, "alt"))
        .or_else(|| element_attr(element, "title"))
        .map(str::to_string)
        .or_else(|| {
            matches!(role, "button" | "link" | "heading" | "option").then(|| node.text_content())
        })
        .unwrap_or_default();
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(512)
        .collect()
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
fn semantic_selected(element: &blitz_dom::ElementData) -> bool {
    element.checkbox_input_checked().unwrap_or(false)
        || element_attr(element, "aria-selected") == Some("true")
        || element_attr(element, "aria-pressed") == Some("true")
        || element_attr(element, "aria-checked") == Some("true")
        || element_attr(element, "checked").is_some()
        || element_attr(element, "selected").is_some()
}

#[cfg(all(feature = "agent-control", unix))]
fn semantic_depth(
    document: &blitz_dom::BaseDocument,
    node_id: blitz_dom::NodeId,
    root: Option<blitz_dom::NodeId>,
    max_depth: u32,
) -> Option<u32> {
    let mut current = Some(node_id);
    let mut depth = 0_u32;
    loop {
        let id = current?;
        if Some(id) == root {
            break;
        }
        let node = document.get_node(id)?;
        current = node.parent;
        if current
            .and_then(|parent| document.get_node(parent))
            .and_then(blitz_dom::Node::element_data)
            .is_some()
        {
            depth = depth.saturating_add(1);
        }
        if root.is_none() && current.is_none() {
            break;
        }
    }
    (max_depth == 0 || depth <= max_depth).then_some(depth)
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
fn node_is_visible(document: &blitz_dom::BaseDocument, node_id: blitz_dom::NodeId) -> bool {
    let mut current = Some(node_id);
    while let Some(id) = current {
        let Some(node) = document.get_node(id) else {
            return false;
        };
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
        if let Some(element) = node.element_data()
            && (element_attr(element, "hidden").is_some()
                || element_attr(element, "aria-hidden") == Some("true"))
        {
            return false;
        }
        current = node.parent;
    }
    true
}

#[cfg(all(feature = "agent-control", unix))]
fn dom_chain_is_attached(
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
fn layout_chain_is_valid(
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

#[cfg(all(feature = "agent-control", unix))]
fn resolve_agent_node(
    document: &mut ScriptDocument,
    raw_node_id: u64,
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
        .filter(|rect| rect.width > 0.0 && rect.height > 0.0)
        .ok_or_else(|| debug_error("notInteractable", "node has no layout box"))?;
    Ok((
        node_id,
        (
            (rect.x + rect.width / 2.0) as f32,
            (rect.y + rect.height / 2.0) as f32,
        ),
    ))
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

fn activate_agent_node(
    document: &mut ScriptDocument,
    raw_node_id: u64,
    count: u8,
) -> Result<(f32, f32), DebugError> {
    let (node_id, position) = resolve_agent_node(document, raw_node_id)?;
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

#[cfg(all(feature = "agent-control", unix))]
fn set_agent_node_value(
    document: &mut ScriptDocument,
    node_id: blitz_dom::NodeId,
    value: String,
) -> Result<(), DebugError> {
    if !document
        .inner()
        .get_node(node_id)
        .and_then(|node| node.element_data())
        .is_some_and(|element| element.text_input_data().is_some())
    {
        return Err(debug_error("notEditable", "node is not a text input"));
    }
    document.inner_mut().set_focus_to(node_id);
    document
        .inner_mut()
        .with_text_input(node_id, |mut editor| editor.select_all());
    document.handle_ui_event(UiEvent::Ime(BlitzImeEvent::Commit(value)));
    Ok(())
}

#[cfg(all(feature = "agent-control", unix))]
fn keyboard_modifiers(modifiers: ControlModifiers) -> KeyboardModifiers {
    let mut output = KeyboardModifiers::empty();
    output.set(KeyboardModifiers::SHIFT, modifiers.shift);
    output.set(KeyboardModifiers::CONTROL, modifiers.control);
    output.set(KeyboardModifiers::ALT, modifiers.alt);
    output.set(KeyboardModifiers::META, modifiers.meta);
    output
}

#[cfg(all(feature = "agent-control", unix))]
fn key_event(phase: KeyPhase, key: Key, code: Code, modifiers: KeyboardModifiers) -> BlitzKeyEvent {
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

#[cfg(all(feature = "agent-control", unix))]
fn pointer_coords((x, y): (f32, f32)) -> PointerCoords {
    PointerCoords {
        page_x: x,
        page_y: y,
        screen_x: x,
        screen_y: y,
        client_x: x,
        client_y: y,
    }
}

#[cfg(all(feature = "agent-control", unix))]
fn pointer_event(
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
fn mouse_button(button: u16) -> Result<MouseEventButton, DebugError> {
    match button {
        0 => Ok(MouseEventButton::Main),
        1 => Ok(MouseEventButton::Auxiliary),
        2 => Ok(MouseEventButton::Secondary),
        3 => Ok(MouseEventButton::Fourth),
        4 => Ok(MouseEventButton::Fifth),
        _ => Err(debug_error(
            "unsupportedButton",
            "pointer button must be 0 through 4",
        )),
    }
}

#[cfg(all(feature = "agent-control", unix))]
fn relaunch_current_process() -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    #[cfg(target_os = "macos")]
    if let Some(bundle) = containing_app_bundle(&executable) {
        let mut command = std::process::Command::new("/usr/bin/open");
        command.arg("-n");
        command.arg(bundle).spawn()?;
        return Ok(());
    }
    std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .spawn()?;
    Ok(())
}

#[cfg(all(feature = "agent-control", target_os = "macos"))]
fn containing_app_bundle(executable: &std::path::Path) -> Option<std::path::PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

#[cfg(all(feature = "agent-control", unix))]
fn debug_error(code: &str, message: &str) -> DebugError {
    DebugError {
        code: code.into(),
        message: message.into(),
    }
}

#[cfg(all(feature = "agent-control", unix))]
fn control_error(code: &str, message: &str) -> DebugResponse {
    DebugResponse::Error(debug_error(code, message))
}

#[cfg(feature = "debug-control")]
fn debug_screenshot_size_from_env() -> Option<(u32, u32)> {
    let value = std::env::var("TAURI_BLITZ_DRIVER_SCREENSHOT_SIZE").ok()?;
    let (width, height) = value.split_once('x')?;
    let width = width.parse().ok()?;
    let height = height.parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

/// Create a Tauri builder configured for the Blitz runtime.
pub fn builder() -> tauri::Builder<BlitzRuntime> {
    let builder = tauri::Builder::new();
    #[cfg(target_os = "macos")]
    let builder = builder.enable_macos_default_menu(false);
    builder
}

fn register_window<T: UserEvent, F: Fn(RawWindow) + Send + 'static>(
    context: &BlitzRuntimeContext<T>,
    application: &mut BlitzApplication<VelloWindowRenderer>,
    pending: PendingWindow<T, BlitzRuntime<T>>,
    after_window_creation: Option<F>,
) -> tauri_runtime::Result<DetachedWindow<T, BlitzRuntime<T>>> {
    runtime_trace("native window registration entered");
    let id = context.next_window_id();
    let label = pending.label;
    let builder = pending.window_builder;
    let pending_webview = pending.webview.ok_or(Error::CreateWindow)?;
    let prepared = BlitzRuntime::<T>::prepared_webview(pending_webview)?;
    let use_https_scheme = prepared.pending.webview_attributes.use_https_scheme;

    let state = Arc::new(NativeWindowState {
        label: label.clone(),
        config: Mutex::new(builder.config.clone()),
        native: Mutex::new(None),
        event_handlers: Mutex::new(HashMap::new()),
    });
    context
        .windows
        .lock()
        .unwrap()
        .insert(id, Arc::clone(&state));

    let attributes = window_attributes(&builder);
    let state_for_creation = Arc::clone(&state);
    // A transparent window needs the surface to composite that way and the
    // frame to be cleared to nothing. The renderer's default clear colour is
    // opaque white, so leaving it alone would hand the compositor a filled
    // frame and the glass behind it would never be visible — the same black
    // window that a missing `with_transparent` produces, from a different
    // cause.
    #[cfg(not(feature = "hybrid-renderer"))]
    let renderer = if builder.config.transparent {
        VelloWindowRenderer::with_options(
            anyrender_vello::VelloRendererOptions::default()
                .composite_alpha_mode(anyrender::CompositeAlphaMode::Transparent)
                .base_color(peniko::Color::TRANSPARENT),
        )
    } else {
        VelloWindowRenderer::new()
    };
    // The hybrid pipeline, same decision through its own options type.
    //
    // It is the renderer that can carry CSS filters: `anyrender_vello_hybrid`
    // forwards them to `vello_hybrid`, which renders a filtered layer into an
    // intermediate texture and applies the filter there. Classic vello has no
    // filter parameter at all, so a build that wants `filter:` wants this one.
    #[cfg(feature = "hybrid-renderer")]
    let renderer = if builder.config.transparent {
        VelloWindowRenderer::with_options(
            anyrender_vello_hybrid::VelloHybridRendererOptions::default()
                .composite_alpha_mode(anyrender::CompositeAlphaMode::Transparent)
                .base_color(peniko::Color::TRANSPARENT),
        )
    } else {
        VelloWindowRenderer::new()
    };
    let window =
        BlitzShellWindowConfig::with_attributes(Box::new(prepared.document), renderer, attributes)
            .with_on_created(move |native| {
                /*
                 * The attributes above were snapshotted at registration, but the native
                 * window is not created until the event loop can make surfaces, and
                 * Tauri's `setup` runs in between. A `show()` in there reaches the
                 * config and finds no native window to forward to, so replaying the
                 * current visibility here is what makes it take effect. Without this a
                 * window configured `visible: false` and shown once the app is ready,
                 * which is the standard way to hide a slow boot, stays hidden forever.
                 */
                let visible = state_for_creation.config.lock().unwrap().visible;
                native.set_visible(visible);
                #[cfg(target_os = "macos")]
                {
                    // A small generic escape hatch for platform integrations owned by
                    // the embedder. TRB stores the native window; it does not know what
                    // effect or application policy the callback applies.
                    let slot = NATIVE_WINDOW.get_or_init(|| Mutex::new(None));
                    if let Ok(mut guard) = slot.lock() {
                        *guard = Some(Arc::clone(&native));
                    }
                }
                *state_for_creation.native.lock().unwrap() = Some(native);
                if let Some(callback) = after_window_creation {
                    let marker = PhantomData;
                    callback(RawWindow { _marker: &marker });
                }
            });
    application.add_window(window);
    runtime_trace("native window queued");

    Ok(DetachedWindow {
        id,
        label,
        dispatcher: BlitzWindowDispatcher {
            id,
            context: context.clone(),
            state,
        },
        webview: Some(DetachedWindowWebview {
            webview: prepared.detached,
            use_https_scheme,
        }),
    })
}

#[cfg(target_os = "macos")]
static NATIVE_WINDOW: std::sync::OnceLock<Mutex<Option<Arc<dyn winit::window::Window>>>> =
    std::sync::OnceLock::new();

/// Run an embedder-owned macOS integration against the native window.
///
/// TRB deliberately exposes the window without interpreting the callback. App
/// chrome, platform effects, and their dependencies remain in the application.
#[cfg(target_os = "macos")]
pub(crate) fn with_native_window(callback: impl FnOnce(&dyn winit::window::Window)) -> bool {
    let Some(slot) = NATIVE_WINDOW.get() else {
        return false;
    };
    let Ok(guard) = slot.lock() else {
        return false;
    };
    let Some(window) = guard.as_ref() else {
        return false;
    };
    callback(window.as_ref());
    true
}

fn window_attributes(builder: &BlitzWindowBuilder) -> WindowAttributes {
    let config = &builder.config;
    let mut attributes = WindowAttributes::default()
        .with_title(config.title.clone())
        .with_surface_size(LogicalSize::new(config.width, config.height))
        .with_resizable(config.resizable)
        .with_maximized(config.maximized)
        .with_visible(config.visible)
        .with_decorations(config.decorations)
        .with_content_protected(config.content_protected)
        // Was accepted by the builder, stored on the config, and never read, so
        // `"transparent": true` was a silent no-op and nothing behind the window
        // could ever show through. It is the first of the three things a glass
        // window needs; the other two are a non-opaque composite mode and a root
        // that does not paint over everything.
        .with_transparent(config.transparent)
        .with_active(config.focus);

    if let (Some(x), Some(y)) = (config.x, config.y) {
        attributes = attributes.with_position(LogicalPosition::new(x, y));
    }
    if let (Some(width), Some(height)) = (config.min_width, config.min_height) {
        attributes = attributes.with_min_surface_size(LogicalSize::new(width, height));
    }
    if let (Some(width), Some(height)) = (config.max_width, config.max_height) {
        attributes = attributes.with_max_surface_size(LogicalSize::new(width, height));
    }
    if config.fullscreen {
        attributes = attributes.with_fullscreen(Some(Fullscreen::Borderless(None)));
    }
    if config.always_on_top {
        attributes = attributes.with_window_level(WindowLevel::AlwaysOnTop);
    } else if config.always_on_bottom {
        attributes = attributes.with_window_level(WindowLevel::AlwaysOnBottom);
    }

    let mut buttons = WindowButtons::empty();
    if config.closable {
        buttons |= WindowButtons::CLOSE;
    }
    if config.minimizable {
        buttons |= WindowButtons::MINIMIZE;
    }
    if config.maximizable {
        buttons |= WindowButtons::MAXIMIZE;
    }
    attributes = attributes.with_enabled_buttons(buttons);

    #[cfg(target_os = "macos")]
    {
        let macos_attributes = match config.title_bar_style {
            tauri_utils::TitleBarStyle::Visible => {
                WindowAttributesMacOS::default().with_title_hidden(config.hidden_title)
            }
            tauri_utils::TitleBarStyle::Transparent => WindowAttributesMacOS::default()
                .with_titlebar_transparent(true)
                .with_title_hidden(config.hidden_title),
            tauri_utils::TitleBarStyle::Overlay => WindowAttributesMacOS::default()
                .with_titlebar_transparent(true)
                .with_title_hidden(config.hidden_title)
                .with_fullsize_content_view(true),
            _ => WindowAttributesMacOS::default().with_title_hidden(config.hidden_title),
        };
        attributes = attributes.with_platform_attributes(Box::new(macos_attributes));
    }

    attributes
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::DocumentConfig;
    use tauri_runtime::window::WindowBuilder;

    #[test]
    fn document_factory_receives_pending_url() {
        set_document_factory(|url| {
            Ok(ScriptDocument::from_html(
                "<div id='ready'>ready</div>",
                DocumentConfig {
                    base_url: Some(url.into()),
                    ..DocumentConfig::default()
                },
            ))
        });

        let document = create_document("tauri://localhost/view").unwrap();
        assert_eq!(
            document.current_url().map(url::Url::as_str),
            Some("tauri://localhost/view")
        );
    }

    #[test]
    fn window_attributes_preserve_initial_native_configuration() {
        let mut builder = BlitzWindowBuilder::new();
        builder.config.title = "Example application".into();
        builder.config.width = 640.0;
        builder.config.height = 480.0;
        builder.config.min_width = Some(320.0);
        builder.config.min_height = Some(240.0);
        builder.config.visible = false;
        builder.config.decorations = false;

        let attributes = window_attributes(&builder);
        assert_eq!(attributes.title, "Example application");
        assert_eq!(
            attributes.surface_size,
            Some(LogicalSize::new(640.0, 480.0).into())
        );
        assert_eq!(
            attributes.min_surface_size,
            Some(LogicalSize::new(320.0, 240.0).into())
        );
        assert!(!attributes.visible);
        assert!(!attributes.decorations);
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn semantic_visibility_includes_display_none_ancestors() {
        let mut document = ScriptDocument::from_html(
            "<main><button id='shown'>Run</button><div style='display:none'><button id='hidden'>Hidden</button></div></main>",
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        let inner = document.inner();
        let node_id = |value: &str| {
            inner
                .tree()
                .iter()
                .find_map(|(id, node)| {
                    node.element_data()
                        .is_some_and(|element| element_attr(element, "id") == Some(value))
                        .then_some(id)
                })
                .unwrap()
        };
        let shown = node_id("shown");
        let hidden = node_id("hidden");
        assert!(node_is_visible(&inner, shown));
        assert!(!node_is_visible(&inner, hidden));
        let shown_node = inner.get_node(shown).unwrap();
        let shown_element = shown_node.element_data().unwrap();
        assert_eq!(semantic_role(shown_element), "button");
        assert_eq!(semantic_name(shown_element, shown_node, "button"), "Run");
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn semantic_geometry_rejects_a_detached_layout_ancestor() {
        let mut document = ScriptDocument::from_html(
            "<main><button id='target'>Run</button></main>",
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        let inner = document.inner();
        let target = inner.query_selector("#target").unwrap().unwrap();
        let node_limit = inner.tree().iter().count();
        assert!(layout_chain_is_valid(&inner, target, node_limit));

        let missing_parent = (1..=1024)
            .map(blitz_dom::NodeId::from_u64)
            .find(|id| inner.get_node(*id).is_none())
            .expect("the fixture must leave at least one node id unused");
        inner
            .get_node(target)
            .unwrap()
            .layout_parent
            .set(Some(missing_parent));

        assert!(!layout_chain_is_valid(&inner, target, node_limit));
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn semantic_geometry_rejects_a_retained_dom_subtree() {
        let mut document = ScriptDocument::from_html(
            "<main><section id='removed'><button id='target'>Run</button></section></main>",
            DocumentConfig::default(),
        );
        let (removed, target) = {
            let inner = document.inner();
            (
                inner.query_selector("#removed").unwrap().unwrap(),
                inner.query_selector("#target").unwrap().unwrap(),
            )
        };
        let node_limit = document.inner().tree().iter().count();
        assert!(dom_chain_is_attached(&document.inner(), target, node_limit));

        blitz_dom::DocumentMutator::new(&mut document.inner_mut()).remove_node(removed);

        let inner = document.inner();
        assert!(
            inner.get_node(target).is_some(),
            "the DOM keeps detached nodes alive for JavaScript wrappers"
        );
        assert!(!dom_chain_is_attached(&inner, target, node_limit));
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn semantic_selection_includes_native_and_aria_states() {
        let document = ScriptDocument::from_html(
            r#"
            <button id="pressed" aria-pressed="true">Pressed</button>
            <div id="checked" role="radio" aria-checked="true">Checked</div>
            <div id="selected" role="option" aria-selected="true">Selected</div>
            <input id="native" type="checkbox" checked>
            <button id="plain">Plain</button>
            "#,
            DocumentConfig::default(),
        );
        let inner = document.inner();
        let selected = |selector: &str| {
            let id = inner.query_selector(selector).unwrap().unwrap();
            semantic_selected(inner.get_node(id).unwrap().element_data().unwrap())
        };

        assert!(selected("#pressed"));
        assert!(selected("#checked"));
        assert!(selected("#selected"));
        assert!(selected("#native"));
        assert!(!selected("#plain"));
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn node_activation_reaches_an_offscreen_mousedown_handler() {
        let mut document = ScriptDocument::from_html(
            r#"
            <button id="target" style="position:absolute;left:-900px;width:80px;height:30px">Run</button>
            <output id="result"></output>
            <script>
              const target = document.getElementById("target");
              const result = document.getElementById("result");
              target.addEventListener("mousedown", () => result.textContent += "down ");
              target.addEventListener("click", () => result.textContent += "click");
            </script>
            "#,
            DocumentConfig::default(),
        );
        document.execute_scripts();
        document.inner_mut().resolve(0.0);
        let (target, result) = {
            let inner = document.inner();
            (
                inner.query_selector("#target").unwrap().unwrap(),
                inner.query_selector("#result").unwrap().unwrap(),
            )
        };
        assert!(
            document
                .inner()
                .get_client_bounding_rect(target)
                .is_some_and(|rect| rect.x < 0.0),
            "the fixture must be outside the viewport"
        );

        activate_agent_node(&mut document, target.as_u64(), 1).unwrap();

        assert_eq!(
            document.inner().get_node(result).unwrap().text_content(),
            "down click"
        );
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn node_activation_bubbles_to_delegated_handlers() {
        let mut document = ScriptDocument::from_html(
            r#"
            <main id="root">
              <button id="target" style="width:80px;height:30px">Run</button>
              <output id="result"></output>
            </main>
            <script>
              const root = document.getElementById("root");
              const result = document.getElementById("result");
              root.addEventListener("mousedown", event => {
                if (event.target.id === "target") result.textContent += "down ";
              });
              root.addEventListener("click", event => {
                if (event.target.id === "target") result.textContent += "click";
              });
            </script>
            "#,
            DocumentConfig::default(),
        );
        document.execute_scripts();
        document.inner_mut().resolve(0.0);
        let (target, result) = {
            let inner = document.inner();
            (
                inner.query_selector("#target").unwrap().unwrap(),
                inner.query_selector("#result").unwrap().unwrap(),
            )
        };

        activate_agent_node(&mut document, target.as_u64(), 1).unwrap();

        assert_eq!(
            document.inner().get_node(result).unwrap().text_content(),
            "down click"
        );
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn node_activation_reaches_solid_style_document_delegation() {
        let mut document = ScriptDocument::from_html(
            r#"
            <main id="root">
              <button id="target" style="width:80px;height:30px">Run</button>
              <output id="result"></output>
            </main>
            <script>
              const target = document.getElementById("target");
              const result = document.getElementById("result");
              target.$$click = () => result.textContent = "delegated";
              document.addEventListener("click", event => {
                let node = event.target;
                while (node) {
                  if (node.$$click) node.$$click(event);
                  node = node.parentNode;
                }
              });
            </script>
            "#,
            DocumentConfig::default(),
        );
        document.execute_scripts();
        document.inner_mut().resolve(0.0);
        let (target, result) = {
            let inner = document.inner();
            (
                inner.query_selector("#target").unwrap().unwrap(),
                inner.query_selector("#result").unwrap().unwrap(),
            )
        };

        activate_agent_node(&mut document, target.as_u64(), 1).unwrap();

        assert_eq!(
            document.inner().get_node(result).unwrap().text_content(),
            "delegated"
        );
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn node_activation_applies_the_native_focus_default() {
        let mut document = ScriptDocument::from_html(
            r#"<main><div id="slider" role="slider" tabindex="0" style="width:80px;height:30px">Value</div></main>"#,
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        let slider = document.inner().query_selector("#slider").unwrap().unwrap();

        activate_agent_node(&mut document, slider.as_u64(), 1).unwrap();

        assert_eq!(document.inner().get_focussed_node_id(), Some(slider));
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn setting_a_node_value_replaces_text_and_dispatches_input() {
        let mut document = ScriptDocument::from_html(
            r#"
            <main>
              <input id="field" value="old" style="width:80px;height:30px">
              <output id="result"></output>
            </main>
            <script>
              const field = document.getElementById("field");
              const result = document.getElementById("result");
              field.addEventListener("input", event => result.textContent = event.target.value);
            </script>
            "#,
            DocumentConfig::default(),
        );
        document.execute_scripts();
        document.inner_mut().resolve(0.0);
        let (field, result) = {
            let inner = document.inner();
            (
                inner.query_selector("#field").unwrap().unwrap(),
                inner.query_selector("#result").unwrap().unwrap(),
            )
        };

        let replacement = "https://example.test/org/repository/issues/40?view=full#comment-2";
        set_agent_node_value(&mut document, field, replacement.into()).unwrap();

        let inner = document.inner();
        let text = inner
            .get_node(field)
            .unwrap()
            .element_data()
            .unwrap()
            .text_input_data()
            .unwrap()
            .editor
            .raw_text();
        assert_eq!(text, replacement);
        assert_eq!(inner.get_node(result).unwrap().text_content(), replacement);
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn node_double_click_is_one_runtime_action() {
        let mut document = ScriptDocument::from_html(
            r#"
            <button id="target" style="width:80px;height:30px">Open row</button>
            <output id="result"></output>
            <script>
              document.getElementById("target").addEventListener("dblclick", () => {
                document.getElementById("result").textContent = "double";
              });
            </script>
            "#,
            DocumentConfig::default(),
        );
        document.execute_scripts();
        let (target, result) = {
            let inner = document.inner();
            (
                inner.query_selector("#target").unwrap().unwrap(),
                inner.query_selector("#result").unwrap().unwrap(),
            )
        };

        activate_agent_node(&mut document, target.as_u64(), 2).unwrap();

        assert_eq!(
            document.inner().get_node(result).unwrap().text_content(),
            "double"
        );
    }

    #[cfg(all(feature = "diagnostics", unix))]
    #[test]
    fn diagnostic_layout_reports_scroll_state_without_script_evaluation() {
        let mut document = ScriptDocument::from_html(
            "<section id='scroller' style='height:100px;overflow-y:auto'><div style='height:400px'>tail</div></section>",
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        let scroller = document
            .inner()
            .tree()
            .iter()
            .find_map(|(id, node)| {
                node.element_data()
                    .is_some_and(|element| element_attr(element, "id") == Some("scroller"))
                    .then_some(id)
            })
            .unwrap();
        document
            .inner_mut()
            .get_node_mut(scroller)
            .unwrap()
            .scroll_offset_mut()
            .y = 60.0;

        let inner = document.inner();
        let row = diagnostic_layout_row(
            &inner,
            &SemanticNode {
                id: scroller.as_u64(),
                parent: None,
                role: "generic".into(),
                name: "Scrollable region".into(),
                value: None,
                enabled: true,
                visible: true,
                selected: false,
                bounds: Some([0.0, 0.0, 100.0, 100.0]),
                slot: None,
            },
        )
        .unwrap();

        assert_eq!(row["scrollOffset"][1], 60.0);
        assert_eq!(row["clientSize"][1], 100.0);
        assert!(row["scrollSize"][1].as_f64().unwrap() >= 100.0);
        assert!(row["scrollRange"][1].as_f64().unwrap() >= 0.0);
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn native_key_event_preserves_physical_code_and_modifiers() {
        let event = key_event(
            KeyPhase::Down,
            Key::Character("2".into()),
            Code::Digit2,
            KeyboardModifiers::META,
        );
        assert_eq!(event.code, Code::Digit2);
        assert!(event.modifiers.meta());
        assert!(event.text.is_none());
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[tokio::test(flavor = "current_thread")]
    async fn control_interface_is_absent_until_explicitly_enabled() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        let bridge: ControlBridge = Arc::new(|_| {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let _ = sender.send(DebugResponse::Ack);
            receiver
        });
        let runtime = Arc::new(Mutex::new(AgentControlRuntime {
            bridge,
            server: None,
        }));
        *AGENT_CONTROL_RUNTIME
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(Arc::downgrade(&runtime));

        assert!(!agent_control_enabled());
        apply_runtime_debug_options(blitz_traits::profiling::DebugOptions {
            inspection_and_agent_control: true,
            deep_intrusive_profiling: true,
        })
        .unwrap();
        assert!(agent_control_enabled());
        assert!(
            deep_profiling_permitted(),
            "the embedder's switch is permission, and it was just granted"
        );
        // Permission plus a consumer. The control server holds a session for
        // the out-of-process tool that can now attach, so collection is running
        // here even though nothing in this process reads a sample.
        assert!(
            deep_profiling_enabled(),
            "a listening control server is the consumer that starts collection"
        );

        // The two switches are independent, and this is the case that proves
        // it: inspection off, profiling still permitted.
        //
        // This once asserted the opposite, on the reasoning that samples are
        // only useful while a socket exists to read them back. ps-blitz
        // e47684f4 removed that AND deliberately, because ANDing made the
        // embedder's profiling toggle silently inert whenever inspection was
        // off: turning inspection off and on again lost the setting entirely.
        apply_runtime_debug_options(blitz_traits::profiling::DebugOptions {
            inspection_and_agent_control: false,
            deep_intrusive_profiling: true,
        })
        .unwrap();
        assert!(!agent_control_enabled());
        assert!(
            deep_profiling_permitted(),
            "deep profiling answers for itself; inspection must not withdraw permission"
        );
        // But collection stops, because closing the server dropped the only
        // consumer. That is the point of the change rather than a regression:
        // with no tool able to attach, the samples had no reader.
        assert!(
            !deep_profiling_enabled(),
            "no consumer can attach, so nothing should still be collecting"
        );

        // Left off, because it is process-global and the next test in this
        // binary starts wherever this one stops.
        apply_runtime_debug_options(blitz_traits::profiling::DebugOptions::default()).unwrap();
        assert!(!deep_profiling_permitted());
        assert!(!deep_profiling_enabled());
    }

    #[cfg(all(feature = "agent-control", target_os = "macos"))]
    #[test]
    fn relaunch_uses_the_app_bundle_instead_of_the_inner_macos_binary() {
        let executable = std::path::Path::new("/Applications/Example.app/Contents/MacOS/example");
        assert_eq!(
            containing_app_bundle(executable).as_deref(),
            Some(std::path::Path::new("/Applications/Example.app"))
        );
        assert_eq!(
            containing_app_bundle(std::path::Path::new(
                "/Applications/Example.app/Contents/Resources/helper"
            )),
            None
        );
        assert_eq!(
            containing_app_bundle(std::path::Path::new("/tmp/example")),
            None
        );
    }
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
     * Poll and resolve before reading anything. The tree is read straight after
     * this, and a document that has not settled reports boxes from before its
     * last mutation, which reads as a component that laid out wrongly rather
     * than one that was measured too early.
     */
    for _ in 0..100 {
        if !document.poll(None) {
            break;
        }
    }
    document.inner_mut().resolve(0.0);
    let inner = document.inner();
    let layout_node_limit = inner.tree().iter().count();
    let root = root.map(blitz_dom::NodeId::from_u64);
    if root.is_some_and(|id| inner.get_node(id).is_none()) {
        return control_error("unknownNode", "the requested root node does not exist");
    }
    let focused_node = inner.get_focussed_node_id().map(|id| id.as_u64());
    let nodes = inner
        .tree()
        .iter()
        .filter_map(|(id, node)| {
            let element = node.element_data()?;
            semantic_depth(&inner, id, root, max_depth)?;
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
            let name = semantic_name(element, node, &role);
            let value = semantic_value(element);
            let parent = semantic_parent(&inner, id, root).map(|id| id.as_u64());
            Some(SemanticNode {
                id: id.as_u64(),
                parent,
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
