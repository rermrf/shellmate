//! Width policy and interaction for the AI slot. Other drawers keep their own width.
use super::*;

pub(super) const SETTING: &str = "ai_drawer_width";
pub(super) const DEFAULT_WIDTH: f32 = 340.;
const MAX_WIDTH: f32 = 900.;
const TERMINAL_MIN: f32 = 480.;
// The right inset and the existing gap are outside the drawer and terminal.
const CHROME: f32 = 24.;

pub(super) fn stored_width(value: Option<&str>) -> f32 {
    value
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| v.is_finite() && (DEFAULT_WIDTH..=MAX_WIDTH).contains(v))
        .unwrap_or(DEFAULT_WIDTH)
}

pub(super) fn clamp_width(width: f32, window: f32, sidebar: f32) -> f32 {
    let max = (window - sidebar - TERMINAL_MIN - CHROME).clamp(0., MAX_WIDTH);
    // Below the combined minima, the terminal reserve takes precedence.
    width.max(DEFAULT_WIDTH).min(max)
}

#[derive(Default)]
pub(super) struct Drag {
    start: Option<(f32, f32)>,
}

impl Drag {
    fn begin(&mut self, x: f32, width: f32, clicks: usize) -> f32 {
        self.start = (clicks < 2).then_some((x, width));
        if clicks >= 2 { DEFAULT_WIDTH } else { width }
    }

    fn width(&self, x: f32) -> Option<f32> {
        self.start.map(|(start, width)| width + start - x)
    }

    pub(super) fn active(&self) -> bool {
        self.start.is_some()
    }
}

impl Workspace {
    pub(super) fn drawer_width(&self, window: &Window) -> f32 {
        clamp_width(
            self.ai_drawer_width,
            f32::from(window.viewport_size().width),
            self.sidebar_motion.frame.width,
        )
    }

    fn save_drawer_width(&self) {
        if let Err(err) = self.hub
            .store
            .borrow_mut()
            .set_setting(SETTING, &self.ai_drawer_width.to_string())
        {
            log::warn!("could not save AI drawer width: {err}");
        }
    }

    fn drawer_resizing(&self, active: bool, cx: &mut Context<Self>) {
        for session in &self.sessions {
            session
                .terminal
                .update(cx, |t, _| t.set_interactive_resize(active));
        }
    }

    pub(super) fn end_drawer_drag(&mut self, cx: &mut Context<Self>) {
        if self.ai_drawer_drag.start.take().is_some() {
            self.drawer_resizing(false, cx);
            self.save_drawer_width();
            cx.notify();
        }
    }

    pub(super) fn drawer_handle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        div()
            .id("ai-drawer-resize")
            .absolute()
            .left(px(-10.))
            .top_0()
            .bottom_0()
            .w(px(8.))
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .flex()
            .justify_center()
            .py(px(12.))
            .group("ai-drawer-resize")
            .child(
                div()
                    .w(px(2.))
                    .h_full()
                    .rounded(px(1.))
                    .bg(if self.ai_drawer_drag.active() {
                        p.acc
                    } else {
                        gpui::transparent_black()
                    })
                    .group_hover("ai-drawer-resize", |d| d.bg(p.acc.opacity(0.55))),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|s, e: &gpui::MouseDownEvent, w, cx| {
                    let width = s.drawer_width(w);
                    s.ai_drawer_width =
                        s.ai_drawer_drag
                            .begin(f32::from(e.position.x), width, e.click_count);
                    s.drawer_resizing(s.ai_drawer_drag.active(), cx);
                    if !s.ai_drawer_drag.active() {
                        s.save_drawer_width();
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
    }

    pub(super) fn move_drawer_drag(
        &mut self,
        e: &gpui::MouseMoveEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if e.pressed_button != Some(MouseButton::Left) {
            self.end_drawer_drag(cx);
            return;
        }
        if let Some(width) = self.ai_drawer_drag.width(f32::from(e.position.x)) {
            self.ai_drawer_width = width;
            self.ai_drawer_width = self.drawer_width(w).max(DEFAULT_WIDTH);
            cx.stop_propagation();
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::{RAIL_WIDTH, SIDEBAR_WIDTH};

    #[test]
    fn drawer_width_minimum_and_window_maximum() {
        assert_eq!(clamp_width(100., 1800., SIDEBAR_WIDTH), 340.);
        assert_eq!(clamp_width(1200., 2000., SIDEBAR_WIDTH), 900.);
        assert_eq!(clamp_width(900., 1400., SIDEBAR_WIDTH), 612.);
        assert_eq!(clamp_width(900., 1400., 0.), 896.);
        assert_eq!(clamp_width(900., 1400., RAIL_WIDTH), 896. - RAIL_WIDTH);
    }

    #[test]
    fn shrinking_window_reserves_terminal_space() {
        assert_eq!(clamp_width(560., 1200., SIDEBAR_WIDTH), 412.);
        assert_eq!(clamp_width(560., 980., SIDEBAR_WIDTH), 192.);
    }

    #[test]
    fn drawer_width_settings_round_trip_and_garbage() {
        let mut store = HostStore::open_in_memory().unwrap();
        for width in [340., 560.5, 900.] {
            store.set_setting(SETTING, &width.to_string()).unwrap();
            assert_eq!(
                stored_width(store.setting(SETTING).unwrap().as_deref()),
                width
            );
        }
        for bad in ["", "junk", "NaN", "inf", "-1", "0", "339", "901"] {
            store.set_setting(SETTING, bad).unwrap();
            assert_eq!(
                stored_width(store.setting(SETTING).unwrap().as_deref()),
                DEFAULT_WIDTH
            );
        }
        assert_eq!(stored_width(None), DEFAULT_WIDTH);
    }

    #[test]
    fn double_click_resets_drawer_width() {
        let mut drag = Drag::default();
        assert_eq!(drag.begin(900., 560., 1), 560.);
        assert_eq!(drag.width(850.), Some(610.));
        assert_eq!(drag.begin(850., 610., 2), DEFAULT_WIDTH);
        assert!(!drag.active());
    }
}
