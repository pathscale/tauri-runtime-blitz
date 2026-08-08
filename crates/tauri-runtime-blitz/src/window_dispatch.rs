use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use tauri_runtime::dpi::{PhysicalPosition, PhysicalSize, Position, Size};
use tauri_runtime::monitor::Monitor;
use tauri_runtime::webview::{DetachedWebview, PendingWebview};
use tauri_runtime::window::{
    CursorIcon, DetachedWindow, PendingWindow, RawWindow, WindowEvent, WindowSizeConstraints,
};
use tauri_runtime::{
    Icon, ProgressBarState, ResizeDirection as RuntimeResizeDirection, UserAttentionType,
    UserEvent, WindowDispatch, WindowEventId,
};
use tauri_utils::config::{Color, LogicalPosition, WindowConfig};
use tauri_utils::{Theme, TitleBarStyle};
use winit::monitor::Fullscreen;
use winit::window::Window;

use crate::BlitzWindowBuilder;
use crate::runtime::{BlitzRuntime, BlitzRuntimeContext};

type WindowEventHandler = Box<dyn Fn(&WindowEvent) + Send + 'static>;

pub(crate) struct NativeWindowState {
    pub label: String,
    pub config: Mutex<WindowConfig>,
    pub native: Mutex<Option<Arc<dyn Window>>>,
    pub event_handlers: Mutex<HashMap<WindowEventId, WindowEventHandler>>,
}

impl fmt::Debug for NativeWindowState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeWindowState")
            .field("label", &self.label)
            .field("config", &self.config.lock().unwrap())
            .field("native_created", &self.native.lock().unwrap().is_some())
            .finish()
    }
}

pub struct BlitzWindowDispatcher<T: UserEvent> {
    pub(crate) id: tauri_runtime::window::WindowId,
    pub(crate) context: BlitzRuntimeContext<T>,
    pub(crate) state: Arc<NativeWindowState>,
}

impl<T: UserEvent> Clone for BlitzWindowDispatcher<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            context: self.context.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl<T: UserEvent> fmt::Debug for BlitzWindowDispatcher<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlitzWindowDispatcher")
            .field("id", &self.id)
            .field("state", &self.state)
            .finish()
    }
}

impl<T: UserEvent> BlitzWindowDispatcher<T> {
    fn with_native<R>(&self, operation: impl FnOnce(&dyn Window) -> R) -> Option<R> {
        self.state.native.lock().unwrap().as_deref().map(operation)
    }

    fn update_config(&self, update: impl FnOnce(&mut WindowConfig)) {
        update(&mut self.state.config.lock().unwrap());
    }
}

impl<T: UserEvent> WindowDispatch<T> for BlitzWindowDispatcher<T> {
    type Runtime = BlitzRuntime<T>;
    type WindowBuilder = BlitzWindowBuilder;

    fn run_on_main_thread<F: FnOnce() + Send + 'static>(
        &self,
        task: F,
    ) -> tauri_runtime::Result<()> {
        self.context.run_on_main_thread(task)
    }

    fn on_window_event<F: Fn(&WindowEvent) + Send + 'static>(&self, handler: F) -> WindowEventId {
        let id = self.context.next_window_event_id();
        self.state
            .event_handlers
            .lock()
            .unwrap()
            .insert(id, Box::new(handler));
        id
    }

    fn scale_factor(&self) -> tauri_runtime::Result<f64> {
        Ok(self.with_native(Window::scale_factor).unwrap_or(1.0))
    }

    fn inner_position(&self) -> tauri_runtime::Result<PhysicalPosition<i32>> {
        Ok(self
            .with_native(Window::surface_position)
            .unwrap_or_default())
    }

    fn outer_position(&self) -> tauri_runtime::Result<PhysicalPosition<i32>> {
        self.with_native(|window| window.outer_position().ok())
            .flatten()
            .ok_or(tauri_runtime::Error::CreateWindow)
    }

    fn inner_size(&self) -> tauri_runtime::Result<PhysicalSize<u32>> {
        Ok(self.with_native(Window::surface_size).unwrap_or_else(|| {
            let config = self.state.config.lock().unwrap();
            PhysicalSize::new(config.width as u32, config.height as u32)
        }))
    }

    fn outer_size(&self) -> tauri_runtime::Result<PhysicalSize<u32>> {
        Ok(self
            .with_native(Window::outer_size)
            .unwrap_or(self.inner_size()?))
    }

    fn is_fullscreen(&self) -> tauri_runtime::Result<bool> {
        Ok(self
            .with_native(|window| window.fullscreen().is_some())
            .unwrap_or_else(|| self.state.config.lock().unwrap().fullscreen))
    }

    fn is_minimized(&self) -> tauri_runtime::Result<bool> {
        Ok(self
            .with_native(|window| window.is_minimized().unwrap_or(false))
            .unwrap_or(false))
    }

    fn is_maximized(&self) -> tauri_runtime::Result<bool> {
        Ok(self
            .with_native(Window::is_maximized)
            .unwrap_or_else(|| self.state.config.lock().unwrap().maximized))
    }

    fn is_focused(&self) -> tauri_runtime::Result<bool> {
        Ok(self.with_native(Window::has_focus).unwrap_or(false))
    }

    fn is_decorated(&self) -> tauri_runtime::Result<bool> {
        Ok(self
            .with_native(Window::is_decorated)
            .unwrap_or_else(|| self.state.config.lock().unwrap().decorations))
    }

    fn is_resizable(&self) -> tauri_runtime::Result<bool> {
        Ok(self
            .with_native(Window::is_resizable)
            .unwrap_or_else(|| self.state.config.lock().unwrap().resizable))
    }

    fn is_maximizable(&self) -> tauri_runtime::Result<bool> {
        Ok(self.state.config.lock().unwrap().maximizable)
    }

    fn is_minimizable(&self) -> tauri_runtime::Result<bool> {
        Ok(self.state.config.lock().unwrap().minimizable)
    }

    fn is_closable(&self) -> tauri_runtime::Result<bool> {
        Ok(self.state.config.lock().unwrap().closable)
    }

    fn is_visible(&self) -> tauri_runtime::Result<bool> {
        Ok(self
            .with_native(|window| window.is_visible().unwrap_or(false))
            .unwrap_or_else(|| self.state.config.lock().unwrap().visible))
    }

    fn is_enabled(&self) -> tauri_runtime::Result<bool> {
        Ok(true)
    }

    fn is_always_on_top(&self) -> tauri_runtime::Result<bool> {
        Ok(self.state.config.lock().unwrap().always_on_top)
    }

    fn title(&self) -> tauri_runtime::Result<String> {
        Ok(self
            .with_native(Window::title)
            .unwrap_or_else(|| self.state.config.lock().unwrap().title.clone()))
    }

    fn current_monitor(&self) -> tauri_runtime::Result<Option<Monitor>> {
        Ok(None)
    }

    fn primary_monitor(&self) -> tauri_runtime::Result<Option<Monitor>> {
        Ok(None)
    }

    fn monitor_from_point(&self, _x: f64, _y: f64) -> tauri_runtime::Result<Option<Monitor>> {
        Ok(None)
    }

    fn available_monitors(&self) -> tauri_runtime::Result<Vec<Monitor>> {
        Ok(Vec::new())
    }

    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        Err(raw_window_handle::HandleError::NotSupported)
    }

    fn theme(&self) -> tauri_runtime::Result<Theme> {
        Ok(match self.with_native(Window::theme).flatten() {
            Some(winit::window::Theme::Dark) => Theme::Dark,
            _ => Theme::Light,
        })
    }

    fn center(&self) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn request_user_attention(
        &self,
        _request_type: Option<UserAttentionType>,
    ) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn create_window<F: Fn(RawWindow) + Send + 'static>(
        &mut self,
        pending: PendingWindow<T, Self::Runtime>,
        after_window_creation: Option<F>,
    ) -> tauri_runtime::Result<DetachedWindow<T, Self::Runtime>> {
        self.context.create_window(pending, after_window_creation)
    }

    fn create_webview(
        &mut self,
        pending: PendingWebview<T, Self::Runtime>,
    ) -> tauri_runtime::Result<DetachedWebview<T, Self::Runtime>> {
        self.context.create_webview(self.id, pending)
    }

    fn set_resizable(&self, resizable: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.resizable = resizable);
        self.with_native(|window| window.set_resizable(resizable));
        Ok(())
    }

    fn set_enabled(&self, _enabled: bool) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_maximizable(&self, maximizable: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.maximizable = maximizable);
        Ok(())
    }

    fn set_minimizable(&self, minimizable: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.minimizable = minimizable);
        Ok(())
    }

    fn set_closable(&self, closable: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.closable = closable);
        Ok(())
    }

    fn set_title<S: Into<String>>(&self, title: S) -> tauri_runtime::Result<()> {
        let title = title.into();
        self.update_config(|config| config.title = title.clone());
        self.with_native(|window| window.set_title(&title));
        Ok(())
    }

    fn maximize(&self) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.maximized = true);
        self.with_native(|window| window.set_maximized(true));
        Ok(())
    }

    fn unmaximize(&self) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.maximized = false);
        self.with_native(|window| window.set_maximized(false));
        Ok(())
    }

    fn minimize(&self) -> tauri_runtime::Result<()> {
        self.with_native(|window| window.set_minimized(true));
        Ok(())
    }

    fn unminimize(&self) -> tauri_runtime::Result<()> {
        self.with_native(|window| window.set_minimized(false));
        Ok(())
    }

    fn show(&self) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.visible = true);
        self.with_native(|window| window.set_visible(true));
        Ok(())
    }

    fn hide(&self) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.visible = false);
        self.with_native(|window| window.set_visible(false));
        Ok(())
    }

    fn close(&self) -> tauri_runtime::Result<()> {
        self.context.close_window(self.id)
    }

    fn destroy(&self) -> tauri_runtime::Result<()> {
        self.context.close_window(self.id)
    }

    fn set_decorations(&self, decorations: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.decorations = decorations);
        self.with_native(|window| window.set_decorations(decorations));
        Ok(())
    }

    fn set_shadow(&self, shadow: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.shadow = shadow);
        Ok(())
    }

    fn set_always_on_bottom(&self, value: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.always_on_bottom = value);
        Ok(())
    }

    fn set_always_on_top(&self, value: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.always_on_top = value);
        Ok(())
    }

    fn set_visible_on_all_workspaces(&self, value: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.visible_on_all_workspaces = value);
        Ok(())
    }

    fn set_background_color(&self, color: Option<Color>) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.background_color = color);
        Ok(())
    }

    fn set_content_protected(&self, value: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.content_protected = value);
        self.with_native(|window| window.set_content_protected(value));
        Ok(())
    }

    fn set_size(&self, size: Size) -> tauri_runtime::Result<()> {
        self.with_native(|window| {
            let _ = window.request_surface_size(size);
        });
        Ok(())
    }

    fn set_min_size(&self, size: Option<Size>) -> tauri_runtime::Result<()> {
        self.with_native(|window| window.set_min_surface_size(size));
        Ok(())
    }

    fn set_max_size(&self, size: Option<Size>) -> tauri_runtime::Result<()> {
        self.with_native(|window| window.set_max_surface_size(size));
        Ok(())
    }

    fn set_size_constraints(
        &self,
        _constraints: WindowSizeConstraints,
    ) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_position(&self, position: Position) -> tauri_runtime::Result<()> {
        self.with_native(|window| window.set_outer_position(position));
        Ok(())
    }

    fn set_fullscreen(&self, fullscreen: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.fullscreen = fullscreen);
        self.with_native(|window| {
            window.set_fullscreen(fullscreen.then_some(Fullscreen::Borderless(None)))
        });
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn set_simple_fullscreen(&self, enable: bool) -> tauri_runtime::Result<()> {
        self.set_fullscreen(enable)
    }

    fn set_focus(&self) -> tauri_runtime::Result<()> {
        self.with_native(Window::focus_window);
        Ok(())
    }

    fn set_focusable(&self, focusable: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.focusable = focusable);
        Ok(())
    }

    fn set_icon(&self, _icon: Icon) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_skip_taskbar(&self, skip: bool) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.skip_taskbar = skip);
        Ok(())
    }

    fn set_cursor_grab(&self, _grab: bool) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_cursor_visible(&self, visible: bool) -> tauri_runtime::Result<()> {
        self.with_native(|window| window.set_cursor_visible(visible));
        Ok(())
    }

    fn set_cursor_icon(&self, _icon: CursorIcon) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_cursor_position<Pos: Into<Position>>(&self, position: Pos) -> tauri_runtime::Result<()> {
        self.with_native(|window| window.set_cursor_position(position.into()));
        Ok(())
    }

    fn set_ignore_cursor_events(&self, ignore: bool) -> tauri_runtime::Result<()> {
        self.with_native(|window| window.set_cursor_hittest(!ignore));
        Ok(())
    }

    fn start_dragging(&self) -> tauri_runtime::Result<()> {
        self.with_native(Window::drag_window);
        Ok(())
    }

    fn start_resize_dragging(
        &self,
        _direction: RuntimeResizeDirection,
    ) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_badge_count(
        &self,
        _count: Option<i64>,
        _desktop_filename: Option<String>,
    ) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_badge_label(&self, _label: Option<String>) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_overlay_icon(&self, _icon: Option<Icon>) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_progress_bar(&self, _state: ProgressBarState) -> tauri_runtime::Result<()> {
        Ok(())
    }

    fn set_title_bar_style(&self, style: TitleBarStyle) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.title_bar_style = style);
        Ok(())
    }

    fn set_traffic_light_position(&self, position: Position) -> tauri_runtime::Result<()> {
        let logical = position.to_logical::<f64>(1.0);
        self.update_config(|config| {
            config.traffic_light_position = Some(LogicalPosition {
                x: logical.x,
                y: logical.y,
            })
        });
        Ok(())
    }

    fn set_theme(&self, theme: Option<Theme>) -> tauri_runtime::Result<()> {
        self.update_config(|config| config.theme = theme);
        let native_theme = theme.map(|theme| match theme {
            Theme::Dark => winit::window::Theme::Dark,
            _ => winit::window::Theme::Light,
        });
        self.with_native(|window| window.set_theme(native_theme));
        Ok(())
    }
}
