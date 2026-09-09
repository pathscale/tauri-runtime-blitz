//! Tauri runtime backed by Blitz and Boa.
//!
//! The first implementation target is Tauri 2.11.x. Its runtime traits are not semver-stable,
//! so versions stay exact and upgrades are deliberate.
//!
//! # What is not here
//!
//! The Blitz control surface. The vocabulary, the core that reads and drives a
//! document, the socket transport and the in-process transport are all
//! `blitz-control-protocol`, and this crate re-exports none of them.
//!
//! Nothing about inspecting a document needs a window, and while it lived here
//! depending on it meant compiling Tauri: on Linux that reaches GTK, so a
//! headless host pulled in system libraries to build a binary that opens
//! nothing. A runtime bridges Tauri to Blitz and owns a native window. It does
//! not proxy an inspection service, because a runtime that forwards one has not
//! removed the dependency edge, only renamed it.
//!
//! An embedder that names a control type depends on `blitz-control-protocol`
//! directly, and registers what `Relaunch` means for its process with
//! `blitz_control_protocol::lifecycle`. What this crate keeps is
//! `set_agent_control_enabled` and `apply_runtime_debug_options`, which are
//! about *this runtime's* listener rather than about the protocol, and which
//! name no protocol type. Nothing public here does.

#[cfg(feature = "runtime")]
use tauri_runtime::Icon;
#[cfg(feature = "runtime")]
use tauri_runtime::dpi::{Position, Size};
#[cfg(feature = "runtime")]
use tauri_runtime::window::{WindowBuilder, WindowBuilderBase, WindowSizeConstraints};
#[cfg(feature = "runtime")]
use tauri_utils::config::{
    Color, LogicalPosition, PreventOverflowConfig, PreventOverflowMargin, WindowConfig,
};
#[cfg(feature = "runtime")]
use tauri_utils::{Theme, TitleBarStyle};

#[cfg(feature = "runtime")]
mod ipc;
#[cfg(feature = "runtime")]
pub use ipc::attach_ipc_handler;
#[cfg(feature = "runtime")]
mod script_queue;
#[cfg(feature = "runtime")]
pub use script_queue::ScriptQueue;
#[cfg(feature = "runtime")]
mod runtime;
#[cfg(all(target_os = "macos", feature = "runtime"))]
mod window_effects;
/// The two capability switches an owner sets, as one decision.
///
/// `blitz-traits` owns the type because the collectors it governs are spread
/// across the shell and script crates. It is the argument of this crate's own
/// [`apply_runtime_debug_options`], so it is named here rather than leaving an
/// embedder to depend on `blitz-traits` for one struct.
#[cfg(feature = "agent-control")]
pub use blitz_traits::profiling::DebugOptions as RuntimeDebugOptions;
#[cfg(feature = "runtime")]
pub use runtime::{
    BlitzEventLoopProxy, BlitzRuntime, BlitzRuntimeHandle, builder, set_document_factory,
    set_runtime_trace,
};
#[cfg(all(feature = "agent-control", unix))]
#[cfg(feature = "runtime")]
pub use runtime::{agent_control_enabled, apply_runtime_debug_options, set_agent_control_enabled};
#[cfg(feature = "agent-control")]
#[cfg(feature = "runtime")]
pub use runtime::{
    begin_deep_profiling, deep_profiling_enabled, deep_profiling_permitted,
    set_deep_profiling_permitted,
};
/// Apply reusable macOS window glass without exposing AppKit to embedders.
#[cfg(all(target_os = "macos", feature = "runtime"))]
pub use window_effects::set_window_glass;
#[cfg(feature = "runtime")]
mod webview;
#[cfg(feature = "runtime")]
pub use webview::{BlitzWebviewDispatcher, PreparedBlitzWebview, prepare_pending_webview};
#[cfg(feature = "runtime")]
mod window_dispatch;
#[cfg(feature = "runtime")]
pub use window_dispatch::BlitzWindowDispatcher;

/// Renderer-neutral window attributes retained until the native Blitz window is created.
#[derive(Debug, Clone)]
#[cfg(feature = "runtime")]
pub struct BlitzWindowBuilder {
    pub config: WindowConfig,
    pub constraints: Option<WindowSizeConstraints>,
    pub icon_set: bool,
    #[cfg(target_os = "macos")]
    pub parent: Option<*mut std::ffi::c_void>,
}

#[cfg(feature = "runtime")]
impl WindowBuilderBase for BlitzWindowBuilder {}

#[cfg(feature = "runtime")]
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
