//! Permission chip with an absolute popup; the closed chip keeps its existing layout.
use super::{CONTEXT_POPOVER_KEYS, Drawer};
use crate::{
    ai::mode_popover::{self, Selection},
    theme::ActivePalette,
};
use gpui::{prelude::*, *};
use shellmate_ai::PermissionMode;
impl Drawer {
    pub(super) fn permission_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = cx.palette().clone();
        if !self.capabilities.available() {
            return div()
                .text_size(px(11.))
                .line_height(px(17.))
                .text_color(p.tx2)
                .child("此会话不支持 AI 执行命令，仅提供建议")
                .into_any_element();
        }
        let current = *self.mode.lock().unwrap();
        let full = current == PermissionMode::Full;
        let anchor_bounds = std::rc::Rc::new(std::cell::Cell::new(None::<Bounds<Pixels>>));
        let measured = anchor_bounds.clone();
        let mut chip = div().relative().flex().flex_col().gap(px(4.)).child(
            div()
                .flex()
                .items_center()
                .on_children_prepainted(move |bounds, _, _| {
                    measured.set(bounds.first().copied());
                })
                .child(
                    div()
                        .id("ai-permission")
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .h(px(26.))
                        .px(px(8.))
                        .rounded(px(7.))
                        .bg(if full { p.errs } else { p.card2 })
                        .text_color(if full { p.err } else { p.tx2 })
                        .text_size(px(11.5))
                        .cursor_pointer()
                        .hover(|d| d.bg(if full { p.err.opacity(0.2) } else { p.card3 }))
                        .child(crate::icons::AppIcon::Shield.icon())
                        .child(current.label())
                        .child("▾")
                        .on_click(cx.listener(|s, _, w, cx| {
                            mode_popover::toggle(
                                &mut s.permission_menu,
                                &mut s.state.context_popover,
                                &mut s.popup,
                            );
                            if s.permission_menu {
                                w.focus(&s.context_focus, cx);
                            } else {
                                s.input.focus_handle(cx).focus(w, cx);
                            }
                            cx.notify();
                        })),
                ),
        );
        if self.permission_menu {
            let mut popover = div()
                .id("ai-mode-popover")
                .key_context(CONTEXT_POPOVER_KEYS)
                .track_focus(&self.context_focus)
                .absolute()
                .bottom(relative(1.))
                .left_0()
                .mb(px(6.))
                .w(px(310.))
                .p(px(6.))
                .rounded(px(16.))
                .bg(p.card)
                .border_1()
                .border_color(p.line)
                .shadow(crate::ui::card_shadow(&p))
                .flex()
                .flex_col()
                .gap(px(2.))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down_out(cx.listener(move |s, e: &MouseDownEvent, _, cx| {
                    if !anchor_bounds.get().is_some_and(|b| b.contains(&e.position)) {
                        mode_popover::close(&mut s.permission_menu);
                        cx.notify();
                    }
                }));
            for (i, (mode, description)) in [
                (PermissionMode::Ask, "每条命令都需要你批准"),
                (PermissionMode::Auto, "只读命令自动执行，其他命令需批准"),
                (PermissionMode::Full, "所有命令自动执行，高危命令仍需确认"),
            ]
            .into_iter()
            .enumerate()
            {
                popover = popover.child(
                    div()
                        .id(("ai-permission-option", i))
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(10.))
                        .cursor_pointer()
                        .when(mode == current, |d| d.bg(p.accs))
                        .hover(|d| d.bg(p.card2))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(8.))
                                .text_size(px(12.5))
                                .child(
                                    div()
                                        .text_color(if mode == PermissionMode::Full {
                                            p.err
                                        } else {
                                            p.tx
                                        })
                                        .child(mode.label()),
                                )
                                .when(mode == current, |d| {
                                    d.child(div().text_size(px(11.)).text_color(p.tx2).child("✓"))
                                }),
                        )
                        .child(
                            div()
                                .mt(px(3.))
                                .whitespace_normal()
                                .text_size(px(11.))
                                .text_color(p.tx2)
                                .child(description),
                        )
                        .on_click(cx.listener(move |s, _, w, cx| {
                            let current = *s.mode.lock().unwrap();
                            let selection =
                                mode_popover::select(&mut s.permission_menu, current, mode);
                            s.input.focus_handle(cx).focus(w, cx);
                            match selection {
                                Selection::ConfirmFull => s.confirm_full(w, cx),
                                Selection::Apply(mode) => {
                                    s.set_permission(mode, cx);
                                }
                            }
                            cx.notify();
                        })),
                );
            }
            chip = chip.child(popover);
        }
        chip.into_any_element()
    }
}
