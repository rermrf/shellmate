//! Password control with no plaintext layout, clipboard copy or undo history.
//! The only app-owned text buffer is zeroizing and bounded; replacement wipes
//! the old allocation instead of leaving deleted characters in String capacity.
use crate::theme::ActivePalette;
use crate::ui::{Surface, field_box};
use gpui::{ElementInputHandler, EntityInputHandler, component::ActiveTheme, prelude::*, *};
use std::{ops::Range, time::Duration};
use zeroize::Zeroizing;

const BULLET: char = '•';
const BLINK: Duration = Duration::from_millis(500);
const TYPING_PAUSE: Duration = Duration::from_millis(300);

fn bullet_range(value: &str, selection: &Range<usize>) -> Range<usize> {
    let start = value[..selection.start].chars().count();
    start..start + value[selection.clone()].chars().count()
}

fn masked_geometry(
    selection: Range<usize>,
    x_for_bullet: impl Fn(usize) -> Pixels,
) -> (Pixels, Option<Range<Pixels>>) {
    let caret = x_for_bullet(selection.end);
    (
        caret,
        (!selection.is_empty()).then(|| x_for_bullet(selection.start)..caret),
    )
}

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
    surface: Surface,
    caret_visible: bool,
    blink: Option<Task<()>>,
}
impl EventEmitter<PasswordEvent> for PasswordInput {}
impl PasswordInput {
    pub fn new(
        placeholder: &'static str,
        surface: Surface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        cx.on_focus(&focus, window, |this, w, cx| {
            if w.is_window_active() { this.start_blink(BLINK, cx); }
        })
            .detach();
        cx.on_blur(&focus, window, |this, _, cx| {
            this.blink = None;
            cx.notify();
        })
        .detach();
        cx.observe_window_activation(window, |this, w, cx| {
            if w.is_window_active() && this.focus.is_focused(w) {
                this.start_blink(BLINK, cx);
            } else {
                this.blink = None;
            }
        }).detach();
        Self {
            value: Zeroizing::new(String::new()),
            focus,
            selection: 0..0,
            marked: None,
            placeholder,
            surface,
            caret_visible: true,
            blink: None,
        }
    }
    fn start_blink(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.caret_visible = true;
        cx.notify();
        if crate::prototype_fixture() {
            return;
        }
        let executor = cx.background_executor().clone();
        self.blink = Some(cx.spawn(async move |this, cx| {
            executor.timer(delay).await;
            while this
                .update(cx, |this, cx| {
                    this.caret_visible = !this.caret_visible;
                    cx.notify();
                })
                .is_ok()
            {
                executor.timer(BLINK).await;
            }
        }));
    }
    fn caret_activity(&mut self, cx: &mut Context<Self>) {
        if self.blink.is_some() {
            self.start_blink(TYPING_PAUSE, cx);
        }
    }
    pub fn expose(&self) -> &str {
        &self.value
    }
    pub fn take(&mut self, cx: &mut Context<Self>) -> Zeroizing<String> {
        let value = std::mem::replace(&mut self.value, Zeroizing::new(String::new()));
        self.selection = 0..0;
        self.marked = None;
        self.caret_activity(cx);
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
        self.caret_activity(cx);
        cx.emit(PasswordEvent::Changed);
        cx.notify();
        true
    }
    fn select_all(
        &mut self,
        _: &gpui::component::input::SelectAll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selection = 0..self.value.len();
        self.caret_activity(cx);
        cx.notify();
        window.notify_text_input_changed(&self.focus);
    }

    fn paste(
        &mut self,
        _: &gpui::component::input::Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                self.caret_activity(cx);
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
        let empty = self.value.is_empty();
        let text: SharedString = if empty {
            self.placeholder.into()
        } else {
            BULLET.to_string().repeat(self.value.chars().count()).into()
        };
        let selection = bullet_range(&self.value, &self.selection);
        let selection_color = cx.theme().selection;
        let active = focused && window.is_window_active();
        let show_caret = active && self.caret_visible;
        field_box(div(), self.surface, &p, focused)
            .id("secure-password")
            .role(Role::PasswordInput)
            .aria_label(self.placeholder)
            .track_focus(&self.focus)
            .tab_index(0)
            .relative()
            .h(px(38.))
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .items_center()
            .text_color(if empty { p.tx3 } else { p.tx })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, w, cx| {
                    this.focus(w, cx);
                    this.selection = this.value.len()..this.value.len();
                    this.caret_activity(cx);
                    w.notify_text_input_changed(&this.focus);
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(Self::key))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::paste))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let run = window.text_style().to_run(text.len());
                        let line = window.text_system().shape_line(text, px(13.), &[run], None);
                        let (caret, highlight) = masked_geometry(selection, |index| {
                            if empty {
                                px(0.)
                            } else {
                                line.x_for_index(index * BULLET.len_utf8())
                            }
                        });
                        let scroll = (caret + px(1.) - bounds.size.width).max(px(0.));
                        let origin = point(bounds.left() - scroll, bounds.top());
                        let caret_bounds = Bounds::new(
                            origin + point(caret, px(0.)),
                            size(px(1.), bounds.size.height),
                        );
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            if active && let Some(range) = highlight {
                                let rect = Bounds::new(
                                    origin + point(range.start, px(0.)),
                                    size(range.end - range.start, bounds.size.height),
                                );
                                window.paint_quad(fill(rect, selection_color));
                            }
                            let _ = line.paint(
                                origin,
                                bounds.size.height,
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                            if show_caret {
                                window.paint_quad(fill(caret_bounds, p.acc));
                            }
                        });
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(caret_bounds, entity),
                            cx,
                        );
                    },
                )
                .w_full()
                .h(px(18.)),
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

pub fn visible_strength(value: &str) -> Option<shellmate_vault::Strength> {
    (!value.is_empty()).then(|| shellmate_vault::strength(value))
}

pub fn strength_color(
    strength: shellmate_vault::Strength,
    p: &crate::theme::Palette,
) -> gpui::Hsla {
    match strength {
        shellmate_vault::Strength::Weak => p.err,
        shellmate_vault::Strength::Medium => p.warn,
        shellmate_vault::Strength::High => p.ok,
    }
}

#[cfg(test)]
mod tests {
    use super::{bullet_range, masked_geometry, visible_strength};
    use gpui::px;

    #[test]
    fn unicode_byte_selection_maps_to_displayed_bullets() {
        assert_eq!(bullet_range("字🦀a", &(3..7)), 1..2);
        assert_eq!(bullet_range("字🦀a", &(8..8)), 3..3);
    }

    #[test]
    fn strength_is_hidden_only_for_an_empty_password() {
        assert_eq!(visible_strength(""), None);
        assert_eq!(visible_strength("a"), Some(shellmate_vault::Strength::Weak));
        assert_eq!(visible_strength(" "), Some(shellmate_vault::Strength::Weak));
    }

    #[test]
    fn masked_caret_uses_empty_middle_and_end_offsets() {
        let advance = |index: usize| px(6.75 * index as f32);
        assert_eq!(masked_geometry(0..0, advance), (px(0.), None));
        assert_eq!(masked_geometry(2..2, advance), (px(13.5), None));
        assert_eq!(masked_geometry(4..4, advance), (px(27.), None));
    }

    #[test]
    fn masked_selection_uses_shaped_endpoints() {
        let offsets = [0., 5.5, 11.25, 17.];
        let advance = |index: usize| px(offsets[index]);
        assert_eq!(
            masked_geometry(1..3, advance),
            (px(17.), Some(px(5.5)..px(17.)))
        );
        assert_eq!(
            masked_geometry(0..3, advance),
            (px(17.), Some(px(0.)..px(17.)))
        );
    }
}
