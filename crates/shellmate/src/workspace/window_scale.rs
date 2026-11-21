//! Keep GPUI's cached geometry in sync after a native cross-display move.
use gpui::{App, Pixels, Size, Window, px, size};

use super::lifecycle::native_window;

/// AppKit can synchronously report a new backing scale inside setFrameOrigin.
/// gpui-pre-macos 0.3.12 resizes the Metal drawable then, but its synchronous
/// resize callback cannot borrow App again while our window update is running.
/// The display notification is deferred by GPUI, so it is a safe second path
/// for both system-driven moves and subsequent display configuration changes.
pub(crate) fn install(window: &Window) {
    window
        .observe_window_display(|window, cx| {
            sync(window, cx);
            window.on_next_frame(sync);
        })
        .detach();
    window.on_next_frame(sync);
}

/// Call after our native move/resize returns, using the already borrowed Window.
/// Do not resize the NSWindow or scale desktop coordinates: those are points.
pub(super) fn sync(window: &mut Window, cx: &mut App) {
    let Some(native) = native_window(window) else {
        return;
    };
    let Some(screen) = native.screen() else {
        return;
    };
    let Some(view) = native.contentView() else {
        return;
    };
    // Match GPUI's platform scale_factor/content_size sources exactly.
    let scale = screen.backingScaleFactor() as f32;
    let frame = view.frame();
    let viewport = size(px(frame.size.width as f32), px(frame.size.height as f32));
    if needs_sync(
        window.scale_factor(),
        window.viewport_size(),
        scale,
        viewport,
    ) {
        log::debug!(
            "window {} backing sync: scale {} -> {}, viewport {:?} -> {:?}",
            window.window_handle().window_id().as_u64(),
            window.scale_factor(),
            scale,
            window.viewport_size(),
            viewport,
        );
        // Refreshes cached scale, viewport, pointer coordinates and all layout.
        // The native callback has already updated the Metal drawable size.
        window.bounds_changed(cx);
    }
}

fn needs_sync(
    cached_scale: f32,
    cached_size: Size<Pixels>,
    scale: f32,
    viewport: Size<Pixels>,
) -> bool {
    scale.is_finite()
        && scale > 0.
        && f32::from(viewport.width).is_finite()
        && viewport.width > px(0.)
        && f32::from(viewport.height).is_finite()
        && viewport.height > px(0.)
        && (cached_scale != scale || cached_size != viewport)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_display_round_trip_changes_scale_without_resizing_logical_window() {
        let viewport = size(px(1000.), px(700.));
        let mut cached = 1.;
        for scale in [2., 2., 1., 1., 2.] {
            assert_eq!(
                needs_sync(cached, viewport, scale, viewport),
                cached != scale
            );
            cached = scale;
            assert!(!needs_sync(cached, viewport, scale, viewport));
        }
    }

    #[test]
    fn content_size_changes_require_layout_even_on_same_scale() {
        let before = size(px(1000.), px(700.));
        for after in [size(px(980.), px(700.)), size(px(1000.), px(620.))] {
            assert!(needs_sync(2., before, 2., after));
            assert!(!needs_sync(2., after, 2., after));
        }
    }

    #[test]
    fn transient_invalid_screen_geometry_cannot_trigger_refresh_loops() {
        let viewport = size(px(1000.), px(700.));
        for scale in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(!needs_sync(1., viewport, scale, viewport));
        }
        for invalid in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(!needs_sync(
                1.,
                viewport,
                2.,
                size(px(invalid), viewport.height)
            ));
            assert!(!needs_sync(
                1.,
                viewport,
                2.,
                size(viewport.width, px(invalid))
            ));
        }
    }
}
