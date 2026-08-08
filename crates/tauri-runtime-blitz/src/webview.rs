use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use blitz_script::ScriptDocument;
use tauri_runtime::dpi::{PhysicalPosition, PhysicalSize, Position, Rect, Size};
use tauri_runtime::webview::{DetachedWebview, PendingWebview};
use tauri_runtime::window::{WebviewEvent, WindowId};
use tauri_runtime::{Cookie, Error, Runtime, UserEvent, WebviewDispatch, WebviewEventId};
use tauri_utils::config::Color;
use url::Url;

use crate::{ScriptQueue, attach_ipc_handler};

static NEXT_EVENT_ID: AtomicU32 = AtomicU32::new(1);

/// Tauri's pending webview state paired with the live Blitz document and dispatcher.
pub struct PreparedBlitzWebview<T: UserEvent, R: Runtime<T>> {
    pub document: ScriptDocument,
    pub detached: DetachedWebview<T, R>,
    pub pending: PendingWebview<T, R>,
}

/// Prepare a Tauri webview around an already-loaded Blitz document.
///
/// Initialization scripts run before the document's own scripts. The IPC handler is installed
/// before first poll, and queued dispatcher work is drained by that native document poll cycle.
pub fn prepare_pending_webview<T, R>(
    mut pending: PendingWebview<T, R>,
    mut document: ScriptDocument,
) -> PreparedBlitzWebview<T, R>
where
    T: UserEvent,
    R: Runtime<T, WebviewDispatcher = BlitzWebviewDispatcher<T, R>>,
{
    for initialization_script in pending.webview_attributes.initialization_scripts.drain(..) {
        document.eval(&initialization_script.script);
    }

    let scripts = ScriptQueue::default();
    scripts.attach_to(&mut document);
    let dispatcher = BlitzWebviewDispatcher::new(
        scripts,
        pending.url.clone(),
        pending.webview_attributes.bounds.unwrap_or_default(),
    );
    let detached = DetachedWebview {
        label: pending.label.clone(),
        dispatcher,
    };

    if let Some(ipc_handler) = pending.ipc_handler.take() {
        attach_ipc_handler(
            &mut document,
            pending.url.clone(),
            detached.clone(),
            ipc_handler,
        );
    }

    PreparedBlitzWebview {
        document,
        detached,
        pending,
    }
}

/// Thread-safe Tauri webview dispatcher backed by a [`ScriptQueue`].
///
/// The dispatcher does not touch Boa directly. Its methods enqueue work that the owning Blitz
/// document drains on its event-loop thread.
pub struct BlitzWebviewDispatcher<T, R> {
    scripts: ScriptQueue,
    url: Arc<Mutex<String>>,
    bounds: Arc<Mutex<Rect>>,
    marker: PhantomData<fn() -> (T, R)>,
}

impl<T, R> BlitzWebviewDispatcher<T, R> {
    pub fn new(scripts: ScriptQueue, url: impl Into<String>, bounds: Rect) -> Self {
        Self {
            scripts,
            url: Arc::new(Mutex::new(url.into())),
            bounds: Arc::new(Mutex::new(bounds)),
            marker: PhantomData,
        }
    }

    pub fn script_queue(&self) -> &ScriptQueue {
        &self.scripts
    }
}

impl<T, R> Clone for BlitzWebviewDispatcher<T, R> {
    fn clone(&self) -> Self {
        Self {
            scripts: self.scripts.clone(),
            url: Arc::clone(&self.url),
            bounds: Arc::clone(&self.bounds),
            marker: PhantomData,
        }
    }
}

impl<T, R> fmt::Debug for BlitzWebviewDispatcher<T, R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlitzWebviewDispatcher")
            .field("scripts", &self.scripts)
            .field("url", &self.url.lock().unwrap())
            .field("bounds", &self.bounds.lock().unwrap())
            .finish()
    }
}

impl<T, R> WebviewDispatch<T> for BlitzWebviewDispatcher<T, R>
where
    T: UserEvent,
    R: Runtime<T, WebviewDispatcher = Self>,
{
    type Runtime = R;

    fn run_on_main_thread<F: FnOnce() + Send + 'static>(
        &self,
        task: F,
    ) -> tauri_runtime::Result<()> {
        self.scripts.enqueue_task(task);
        Ok(())
    }

    fn on_webview_event<F: Fn(&WebviewEvent) + Send + 'static>(
        &self,
        _handler: F,
    ) -> WebviewEventId {
        NEXT_EVENT_ID.fetch_add(1, Ordering::Relaxed)
    }

    fn with_webview<F: FnOnce(Box<dyn std::any::Any>) + Send + 'static>(
        &self,
        _handler: F,
    ) -> tauri_runtime::Result<()> {
        Ok(())
    }

    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn open_devtools(&self) {}

    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn close_devtools(&self) {}

    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn is_devtools_open(&self) -> tauri_runtime::Result<bool> {
        Ok(false)
    }

    fn url(&self) -> tauri_runtime::Result<String> {
        Ok(self.url.lock().unwrap().clone())
    }

    fn bounds(&self) -> tauri_runtime::Result<Rect> {
        Ok(*self.bounds.lock().unwrap())
    }

    fn position(&self) -> tauri_runtime::Result<PhysicalPosition<i32>> {
        Ok(self.bounds()?.position.to_physical(1.0))
    }

    fn size(&self) -> tauri_runtime::Result<PhysicalSize<u32>> {
        Ok(self.bounds()?.size.to_physical(1.0))
    }

    fn navigate(&self, url: Url) -> tauri_runtime::Result<()> {
        *self.url.lock().unwrap() = url.to_string();
        Ok(())
    }

    fn reload(&self) -> tauri_runtime::Result<()> {
        Err(Error::FailedToSendMessage)
    }

    fn print(&self) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn close(&self) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_bounds(&self, bounds: Rect) -> tauri_runtime::Result<()> {
        *self.bounds.lock().unwrap() = bounds;
        Ok(())
    }

    fn set_size(&self, size: Size) -> tauri_runtime::Result<()> {
        self.bounds.lock().unwrap().size = size;
        Ok(())
    }

    fn set_position(&self, position: Position) -> tauri_runtime::Result<()> {
        self.bounds.lock().unwrap().position = position;
        Ok(())
    }

    fn set_focus(&self) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn hide(&self) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn show(&self) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn eval_script<S: Into<String>>(&self, script: S) -> tauri_runtime::Result<()> {
        self.scripts.enqueue(script);
        Ok(())
    }

    fn eval_script_with_callback<S: Into<String>>(
        &self,
        script: S,
        callback: impl Fn(String) + Send + 'static,
    ) -> tauri_runtime::Result<()> {
        self.scripts.enqueue_with_callback(script, callback);
        Ok(())
    }

    fn reparent(&self, _window_id: WindowId) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn cookies_for_url(&self, _url: Url) -> tauri_runtime::Result<Vec<Cookie<'static>>> {
        Ok(Vec::new())
    }

    fn cookies(&self) -> tauri_runtime::Result<Vec<Cookie<'static>>> {
        Ok(Vec::new())
    }

    fn set_cookie(&self, _cookie: Cookie<'_>) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn delete_cookie(&self, _cookie: Cookie<'_>) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_auto_resize(&self, _auto_resize: bool) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_zoom(&self, _scale_factor: f64) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_background_color(&self, _color: Option<Color>) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn clear_all_browsing_data(&self) -> tauri_runtime::Result<()> {
        Ok(())
    }
}
