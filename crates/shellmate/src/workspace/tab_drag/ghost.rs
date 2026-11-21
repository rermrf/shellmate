//! The click-through AppKit panel used while the pointer is outside Shellmate windows.
use super::*;
use gpui::{
    WeakEntity, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions,
};
use objc2_app_kit::{NSWindowAnimationBehavior, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSSize};

pub(super) struct FloatingTab {
    workspace: WeakEntity<Workspace>,
    session_id: usize,
    size: Size<Pixels>,
}

impl Render for FloatingTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self
            .workspace
            .update(cx, |ws, cx| {
                let index = ws.sessions.iter().position(|s| s.id == self.session_id)?;
                let palette = cx.palette().clone();
                Some(
                    ws.tab_face(index, self.size.width, true, cx)
                        .h(self.size.height)
                        .bg(palette.card),
                )
            })
            .ok()
            .flatten();
        div().font_family(theme::UI_FONT).children(tab)
    }
}

/// Both coordinate systems use points. Only the vertical origin and direction differ.
fn appkit_origin(origin: Point<Pixels>, height: Pixels, primary_top: f64) -> NSPoint {
    NSPoint::new(
        origin.x.to_f64(),
        primary_top - origin.y.to_f64() - height.to_f64(),
    )
}

impl FloatingTab {
    fn for_drag(drag: ActiveDrag, cx: &App) -> Option<Self> {
        let workspace = Hub::entry(drag.source, cx)?.workspace;
        let owner = workspace.upgrade()?;
        let ws = owner.read(cx);
        let index = ws.sessions.iter().position(|s| s.id == drag.session_id)?;
        let (_, rect) = ws.tab_strip.tabs.iter().find(|(i, _)| *i == index)?;
        Some(Self {
            workspace,
            session_id: drag.session_id,
            size: rect.size,
        })
    }

    fn open(
        self,
        origin: Point<Pixels>,
        primary_top: f64,
        cx: &mut App,
    ) -> anyhow::Result<WindowHandle<Self>> {
        let size = self.size;
        let options = WindowOptions {
            kind: WindowKind::PopUp,
            titlebar: None,
            focus: false,
            show: false,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(origin, size))),
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };
        let handle = cx.open_window(options, |window, cx| {
            window_scale::install(window);
            cx.new(|_| self)
        })?;
        handle.update(cx, |_, window, cx| {
            let Some(panel) = lifecycle::native_window(window) else {
                window.remove_window();
                anyhow::bail!("floating tab has no NSWindow");
            };
            // GPUI creates PopUp as a nonactivating NSPanel with a transparent
            // titlebar. Remove its titled frame before showing the measured tab.
            panel.setStyleMask(NSWindowStyleMask::NonactivatingPanel);
            panel.setExcludedFromWindowsMenu(true);
            panel.setIgnoresMouseEvents(true);
            panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
            panel.setHasShadow(true);
            panel.setContentSize(NSSize::new(size.width.to_f64(), size.height.to_f64()));
            panel.setFrameOrigin(appkit_origin(origin, size.height, primary_top));
            panel.orderFront(None);
            window_scale::sync(window, cx);
            Ok(())
        })??;
        Ok(handle)
    }
}

pub(super) fn follow(drag: ActiveDrag, pointer: Point<Pixels>, cx: &mut App) {
    let origin = pointer - drag.grab_offset;
    let Some(top) = primary_screen_top() else {
        return;
    };
    if let Some(handle) = cx.global::<Hub>().tab_drag.ghost {
        let moved = handle.update(cx, |view, window, cx| {
            if let Some(panel) = lifecycle::native_window(window) {
                panel.setFrameOrigin(appkit_origin(origin, view.size.height, top));
                window_scale::sync(window, cx);
            }
        });
        if moved.is_ok() {
            return;
        }
        cx.global_mut::<Hub>().tab_drag.ghost = None;
    }
    if let Some(tab) = FloatingTab::for_drag(drag, cx) {
        match tab.open(origin, top, cx) {
            Ok(handle) => cx.global_mut::<Hub>().tab_drag.ghost = Some(handle),
            Err(err) => log::warn!("could not open floating tab: {err:#}"),
        }
    }
}

pub(super) fn close(cx: &mut App) {
    if let Some(handle) = cx.global_mut::<Hub>().tab_drag.ghost.take() {
        let _ = handle.update(cx, |_, window, _| {
            if let Some(panel) = lifecycle::native_window(window) {
                panel.orderOut(None);
            }
            window.remove_window();
        });
    }
}

/// Move a complete workspace without activating it, using desktop top-left points.
pub(super) fn move_window(window: &mut Window, origin: Point<Pixels>, cx: &mut App) {
    if let Some(native) = lifecycle::native_window(window)
        && let Some(top) = primary_screen_top()
    {
        native.setFrameOrigin(appkit_origin(origin, px(native.frame().size.height as f32), top));
        window_scale::sync(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_the_grabbed_tab_to_appkit_points_across_displays() {
        let grab = point(px(25.5), px(11.));
        for (pointer, expected) in [
            (point(px(300.), px(140.)), NSPoint::new(274.5, 915.)),
            (point(px(2040.), px(-120.)), NSPoint::new(2014.5, 1175.)),
            (point(px(-600.), px(1200.)), NSPoint::new(-625.5, -145.)),
        ] {
            assert_eq!(appkit_origin(pointer - grab, px(36.), 1080.), expected);
        }
    }
}
