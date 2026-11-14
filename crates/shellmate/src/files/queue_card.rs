//! The transfer queue card at the bottom of the panel: one row per item of the connection's queue.
//! What each row says is decided in [`super::transfers`]; this only draws it.

use gpui::prelude::*;
use gpui::{AnyElement, Context, FontWeight, Hsla, Stateful, div, px, relative};
use shellmate_sftp::transfer::{Direction, TransferState, TransferView};

use super::panel::FilesPanel;
use super::transfers::{Tone, arrow, can_cancel, main_action, sizes_text, status_line, tone};
use crate::theme::{CARD_RADIUS, MONO_FONT, Palette};
use crate::ui::card_shadow;

/// The colour of a row's status text for a tone.
fn text_color(tone: Tone, p: &Palette) -> Hsla {
    match tone {
        Tone::Normal => p.tx2,
        Tone::Ok => p.ok,
        Tone::Warn => p.warn,
        Tone::Err => p.err,
        Tone::Faint => p.tx3,
    }
}

/// The colour of a row's bar for a tone.
fn bar_color(tone: Tone, p: &Palette) -> Hsla {
    match tone {
        Tone::Normal => p.acc,
        other => text_color(other, p),
    }
}

impl FilesPanel {
    /// A small button of the card.
    fn card_button(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        width: Option<f32>,
        p: &Palette,
    ) -> Stateful<gpui::Div> {
        div()
            .id(id.into())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(26.))
            .rounded(px(8.))
            .text_size(px(12.))
            .bg(p.card2)
            .text_color(p.tx)
            .cursor_pointer()
            .hover(|d| d.bg(p.card3))
            .map(|d| match width {
                Some(w) => d.w(px(w)),
                None => d.px(px(10.)),
            })
            .child(label)
    }

    fn render_transfer_row(&self, view: &TransferView, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let id = view.id;
        let (tile_bg, tile_fg) = match view.direction {
            Direction::Upload => (p.accs, p.acc),
            Direction::Download => (p.oks, p.ok),
        };
        let tone = tone(view);
        let fraction = if view.state == TransferState::Done { 1.0 } else { view.fraction() };
        let action = main_action(view);
        let main = action.map(|action| {
            Self::card_button(("tq-main", id.0), action.label(), Some(56.), p)
                .on_click(cx.listener(move |this, _, _, cx| this.row_action(id, action, cx)))
        });
        let cancel = can_cancel(view).then(|| {
            Self::card_button(("tq-cancel", id.0), "取消", None, p).text_color(p.tx2).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.cancel_item(id);
                    cx.notify();
                },
            ))
        });
        // Rows keep their height: with many items the list scrolls instead of squeezing them.
        div()
            .id(("tq-row", id.0))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .h(px(44.))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(28.))
                    .rounded(px(9.))
                    .bg(tile_bg)
                    .text_color(tile_fg)
                    .text_size(px(13.))
                    .font_weight(FontWeight::BOLD)
                    .child(arrow(view)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.tx)
                            .child(view.name.clone()),
                    )
                    .child(
                        div().truncate().text_size(px(11.5)).text_color(text_color(tone, p)).child(status_line(view)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(6.))
                    .rounded(px(3.))
                    .bg(p.card3)
                    .overflow_hidden()
                    .child(div().h_full().w(relative(fraction as f32)).rounded(px(3.)).bg(bar_color(tone, p))),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(150.))
                    .text_right()
                    .whitespace_nowrap()
                    .text_size(px(12.))
                    .font_family(MONO_FONT)
                    .text_color(p.tx2)
                    .child(sizes_text(view)),
            )
            .child(div().flex().flex_none().w(px(56.)).children(main))
            .child(div().flex().flex_none().w(px(44.)).justify_end().children(cancel))
            .into_any_element()
    }

    /// The card: hidden while the connection's queue is empty.
    pub(super) fn render_queue(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let list = self.transfers.as_ref()?.read(cx);
        if list.is_empty() {
            return None;
        }
        let (summary, has_finished) = (list.summary(), list.has_finished());
        // Newest on top, as in the prototype.
        let items: Vec<TransferView> = list.items().iter().rev().cloned().collect();
        let rows: Vec<AnyElement> = items.iter().map(|view| self.render_transfer_row(view, p, cx)).collect();
        let clear = Self::card_button("tq-clear", "清除已完成", None, p).text_color(p.tx2);
        let clear = if has_finished {
            clear.on_click(cx.listener(|this, _, _, cx| {
                this.clear_finished();
                cx.notify();
            }))
        } else {
            clear.opacity(0.5).cursor_not_allowed()
        };
        Some(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .rounded(CARD_RADIUS)
                .bg(p.card)
                .shadow(card_shadow(p))
                .pt(px(12.))
                .pr(px(14.))
                .pb(px(10.))
                .pl(px(18.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .mb(px(6.))
                        .child(
                            div().text_size(px(14.)).font_weight(FontWeight::BOLD).text_color(p.tx).child("传输队列"),
                        )
                        .child(div().text_size(px(12.)).text_color(p.tx2).child(summary))
                        .child(div().flex_1())
                        .child(clear),
                )
                .child(
                    div()
                        .id("tq-list")
                        .flex()
                        .flex_col()
                        .max_h(px(176.))
                        .overflow_y_scroll()
                        .track_scroll(&self.queue_scroll)
                        .children(rows),
                )
                .into_any_element(),
        )
    }
}
