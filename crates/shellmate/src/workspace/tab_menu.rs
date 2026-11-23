//! The tab's right-click menu. The workspace owns it and draws it like the file list's row menu
//! (`crate::ui::menu`); 「移到窗口」 opens a second card beside its row.
use super::*;
use crate::ui::menu;
use gpui::{AnyElement, AnyWindowHandle, MouseDownEvent, Pixels, Point, anchored, deferred, point};

actions!(tab_menu, [CloseMenu]);

const KEY_CONTEXT: &str = "TabMenu";

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CloseMenu, Some(KEY_CONTEXT))]);
}

/// The open menu: for the tab `id`, at the place it was opened.
pub(super) struct TabMenu {
    id: usize,
    position: Point<Pixels>,
    /// The list of other windows is showing beside its row.
    flyout: bool,
    /// The other windows as they were when the menu opened.
    windows: Vec<(AnyWindowHandle, SharedString)>,
    focus: FocusHandle,
}

#[derive(Clone, Copy)]
enum Choice {
    NewWindow,
    MoveTo(AnyWindowHandle),
    Close,
}

#[derive(Debug, PartialEq, Eq)]
struct Enabled {
    new_window: bool,
    move_to_window: bool,
}

/// A window's only tab cannot go to a new window (that would only swap the windows); the list of
/// windows needs another window. Moving the only tab to an existing window is fine: its window closes.
fn enabled(tabs: usize, other_windows: usize) -> Enabled {
    Enabled { new_window: tabs > 1, move_to_window: other_windows > 0 }
}

/// Rows of the card: 移到新窗口, 移到窗口, a separator, 关闭会话.
const ITEMS: usize = 3;
const SEPARATORS: usize = 1;
const MOVE_ROW: usize = 1;
/// Space kept to the window's edge, as for every anchored card.
const MARGIN: f32 = 8.;
/// Space between a row's edge and the card that opens from it.
const FLYOUT_GAP: f32 = 8.;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn right(&self) -> f32 {
        self.x + self.w
    }

    fn bottom(&self) -> f32 {
        self.y + self.h
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    fn origin(&self) -> Point<Pixels> {
        point(px(self.x), px(self.y))
    }
}

/// Where the cards are in the window. `anchored` is handed these places, so they are where the cards are
/// drawn, and the pointer can be told apart from the cards without measuring.
#[derive(Debug, PartialEq)]
struct Geometry {
    card: Rect,
    flyout: Option<Rect>,
}

/// What `anchored().snap_to_window_with_margin` does with an edge that sticks out.
fn snap(at: f32, extent: f32, limit: f32) -> f32 {
    let mut origin = at;
    if origin + extent > limit {
        origin -= origin + extent - limit + MARGIN;
    }
    if origin < 0. {
        origin = MARGIN;
    }
    origin
}

impl Geometry {
    /// `flyout_rows` is the number of other windows while their list is showing.
    fn new(at: (f32, f32), flyout_rows: Option<usize>, window: (f32, f32)) -> Self {
        let (w, h) = (menu::WIDTH, menu::card_height(ITEMS, SEPARATORS));
        let card = Rect { x: snap(at.0, w, window.0).round(), y: snap(at.1, h, window.1).round(), w, h };
        let flyout = flyout_rows.map(|rows| {
            let (w, h) = (menu::WIDTH, menu::card_height(rows, 0));
            // The card beside the row, on the right if it fits there and else on the left.
            let right = card.right() - menu::PADDING + FLYOUT_GAP;
            let left = card.x + menu::PADDING - FLYOUT_GAP - w;
            let x = if right + w <= window.0 - MARGIN {
                right
            } else if left >= MARGIN {
                left
            } else {
                (window.0 - MARGIN - w).max(MARGIN)
            };
            // The card's padding sits above the row, which makes the first item line up with it.
            let y = (card.y + MOVE_ROW as f32 * menu::ITEM_HEIGHT).min(window.1 - MARGIN - h).max(MARGIN);
            Rect { x: x.round(), y: y.round(), w, h }
        });
        Self { card, flyout }
    }
}

/// The other windows with the labels of the 「移到窗口」 list: `窗口 N · <the tab in front>`.
fn other_windows(source: AnyWindowHandle, cx: &App) -> Vec<(AnyWindowHandle, SharedString)> {
    Hub::windows(cx)
        .iter()
        .filter(|entry| entry.handle != source)
        .filter_map(|entry| {
            let workspace = entry.workspace.upgrade()?;
            let title = workspace.read(cx).active_tab_title(cx).unwrap_or_else(|| "Shellmate".into());
            Some((entry.handle, format!("窗口 {} · {title}", entry.number).into()))
        })
        .collect()
}

impl Workspace {
    /// A right-click on the tab `id`.
    pub(super) fn open_tab_menu(&mut self, id: usize, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sessions.iter().any(|s| s.id == id) {
            return;
        }
        self.session_menu = None;
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let windows = other_windows(self.handle, cx);
        self.tab_menu = Some(TabMenu { id, position, flyout: false, windows, focus });
        cx.notify();
    }

    pub(super) fn close_tab_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab_menu.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn set_flyout(&mut self, open: bool, cx: &mut Context<Self>) {
        if let Some(menu) = &mut self.tab_menu
            && menu.flyout != open
        {
            menu.flyout = open;
            cx.notify();
        }
    }

    /// The pointer came to a row of the card that opens nothing.
    fn hover_hides_flyout(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if *hovered {
            self.set_flyout(false, cx);
        }
    }

    fn tab_menu_geometry(&self, window: &Window) -> Option<Geometry> {
        let menu = self.tab_menu.as_ref().filter(|menu| self.sessions.iter().any(|s| s.id == menu.id))?;
        let allowed = enabled(self.sessions.len(), menu.windows.len());
        let size = window.viewport_size();
        Some(Geometry::new(
            (f32::from(menu.position.x), f32::from(menu.position.y)),
            (menu.flyout && allowed.move_to_window).then_some(menu.windows.len()),
            (f32::from(size.width), f32::from(size.height)),
        ))
    }

    fn run_tab_menu(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.tab_menu.take() else { return };
        // The keys go back to the tab first, so a confirmation or a move starts from there.
        self.focus_active(window, cx);
        cx.notify();
        let id = menu.id;
        match choice {
            Choice::NewWindow => transfer::move_to_new(self.handle, id, window, cx),
            Choice::MoveTo(destination) => {
                let source = self.handle;
                cx.defer(move |cx| {
                    transfer::move_sessions(source, transfer::Destination::Existing(destination, usize::MAX), vec![id], cx);
                });
            }
            // The path of the tab's × button, with its confirmation of what the close would end.
            Choice::Close => {
                if let Some(ix) = self.sessions.iter().position(|s| s.id == id) {
                    self.close(ix, window, cx);
                }
            }
        }
    }

    pub(super) fn render_tab_menu(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let (Some(menu), Some(geometry)) = (self.tab_menu.as_ref(), self.tab_menu_geometry(window)) else {
            return Vec::new();
        };
        let p = cx.palette().clone();
        let allowed = enabled(self.sessions.len(), menu.windows.len());
        let new_window = menu::item("tab-menu-new-window", allowed.new_window, false, &p)
            .on_hover(cx.listener(Self::hover_hides_flyout))
            .when(allowed.new_window, |d| {
                d.on_click(cx.listener(|this, _, window, cx| this.run_tab_menu(Choice::NewWindow, window, cx)))
            })
            .child("移到新窗口");
        let move_to = menu::item("tab-menu-windows", allowed.move_to_window, false, &p)
            .justify_between()
            .gap(px(8.))
            .when(geometry.flyout.is_some(), |d| d.bg(p.card2))
            .when(allowed.move_to_window, |d| {
                d.on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if *hovered {
                        this.set_flyout(true, cx);
                    }
                }))
                .on_click(cx.listener(|this, _, _, cx| this.set_flyout(true, cx)))
            })
            .child(div().flex_1().child("移到窗口"))
            .child(div().flex_none().text_color(p.tx3).child("›"));
        let close = menu::item("tab-menu-close", true, true, &p)
            .on_hover(cx.listener(Self::hover_hides_flyout))
            .on_click(cx.listener(|this, _, window, cx| this.run_tab_menu(Choice::Close, window, cx)))
            .child("关闭会话");
        let card = deferred(
            anchored().position(geometry.card.origin()).snap_to_window_with_margin(px(MARGIN)).child(
                menu::card("tab-menu", &p)
                    .key_context(KEY_CONTEXT)
                    .track_focus(&menu.focus)
                    .on_action(cx.listener(|this, _: &CloseMenu, window, cx| this.close_tab_menu(window, cx)))
                    // The flyout is not inside the card, but a click in it is not outside the menu.
                    .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
                        let in_flyout = this
                            .tab_menu_geometry(window)
                            .and_then(|geometry| geometry.flyout)
                            .is_some_and(|flyout| flyout.contains(x, y));
                        if !in_flyout {
                            this.close_tab_menu(window, cx);
                        }
                    }))
                    .child(new_window)
                    .child(move_to)
                    .child(menu::separator(&p))
                    .child(close),
            ),
        )
        .with_priority(2)
        .into_any_element();
        let flyout = geometry.flyout.map(|flyout| {
            deferred(
                anchored().position(flyout.origin()).snap_to_window_with_margin(px(MARGIN)).child(
                    menu::card("tab-menu-flyout", &p).children(menu.windows.iter().enumerate().map(|(ix, (handle, label))| {
                        let handle = *handle;
                        menu::item(("tab-menu-window", ix), true, false, &p)
                            .on_click(cx.listener(move |this, _, window, cx| this.run_tab_menu(Choice::MoveTo(handle), window, cx)))
                            .child(crate::ui::ellipsis(("tab-menu-window-label", ix), label.clone()).flex_1())
                    })),
                ),
            )
            .with_priority(3)
            .into_any_element()
        });
        std::iter::once(card).chain(flyout).collect()
    }

    /// Debug review hook: the menu of the tab in front, with or without the list of windows.
    #[cfg(debug_assertions)]
    pub(super) fn demo_tab_menu(&mut self, flyout: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.main_id() else { return };
        self.open_tab_menu(id, point(px(312.), px(38.)), window, cx);
        if let Some(menu) = &mut self.tab_menu {
            menu.flyout = flyout;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: (f32, f32) = (1440., 900.);

    #[test]
    fn a_windows_only_tab_cannot_go_to_a_new_window() {
        assert_eq!(enabled(1, 0), Enabled { new_window: false, move_to_window: false });
        assert_eq!(enabled(1, 2), Enabled { new_window: false, move_to_window: true });
        assert_eq!(enabled(2, 0), Enabled { new_window: true, move_to_window: false });
        assert_eq!(enabled(4, 1), Enabled { new_window: true, move_to_window: true });
    }

    #[test]
    fn the_card_is_where_it_was_opened_and_fits_the_window() {
        let at = |x, y| Geometry::new((x, y), None, WINDOW).card;
        assert_eq!(at(312., 38.), Rect { x: 312., y: 38., w: 196., h: 117. });
        // A fractional pointer position lands on a whole pixel, as anchored does.
        assert_eq!(at(312.4, 38.5).x, 312.);
        assert_eq!(at(312.4, 38.5).y, 39.);
        // Past the right and bottom edges the card moves in, keeping the margin.
        assert_eq!(at(1300., 100.).x, 1440. - 8. - 196.);
        assert_eq!(at(100., 850.).y, 900. - 8. - 117.);
        // Within the margin of the edge, but not past it: stays.
        assert_eq!(at(1240., 100.).x, 1240.);
        // Left and top edges.
        assert_eq!(at(-5., -5.).x, 8.);
        assert_eq!(at(-5., -5.).y, 8.);
    }

    #[test]
    fn the_flyout_opens_to_the_right_of_the_row_with_its_first_item_in_line() {
        let geometry = Geometry::new((312., 38.), Some(2), WINDOW);
        let flyout = geometry.flyout.expect("flyout");
        // The row ends 6 inside the card; the gap counts from there.
        assert_eq!(flyout.x, 312. + 196. - 6. + 8.);
        // The second row starts one item below the card's padding; the flyout's padding goes above it.
        assert_eq!(flyout.y, 38. + 32.);
        assert_eq!((flyout.w, flyout.h), (196., 12. + 2. * 32.));
        assert_eq!(Geometry::new((312., 38.), None, WINDOW).flyout, None);
    }

    #[test]
    fn the_flyout_flips_to_the_left_when_the_right_has_no_room() {
        let geometry = Geometry::new((1300., 100.), Some(3), WINDOW);
        let (card, flyout) = (geometry.card, geometry.flyout.expect("flyout"));
        assert_eq!(flyout.x, card.x + 6. - 8. - 196.);
        assert!(flyout.right() < card.x);
        // Just fits on the right: stays there.
        let fits = Geometry::new((1440. - 8. - 196. - 196. - 2., 100.), Some(3), WINDOW);
        let (card, flyout) = (fits.card, fits.flyout.expect("flyout"));
        assert_eq!(flyout.x, card.right() - 6. + 8.);
        // One pixel more and it flips.
        let tight = Geometry::new((1440. - 8. - 196. - 196. - 1., 100.), Some(3), WINDOW);
        assert!(tight.flyout.expect("flyout").x < tight.card.x);
    }

    #[test]
    fn the_flyout_stays_inside_a_window_too_narrow_for_both_sides() {
        let geometry = Geometry::new((100., 100.), Some(2), (400., 600.));
        let flyout = geometry.flyout.expect("flyout");
        assert_eq!(flyout.x, 400. - 8. - 196.);
        assert!(flyout.right() <= 400. - 8.);
    }

    #[test]
    fn the_flyout_moves_up_to_stay_above_the_bottom_margin() {
        // Five windows make a taller card than the room below the row.
        let geometry = Geometry::new((100., 700.), Some(5), WINDOW);
        let flyout = geometry.flyout.expect("flyout");
        assert_eq!(flyout.h, 12. + 5. * 32.);
        assert_eq!(flyout.bottom(), 900. - 8.);
        // Whereas with room it lines up with the row.
        let roomy = Geometry::new((100., 100.), Some(5), WINDOW).flyout.expect("flyout");
        assert_eq!(roomy.y, 100. + 32.);
    }

    #[test]
    fn a_click_in_the_flyout_is_not_outside_the_menu() {
        let geometry = Geometry::new((312., 38.), Some(2), WINDOW);
        let flyout = geometry.flyout.expect("flyout");
        assert!(flyout.contains(flyout.x, flyout.y));
        assert!(flyout.contains(flyout.x + 100., flyout.y + 40.));
        assert!(!flyout.contains(flyout.right(), flyout.y));
        assert!(!flyout.contains(flyout.x + 10., flyout.bottom()));
        assert!(!flyout.contains(geometry.card.x + 10., geometry.card.y + 10.));
    }
}
