use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
#[cfg(all(feature = "agent-control", unix))]
use std::sync::Weak;
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
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
use blitz_control_protocol::document::snapshot_document;
#[cfg(all(feature = "diagnostics", unix))]
use blitz_control_protocol::{
    DebugSnapshot, DiagnosticsRequest, SnapshotRequest, WindowComposition,
};
/*
 * The control surface is `blitz-control-protocol`, not this crate.
 *
 * The vocabulary, the core that reads and drives a document, the socket and
 * the in-process transport over it all live there. What is left here is a
 * window runtime that happens to own a document, wiring its own document into
 * that surface: `DocumentControl` performs the actions, this decides which
 * window's document to perform them on and what to do about `Relaunch` and
 * `Quit`, which are about the process rather than the document.
 *
 * Nothing is re-exported. A consumer that names a control type depends on the
 * crate that defines it, because a runtime that forwards one has not removed
 * the edge, only renamed it.
 */
use crate::window_dispatch::{BlitzWindowDispatcher, NativeWindowState};
use crate::{
    BlitzWebviewDispatcher, BlitzWindowBuilder, PreparedBlitzWebview, prepare_pending_webview,
};
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::in_process::DocumentControl;
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::server::{
    AgentControlServer, ControlBridge, ControlBridgeRequest, Host,
};
#[cfg(all(feature = "agent-control", unix))]
use blitz_control_protocol::{AgentControlRequest, DebugError, DebugEvent, DebugResponse};

type DocumentFactory = dyn Fn(&str) -> Result<ScriptDocument, String> + Send + Sync + 'static;
type RuntimeTrace = dyn Fn(&str) + Send + Sync + 'static;

static DOCUMENT_FACTORY: OnceLock<RwLock<Option<Arc<DocumentFactory>>>> = OnceLock::new();
static RUNTIME_TRACE: OnceLock<RwLock<Option<Arc<RuntimeTrace>>>> = OnceLock::new();
#[cfg(all(feature = "agent-control", unix))]
static AGENT_CONTROL_RUNTIME: OnceLock<Mutex<Option<Weak<Mutex<AgentControlRuntime>>>>> =
    OnceLock::new();

/// One lock for every test that starts or stops the control server.
///
/// `AGENT_CONTROL_RUNTIME` is process-wide, so two tests that enable
/// inspection at the same time fight over one listener. This crate keeps its
/// own lock rather than sharing the transport's: the state being serialised is
/// this runtime's, and the transport has no idea a second one exists.
#[cfg(all(feature = "agent-control", unix, test))]
static CONTROL_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
                Host {
                    name: env!("CARGO_PKG_NAME").into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    diagnostics: cfg!(feature = "diagnostics"),
                },
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
    // Only a build that can collect diagnostics holds a sampling session, so
    // only that build has one to refresh.
    #[cfg(feature = "diagnostics")]
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
    /// The in-process transport over this window's document.
    ///
    /// It performs the actions and holds what one request leaves for the next:
    /// the revision, the injected pointer, which buttons it holds, and the
    /// capture surface. All of that was here, in fields beside a second copy of
    /// the action dispatch that had already drifted from the browser's.
    #[cfg(all(feature = "agent-control", unix))]
    control: DocumentControl,
    #[cfg(all(feature = "agent-control", unix))]
    paint_revision: u64,
    #[cfg(all(feature = "agent-control", unix))]
    control_events: tokio::sync::watch::Sender<Option<DebugEvent>>,
}

impl<T: UserEvent> RuntimeApplication<T> {
    fn emit(&mut self, event: RunEvent<T>) {
        if let Some(callback) = &mut self.callback {
            callback(event);
        }
    }

    #[cfg(all(feature = "diagnostics", unix))]
    fn agent_document(&mut self) -> Option<&mut ScriptDocument> {
        self.blitz
            .get_mut()
            .windows
            .values_mut()
            .find_map(|view| view.try_downcast_doc_mut::<ScriptDocument>())
    }

    /// Answer one agent-control request against this window's document.
    ///
    /// Reading the tree and performing an action are the in-process transport's
    /// job, and it does them the same way for every host. What is left here is
    /// what only a runtime can answer: which window's document is the active
    /// one, and what `Relaunch` and `Quit` mean for a process it started.
    #[cfg(all(feature = "agent-control", unix))]
    fn handle_builtin_agent(&mut self, request: AgentControlRequest) -> DebugResponse {
        match request {
            AgentControlRequest::Inspect { .. } | AgentControlRequest::Act(_) => {
                let Self { blitz, control, .. } = self;
                let Some(document) = blitz
                    .get_mut()
                    .windows
                    .values_mut()
                    .find_map(|view| view.try_downcast_doc_mut::<ScriptDocument>())
                else {
                    return DebugResponse::Error(DebugError {
                        code: "documentUnavailable".into(),
                        message: "no active script document".into(),
                    });
                };
                control.agent(document, request)
            }
            // The embedder's answer if it registered one, and this runtime's
            // own if it did not. Whose answer it is is not this crate's to
            // decide: an application that must drain a database before it
            // restarts registers with `blitz_control_protocol::lifecycle`, and
            // does not reach a window runtime to do it.
            AgentControlRequest::Relaunch => blitz_control_protocol::lifecycle::lifecycle_handler()
                .map(|handler| handler(AgentControlRequest::Relaunch))
                .unwrap_or_else(|| match relaunch_current_process() {
                    Ok(()) => DebugResponse::Ack,
                    Err(error) => DebugResponse::Error(DebugError {
                        code: "relaunchFailed".into(),
                        message: error.to_string(),
                    }),
                }),
            AgentControlRequest::Quit => DebugResponse::Ack,
            _ => DebugResponse::Error(DebugError {
                code: "unsupportedRequest".into(),
                message: "this runtime does not implement that agent-control request".into(),
            }),
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
            // The one diagnostic only a runtime can answer. A renderer capture
            // cannot see whether an opaque platform window or a tinted native
            // glass view was placed behind its transparent root, and this crate
            // is what put it there.
            DiagnosticsRequest::WindowComposition => {
                DebugResponse::WindowComposition(native_window_composition())
            }
            DiagnosticsRequest::Capture(request) => self
                .capture_image(request)
                .map(DebugResponse::Captured)
                .unwrap_or_else(DebugResponse::Error),
            DiagnosticsRequest::Observe { .. } => DebugResponse::Error(DebugError {
                code: "streamingUnavailable".into(),
                message: "diagnostic subscriptions are not implemented; request snapshots or \
                          metrics"
                    .into(),
            }),
            _ => DebugResponse::Error(DebugError {
                code: "unsupportedRequest".into(),
                message: "this runtime does not implement that diagnostics request".into(),
            }),
        }
    }

    /// Draw the document offscreen and hand back the pixels.
    ///
    /// The point of this is that it is the *same* paint. `blitz_paint::paint_scene`
    /// is the entry point the window renderer uses, so a capture cannot agree
    /// with a broken frame or disagree with a good one: whatever the window
    /// would show is what lands in this buffer.
    #[cfg(all(feature = "diagnostics", unix))]
    fn capture_image(
        &mut self,
        request: blitz_control_protocol::CaptureRequest,
    ) -> Result<blitz_control_protocol::CapturedImage, DebugError> {
        let Self { blitz, control, .. } = self;
        let document = blitz
            .get_mut()
            .windows
            .values_mut()
            .find_map(|view| view.try_downcast_doc_mut::<ScriptDocument>())
            .ok_or_else(|| DebugError {
                code: "documentUnavailable".into(),
                message: "no active script document".into(),
            })?;
        control.capture(document, request)
    }
}

impl<T: UserEvent> RuntimeApplication<T> {
    #[cfg(all(feature = "diagnostics", unix))]
    fn collect_diagnostics(
        &mut self,
        request: SnapshotRequest,
    ) -> Result<DebugSnapshot, DebugError> {
        let revision = self.control.next_revision();
        let document = self.agent_document().ok_or_else(|| DebugError {
            code: "documentUnavailable".into(),
            message: "no active script document".into(),
        })?;
        snapshot_document(document, request, revision)
    }
}

impl<T: UserEvent> RuntimeApplication<T> {
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
                            let delegated =
                                blitz_control_protocol::lifecycle::lifecycle_handler().is_some();
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
                        // A build without `diagnostics` answers rather than
                        // failing to compile the arm. The transport forwards
                        // every request now, because whether a host can collect
                        // a snapshot is the host's answer and not something a
                        // socket can infer; `Host::diagnostics` is what keeps
                        // the tool out of `tools/list` so a client is not
                        // invited to ask in the first place.
                        #[cfg(not(feature = "diagnostics"))]
                        ControlBridgeRequest::Diagnostics(_) => DebugResponse::Error(DebugError {
                            code: "diagnosticsUnavailable".into(),
                            message: "this build has no diagnostics feature; rebuild with \
                                      tauri-runtime-blitz/diagnostics"
                                .into(),
                        }),
                        // Answered here, and only here. There used to be a
                        // `set_diagnostics_handler` an embedder could install
                        // to answer instead, which is a runtime proxying an
                        // inspection service: the collectors, the offscreen
                        // paint and the snapshot readers are all
                        // `blitz-control-protocol`, and an embedder that wants
                        // to serve them serves them itself rather than through
                        // the crate that owns the window. Nothing installed
                        // one.
                        #[cfg(feature = "diagnostics")]
                        ControlBridgeRequest::Diagnostics(request) => {
                            self.handle_builtin_diagnostics(request)
                        }
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
                    AgentControlServer::start_with_events(
                        bridge.clone(),
                        Host {
                            name: env!("CARGO_PKG_NAME").into(),
                            version: env!("CARGO_PKG_VERSION").into(),
                            diagnostics: cfg!(feature = "diagnostics"),
                        },
                        event_receiver,
                    )
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
            control: DocumentControl::new(),
            #[cfg(all(feature = "agent-control", unix))]
            paint_revision: 0,
            #[cfg(all(feature = "agent-control", unix))]
            control_events,
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
