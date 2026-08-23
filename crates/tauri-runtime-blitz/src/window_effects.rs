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

const BACKDROP_ID: &str = "trb-window-glass-backdrop";

#[derive(Clone, Copy, PartialEq, Eq)]
struct GlassConfig {
    tint: Option<(u8, u8, u8, u8)>,
    radius_bits: Option<u64>,
    enabled: bool,
}

static APPLIED: OnceLock<Mutex<Option<GlassConfig>>> = OnceLock::new();

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
    if *applied == Some(requested) {
        return;
    }
    if with_native_window(|window| apply(window, tint, radius, enabled)) {
        *applied = Some(requested);
    }
}

fn apply(
    window: &dyn winit::window::Window,
    tint: Option<(u8, u8, u8, u8)>,
    radius: Option<f64>,
    enabled: bool,
) {
    if !enabled {
        remove_backdrop(window);
        let _ = clear_liquid_glass(window);
        return;
    }
    if install_backdrop(window, tint, radius) {
        return;
    }

    let _ = clear_liquid_glass(window);
    let mut options = LiquidGlassOptions::new(NSGlassEffectViewStyle::Regular);
    if let Some(tint) = tint {
        options = options.tint_color(tint);
    }
    if let Some(radius) = radius {
        options = options.radius(radius);
    }
    if matches!(
        apply_liquid_glass(window, options),
        Err(window_vibrancy::Error::UnsupportedPlatformVersion(_))
    ) {
        let _ = apply_vibrancy(
            window,
            NSVisualEffectMaterial::UnderWindowBackground,
            None,
            None,
        );
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
