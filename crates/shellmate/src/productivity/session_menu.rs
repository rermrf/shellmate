//! Menus for the tab overflow list and the secondary split session.
use super::multi::{SessionRow, Status};
use crate::{
    theme::ActivePalette,
    ui::{card_shadow, dot, scroll::FADE_HEIGHT},
};
use gpui::{
    App, Context, EventEmitter, FocusHandle, KeyBinding, Pixels, ScrollHandle, Window, actions, div, point,
    prelude::*, px,
};

const ROW: f32 = 34.;
const MAX_ROWS: f32 = 8.;
/// The card's padding (6 + 6) and border (1 + 1) around the list.
const CHROME: f32 = 14.;
const WINDOW_MARGIN: f32 = 8.;

/// The tallest the list may be: eight rows, and nothing below the window's bottom margin. The card's
/// top edge sits at `top`, so what is below it is what is left. One row always stays.
fn list_cap(top: f32, window_height: f32) -> f32 {
    (MAX_ROWS * ROW).min(window_height - top - WINDOW_MARGIN - CHROME).max(ROW)
}

/// How far to scroll from the top so that row `ix` of `rows` shows in a list `view` tall: not at all when the
/// row is already clear of the bottom fade, else the row goes to the middle, as far as the list can scroll.
fn reveal_offset(ix: usize, rows: usize, view: f32) -> f32 {
    let top = ix as f32 * ROW;
    if top + ROW <= view - FADE_HEIGHT {
        return 0.;
    }
    let max = (rows as f32 * ROW - view).max(0.);
    (top - (view - ROW) / 2.).clamp(0., max)
}

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
    /// Where the card's top edge is in the window.
    top: Pixels,
    scroll: ScrollHandle,
    /// The list still has to scroll to the active row (once, when it opens).
    reveal: bool,
    focus: FocusHandle,
}
impl EventEmitter<Event> for SessionMenu {}
impl SessionMenu {
    pub fn new(
        rows: Vec<SessionRow>,
        active: Option<usize>,
        top: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self { rows, active, top, scroll: ScrollHandle::new(), reveal: true, focus }
    }
}
impl Render for SessionMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let cap = list_cap(f32::from(self.top), f32::from(window.viewport_size().height));
        if std::mem::take(&mut self.reveal)
            && let Some(ix) = self.rows.iter().position(|row| Some(row.id) == self.active)
        {
            let y = reveal_offset(ix, self.rows.len(), cap);
            self.scroll.set_offset(point(px(0.), px(-y)));
        }
        div()
            .id("session-menu")
            .occlude()
            .key_context("SessionMenu")
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &Close, _, cx| cx.emit(Event::Close)))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(Event::Close)))
            .w(px(260.))
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
                    .h(px(ROW))
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
                    .when(s.attention, |d| d.child(dot(p.acc, crate::tabs::DOT)))
                    .child(div().flex_none().whitespace_nowrap().text_size(px(11.)).text_color(p.tx2).child(s.subtitle()))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(Event::Pick(id))))
            })))
                .track_scroll(&self.scroll)
                .flex_none()
                .max_h(px(cap))
                .surface(p.card)
                .bottom_padding(px(0.)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_shows_eight_rows_unless_the_window_has_less_room() {
        // Roomy: eight rows, whatever the window height.
        assert_eq!(list_cap(52., 900.), 272.);
        assert_eq!(list_cap(52., 2160.), 272.);
        // Exactly eight rows plus the card and the margin fit below the trigger.
        assert_eq!(list_cap(100., 100. + 272. + 14. + 8.), 272.);
        // One pixel less room: the list gives it up.
        assert_eq!(list_cap(100., 100. + 272. + 14. + 8. - 1.), 271.);
        // Below the trigger there are 300 - 52 = 248 points: card chrome and margin come off.
        assert_eq!(list_cap(52., 300.), 248. - 14. - 8.);
        // Never less than a row, however small the window.
        assert_eq!(list_cap(52., 60.), 34.);
        assert_eq!(list_cap(500., 300.), 34.);
    }

    #[test]
    fn opening_scrolls_the_active_row_into_view() {
        let view = 8. * 34.;
        // Rows that are visible clear of the fade stay where they are.
        for ix in 0..=6 {
            assert_eq!(reveal_offset(ix, 20, view), 0., "row {ix}");
        }
        // Row 7 ends at the bottom edge, under the fade: it moves to the middle.
        assert_eq!(reveal_offset(7, 20, view), 7. * 34. - (272. - 34.) / 2.);
        assert_eq!(reveal_offset(11, 20, view), 11. * 34. - (272. - 34.) / 2.);
        // The last rows stop at the end of the list.
        assert_eq!(reveal_offset(19, 20, view), 20. * 34. - 272.);
        assert_eq!(reveal_offset(18, 20, view), 20. * 34. - 272.);
        // A list that fits, or only just overflows, never scrolls past its end.
        assert_eq!(reveal_offset(5, 6, 6. * 34.), 0.);
        assert_eq!(reveal_offset(8, 9, view), 34.);
        assert_eq!(reveal_offset(0, 1, 34.), 0.);
    }

    #[test]
    fn a_short_window_scrolls_by_what_is_visible() {
        // 78 points of list: the active row 3 is below it and lands in the middle of the view.
        let view = 78.;
        assert_eq!(reveal_offset(3, 12, view), 3. * 34. - (78. - 34.) / 2.);
        assert_eq!(reveal_offset(0, 12, view), 0.);
        assert!(reveal_offset(11, 12, view) <= 12. * 34. - view);
    }
}
