//! User message metadata: stable send time and a quiet copy action on hover.
use crate::{icons::AppIcon, theme::ActivePalette};
use gpui::{
    component::button::{Button, ButtonVariants},
    prelude::*,
    *,
};
use std::time::Duration;

struct State {
    hovered: bool,
    copied: bool,
    focus: FocusHandle,
    timer: Option<Task<()>>,
    _focus: Subscription,
    _blur: Subscription,
}
impl State {
    fn new(w: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let focused = cx.on_focus(&focus, w, |_, _, cx| cx.notify());
        let blurred = cx.on_blur(&focus, w, |_, _, cx| cx.notify());
        Self {
            hovered: false,
            copied: false,
            focus,
            timer: None,
            _focus: focused,
            _blur: blurred,
        }
    }
}
fn time_label(sent_at: i64, now: i64) -> String {
    // Same day: HH:mm; older messages also retain the date. The tooltip has seconds.
    let full = super::timing::timestamp(sent_at, now, false);
    full.rsplit_once(':')
        .map_or(full.clone(), |(text, _)| text.to_owned())
}
pub(super) fn message(
    id: String,
    text: &str,
    sent_at_ms: Option<i64>,
    content: Div,
    selection: Entity<crate::ai::selection_scroll::SelectionScroll>,
    w: &mut Window,
    cx: &mut App,
) -> Stateful<Div> {
    let state = w.use_keyed_state(SharedString::from(format!("{id}:meta")), cx, State::new);
    let focus = state.read(cx).focus.clone();
    let visible = state.read(cx).hovered || focus.contains_focused(w, cx);
    let p = cx.palette().clone();
    let mut meta = div()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(4.))
        .h(px(24.));
    if let Some(sent_at) = sent_at_ms {
        let now = shellmate_ai::UserTurn::now_ms();
        let tip = format!("发送于 {}", super::timing::timestamp(sent_at, now, true));
        meta = meta.child(
            div()
                .id(SharedString::from(format!("{id}:time")))
                .text_size(px(10.5))
                .line_height(px(16.))
                .text_color(p.tx3)
                .when(!visible, |time| time.opacity(0.))
                .child(time_label(sent_at, now))
                .tooltip(move |w, cx| {
                    gpui::component::tooltip::Tooltip::new(tip.clone()).build(w, cx)
                }),
        );
    }
    // Keep the target's space reserved while hidden.
    meta = meta.child(
        copy_button(&id, text, "复制消息", state.clone(), selection, cx)
            .when(!visible, |button| button.opacity(0.)),
    );
    div()
        .id(SharedString::from(format!("{id}:host")))
        .flex_none()
        .min_w_0()
        .max_w_full()
        .flex()
        .flex_col()
        .items_end()
        .gap(px(4.))
        .on_hover(move |hovered, _, cx| {
            state.update(cx, |s, cx| {
                s.hovered = *hovered;
                cx.notify();
            })
        })
        .child(content)
        .child(meta)
}

fn copy_button(
    id: &str,
    text: &str,
    label: &'static str,
    state: Entity<State>,
    selection: Entity<crate::ai::selection_scroll::SelectionScroll>,
    cx: &App,
) -> Button {
    let focus = state.read(cx).focus.clone();
    let copied = state.read(cx).copied;
    let p = cx.palette().clone();
    let copy = text.to_owned();
    let feedback = state.clone();
    Button::new(SharedString::from(format!("{id}:copy")))
        .ghost()
        .size(px(24.))
        .p_0()
        .rounded(px(8.))
        .border_0()
        .track_focus(&focus)
        .icon(if copied {
            AppIcon::Check.at(14.)
        } else {
            AppIcon::Copy.at(14.)
        })
        .text_color(if copied { p.acc } else { p.tx3 })
        .tooltip(if copied { "已复制" } else { label })
        .on_mouse_down(MouseButton::Left, move |_, w, cx| {
            selection.update(cx, |s, _| s.stop());
            gpui::base::GlobalState::suppress_text_selection(cx);
            w.prevent_default();
            cx.stop_propagation();
        })
        .on_click(move |_, w, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
            feedback.update(cx, |s, cx| {
                s.copied = true;
                s.timer = Some(cx.spawn_in(w, async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(1500))
                        .await;
                    this.update(cx, |s, cx| {
                        s.copied = false;
                        s.timer = None;
                        cx.notify();
                    })
                    .ok();
                }));
                cx.notify();
            });
        })
}

pub(super) fn reply_copy(
    id: &str,
    text: &str,
    group: &SharedString,
    selection: Entity<crate::ai::selection_scroll::SelectionScroll>,
    w: &mut Window,
    cx: &mut App,
) -> Div {
    let state = w.use_keyed_state(
        SharedString::from(format!("{id}:copy-feedback")),
        cx,
        State::new,
    );
    let focused = state.read(cx).focus.contains_focused(w, cx);
    div()
        .flex_none()
        .size(px(24.))
        .when(!focused, |d| {
            d.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.))
        })
        .child(copy_button(id, text, "复制回复", state, selection, cx))
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamps_show_minutes_and_old_dates_with_full_seconds_in_tooltip() {
        let now = shellmate_ai::UserTurn::now_ms();
        let today = super::time_label(now, now);
        assert_eq!(today.len(), 5);
        assert_eq!(&today[2..3], ":");
        let yesterday = super::time_label(now - 172_800_000, now);
        assert_eq!(yesterday.len(), 16);
        assert_eq!(super::super::timing::timestamp(now, now, true).len(), 19);
    }
}
