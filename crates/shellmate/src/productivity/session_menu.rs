//! Menus for the tab overflow list and the secondary split session.
use super::multi::{SessionRow, Status};
use crate::{
    theme::ActivePalette,
    ui::{card_shadow, dot},
};
use gpui::{App, Context, EventEmitter, FocusHandle, KeyBinding, Window, actions, div, prelude::*, px};

actions!(session_menu, [Close]);
pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Close, Some("SessionMenu"))]);
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Tabs,
    Split,
}
pub enum Event {
    Pick(usize),
    Close,
}
pub struct SessionMenu {
    pub rows: Vec<SessionRow>,
    pub active: Option<usize>,
    focus: FocusHandle,
}
impl EventEmitter<Event> for SessionMenu {}
impl SessionMenu {
    pub fn new(
        rows: Vec<SessionRow>,
        active: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self { rows, active, focus }
    }
}
impl Render for SessionMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        div()
            .id("session-menu")
            .occlude()
            .key_context("SessionMenu")
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &Close, _, cx| cx.emit(Event::Close)))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(Event::Close)))
            .w(px(260.))
            .max_h(px(420.))
            .flex().flex_col()
            .p(px(6.))
            .rounded(px(14.))
            .bg(p.card)
            .border_1()
            .border_color(p.line)
            .shadow(card_shadow(&p))
            .child(crate::ui::scroll::scroll_body("session-menu-rows", div().children(self.rows.iter().map(|s| {
                let id = s.id;
                let color = match s.status {
                    Status::Connected => p.ok,
                    Status::Failed => p.err,
                    _ => p.warn,
                };
                div()
                    .id(("menu-session", id))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .h(px(34.))
                    .rounded(px(9.))
                    .cursor_pointer()
                    .when(self.active == Some(id), |d| d.bg(p.accs))
                    .hover(|d| d.bg(p.card2))
                    .child(dot(color, 7.))
                    .child(
                        crate::ui::ellipsis(("menu-session-title",id),s.title.clone()).flex_1()
                            .text_size(px(13.))
                            .text_color(if self.active == Some(id) { p.acc } else { p.tx }),
                    )
                    .child(div().flex_none().whitespace_nowrap().text_size(px(11.)).text_color(p.tx2).child(s.subtitle()))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(Event::Pick(id))))
            }))).flex_none().max_h(px(406.)).surface(p.card).bottom_padding(px(0.)))
    }
}
