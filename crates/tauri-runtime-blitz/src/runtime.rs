use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicBool;
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

#[cfg(all(feature = "diagnostics", unix))]
use crate::agent::{CaptureSurface, capture_document_with_surface, snapshot_document};
#[cfg(all(feature = "agent-control", unix))]
/*
 * Gated to match the items themselves. The capture path is `diagnostics`, the
 * activation path is `agent-control`, and importing both under one attribute
 * made a build with only the latter fail on three names it never uses.
 */
#[cfg(all(feature = "agent-control", unix))]
use crate::agent::{
    activate_agent_node, control_error, debug_error, focus_agent_node, inspect_document, key_event,
    keyboard_modifiers, pointer_coords, pointer_event, resolve_agent_node,
};
#[cfg(all(feature = "agent-control", unix, test))]
use crate::agent_control_server::CONTROL_TEST_LOCK;
#[cfg(all(feature = "diagnostics", unix))]
use crate::control_protocol::{
    DebugSnapshot, DiagnosticsRequest, SnapshotRequest, WindowComposition,
};
use crate::window_dispatch::{BlitzWindowDispatcher, NativeWindowState};
use crate::{
    BlitzWebviewDispatcher, BlitzWindowBuilder, PreparedBlitzWebview, prepare_pending_webview,
};
#[cfg(all(feature = "agent-control", unix))]
use crate::{
    agent_control_server::{AgentControlServer, ControlBridge, ControlBridgeRequest},
    control_protocol::{
        AgentAction, AgentControlRequest, DebugError, DebugEvent, DebugResponse, InputCommand,
        KeyPhase, PointerPhase,
    },
};
use blitz_dom::Document;
#[cfg(all(feature = "agent-control", unix))]
use blitz_traits::events::{
    BlitzImeEvent, BlitzWheelDelta, BlitzWheelEvent, MouseEventButton, MouseEventButtons, Point,
    UiEvent,
};
#[cfg(all(feature = "agent-control", unix))]
use keyboard_types::{Code, Key, Modifiers as KeyboardModifiers};

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
    events: tokio::sync::watch::Sender<Option<DebugEvent>>,
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
        (true, false) => {
            runtime.server = Some(AgentControlServer::start_with_events(
                runtime.bridge.clone(),
                runtime.events.subscribe(),
            )?)
        }
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
    // The listener may predate this permission change. Refresh its session in
    // place; otherwise a server created while profiling was off holds `None`
    // forever and diagnostics silently return `script: null` after the owner
    // turns profiling on.
    if options.inspection_and_agent_control {
        let runtime = AGENT_CONTROL_RUNTIME
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| std::io::Error::other("the Blitz runtime is not initialized"))?;
        if let Some(server) = runtime.lock().unwrap().server.as_mut() {
            server.refresh_deep_profiling();
        }
    }
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
    paint_revision: u64,
    #[cfg(all(feature = "agent-control", unix))]
    control_events: tokio::sync::watch::Sender<Option<DebugEvent>>,
    #[cfg(all(feature = "agent-control", unix))]
    agent_pointer: (f32, f32),
    #[cfg(all(feature = "agent-control", unix))]
    agent_buttons: MouseEventButtons,
    #[cfg(all(feature = "diagnostics", unix))]
    capture_surface: Option<CaptureSurface>,
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
            _ => control_error(
                "unsupportedRequest",
                "this runtime does not implement that agent-control request",
            ),
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
                    node_ids: Vec::new(),
                })
                .map(|snapshot| DebugResponse::Metrics(snapshot.metrics))
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::WaitForIdle => self
                .collect_diagnostics(SnapshotRequest {
                    include_dom: false,
                    include_layout: false,
                    include_computed_style: false,
                    node_ids: Vec::new(),
                })
                .map(|snapshot| DebugResponse::Idle(snapshot.revisions))
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::WindowComposition => {
                DebugResponse::WindowComposition(native_window_composition())
            }
            DiagnosticsRequest::Capture(request) => self
                .capture_image(request)
                .map(DebugResponse::Captured)
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::Observe { .. } => control_error(
                "streamingUnavailable",
                "diagnostic subscriptions are not implemented; request snapshots or metrics",
            ),
            _ => control_error(
                "unsupportedRequest",
                "this runtime does not implement that diagnostics request",
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
        let Self {
            blitz,
            capture_surface,
            ..
        } = self;
        let document = blitz
            .get_mut()
            .windows
            .values_mut()
            .find_map(|view| view.try_downcast_doc_mut::<ScriptDocument>())
            .ok_or_else(|| debug_error("documentUnavailable", "no active script document"))?;
        capture_document_with_surface(document, request, capture_surface)
    }
}

impl<T: UserEvent> RuntimeApplication<T> {
    #[cfg(all(feature = "diagnostics", unix))]
    fn collect_diagnostics(
        &mut self,
        request: SnapshotRequest,
    ) -> Result<DebugSnapshot, DebugError> {
        self.agent_revision += 1;
        let revision = self.agent_revision;
        let document = self
            .agent_document()
            .ok_or_else(|| debug_error("documentUnavailable", "no active script document"))?;
        snapshot_document(document, request, revision)
    }
}

impl<T: UserEvent> RuntimeApplication<T> {
    #[cfg(all(feature = "agent-control", unix))]
    fn perform_agent_action(&mut self, action: AgentAction) -> Result<(), DebugError> {
        match action {
            AgentAction::Focus { node_id } => {
                let node_id = blitz_dom::NodeId::from_u64(node_id);
                let document = self
                    .agent_document()
                    .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                focus_agent_node(document, node_id)?;
            }
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
                let (node_id, position) = {
                    let document = self
                        .agent_document()
                        .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
                    resolve_agent_node(document, node_id)?
                };
                self.agent_pointer = position;
                let document = self.agent_document().unwrap();
                document.handle_pointer_move_to_node(
                    pointer_event(
                        position,
                        MouseEventButton::Main,
                        MouseEventButtons::default(),
                        KeyboardModifiers::empty(),
                    ),
                    node_id,
                );
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
                let mut events = Vec::new();
                document
                    .inner_mut()
                    .scroll_to_node_with_events(node_id, |event| events.push(event));
                for event in events {
                    document.dispatch_dom_event(event);
                }
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
                let mut events = Vec::new();
                document
                    .inner_mut()
                    .scroll_nearest_container_by_with_events(node_id, delta_x, delta_y, |event| {
                        events.push(event)
                    });
                for event in events {
                    document.dispatch_dom_event(event);
                }
            }
            AgentAction::Input(input) => self.perform_agent_input(input)?,
            _ => {
                return Err(debug_error(
                    "unsupportedAction",
                    "this runtime does not implement that agent action",
                ));
            }
        }

        /*
         * An acknowledged action includes its synchronous application result.
         *
         * DOM event handlers enqueue Solid's reactive work on the document
         * poll hook. Returning before that hook runs makes the next Inspect
         * observe the tree from before the click or input, and tight QA loops
         * can keep reading that stale tree until the client disconnects. Drain
         * every immediately runnable consequence here; timers and authored
         * animation frames remain asynchronous and are observed normally.
         */
        let document = self
            .agent_document()
            .ok_or_else(|| debug_error("documentUnavailable", "no active document"))?;
        settle_agent_action(document)
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
            _ => {
                return Err(debug_error(
                    "unsupportedInput",
                    "this runtime does not implement that input command",
                ));
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
        #[cfg(all(feature = "agent-control", unix))]
        let paint_committed = matches!(event, WinitWindowEvent::RedrawRequested);
        self.blitz
            .get_mut()
            .window_event(event_loop, window_id, event);
        #[cfg(all(feature = "agent-control", unix))]
        if paint_committed {
            self.paint_revision = self.paint_revision.saturating_add(1);
            self.control_events
                .send_replace(Some(DebugEvent::PaintCommitted {
                    revision: self.paint_revision,
                }));
        }
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
        let (agent_control, control_events) = {
            let control_context = context.clone();
            let bridge: ControlBridge = Arc::new(move |request| {
                let (response, receiver) = tokio::sync::oneshot::channel();
                let _ = control_context.send(RuntimeMessage::Control { request, response });
                receiver
            });
            // A watch channel retains only the newest revision. Inspection is
            // therefore bounded even when a client stalls or an animation
            // presents much faster than the client can consume notifications.
            let (event_sender, event_receiver) = tokio::sync::watch::channel(None);
            // The embedder's persisted control state is applied later during Tauri
            // setup. This enable-only rescue must start earlier: when control
            // was switched off, setup is unreachable to the very automation
            // needed to switch it back on. It also lets QA attach while a large
            // application is still booting.
            let server = if std::env::args().any(|arg| arg == "--blitz-control") {
                Some(
                    AgentControlServer::start_with_events(bridge.clone(), event_receiver)
                        .map_err(|error| Error::CreateWebview(Box::new(error)))?,
                )
            } else {
                None
            };
            let runtime = Arc::new(Mutex::new(AgentControlRuntime {
                bridge,
                events: event_sender.clone(),
                server,
            }));
            *AGENT_CONTROL_RUNTIME
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap() = Some(Arc::downgrade(&runtime));
            (runtime, event_sender)
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
            paint_revision: 0,
            #[cfg(all(feature = "agent-control", unix))]
            control_events,
            #[cfg(all(feature = "agent-control", unix))]
            agent_pointer: (0.0, 0.0),
            #[cfg(all(feature = "agent-control", unix))]
            agent_buttons: MouseEventButtons::default(),
            #[cfg(all(feature = "diagnostics", unix))]
            capture_surface: None,
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
const MAX_AGENT_SETTLE_POLLS: usize = 100;

/// Drain the synchronous consequences of one driven action before acknowledging it.
///
/// Timers and animation frames remain asynchronous. A poll hook that stays runnable
/// is different: returning `Ack` in that state lets the next inspection observe a
/// half-applied action. Report the exhaustion so callers can fail the interaction
/// instead of compensating with an arbitrary sleep.
#[cfg(all(feature = "agent-control", unix))]
fn settle_agent_action(document: &mut ScriptDocument) -> Result<(), DebugError> {
    for _ in 0..MAX_AGENT_SETTLE_POLLS {
        if !document.poll(None) {
            document.inner_mut().resolve(0.0);
            return Ok(());
        }
    }

    document.inner_mut().resolve(0.0);
    Err(debug_error(
        "actionDidNotSettle",
        "the action kept synchronous script work runnable past the settlement budget",
    ))
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
    #[cfg(target_os = "macos")]
    NATIVE_SURFACE_TRANSPARENT.store(builder.config.transparent, Ordering::Release);
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

#[cfg(target_os = "macos")]
static NATIVE_SURFACE_TRANSPARENT: AtomicBool = AtomicBool::new(false);

#[cfg(all(feature = "diagnostics", unix, target_os = "macos"))]
fn native_window_composition() -> WindowComposition {
    crate::window_effects::composition(NATIVE_SURFACE_TRANSPARENT.load(Ordering::Acquire))
}

#[cfg(all(feature = "diagnostics", unix, not(target_os = "macos")))]
fn native_window_composition() -> WindowComposition {
    WindowComposition::default()
}

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
    use crate::agent::{
        SemanticCandidate, diagnostic_layout_row, diagnostic_style_row, dom_chain_is_attached,
        element_attr, layout_chain_is_valid, layout_chain_validities, node_is_visible,
        semantic_name, semantic_role, semantic_selected,
    };
    use crate::control_protocol::SemanticNode;
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
        let labels = crate::agent::LabelIndex::build(&inner);
        assert_eq!(
            semantic_name(shown_element, shown_node, "button", &inner, shown, &labels),
            "Run"
        );
    }

    /// A name is made of rendered text, so a `<style>` inside a link is not
    /// part of it.
    ///
    /// `textContent` is the DOM property and includes every text node in the
    /// subtree, stylesheets and scripts among them. An accessible name does
    /// not, because those elements are not rendered.
    ///
    /// Measured on honey.id, whose header logo is an anchor wrapping an inline
    /// SVG with a `<style>` in it. The home link arrived named
    /// ".animated-logo path { fill-opacity: 0; ... }", which is unusable to a
    /// person and unaddressable to a check.
    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn a_style_element_is_not_part_of_a_name() {
        let mut document = ScriptDocument::from_html(
            "<main>\
               <a id='logo' href='/'>\
                 <svg><style>.logo path { fill: red; }</style></svg>\
                 Honey\
               </a>\
               <a id='scripted' href='/x'><script>var noise = 1;</script>Docs</a>\
             </main>",
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        let inner = document.inner();
        let labels = crate::agent::LabelIndex::build(&inner);
        let named = |value: &str| {
            let id = inner
                .tree()
                .iter()
                .find_map(|(id, node)| {
                    node.element_data()
                        .is_some_and(|element| element_attr(element, "id") == Some(value))
                        .then_some(id)
                })
                .unwrap();
            let node = inner.get_node(id).unwrap();
            let element = node.element_data().unwrap();
            let role = semantic_role(element);
            semantic_name(element, node, &role, &inner, id, &labels)
        };

        assert_eq!(named("logo"), "Honey", "a stylesheet is not part of a name");
        assert_eq!(named("scripted"), "Docs", "and neither is a script");
    }

    /// A form control is named by the label pointing at it.
    ///
    /// The name came from `aria-label`, `alt` and `title` and from nothing
    /// else, so the ordinary way to label a field produced no name at all and
    /// every text input on every page arrived anonymous. That is not only a
    /// reporting defect: a harness addresses a control by name, so an
    /// anonymous field cannot be typed into and a check that means "enter a URL
    /// and save" cannot be written.
    ///
    /// Measured on support.cafe's connection settings, whose three fields each
    /// carry a correct `<Label for>` and all reported as `textbox ""`.
    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn a_field_is_named_by_its_label() {
        let mut document = ScriptDocument::from_html(
            "<main>\
               <label for='endpoint'>Endpoint URL</label><input id='endpoint'>\
               <label>Wrapped<input id='wrapped'></label>\
               <label for='overridden'>Ignored</label>\
               <input id='overridden' aria-label='Author own name'>\
               <input id='placeheld' placeholder='Search everything'>\
               <input id='nameless'>\
             </main>",
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        let inner = document.inner();
        let labels = crate::agent::LabelIndex::build(&inner);
        let named = |value: &str| {
            let id = inner
                .tree()
                .iter()
                .find_map(|(id, node)| {
                    node.element_data()
                        .is_some_and(|element| element_attr(element, "id") == Some(value))
                        .then_some(id)
                })
                .unwrap();
            let node = inner.get_node(id).unwrap();
            let element = node.element_data().unwrap();
            let role = semantic_role(element);
            semantic_name(element, node, &role, &inner, id, &labels)
        };

        assert_eq!(named("endpoint"), "Endpoint URL", "a `for` association");
        assert_eq!(named("wrapped"), "Wrapped", "a label wrapped around it");
        assert_eq!(
            named("overridden"),
            "Author own name",
            "`aria-label` is the author overriding the visible text, and wins"
        );
        assert_eq!(
            named("placeheld"),
            "Search everything",
            "a placeholder is the last resort, and names most search fields"
        );
        assert_eq!(named("nameless"), "", "nothing names it, so it has no name");
    }

    /// A live region is named by what it says.
    ///
    /// `alert` and `status` are the roles an application uses to report that
    /// something happened -- a refusal, a saved confirmation -- and what they
    /// report is their content. Anonymous, "the reason is shown" is not a
    /// question a check can ask, so every validation outcome has to be
    /// approximated by something else that moved.
    ///
    /// The negative half is the point of the test. A wrapper's text content is
    /// its whole subtree, so naming generic containers would give every one of
    /// them a name made of the entire page, and any name-matching selector
    /// would then match everything.
    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn a_live_region_is_named_by_what_it_says() {
        let mut document = ScriptDocument::from_html(
            "<main>\
               <p id='refusal' role='alert'>that is not an address</p>\
               <p id='saved' role='status'>Settings saved</p>\
               <div id='wrapper'><span>inner text</span></div>\
             </main>",
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        let inner = document.inner();
        let labels = crate::agent::LabelIndex::build(&inner);
        let named = |value: &str| {
            let id = inner
                .tree()
                .iter()
                .find_map(|(id, node)| {
                    node.element_data()
                        .is_some_and(|element| element_attr(element, "id") == Some(value))
                        .then_some(id)
                })
                .unwrap();
            let node = inner.get_node(id).unwrap();
            let element = node.element_data().unwrap();
            let role = semantic_role(element);
            semantic_name(element, node, &role, &inner, id, &labels)
        };

        assert_eq!(named("refusal"), "that is not an address");
        assert_eq!(named("saved"), "Settings saved");
        assert_eq!(
            named("wrapper"),
            "",
            "a container named by its subtree would make every selector match everything"
        );
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn rooted_inspection_returns_only_the_requested_dom_subtree() {
        let mut document = ScriptDocument::from_html(
            "<main><section id='left'><button>Left action</button></section><section id='right'><button>Right action</button></section></main>",
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        let root = document
            .inner()
            .query_selector("#left")
            .unwrap()
            .unwrap()
            .as_u64();

        let DebugResponse::AgentSnapshot(snapshot) =
            inspect_document(&mut document, Some(root), 40, 7)
        else {
            panic!("rooted inspection did not return a semantic snapshot");
        };
        let names: Vec<_> = snapshot
            .nodes
            .iter()
            .map(|node| node.name.as_str())
            .collect();
        assert!(names.contains(&"Left action"));
        assert!(!names.contains(&"Right action"));
        assert_eq!(snapshot.revision, 7);
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
        let candidate = || SemanticCandidate {
            id: target,
            parent: None,
            visible: true,
        };
        assert_eq!(
            layout_chain_validities(&inner, &[candidate()], node_limit).get(&target),
            Some(&true)
        );

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
        assert_eq!(
            layout_chain_validities(&inner, &[candidate()], node_limit).get(&target),
            Some(&false)
        );
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
            <button id="current-page" aria-current="page">Current page</button>
            <input id="native" type="checkbox" checked>
            <button id="plain">Plain</button>
            <!--
              A framework rendering a controlled value writes the value out
              rather than omitting the attribute. Solid emits `checked="false"`
              for `checked={false}`, and reading presence alone called that
              selected: every Switch, Radio and Checkbox in the QA harness
              reported `selected: true` before anything was pressed, and could
              never change, which read as three components ignoring a click.
            -->
            <input id="native-unchecked" type="checkbox" checked="false">
            <div id="aria-false" role="option" aria-selected="false">Not selected</div>
            <button id="current-false" aria-current="false">Not current</button>
            <div id="selected-false" role="option" selected="false">Not selected</div>
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
        assert!(selected("#current-page"));
        assert!(selected("#native"));
        assert!(!selected("#plain"));
        assert!(!selected("#native-unchecked"));
        assert!(!selected("#aria-false"));
        assert!(!selected("#current-false"));
        assert!(!selected("#selected-false"));
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
    fn semantic_focus_does_not_activate_the_target() {
        let mut document = ScriptDocument::from_html(
            r#"
            <main>
              <button id="target" style="width:80px;height:30px">Start fork</button>
              <output id="result"></output>
            </main>
            <script>
              document.getElementById("target").addEventListener("click", () => {
                document.getElementById("result").textContent = "activated";
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

        focus_agent_node(&mut document, target).unwrap();

        assert_eq!(document.inner().get_focussed_node_id(), Some(target));
        assert_eq!(
            document.inner().get_node(result).unwrap().text_content(),
            ""
        );
    }

    /// macOS rather than `unix`, because this one needs a face.
    ///
    /// `set_agent_node_value` must *replace* a field's contents, not append to
    /// them, and that comes down to select-all selecting something. Selection
    /// is over shaped text, so with no font registered parley measures nothing,
    /// the select-all covers nothing, and the replacement lands after the old
    /// value. The assertion then fails for a reason that has nothing to do with
    /// this crate's value plumbing.
    ///
    /// It was deleted outright once, on the grounds that it was really a test
    /// of the font stack. It is not: the plumbing it covers is this crate's,
    /// and only the *observation* of it needs a face. So it runs where one is
    /// guaranteed and free, through Core Text, with no `fontconfig` and nothing
    /// installed. On Linux it compiles out and this crate stays clean.
    #[cfg(all(feature = "agent-control", target_os = "macos"))]
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
    fn repeated_live_captures_reuse_one_surface_and_keep_identical_pixels() {
        use blitz_traits::shell::{ColorScheme, Viewport};

        let mut document = ScriptDocument::from_html(
            "<body style='margin:0;background:transparent'><main style='width:320px;height:200px;background:rgba(24,32,42,.55);color:#f4f5f7'>\
               <h1>Capture cache</h1><button>Ready</button>\
             </main></body>",
            DocumentConfig::default(),
        );
        document
            .inner_mut()
            .set_viewport(Viewport::new(320, 200, 1.0, ColorScheme::Dark));
        document.inner_mut().resolve(0.0);

        let mut surface = None;
        let request = blitz_control_protocol::CaptureRequest::default();
        let first = capture_document_with_surface(&mut document, request, &mut surface).unwrap();
        let first_renderer = surface
            .as_ref()
            .map(|cached| std::ptr::from_ref(&cached.renderer))
            .unwrap();
        let second = capture_document_with_surface(&mut document, request, &mut surface).unwrap();
        let second_renderer = surface
            .as_ref()
            .map(|cached| std::ptr::from_ref(&cached.renderer))
            .unwrap();

        assert_eq!(first_renderer, second_renderer);
        assert_eq!(first.width, 320);
        assert_eq!(first.height, 200);
        assert_eq!(first.rgba_base64, second.rgba_base64);
    }

    #[cfg(all(feature = "diagnostics", unix))]
    #[test]
    fn node_region_matches_the_same_pixels_cut_from_a_full_document_capture() {
        use base64::Engine as _;
        use blitz_traits::shell::{ColorScheme, Viewport};

        let mut document = ScriptDocument::from_html(
            "<main style='width:320px;height:200px;background:#18202a;padding:24px'>\
               <button id='target' style='width:96px;height:40px;background:#d24db8;color:#111'>Ready</button>\
             </main>",
            DocumentConfig::default(),
        );
        document
            .inner_mut()
            .set_viewport(Viewport::new(320, 200, 1.0, ColorScheme::Dark));
        document.inner_mut().resolve(0.0);
        let (target, left, top) = {
            let inner = document.inner();
            let target = inner.query_selector("#target").unwrap().unwrap();
            let position = inner.get_node(target).unwrap().absolute_position(0.0, 0.0);
            (
                target,
                position.x.round() as usize,
                position.y.round() as usize,
            )
        };

        let mut surface = None;
        let full = capture_document_with_surface(
            &mut document,
            blitz_control_protocol::CaptureRequest::default(),
            &mut surface,
        )
        .unwrap();
        let node = capture_document_with_surface(
            &mut document,
            blitz_control_protocol::CaptureRequest {
                node_id: Some(target.as_u64()),
                scale: 1.0,
            },
            &mut surface,
        )
        .unwrap();

        let full_rgba = base64::engine::general_purpose::STANDARD
            .decode(full.rgba_base64)
            .unwrap();
        let node_rgba = base64::engine::general_purpose::STANDARD
            .decode(node.rgba_base64)
            .unwrap();
        let mut expected = Vec::with_capacity(node_rgba.len());
        for row in 0..node.height as usize {
            let start = ((top + row) * full.width as usize + left) * 4;
            expected.extend_from_slice(&full_rgba[start..start + node.width as usize * 4]);
        }

        assert!(node.width > 0);
        assert!(node.height > 0);
        assert_eq!(node_rgba, expected);
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
                dom_id: Some("scroller".into()),
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

        assert_eq!(row.scroll_offset.y, 60.0);
        assert_eq!(row.client_size.height, 100.0);
        assert!(row.scroll_size.height >= 100.0);
        assert!(row.scroll_range.height >= 0.0);
    }

    #[cfg(all(feature = "diagnostics", unix))]
    #[test]
    fn diagnostic_style_reports_resolved_font_and_border() {
        let mut document = ScriptDocument::from_html(
            "<main id='target' style='font-size:1.4375rem;border:2px solid #123456'>Readable</main>",
            DocumentConfig::default(),
        );
        document.inner_mut().resolve(0.0);
        let target = document.inner().query_selector("#target").unwrap().unwrap();
        let inner = document.inner();
        let row = diagnostic_style_row(
            &inner,
            &SemanticNode {
                dom_id: Some("target".into()),
                id: target.as_u64(),
                parent: None,
                role: "main".into(),
                name: "Readable".into(),
                value: None,
                enabled: true,
                visible: true,
                selected: false,
                bounds: Some([0.0, 0.0, 100.0, 24.0]),
                slot: None,
            },
        )
        .unwrap();

        assert_eq!(row["fontSize"], "23px");
        assert_eq!(row["borderColor"], "#123456ff");
        assert_eq!(row["borderWidth"], "2px");
        assert_eq!(row["hasTextContent"], true);
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
    #[test]
    fn agent_action_settlement_drains_until_the_document_is_idle() {
        let mut document =
            ScriptDocument::from_html("<main>Ready</main>", DocumentConfig::default());
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&polls);
        document.set_poll_hook(move |_, _| {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3
        });

        settle_agent_action(&mut document).unwrap();

        assert_eq!(polls.load(std::sync::atomic::Ordering::Relaxed), 4);
    }

    #[cfg(all(feature = "agent-control", unix))]
    #[test]
    fn agent_action_settlement_never_acks_a_still_runnable_document() {
        let mut document =
            ScriptDocument::from_html("<main>Busy</main>", DocumentConfig::default());
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&polls);
        document.set_poll_hook(move |_, _| {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        });

        let error = settle_agent_action(&mut document).unwrap_err();

        assert_eq!(error.code, "actionDidNotSettle");
        assert_eq!(
            polls.load(std::sync::atomic::Ordering::Relaxed),
            MAX_AGENT_SETTLE_POLLS
        );
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
        let (events, _event_receiver) = tokio::sync::watch::channel(None);
        let runtime = Arc::new(Mutex::new(AgentControlRuntime {
            bridge,
            events,
            server: None,
        }));
        *AGENT_CONTROL_RUNTIME
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(Arc::downgrade(&runtime));

        assert!(!agent_control_enabled());
        apply_runtime_debug_options(blitz_traits::profiling::DebugOptions {
            inspection_and_agent_control: true,
            deep_intrusive_profiling: false,
        })
        .unwrap();
        assert!(agent_control_enabled());
        assert!(!deep_profiling_enabled());

        // Enabling profiling after the listener already exists must attach a
        // consumer to that same server. AZ starts in exactly this order when
        // its runtime constructs before persisted/CLI debug options are read.
        apply_runtime_debug_options(blitz_traits::profiling::DebugOptions {
            inspection_and_agent_control: true,
            deep_intrusive_profiling: true,
        })
        .unwrap();
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
