//! Reusable macOS window effects.
//!
//! The renderer owns the native window, so effects belong at this boundary.
//! Embedders provide style values; they never need AppKit or raw handles.

use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSColor, NSGlassEffectView,
    NSGlassEffectViewStyle as AppKitGlassStyle, NSUserInterfaceItemIdentification, NSView,
    NSWindowOrderingMode,
};
use objc2_foundation::MainThreadMarker;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use window_vibrancy::{
    LiquidGlassOptions, NSGlassEffectViewStyle, NSVisualEffectMaterial, apply_liquid_glass,
    apply_vibrancy, clear_liquid_glass,
};

use crate::runtime::with_native_window;
#[cfg(feature = "agent-control")]
use blitz_control_protocol::WindowComposition;

const BACKDROP_ID: &str = "trb-window-glass-backdrop";

#[derive(Clone, Copy, PartialEq, Eq)]
struct GlassConfig {
    tint: Option<(u8, u8, u8, u8)>,
    radius_bits: Option<u64>,
    enabled: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GlassBackend {
    Disabled,
    NativeGlass,
    LiquidGlass,
    Vibrancy,
    Unavailable,
}

impl GlassBackend {
    fn name(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NativeGlass => "nativeGlass",
            Self::LiquidGlass => "liquidGlass",
            Self::Vibrancy => "vibrancy",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct AppliedGlass {
    config: GlassConfig,
    backend: GlassBackend,
}

static APPLIED: OnceLock<Mutex<Option<AppliedGlass>>> = OnceLock::new();

/// Apply a styled glass backdrop behind the renderer.
///
/// Repeating the same request is free. On macOS before Liquid Glass, this uses
/// the system under-window vibrancy material. A missing effect never prevents
/// the application window from running.
pub fn set_window_glass(tint: Option<(u8, u8, u8, u8)>, radius: Option<f64>, enabled: bool) {
    let requested = GlassConfig {
        tint,
        radius_bits: radius.map(f64::to_bits),
        enabled,
    };
    let Ok(mut applied) = APPLIED.get_or_init(|| Mutex::new(None)).lock() else {
        return;
    };
    if applied
        .as_ref()
        .is_some_and(|state| state.config == requested)
    {
        return;
    }
    let mut backend = None;
    if with_native_window(|window| backend = Some(apply(window, tint, radius, enabled))) {
        *applied = Some(AppliedGlass {
            config: requested,
            backend: backend.unwrap_or(GlassBackend::Unavailable),
        });
    }
}

fn apply(
    window: &dyn winit::window::Window,
    tint: Option<(u8, u8, u8, u8)>,
    radius: Option<f64>,
    enabled: bool,
) -> GlassBackend {
    if !enabled {
        remove_backdrop(window);
        let _ = clear_liquid_glass(window);
        return GlassBackend::Disabled;
    }
    if install_backdrop(window, tint, radius) {
        return GlassBackend::NativeGlass;
    }

    let _ = clear_liquid_glass(window);
    let mut options = LiquidGlassOptions::new(NSGlassEffectViewStyle::Regular);
    if let Some(tint) = tint {
        options = options.tint_color(tint);
    }
    if let Some(radius) = radius {
        options = options.radius(radius);
    }
    match apply_liquid_glass(window, options) {
        Ok(()) => GlassBackend::LiquidGlass,
        Err(window_vibrancy::Error::UnsupportedPlatformVersion(_)) => {
            if apply_vibrancy(
                window,
                NSVisualEffectMaterial::UnderWindowBackground,
                None,
                None,
            )
            .is_ok()
            {
                GlassBackend::Vibrancy
            } else {
                GlassBackend::Unavailable
            }
        }
        Err(_) => GlassBackend::Unavailable,
    }
}

/// Native window composition which the runtime actually installed.
///
/// This reports the post-application backend rather than echoing CSS. A
/// successful `NativeGlass` state means the `NSGlassEffectView` was created and
/// received this exact tint; a fallback names itself instead of claiming that
/// unsupported tinting reached AppKit.
#[cfg(feature = "agent-control")]
pub(crate) fn composition(surface_transparent: bool) -> WindowComposition {
    let state = APPLIED
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|state| *state);
    composition_from(surface_transparent, state)
}

#[cfg(feature = "agent-control")]
fn composition_from(surface_transparent: bool, state: Option<AppliedGlass>) -> WindowComposition {
    let Some(applied) = state else {
        return WindowComposition {
            supported: true,
            surface_transparent,
            glass_backend: Some("notApplied".into()),
            ..WindowComposition::default()
        };
    };
    let carries_tint = matches!(
        applied.backend,
        GlassBackend::NativeGlass | GlassBackend::LiquidGlass
    );
    WindowComposition {
        supported: true,
        surface_transparent,
        glass_enabled: applied.config.enabled,
        glass_backend: Some(applied.backend.name().into()),
        tint_rgba: carries_tint
            .then_some(applied.config.tint)
            .flatten()
            .map(|(r, g, b, a)| [r, g, b, a]),
        radius: carries_tint
            .then_some(applied.config.radius_bits)
            .flatten()
            .map(f64::from_bits),
    }
}

fn install_backdrop(
    window: &dyn winit::window::Window,
    tint: Option<(u8, u8, u8, u8)>,
    radius: Option<f64>,
) -> bool {
    // The typed binding panics while resolving a class that does not exist.
    // Check dynamically so macOS before Liquid Glass reaches vibrancy instead.
    if AnyClass::get(c"NSGlassEffectView").is_none() {
        return false;
    }
    let Some(main_thread) = MainThreadMarker::new() else {
        return false;
    };
    let Ok(handle) = window.window_handle() else {
        return false;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return false;
    };
    let content = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let Some(container) = (unsafe { content.superview() }) else {
        return false;
    };

    remove_backdrop(window);
    let glass: Retained<NSGlassEffectView> =
        NSGlassEffectView::initWithFrame(main_thread.alloc(), container.bounds());
    glass.setStyle(AppKitGlassStyle::Regular);
    if let Some(radius) = radius {
        glass.setCornerRadius(radius);
    }
    if let Some((red, green, blue, alpha)) = tint {
        let color = NSColor::colorWithRed_green_blue_alpha(
            channel(red),
            channel(green),
            channel(blue),
            channel(alpha),
        );
        glass.setTintColor(Some(&color));
    }
    glass.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    glass.setIdentifier(Some(&objc2_foundation::NSString::from_str(BACKDROP_ID)));
    container.addSubview_positioned_relativeTo(&glass, NSWindowOrderingMode::Below, Some(content));
    true
}

fn remove_backdrop(window: &dyn winit::window::Window) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    let content = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let Some(container) = (unsafe { content.superview() }) else {
        return;
    };
    for view in container.subviews().iter() {
        if view
            .identifier()
            .is_some_and(|id| id.to_string() == BACKDROP_ID)
        {
            view.removeFromSuperview();
        }
    }
}

fn channel(value: u8) -> f64 {
    f64::from(value) / 255.0
}

#[cfg(test)]
mod tests {
    use super::AnyClass;
    #[cfg(feature = "agent-control")]
    use super::{AppliedGlass, GlassBackend, GlassConfig, composition_from};

    /// The normal macOS 14 job proves the fallback. The macOS 26 matrix leg
    /// runs this ignored test explicitly, proving the typed path's native class
    /// is present before a release claims Liquid Glass support.
    #[test]
    #[ignore = "requires the macOS 26 CI runner"]
    fn liquid_glass_class_is_available() {
        assert!(AnyClass::get(c"NSGlassEffectView").is_some());
    }

    #[cfg(feature = "agent-control")]
    #[test]
    fn composition_reports_only_tint_a_backend_actually_installed() {
        let config = GlassConfig {
            tint: Some((174, 50, 112, 0)),
            radius_bits: Some(12.0_f64.to_bits()),
            enabled: true,
        };
        let native = composition_from(
            true,
            Some(AppliedGlass {
                config,
                backend: GlassBackend::NativeGlass,
            }),
        );
        assert!(native.surface_transparent);
        assert_eq!(native.tint_rgba, Some([174, 50, 112, 0]));
        assert_eq!(native.radius, Some(12.0));

        let fallback = composition_from(
            true,
            Some(AppliedGlass {
                config,
                backend: GlassBackend::Vibrancy,
            }),
        );
        assert_eq!(fallback.glass_backend.as_deref(), Some("vibrancy"));
        assert_eq!(fallback.tint_rgba, None);
        assert_eq!(fallback.radius, None);
    }
}
