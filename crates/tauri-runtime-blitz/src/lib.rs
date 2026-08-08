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
mod script_queue;
pub use script_queue::ScriptQueue;
mod webview;
pub use webview::BlitzWebviewDispatcher;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_agencyzero_window_attributes() {
        let mut config = WindowConfig::default();
        config.title = "AgencyZero".into();
        config.width = 1344.0;
        config.height = 900.0;
        config.min_width = Some(960.0);
        config.min_height = Some(640.0);
        config.resizable = true;
        config.visible = false;
        config.hidden_title = true;
        config.title_bar_style = TitleBarStyle::Overlay;

        let builder = BlitzWindowBuilder::with_config(&config);
        assert_eq!(builder.config.title, "AgencyZero");
        assert_eq!(
            (builder.config.width, builder.config.height),
            (1344.0, 900.0)
        );
        assert_eq!(builder.config.min_width, Some(960.0));
        assert_eq!(builder.config.min_height, Some(640.0));
        assert!(builder.config.resizable);
        assert!(!builder.config.visible);
        assert!(builder.config.hidden_title);
        assert_eq!(builder.config.title_bar_style, TitleBarStyle::Overlay);
    }
}
