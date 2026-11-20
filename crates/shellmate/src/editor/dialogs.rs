//! The two questions the editor asks: what to do about a file that changed on the server, and what
//! to do about changes that are not uploaded.

use std::rc::Rc;

use gpui::component::button::{Button, ButtonVariants as _};
use gpui::prelude::*;
use gpui::{App, Context, FontWeight, IntoElement, Render, Window, div, px};

use super::diff::DiffLine;
use crate::dialogs::{DialogBody, Notice, Tone, actions, button, hint, notice, palette, stack};
use crate::theme::MONO_FONT;

// ---- 远端文件已被修改 -----------------------------------------------------------------------------------------

/// What the user chose about a conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    /// 取消 (or Esc): back to the buffer.
    Cancel,
    /// 载入远端版本: the buffer becomes the server's text.
    LoadRemote,
    /// 覆盖远端: upload anyway.
    Overwrite,
}

/// Hears the answer.
pub type ConflictAnswer = Rc<dyn Fn(ConflictChoice, &mut Window, &mut App)>;

/// "远端文件已被修改": when, a few lines of what differs, and three ways out.
pub struct ConflictDialog {
    local: bool,
    file: String,
    /// When the server's copy changed (`HH:MM`), if it said.
    remote_time: Option<String>,
    snippet: Vec<DiffLine>,
    /// The server's copy can be shown as text (and so loaded).
    remote_is_text: bool,
    answer: ConflictAnswer,
    answered: bool,
}

impl ConflictDialog {
    /// Opens the dialog.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        file: String,
        remote_time: Option<String>,
        snippet: Vec<DiffLine>,
        remote_is_text: bool,
        local: bool,
        answer: ConflictAnswer,
        window: &mut Window,
        cx: &mut App,
    ) {
        let body = cx.new(|_| Self { file, remote_time, snippet, remote_is_text, local, answer, answered: false });
        crate::dialogs::open(window, cx, if local { "磁盘文件已被修改" } else { "远端文件已被修改" }, 520., body);
    }

    fn choose(&mut self, choice: ConflictChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.answered = true;
        // Closed first: what the answer sets going (a save, say) may open another dialog.
        crate::dialogs::modal::close(window, cx);
        (self.answer)(choice, window, cx);
    }
}

impl DialogBody for ConflictDialog {
    /// Enter does nothing: every answer decides about somebody's changes.
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        false
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.answered {
            self.answered = true;
            (self.answer)(ConflictChoice::Cancel, window, cx);
        }
    }
}

impl Render for ConflictDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let when = match &self.remote_time {
            Some(time) => format!("{} 在你打开之后于 {time} 被修改。", self.file),
            None => format!("{} 在你打开之后被修改。", self.file),
        };
        let diff = (!self.snippet.is_empty()).then(|| {
            let lines = div()
                .flex()
                .flex_col()
                .px(px(12.))
                .py(px(10.))
                .rounded(px(12.))
                .bg(p.card2)
                .font_family(MONO_FONT)
                .text_size(px(12.))
                .line_height(px(19.))
                .children(self.snippet.iter().map(|line| {
                    let color = match line {
                        DiffLine::Removed(_) => p.err,
                        DiffLine::Added(_) => p.ok,
                        DiffLine::Hunk(_) => p.tx2,
                        DiffLine::Context(_) | DiffLine::More(_) => p.tx3,
                    };
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(color)
                        .when(matches!(line, DiffLine::Hunk(_)), |d| d.font_weight(FontWeight::SEMIBOLD))
                        .child(line.text())
                }));
            let legend = div().text_size(px(11.5)).text_color(p.tx3).child(if self.local { "- 你的版本 · + 磁盘版本" } else { "- 你的版本 · + 远端版本" });
            div().flex().flex_col().gap(px(6.)).child(legend).child(lines)
        });
        let note = if !self.remote_is_text {
            Some(
                notice(
                    Notice::Warn,
                    if self.local { "磁盘文件现在不是可显示的文本：无法比较，也不能载入。" } else { "远端文件现在不是可显示的文本（二进制、非 UTF-8 或超过 5 MB）：无法比较，也不能载入。" },
                    &p,
                )
                .into_any_element(),
            )
        } else if self.snippet.is_empty() {
            Some(notice(Notice::Info, if self.local { "文本内容相同，只是磁盘上的修改时间变了。" } else { "两边的文本内容相同，只是远端的修改时间变了。" }, &p).into_any_element())
        } else {
            None
        };
        stack().child(hint(when, &p)).child(hint(if self.local { "直接保存会覆盖磁盘上的改动。" } else { "直接上传会覆盖对方的改动。" }, &p)).children(diff).children(note).child(
            actions(vec![
                button("conflict-cancel", "取消", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                button("conflict-load", if self.local { "载入磁盘版本" } else { "载入远端版本" }, Tone::Plain, self.remote_is_text)
                    .on_click(cx.listener(|this, _, window, cx| this.choose(ConflictChoice::LoadRemote, window, cx))),
                button("conflict-overwrite", if self.local { "覆盖磁盘版本" } else { "覆盖远端" }, Tone::Danger, true)
                    .on_click(cx.listener(|this, _, window, cx| this.choose(ConflictChoice::Overwrite, window, cx))),
            ]),
        )
    }
}

// ---- 有未上传的修改 -------------------------------------------------------------------------------------------

/// What the user chose about closing with changes that are not uploaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsavedChoice {
    /// 取消 (or Esc): stay in the editor.
    Cancel,
    /// 放弃修改: close and lose them.
    Discard,
    /// 上传并关闭: close if the upload works.
    SaveAndClose,
}

/// Hears the answer.
pub type UnsavedAnswer = Rc<dyn Fn(UnsavedChoice, &mut Window, &mut App)>;

/// "有未上传的修改".
pub struct UnsavedDialog {
    local: bool,
    file: String,
    answer: UnsavedAnswer,
    answered: bool,
}

impl UnsavedDialog {
    /// Opens the dialog.
    pub fn open(file: String, local: bool, answer: UnsavedAnswer, window: &mut Window, cx: &mut App) {
        let body = cx.new(|_| Self { file, local, answer, answered: false });
        crate::dialogs::open(window, cx, if local { "有未保存的修改" } else { "有未上传的修改" }, 460., body);
    }

    fn choose(&mut self, choice: UnsavedChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.answered = true;
        crate::dialogs::modal::close(window, cx);
        (self.answer)(choice, window, cx);
    }
}

impl DialogBody for UnsavedDialog {
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        false
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.answered {
            self.answered = true;
            (self.answer)(UnsavedChoice::Cancel, window, cx);
        }
    }
}

impl Render for UnsavedDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        stack().child(hint(format!("关闭后，对 {} 的修改将会丢失。", self.file), &p)).child(actions(vec![
            button("unsaved-cancel", "取消", Tone::Plain, true)
                .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
            Button::new("unsaved-discard")
                .label("放弃修改")
                .danger()
                .outline()
                .on_click(cx.listener(|this, _, window, cx| this.choose(UnsavedChoice::Discard, window, cx))),
            button("unsaved-save", if self.local { "保存并关闭" } else { "上传并关闭" }, Tone::Primary, true)
                .on_click(cx.listener(|this, _, window, cx| this.choose(UnsavedChoice::SaveAndClose, window, cx))),
        ]))
    }
}
