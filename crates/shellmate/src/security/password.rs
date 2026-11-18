//! Password control with no plaintext layout, clipboard copy or undo history.
//! The only app-owned text buffer is zeroizing and bounded; replacement wipes
//! the old allocation instead of leaving deleted characters in String capacity.
use crate::theme::ActivePalette;
use gpui::{ElementInputHandler, EntityInputHandler, prelude::*, *};
use std::ops::Range;
use zeroize::Zeroizing;
pub enum PasswordEvent {
    Changed,
    Submit,
}
pub struct PasswordInput {
    value: Zeroizing<String>,
    focus: FocusHandle,
    selection: Range<usize>,
    marked: Option<Range<usize>>,
    placeholder: &'static str,
}
impl EventEmitter<PasswordEvent> for PasswordInput {}
impl PasswordInput {
    pub fn new(placeholder: &'static str, cx: &mut Context<Self>) -> Self {
        Self {
            value: Zeroizing::new(String::new()),
            focus: cx.focus_handle(),
            selection: 0..0,
            marked: None,
            placeholder,
        }
    }
    pub fn expose(&self) -> &str {
        &self.value
    }
    pub fn take(&mut self, cx: &mut Context<Self>) -> Zeroizing<String> {
        let value = std::mem::replace(&mut self.value, Zeroizing::new(String::new()));
        self.selection = 0..0;
        self.marked = None;
        cx.notify();
        value
    }
    pub fn set(&mut self, value: &str, cx: &mut Context<Self>) {
        self.selection = 0..self.value.len();
        self.replace(None, value, cx);
    }
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }
    fn byte(&self, utf16: usize) -> usize {
        let mut n = 0;
        for (i, c) in self.value.char_indices() {
            if n >= utf16 {
                return i;
            }
            n += c.len_utf16();
        }
        self.value.len()
    }
    fn utf16(&self, byte: usize) -> usize {
        self.value[..byte].encode_utf16().count()
    }
    fn replace(&mut self, range: Option<Range<usize>>, text: &str, cx: &mut Context<Self>) -> bool {
        let range = range
            .map(|r| self.byte(r.start)..self.byte(r.end))
            .or_else(|| self.marked.clone())
            .unwrap_or(self.selection.clone());
        if range.start > range.end
            || self.value.len() + text.len() > 4096
            || text.contains(['\n', '\r'])
        {
            return false;
        }
        let mut next = Zeroizing::new(String::with_capacity(self.value.len() + text.len()));
        next.push_str(&self.value[..range.start]);
        next.push_str(text);
        next.push_str(&self.value[range.end..]);
        self.value = next;
        let end = range.start + text.len();
        self.selection = end..end;
        self.marked = None;
        cx.emit(PasswordEvent::Changed);
        cx.notify();
        true
    }
    fn select_all(&mut self, _: &gpui::component::input::SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.selection = 0..self.value.len();
        cx.notify();
        window.notify_text_input_changed(&self.focus);
    }

    fn paste(&mut self, _: &gpui::component::input::Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace(None, &Zeroizing::new(text), cx);
        }
        window.notify_text_input_changed(&self.focus);
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        match key.key.as_str() {
            "enter" => {
                cx.emit(PasswordEvent::Submit);
                cx.stop_propagation();
            }
            "a" if key.modifiers.platform => {
                self.select_all(&gpui::component::input::SelectAll, window, cx);
                cx.stop_propagation();
            }
            "v" if key.modifiers.platform => {
                self.paste(&gpui::component::input::Paste, window, cx);
                cx.stop_propagation();
            }
            "c" | "x" if key.modifiers.platform => cx.stop_propagation(),
            "backspace" => {
                if self.selection.is_empty() {
                    let end = self.selection.end;
                    let start = self.value[..end]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(i, _)| i);
                    self.selection = start..end;
                }
                self.replace(None, "", cx);
                cx.stop_propagation();
            }
            "delete" => {
                if self.selection.is_empty() {
                    let start = self.selection.start;
                    let end = self.value[start..]
                        .chars()
                        .next()
                        .map_or(start, |c| start + c.len_utf8());
                    self.selection = start..end;
                }
                self.replace(None, "", cx);
                cx.stop_propagation();
            }
            "left" | "right" | "home" | "end" => {
                let pos = match key.key.as_str() {
                    "home" => 0,
                    "end" => self.value.len(),
                    "left" => self.value[..self.selection.start]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(i, _)| i),
                    _ => {
                        self.selection.end
                            + self.value[self.selection.end..]
                                .chars()
                                .next()
                                .map_or(0, char::len_utf8)
                    }
                };
                self.selection = pos..pos;
                cx.stop_propagation();
                cx.notify();
            }
            _ => {}
        }
        window.notify_text_input_changed(&self.focus);
    }
}
impl Focusable for PasswordInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for PasswordInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let focus = self.focus.clone();
        let entity = cx.entity();
        let focused = self.focus.is_focused(window);
        let text = if self.value.is_empty() {
            self.placeholder.to_owned()
        } else {
            "•".repeat(self.value.chars().count().min(60))
        };
        div()
            .id("secure-password")
            .track_focus(&self.focus)
            .tab_index(0)
            .relative()
            .h(px(38.))
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .px(px(12.))
            .rounded(px(11.))
            .bg(p.card2)
            .border_1()
            .border_color(if focused { p.acc } else { p.line })
            .flex()
            .items_center()
            .text_size(px(13.))
            .text_color(if self.value.is_empty() { p.tx3 } else { p.tx })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, w, cx| {
                    this.focus(w, cx);
                    this.selection = this.value.len()..this.value.len();
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(Self::key))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::paste))
            .child(text)
            .when(focused, |d| d.child(div().w(px(1.)).h(px(17.)).bg(p.acc)))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, w, cx| {
                        w.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                    },
                )
                .absolute()
                .inset_0(),
            )
    }
}
impl EntityInputHandler for PasswordInput {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.utf16(self.selection.start)..self.utf16(self.selection.end),
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|r| self.utf16(r.start)..self.utf16(r.end))
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(range, text, cx);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.replace(range, text, cx) {
            self.marked = Some(self.selection.end.saturating_sub(text.len())..self.selection.end);
        }
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.utf16(self.selection.end))
    }
}

pub fn strength_color(value: &str, p: &crate::theme::Palette) -> gpui::Hsla {
    match shellmate_vault::strength(value) {
        shellmate_vault::Strength::Weak => p.err,
        shellmate_vault::Strength::Medium => p.warn,
        shellmate_vault::Strength::High => p.ok,
    }
}
