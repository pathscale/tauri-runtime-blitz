//! Tauri runtime backed by Blitz and Boa.
//!
//! The first implementation target is Tauri 2.11.x. Its runtime traits are not semver-stable,
//! so versions stay exact and upgrades are deliberate.

use tauri_runtime::Icon;
use tauri_runtime::dpi::{Position, Size};
use tauri_runtime::window::{WindowBuilder, WindowBuilderBase, WindowSizeConstraints};
use tauri_utils::config::{
    Color, LogicalPosition, PreventOverflowConfig, PreventOverflowMargin, WindowConfig,
};
use tauri_utils::{Theme, TitleBarStyle};

mod ipc;
pub use ipc::attach_ipc_handler;
#[cfg(all(feature = "agent-control", unix))]
mod agent_control_server;
/// Serving inspection from a host that is not this runtime.
///
/// The server binds a socket and forwards framed requests to a closure; it has
/// no window, no event loop and no Tauri in it, and the module's own tests
/// start one from a bare closure. Exported so a headless host holding a
/// document can be inspected the same way the application is.
///
/// While this was private, inspecting a Blitz document from outside required
/// opening a window. A QA harness that must not take over the desktop was
/// pushed into screenshots and tree-file dumps instead, and neither can answer
/// a question that involves clicking something.
#[cfg(all(feature = "agent-control", unix))]
pub use agent_control_server::{AgentControlServer, ControlBridge, ControlBridgeRequest};
/// The wire protocol, which lives in its own crate so clients can speak it
/// without building a renderer. Re-exported under the name this crate has
/// always used it by, so `control_protocol::` paths keep resolving.
#[cfg(feature = "agent-control")]
pub use blitz_control_protocol as control_protocol;
/// Answer an `Inspect` request against a document, from the same code the
/// runtime uses. A headless host needs this to serve the socket it now can
/// host; reimplementing it is how a harness ends up disagreeing with the
/// inspector about what a node is called.
#[cfg(all(feature = "agent-control", unix))]
pub use runtime::{click_agent_node, hover_agent_node, inspect_document, press_agent_key};
mod script_queue;
pub use script_queue::ScriptQueue;
mod runtime;
#[cfg(target_os = "macos")]
mod window_effects;
#[cfg(feature = "agent-control")]
pub use blitz_traits::profiling::DebugOptions as RuntimeDebugOptions;
pub use runtime::{
    BlitzEventLoopProxy, BlitzRuntime, BlitzRuntimeHandle, builder, set_document_factory,
    set_runtime_trace,
};
#[cfg(all(feature = "agent-control", unix))]
pub use runtime::{
    agent_control_enabled, apply_runtime_debug_options, set_agent_control_enabled,
    set_agent_control_handler,
};
#[cfg(feature = "agent-control")]
pub use runtime::{
    begin_deep_profiling, deep_profiling_enabled, deep_profiling_permitted,
    set_deep_profiling_permitted,
};
#[cfg(all(feature = "diagnostics", unix))]
pub use runtime::{capture_document, set_diagnostics_handler};
/// Apply reusable macOS window glass without exposing AppKit to embedders.
#[cfg(target_os = "macos")]
pub use window_effects::set_window_glass;
mod webview;
pub use webview::{BlitzWebviewDispatcher, PreparedBlitzWebview, prepare_pending_webview};
mod window_dispatch;
pub use window_dispatch::BlitzWindowDispatcher;

/// Renderer-neutral window attributes retained until the native Blitz window is created.
#[derive(Debug, Clone)]
pub struct BlitzWindowBuilder {
    pub config: WindowConfig,
    pub constraints: Option<WindowSizeConstraints>,
    pub icon_set: bool,
    #[cfg(target_os = "macos")]
    pub parent: Option<*mut std::ffi::c_void>,
}

impl WindowBuilderBase for BlitzWindowBuilder {}

impl WindowBuilder for BlitzWindowBuilder {
    fn new() -> Self {
        Self {
            config: WindowConfig::default(),
            constraints: None,
            icon_set: false,
            #[cfg(target_os = "macos")]
            parent: None,
        }
    }

    fn with_config(config: &WindowConfig) -> Self {
        Self {
            config: config.clone(),
            constraints: None,
            icon_set: false,
            #[cfg(target_os = "macos")]
            parent: None,
        }
    }

    fn center(mut self) -> Self {
        self.config.center = true;
        self
    }

    fn position(mut self, x: f64, y: f64) -> Self {
        self.config.x = Some(x);
        self.config.y = Some(y);
        self
    }

    fn inner_size(mut self, width: f64, height: f64) -> Self {
        self.config.width = width;
        self.config.height = height;
        self
    }

    fn min_inner_size(mut self, width: f64, height: f64) -> Self {
        self.config.min_width = Some(width);
        self.config.min_height = Some(height);
        self
    }

    fn max_inner_size(mut self, width: f64, height: f64) -> Self {
        self.config.max_width = Some(width);
        self.config.max_height = Some(height);
        self
    }

    fn inner_size_constraints(mut self, constraints: WindowSizeConstraints) -> Self {
        self.constraints = Some(constraints);
        self
    }

    fn prevent_overflow(mut self) -> Self {
        self.config.prevent_overflow = Some(PreventOverflowConfig::Enable(true));
        self
    }

    fn prevent_overflow_with_margin(mut self, margin: Size) -> Self {
        let margin = margin.to_physical::<u32>(1.0);
        self.config.prevent_overflow = Some(PreventOverflowConfig::Margin(PreventOverflowMargin {
            width: margin.width,
            height: margin.height,
        }));
        self
    }

    fn resizable(mut self, value: bool) -> Self {
        self.config.resizable = value;
        self
    }

    fn maximizable(mut self, value: bool) -> Self {
        self.config.maximizable = value;
        self
    }

    fn minimizable(mut self, value: bool) -> Self {
        self.config.minimizable = value;
        self
    }

    fn closable(mut self, value: bool) -> Self {
        self.config.closable = value;
        self
    }

    fn title<S: Into<String>>(mut self, title: S) -> Self {
        self.config.title = title.into();
        self
    }

    fn fullscreen(mut self, value: bool) -> Self {
        self.config.fullscreen = value;
        self
    }

    fn focused(mut self, value: bool) -> Self {
        self.config.focus = value;
        self
    }

    fn focusable(mut self, value: bool) -> Self {
        self.config.focusable = value;
        self
    }

    fn maximized(mut self, value: bool) -> Self {
        self.config.maximized = value;
        self
    }

    fn visible(mut self, value: bool) -> Self {
        self.config.visible = value;
        self
    }

    #[cfg(any(not(target_os = "macos"), feature = "macos-private-api"))]
    fn transparent(mut self, value: bool) -> Self {
        self.config.transparent = value;
        self
    }

    fn decorations(mut self, value: bool) -> Self {
        self.config.decorations = value;
        self
    }

    fn always_on_bottom(mut self, value: bool) -> Self {
        self.config.always_on_bottom = value;
        self
    }

    fn always_on_top(mut self, value: bool) -> Self {
        self.config.always_on_top = value;
        self
    }

    fn visible_on_all_workspaces(mut self, value: bool) -> Self {
        self.config.visible_on_all_workspaces = value;
        self
    }

    fn content_protected(mut self, value: bool) -> Self {
        self.config.content_protected = value;
        self
    }

    fn icon(mut self, _: Icon) -> tauri_runtime::Result<Self> {
        self.icon_set = true;
        Ok(self)
    }

    fn skip_taskbar(mut self, value: bool) -> Self {
        self.config.skip_taskbar = value;
        self
    }

    fn background_color(mut self, color: Color) -> Self {
        self.config.background_color = Some(color);
        self
    }

    fn shadow(mut self, value: bool) -> Self {
        self.config.shadow = value;
        self
    }

    #[cfg(target_os = "macos")]
    fn parent(mut self, parent: *mut std::ffi::c_void) -> Self {
        self.parent = Some(parent);
        self
    }

    #[cfg(target_os = "macos")]
    fn title_bar_style(mut self, style: TitleBarStyle) -> Self {
        self.config.title_bar_style = style;
        self
    }

    #[cfg(target_os = "macos")]
    fn traffic_light_position<P: Into<Position>>(mut self, position: P) -> Self {
        let position = position.into();
        let logical = position.to_logical::<f64>(1.0);
        self.config.traffic_light_position = Some(LogicalPosition {
            x: logical.x,
            y: logical.y,
        });
        self
    }

    #[cfg(target_os = "macos")]
    fn hidden_title(mut self, hidden: bool) -> Self {
        self.config.hidden_title = hidden;
        self
    }

    #[cfg(target_os = "macos")]
    fn tabbing_identifier(mut self, identifier: &str) -> Self {
        self.config.tabbing_identifier = Some(identifier.into());
        self
    }

    fn theme(mut self, theme: Option<Theme>) -> Self {
        self.config.theme = theme;
        self
    }

    fn has_icon(&self) -> bool {
        self.icon_set
    }

    fn get_theme(&self) -> Option<Theme> {
        self.config.theme
    }

    fn window_classname<S: Into<String>>(mut self, name: S) -> Self {
        self.config.window_classname = Some(name.into());
        self
    }
}
