use super::CopyFixture;
use crate::{ai::selection_scroll::SelectionScroll, theme::ActivePalette};
use gpui::{component::button::Button, prelude::*, *};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

const FEEDBACK: Duration = Duration::from_millis(1500);
fn hits_copy(bounds: Option<Bounds<Pixels>>, position: Point<Pixels>) -> bool {
    bounds.is_some_and(|bounds| bounds.contains(&position))
}
#[derive(Default)]
struct Feedback {
    until: Option<Instant>,
}
impl Feedback {
    fn copied(&mut self, now: Instant) {
        self.until = Some(now + FEEDBACK);
    }
    fn active(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }
}
struct State {
    hovered: bool,
    focus: FocusHandle,
    feedback: Feedback,
    timer: Option<Task<()>>,
    _focus: Subscription,
    _blur: Subscription,
}
impl State {
    fn new(w: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let focused = cx.on_focus(&focus, w, |_, _, cx| cx.notify());
        let blur = cx.on_blur(&focus, w, |_, _, cx| cx.notify());
        Self {
            hovered: false,
            focus,
            feedback: Default::default(),
            timer: None,
            _focus: focused,
            _blur: blur,
        }
    }
}
/// This is a sibling of the scroll viewport, never part of its scrolling width.
pub(super) fn overlay(
    block: Div,
    id: SharedString,
    code: String,
    selection: Entity<SelectionScroll>,
    fixture: CopyFixture,
    w: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let state = w.use_keyed_state(
        SharedString::from(format!("{id}:copy-state")),
        cx,
        State::new,
    );
    let focus = state.read(cx).focus.clone();
    let visible =
        state.read(cx).hovered || focus.contains_focused(w, cx) || fixture != CopyFixture::None;
    let copied = fixture == CopyFixture::Copied || state.read(cx).feedback.active(Instant::now());
    let hover = state.clone();
    let p = cx.palette().clone();
    let button_bounds = Rc::new(Cell::new(None));
    let measured = button_bounds.clone();
    block
        .id(SharedString::from(format!("{id}:copy-host")))
        .relative()
        .on_hover(move |hovered, _, cx| {
            hover.update(cx, |s, cx| {
                s.hovered = *hovered;
                cx.notify();
            })
        })
        .child(
            div()
                .absolute()
                .top(px(5.))
                .right(px(5.))
                .w(px(if copied { 56. } else { 44. }))
                .h(px(24.))
                .on_children_prepainted(move |bounds, _, _| measured.set(bounds.first().copied()))
                .child(
                    Button::new(SharedString::from(format!("{id}:copy")))
                        .w_full()
                        .occlude()
                        .track_focus(&focus)
                        .label(if copied { "已复制" } else { "复制" })
                        .h(px(24.))
                        .px(px(8.))
                        .rounded(px(8.))
                        .text_size(px(11.5))
                        .bg(p.card2)
                        .text_color(p.tx2)
                        .border_1()
                        .border_color(p.line)
                        .when(!visible, |b| b.opacity(0.))
                        .on_mouse_down(MouseButton::Left, move |event, w, cx| {
                            if !hits_copy(button_bounds.get(), event.position) {
                                return;
                            }
                            selection.update(cx, |s, _| s.stop());
                            gpui::base::GlobalState::suppress_text_selection(cx);
                            w.prevent_default();
                            cx.stop_propagation();
                        })
                        .on_click(move |_, w, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                            state.update(cx, |s, cx| {
                                s.feedback.copied(Instant::now());
                                s.timer = Some(cx.spawn_in(w, async move |this, cx| {
                                    cx.background_executor().timer(FEEDBACK).await;
                                    this.update(cx, |s, cx| {
                                        s.feedback = Feedback::default();
                                        s.timer = None;
                                        cx.notify();
                                    })
                                    .ok();
                                }));
                                cx.notify();
                            });
                        }),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{FEEDBACK, Feedback, hits_copy};
    use gpui::{Bounds, point, px, size};
    use std::time::{Duration, Instant};
    #[test]
    fn copy_feedback_expires_and_repeated_copy_restarts_it() {
        let now = Instant::now();
        let mut state = Feedback::default();
        assert!(!state.active(now));
        state.copied(now);
        assert!(state.active(now));
        assert!(state.active(now + FEEDBACK - Duration::from_millis(1)));
        assert!(!state.active(now + FEEDBACK));
        state.copied(now + Duration::from_secs(1));
        assert!(state.active(now + FEEDBACK));
    }
    #[test]
    fn only_the_small_copy_button_may_suppress_selection() {
        let bounds = Bounds::new(point(px(1365.), px(264.5)), size(px(44.), px(24.)));
        assert!(hits_copy(Some(bounds), point(px(1380.), px(280.))));
        for position in [
            point(px(1114.), px(280.)),
            point(px(1300.), px(280.)),
            point(px(1434.), px(280.)),
            point(px(1380.), px(300.)),
        ] {
            assert!(!hits_copy(Some(bounds), position));
        }
        assert!(!hits_copy(None, point(px(1380.), px(280.))));
    }
}
