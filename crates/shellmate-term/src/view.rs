//! Interactive terminal view: keyboard, IME, mouse selection, scrolling and search.

use std::ops::Range;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{TermMode, point_to_viewport, viewport_to_point};
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{Input, InputEvent, InputState};
use gpui::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, EntityInputHandler, FocusHandle, Focusable,
    KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    AnyElement, ScrollDelta, ScrollEvent, Subscription, UTF16Selection, Window, actions, div, point, px,
    size,
};

use crate::colors::to_hsla;
use crate::element::{TerminalElement, TerminalStyle};
use crate::keys;
use crate::terminal::{SearchMatch, Terminal, TerminalEvent, TerminalSize};

actions!(
    terminal,
    [
        Copy,
        Paste,
        SelectAll,
        Clear,
        ToggleSearch,
        SearchUp,
        SearchDown,
        CloseSearch,
        SendTab,
        SendBackTab,
        ScrollPageUp,
        ScrollPageDown,
        ScrollToTop,
        ScrollToBottom,
    ]
);

const CONTEXT: &str = "Terminal";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("cmd-f", ToggleSearch, Some(CONTEXT)),
        KeyBinding::new("cmd-g", SearchUp, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-g", SearchDown, Some(CONTEXT)),
        KeyBinding::new("escape", CloseSearch, Some("TerminalSearch")),
        // gpui-kit's Root binds tab/shift-tab to focus navigation, and bindings
        // win over key_down listeners, so the shell would never see Tab
        // (completion). Claim them in the deeper Terminal context.
        KeyBinding::new("tab", SendTab, Some(CONTEXT)),
        KeyBinding::new("shift-tab", SendBackTab, Some(CONTEXT)),
        KeyBinding::new("shift-pageup", ScrollPageUp, Some(CONTEXT)),
        KeyBinding::new("shift-pagedown", ScrollPageDown, Some(CONTEXT)),
        KeyBinding::new("cmd-home", ScrollToTop, Some(CONTEXT)),
        KeyBinding::new("cmd-end", ScrollToBottom, Some(CONTEXT)),
    ]);
}

/// Where the grid was last painted, for mapping mouse and IME positions.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub origin: gpui::Point<Pixels>,
    pub size: TerminalSize,
}

struct Search {
    input: gpui::Entity<InputState>,
    matches: Vec<SearchMatch>,
    active: Option<usize>,
    _subscription: Subscription,
}

pub struct TerminalView {
    terminal: gpui::Entity<Terminal>,
    focus_handle: FocusHandle,
    style: TerminalStyle,
    pub(crate) geometry: Option<Geometry>,
    marked_text: Option<String>,
    selecting: bool,
    scroll_remainder: f32,
    search: Option<Search>,
    search_icon: Option<Icon>,
    /// Copy the selection to the clipboard as soon as the mouse is released.
    pub copy_on_select: bool,
    /// Right click pastes the clipboard (prototype default).
    pub right_click_paste: bool,
    pub history_shortcut: bool,
    /// Rounds the top corners so the view fits inside a rounded card.
    pub top_radius: Pixels,
    _subscriptions: Vec<Subscription>,
}

impl TerminalView {
    pub fn new(terminal: gpui::Entity<Terminal>, style: TerminalStyle, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&terminal, |_, _, cx| cx.notify()),
            cx.subscribe(&terminal, |this: &mut Self, _, event: &TerminalEvent, cx| {
                if matches!(event, TerminalEvent::Output) && this.search.is_some() {
                    this.refresh_search(false, cx);
                }
            }),
        ];
        Self {
            terminal,
            focus_handle: cx.focus_handle(),
            style,
            geometry: None,
            marked_text: None,
            selecting: false,
            scroll_remainder: 0.,
            search: None,
            search_icon: None,
            copy_on_select: true,
            right_click_paste: true,
            history_shortcut: true,
            top_radius: px(0.),
            _subscriptions: subscriptions,
        }
    }

    /// Supply the host application's search glyph without coupling the terminal to its asset paths.
    pub fn with_search_icon(mut self, icon: Icon) -> Self {
        self.search_icon=Some(icon);
        self
    }

    pub fn terminal(&self) -> &gpui::Entity<Terminal> {
        &self.terminal
    }

    pub fn set_style(&mut self, style: TerminalStyle, cx: &mut Context<Self>) {
        let palette = style.palette.clone();
        self.style = style;
        self.terminal.update(cx, |t, _| t.set_palette(palette));
        cx.notify();
    }

    pub fn is_search_open(&self) -> bool {
        self.search.is_some()
    }

    // ---- actions -------------------------------------------------------

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.terminal.read(cx).selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        self.paste_clipboard(cx);
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.terminal.update(cx, |t, _| t.paste(&text));
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        {
            let term = self.terminal.read(cx).term().clone();
            let mut term = term.lock();
            let mut selection =
                Selection::new(SelectionType::Simple, GridPoint::new(term.topmost_line(), Column(0)), Side::Left);
            selection.update(GridPoint::new(term.bottommost_line(), term.last_column()), Side::Right);
            term.selection = Some(selection);
        }
        cx.notify();
    }

    fn send_tab(&mut self, _: &SendTab, window: &mut Window, cx: &mut Context<Self>) {
        self.send_focused(b"\t", window, cx);
    }

    fn send_back_tab(&mut self, _: &SendBackTab, window: &mut Window, cx: &mut Context<Self>) {
        self.send_focused(b"\x1b[Z", window, cx);
    }

    /// Writes `bytes` when the grid itself has focus; otherwise (e.g. the search
    /// field is focused) lets the key keep its normal meaning.
    fn send_focused(&mut self, bytes: &[u8], window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.is_focused(window) && self.marked_text.is_none() {
            self.terminal.update(cx, |t, _| t.input(bytes));
        } else {
            cx.propagate();
        }
    }

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |t, _| t.clear());
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |t, _| t.scroll_page(true));
        cx.notify();
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |t, _| t.scroll_page(false));
        cx.notify();
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |t, _| t.scroll_to_edge(true));
        cx.notify();
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |t, _| t.scroll_to_edge(false));
        cx.notify();
    }

    // ---- search --------------------------------------------------------

    pub fn toggle_search(&mut self, _: &ToggleSearch, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = &self.search {
            let input = search.input.clone();
            input.update(cx, |state, cx| state.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("在输出中搜索"));
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| match event {
            InputEvent::Change => this.refresh_search(true, cx),
            InputEvent::PressEnter { shift, .. } => this.step_search(!*shift, cx),
            _ => {}
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.search = Some(Search { input, matches: Vec::new(), active: None, _subscription: subscription });
        cx.notify();
    }

    fn close_search(&mut self, _: &CloseSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn search_up(&mut self, _: &SearchUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step_search(true, cx);
    }

    fn search_down(&mut self, _: &SearchDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step_search(false, cx);
    }

    /// Re-runs the query. `jump` selects the newest match and scrolls to it.
    fn refresh_search(&mut self, jump: bool, cx: &mut Context<Self>) {
        let Some(search) = &mut self.search else { return };
        let query = search.input.read(cx).value();
        search.matches = self.terminal.read(cx).search(&query);
        let count = search.matches.len();
        search.active = match search.active {
            _ if count == 0 => None,
            Some(i) if !jump => Some(i.min(count - 1)),
            _ => Some(count - 1),
        };
        if jump && let Some(m) = search.active.map(|i| search.matches[i].clone()) {
            self.terminal.update(cx, |t, _| t.scroll_to_point(*m.start()));
        }
        cx.notify();
    }

    /// Moves to the previous (older, `up`) or next match, wrapping around.
    fn step_search(&mut self, up: bool, cx: &mut Context<Self>) {
        let Some(search) = &mut self.search else { return };
        let count = search.matches.len();
        if count == 0 {
            return;
        }
        let next = match search.active {
            None => count - 1,
            Some(i) if up => (i + count - 1) % count,
            Some(i) => (i + 1) % count,
        };
        search.active = Some(next);
        let start = *search.matches[next].start();
        self.terminal.update(cx, |t, _| t.scroll_to_point(start));
        cx.notify();
    }

    // ---- keyboard ------------------------------------------------------

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.is_focused(window) || self.marked_text.is_some() {
            return;
        }
        let keystroke = &event.keystroke;
        if crate::behavior::history_shortcut(self.history_shortcut,keystroke) {
            self.terminal.update(cx, |_, cx| cx.emit(TerminalEvent::HistoryRequested));
            cx.stop_propagation();
            return;
        }
        if self.terminal.read(cx).is_exited() {
            if keystroke.key == "enter" && !keystroke.modifiers.modified() {
                self.terminal.update(cx, |t, cx| t.restart(cx));
                cx.stop_propagation();
            }
            return;
        }
        let mode = self.terminal.read(cx).mode();
        if let Some(seq) = keys::to_esc_str(keystroke, &mode) {
            self.terminal.update(cx, |t, _| t.input(seq.as_bytes()));
            cx.stop_propagation();
        }
    }

    // ---- mouse ---------------------------------------------------------

    fn grid_point(&self, position: gpui::Point<Pixels>, cx: &App) -> Option<(GridPoint, Side)> {
        let g = self.geometry?;
        let x = f32::from(position.x - g.origin.x).max(0.);
        let y = f32::from(position.y - g.origin.y).max(0.);
        let (cw, lh) = (f32::from(g.size.cell_width), f32::from(g.size.line_height));
        let col = ((x / cw) as usize).min(g.size.cols as usize - 1);
        let line = ((y / lh) as usize).min(g.size.rows as usize - 1);
        let side = if (x / cw).fract() > 0.5 { Side::Right } else { Side::Left };
        let offset = self.terminal.read(cx).term().lock().grid().display_offset();
        Some((viewport_to_point(offset, GridPoint::new(line, Column(col))), side))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        self.terminal.update(cx,|_,cx|cx.emit(TerminalEvent::Focused));
        let Some((point, side)) = self.grid_point(event.position, cx) else { return };
        let ty = match event.click_count {
            2 => SelectionType::Semantic,
            3.. => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        {
            let term = self.terminal.read(cx).term().clone();
            let mut term = term.lock();
            match (&mut term.selection, event.modifiers.shift) {
                (Some(selection), true) => selection.update(point, side),
                _ => term.selection = Some(Selection::new(ty, point, side)),
            }
        }
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some((point, side)) = self.grid_point(event.position, cx) else { return };
        {
            let term = self.terminal.read(cx).term().clone();
            let mut term = term.lock();
            if let Some(selection) = &mut term.selection {
                selection.update(point, side);
            }
        }
        cx.notify();
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.selecting) {
            return;
        }
        let selected=self.terminal.read(cx).selection_text();
        if selected.is_none(){self.terminal.read(cx).term().lock().selection=None;}
        if let Some(text)=crate::behavior::selection_copy(self.copy_on_select,selected){cx.write_to_clipboard(ClipboardItem::new_string(text));}
        cx.notify();
    }

    fn right_mouse_down(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        self.terminal.update(cx,|_,cx|cx.emit(TerminalEvent::Focused));
        if crate::behavior::right_paste(self.right_click_paste) {
            self.paste_clipboard(cx);
        }
    }

    fn scroll_wheel(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(g) = self.geometry else { return };
        let delta = match event.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / f32::from(g.size.line_height),
        };
        self.scroll_remainder += delta;
        let lines = self.scroll_remainder.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_remainder -= lines as f32;

        let mode = self.terminal.read(cx).mode();
        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) && !event.modifiers.shift {
            // Full-screen apps (less, vim) get arrow keys instead of scrollback.
            let app = mode.contains(TermMode::APP_CURSOR);
            let seq = match (lines > 0, app) {
                (true, true) => "\x1bOA",
                (true, false) => "\x1b[A",
                (false, true) => "\x1bOB",
                (false, false) => "\x1b[B",
            };
            let bytes = seq.repeat(lines.unsigned_abs() as usize);
            self.terminal.update(cx, |t, _| t.input(bytes.as_bytes()));
        } else {
            self.terminal.update(cx, |t, _| t.scroll(lines));
        }
        cx.notify();
    }

    fn cursor_bounds(&self, cx: &App) -> Option<Bounds<Pixels>> {
        let g = self.geometry?;
        let term = self.terminal.read(cx).term().lock();
        let vp = point_to_viewport(term.grid().display_offset(), term.grid().cursor.point)?;
        Some(Bounds::new(
            point(g.origin.x + g.size.cell_width * vp.column.0 as f32, g.origin.y + g.size.line_height * vp.line as f32),
            size(g.size.cell_width, g.size.line_height),
        ))
    }

    fn render_search(&self, search: &Search, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let status = match (search.matches.len(), search.active) {
            (0, _) => "无结果".to_string(),
            (n, Some(i)) => format!("{}/{}", i + 1, n),
            (n, None) => n.to_string(),
        };
        h_flex()
            .key_context("TerminalSearch")
            .absolute()
            .top_2()
            .right_3()
            .w(px(340.))
            .gap_1()
            .py_1()
            .pl_1()
            .pr_1()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .rounded_lg()
            .shadow_md()
            .children(self.search_icon.clone().map(|icon|icon.text_color(theme.foreground)))
            .child(div().flex_1().child(Input::new(&search.input).small().appearance(false)))
            .child(div().text_xs().text_color(theme.muted_foreground).px_1().child(status))
            .child(
                Button::new("search-up")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronUp)
                    .tooltip("上一个 ↵")
                    .on_click(cx.listener(|this, _, _, cx| this.step_search(true, cx))),
            )
            .child(
                Button::new("search-down")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .tooltip("下一个 ⇧↵")
                    .on_click(cx.listener(|this, _, _, cx| this.step_search(false, cx))),
            )
            .child(
                Button::new("search-close")
                    .ghost()
                    .xsmall()
                    .label("×").text_size(px(14.)).line_height(px(14.))
                    .tooltip("关闭 Esc")
                    .on_click(cx.listener(|this, _, window, cx| this.close_search(&CloseSearch, window, cx))),
            )
            .into_any_element()
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let (matches, active) = self
            .search
            .as_ref()
            .map(|s| (s.matches.clone(), s.active))
            .unwrap_or_default();
        let element = TerminalElement::new(
            cx.entity(),
            self.terminal.clone(),
            self.focus_handle.clone(),
            focused,
            self.style.clone(),
            self.marked_text.clone(),
            matches,
            active,
        );
        let search = self.search.as_ref().map(|s| self.render_search(s, cx));

        div()
            .id("terminal-view")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::send_tab))
            .on_action(cx.listener(Self::send_back_tab))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(Self::close_search))
            .on_action(cx.listener(Self::search_up))
            .on_action(cx.listener(Self::search_down))
            .on_action(cx.listener(Self::scroll_page_up))
            .on_action(cx.listener(Self::scroll_page_down))
            .on_action(cx.listener(Self::scroll_to_top))
            .on_action(cx.listener(Self::scroll_to_bottom))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::right_mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll(cx.listener(Self::scroll_wheel))
            .size_full()
            .relative()
            .overflow_hidden()
            .rounded_tl(self.top_radius)
            .rounded_tr(self.top_radius)
            .when(self.terminal.read(cx).is_replay(), |d| d.rounded_bl(self.top_radius).rounded_br(self.top_radius))
            .bg(to_hsla(self.style.palette.background))
            .child(div().size_full().px(px(18.)).py(px(14.)).cursor(CursorStyle::IBeam).child(element))
            .children(search)
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_text.as_ref().map(|t| 0..t.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        if !text.is_empty() {
            self.terminal.update(cx, |t, _| t.input(text.as_bytes()));
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal.read(cx).is_replay(){return;}
        self.marked_text = (!new_text.is_empty()).then(|| new_text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds(cx)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}
