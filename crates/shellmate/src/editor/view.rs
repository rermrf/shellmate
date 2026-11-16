//! The editor modal: a title row with the file's name, language, place and status; the code editor
//! of gpui-component with line numbers; and a footer. Everything it decides lives in
//! [`super::model`]; this feeds the model what happens and draws what it says.

use std::cell::Cell;

use gpui::component::WindowExt as _;
use gpui::component::input::{Editor, EditorState, Escape, InputEvent, RopeExt as _};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, EventEmitter, FontWeight, KeyBinding, Subscription, Task, WeakEntity, Window, actions, div,
    px, white,
};
use shellmate_sftp::{SftpError, TextFile, path};

use super::dialogs::{ConflictAnswer, ConflictChoice, ConflictDialog, UnsavedAnswer, UnsavedChoice, UnsavedDialog};
use super::diff;
use super::lang::{self, Lang};
use super::model::{EditModel, SaveEnd, SaveRequest, Tone};
use crate::files::format::{clock_hm, clock_now};
use crate::files::panel::FilesPanel;
use crate::theme::{ActivePalette, MONO_FONT, Palette};

/// The key context of the editor: ⌘S and Esc mean something here, and only here (the file panel and
/// the terminal behind the modal never see them).
pub const CONTEXT: &str = "Editor";

actions!(editor, [EditorSave]);

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("cmd-s", EditorSave, Some(CONTEXT))]);
}

/// What the editor tells the window.
pub enum EditorEvent {
    /// The file was uploaded: the directory it is in changed.
    Saved(String),
    /// The editor is gone.
    Closed,
}

impl EventEmitter<EditorEvent> for FileEditor {}

/// `\r\n` and `\r` as `\n`: the buffer is saved in the line ending style the file had.
fn normalize(text: &str) -> String {
    if text.contains('\r') { text.replace("\r\n", "\n").replace('\r', "\n") } else { text.to_string() }
}

/// The number of lines of a text for the footer: `rope_lines` is what the rope counts (one more than
/// the line breaks), and a final newline does not start another line.
fn count_lines(len: usize, rope_lines: usize, ends_with_newline: bool) -> usize {
    match (len, ends_with_newline) {
        (0, _) => 0,
        (_, true) => rope_lines.saturating_sub(1),
        (_, false) => rope_lines,
    }
}

pub struct FileEditor {
    model: EditModel,
    /// The file's name, for the title and the dialogs.
    name: String,
    /// What the title shows after the name: `主机名:路径`.
    place: String,
    /// The path that was opened (saving goes there: a link is followed by the server).
    path: String,
    lang: Lang,
    editor: Entity<EditorState>,
    /// The file panel of the tab it was opened from: it holds the SFTP session.
    panel: WeakEntity<FilesPanel>,
    /// The text a running save sends: the left side of the diff if the save ends in a conflict.
    saving_text: Option<String>,
    save_task: Option<Task<()>>,
    /// A dialog is over the editor (it has its own Esc).
    dialog_open: bool,
    _subscriptions: Vec<Subscription>,
}

impl FileEditor {
    pub fn new(
        file: TextFile,
        host: &str,
        panel: WeakEntity<FilesPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = path::file_name(&file.path).unwrap_or(&file.path).to_string();
        let lang = lang::language(&name);
        let began = std::time::Instant::now();
        // No soft wrap, as in the prototype (`wrap=off`): long lines scroll sideways. Wrapping has to measure
        // every line of the file before the first frame, which takes seconds for a file of 5 MB.
        let editor = cx.new(|cx| EditorState::new(window, cx).language(lang.token).soft_wrap(false));
        // Not `default_value`: that is applied at the first paint, and may count as a change.
        editor.update(cx, |state, cx| state.set_value(file.text.clone(), window, cx));
        log::info!("编辑器：{} 载入缓冲区 {} 字节，用时 {} ms", name, file.text.len(), began.elapsed().as_millis());
        let subscription = cx.subscribe_in(&editor, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.on_edited(cx);
            }
        });
        Self {
            model: EditModel::new(&file),
            name,
            place: format!("{host}:{}", file.path),
            path: file.path,
            lang,
            editor,
            panel,
            saving_text: None,
            save_task: None,
            dialog_open: false,
            _subscriptions: vec![subscription],
        }
    }

    /// Opens `editor` as a modal over the window (one at a time: the modal sees to that).
    pub fn open(editor: Entity<Self>, window: &mut Window, cx: &mut App) {
        let first_build = Cell::new(true);
        crate::dialogs::open_modal(window, cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                editor.update(cx, |editor, cx| editor.focus(window, cx));
            }
            let on_cancel = editor.clone();
            dialog
                .centered()
                .w(px(1200.)).rounded(px(24.)).border_0().shadow(crate::dialogs::modal_shadow(None))
                .p(px(0.))
                .close_button(false)
                .overlay_closable(false)
                // Enter is a newline; Esc is 关闭, which may need to ask first.
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, window, cx| {
                    on_cancel.update(cx, |editor, cx| editor.request_close(window, cx));
                    false
                })
                .child(editor.clone())
        });
    }

    fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| state.focus(window, cx));
    }

    /// The path of the directory the file is in.
    fn dir(&self) -> String {
        path::parent(&self.path).unwrap_or_else(|| "/".to_string())
    }

    /// The buffer's text, line ends as `\n`.
    pub fn text(&self, cx: &App) -> String {
        normalize(&self.editor.read(cx).text().to_string())
    }

    /// The buffer changed (typing, pasting, undo).
    fn on_edited(&mut self, cx: &mut Context<Self>) {
        self.model.edited();
        cx.notify();
    }

    // ---- saving ------------------------------------------------------------------------------------

    fn on_save(&mut self, _: &EditorSave, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx);
    }

    /// ⌘S / 保存并上传.
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(request) = self.model.begin_save() {
            self.start_save(request, window, cx);
        }
    }

    fn start_save(&mut self, request: SaveRequest, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text(cx);
        self.saving_text = Some(text.clone());
        let receiver = match self.panel.upgrade() {
            Some(panel) => {
                let path = self.path.clone();
                panel.update(cx, |panel, cx| panel.save_text(path, text, request.format, request.expected, cx))
            }
            None => {
                // The tab went away while the editor was open: there is no session to send it on.
                let (answer, receiver) = async_channel::bounded(1);
                let _ = answer.try_send(Err(SftpError::Disconnected("该标签页已关闭".into())));
                receiver
            }
        };
        self.save_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = receiver.recv().await.unwrap_or_else(|_| Err(SftpError::Disconnected(String::new())));
            this.update_in(cx, |this, window, cx| this.on_save_result(result, window, cx)).ok();
        }));
        cx.notify();
    }

    fn on_save_result(
        &mut self,
        result: Result<shellmate_sftp::SaveOutcome, SftpError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.model.save_done(result, &clock_now()) {
            SaveEnd::Saved { close } => {
                self.saving_text = None;
                cx.emit(EditorEvent::Saved(self.dir()));
                if close {
                    self.close(window, cx);
                }
            }
            SaveEnd::Conflict => self.ask_about_conflict(window, cx),
            // The panel has been told about a lost session already; the status line has the reason.
            SaveEnd::Failed { .. } => self.saving_text = None,
            SaveEnd::Ignored => {}
        }
        cx.notify();
    }

    // ---- the conflict dialog ----------------------------------------------------------------------

    fn ask_about_conflict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(info) = self.model.conflict().cloned() else { return };
        let mine = self.saving_text.take().unwrap_or_else(|| self.text(cx));
        let snippet = info.remote_text.as_deref().map(|remote| diff::snippet(&mine, remote)).unwrap_or_default();
        let remote_time = info.remote_mtime.map(clock_hm);
        let editor = cx.entity().downgrade();
        let answer: ConflictAnswer = std::rc::Rc::new(move |choice, window, cx| {
            let editor = editor.clone();
            // The dialog is closing as it answers: what the answer sets going waits until it is gone.
            window.defer(cx, move |window, cx| {
                editor.update(cx, |editor, cx| editor.on_conflict_choice(choice, window, cx)).ok();
            });
        });
        self.dialog_open = true;
        ConflictDialog::open(self.name.clone(), remote_time, snippet, info.remote_text.is_some(), answer, window, cx);
    }

    fn on_conflict_choice(&mut self, choice: ConflictChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog_open = false;
        match choice {
            ConflictChoice::Cancel => self.model.cancel_conflict(),
            ConflictChoice::LoadRemote => {
                if let Some(text) = self.model.load_remote() {
                    self.editor.update(cx, |state, cx| state.set_value(text, window, cx));
                }
            }
            ConflictChoice::Overwrite => {
                if let Some(request) = self.model.overwrite() {
                    self.start_save(request, window, cx);
                }
            }
        }
        self.focus(window, cx);
        cx.notify();
    }

    // ---- closing -----------------------------------------------------------------------------------

    /// 关闭 / Esc: asks first when something is not uploaded.
    pub fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog_open {
            return;
        }
        if self.model.close_needs_confirmation() {
            self.ask_about_unsaved(window, cx);
        } else {
            self.close(window, cx);
        }
    }

    fn ask_about_unsaved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.entity().downgrade();
        let answer: UnsavedAnswer = std::rc::Rc::new(move |choice, window, cx| {
            let editor = editor.clone();
            window.defer(cx, move |window, cx| {
                editor.update(cx, |editor, cx| editor.on_unsaved_choice(choice, window, cx)).ok();
            });
        });
        self.dialog_open = true;
        UnsavedDialog::open(self.name.clone(), answer, window, cx);
    }

    fn on_unsaved_choice(&mut self, choice: UnsavedChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog_open = false;
        match choice {
            UnsavedChoice::Cancel => self.focus(window, cx),
            UnsavedChoice::Discard => self.close(window, cx),
            // Closes when the upload works; a conflict or a failure keeps the editor open.
            UnsavedChoice::SaveAndClose => match self.model.begin_save_and_close() {
                Some(request) => self.start_save(request, window, cx),
                None => self.focus(window, cx),
            },
        }
        cx.notify();
    }

    /// Closes the modal.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        cx.emit(EditorEvent::Closed);
    }

    fn on_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        self.request_close(window, cx);
    }

    // ---- demo hooks ----------------------------------------------------------------------------------

    /// Debug review hook: types `text` at the end of the buffer, as if the user had.
    #[cfg(debug_assertions)]
    pub fn demo_type(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_string();
        self.editor.update(cx, |state, cx| {
            let end = state.text().len();
            state.set_cursor_position(state.text().offset_to_position(end), window, cx);
            state.insert(text, window, cx);
        });
        self.on_edited(cx);
    }

    /// Debug review hook: a button of the dialog that is open over the editor (the dialog is closed
    /// first, as its own button does): `load`, `overwrite`, `cancel` for a conflict;
    /// `unsaved-save`, `unsaved-discard`, `unsaved-cancel` for the question about closing.
    #[cfg(debug_assertions)]
    pub fn demo_choose(&mut self, choice: &str, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        match choice {
            "load" => self.on_conflict_choice(ConflictChoice::LoadRemote, window, cx),
            "overwrite" => self.on_conflict_choice(ConflictChoice::Overwrite, window, cx),
            "cancel" => self.on_conflict_choice(ConflictChoice::Cancel, window, cx),
            "unsaved-save" => self.on_unsaved_choice(UnsavedChoice::SaveAndClose, window, cx),
            "unsaved-discard" => self.on_unsaved_choice(UnsavedChoice::Discard, window, cx),
            "unsaved-cancel" => self.on_unsaved_choice(UnsavedChoice::Cancel, window, cx),
            other => log::warn!("unknown choice {other:?}"),
        }
    }

    /// Debug review hook: ⌘S.
    #[cfg(debug_assertions)]
    pub fn demo_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx);
    }

    /// Debug review hook: 关闭.
    #[cfg(debug_assertions)]
    pub fn demo_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request_close(window, cx);
    }

    /// Debug review hook: types `count` single characters one after the other and logs what each
    /// took (the buffer update and the change handling).
    #[cfg(debug_assertions)]
    pub fn demo_type_timed(&mut self, count: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut took = Vec::with_capacity(count);
        for n in 0..count {
            let began = std::time::Instant::now();
            self.demo_type(if n % 20 == 19 { "\n" } else { "x" }, window, cx);
            took.push(began.elapsed().as_micros());
        }
        took.sort_unstable();
        log::info!(
            "编辑器：{count} 次输入，每次 中位 {} µs / 最慢 {} µs",
            took[took.len() / 2],
            took.last().copied().unwrap_or(0)
        );
    }

    fn lines(&self, cx: &App) -> usize {
        let text = self.editor.read(cx).text();
        let len = text.len();
        count_lines(len, text.lines_len(), len > 0 && text.char_at(len - 1) == Some('\n'))
    }

    fn tone_color(tone: Tone, p: &Palette) -> gpui::Hsla {
        match tone {
            Tone::Muted => p.tx2,
            Tone::Warn => p.warn,
            Tone::Accent => p.acc,
            Tone::Ok => p.ok,
            Tone::Err => p.err,
        }
    }
}

impl Render for FileEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let (status, tone) = self.model.status();
        let saving = self.model.is_saving();
        let footer = self.model.footer(self.lines(cx));
        // The prototype's modal is 1200 × 800; a smaller window gets a smaller one.
        let height = (window.viewport_size().height - px(100.)).clamp(px(320.), px(800.));
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .h(px(34.))
                .px(px(if primary { 16. } else { 14. }))
                .rounded(px(11.))
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .bg(if primary { p.acc } else { p.card2 })
                .text_color(if primary { white() } else { p.tx })
                .cursor_pointer()
                .hover(|d| d.opacity(0.88))
                .child(label)
        };
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_escape))
            .flex()
            .flex_col()
            .w_full()
            .h(height)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(12.))
                    .pl(px(22.))
                    .pr(px(18.))
                    .pt(px(16.))
                    .pb(px(14.))
                    .border_b_1()
                    .border_color(p.line)
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_baseline()
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(17.)).line_height(px(20.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(p.tx)
                                    .child(self.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .px(px(8.))
                                    .py(px(2.))
                                    .rounded(px(7.))
                                    .bg(p.card3)
                                    .text_size(px(11.5)).line_height(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.tx2)
                                    .child(self.lang.badge),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(MONO_FONT)
                                    .text_size(px(12.)).line_height(px(16.))
                                    .text_color(p.tx2)
                                    .child(self.place.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(420.))
                            .truncate()
                            .text_size(px(12.5)).line_height(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Self::tone_color(tone, &p))
                            .child(status),
                    )
                    .child(
                        button("editor-close", "关闭", false)
                            .on_click(cx.listener(|this, _, window, cx| this.request_close(window, cx))),
                    )
                    .child(
                        button("editor-save", "保存并上传 ⌘S", true)
                            .when(saving, |d| d.opacity(0.5).cursor_not_allowed())
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(Editor::new(&self.editor).appearance(false).bordered(false).h_full().font_family(MONO_FONT).text_size(px(13.)).line_height(px(21.)).pt(px(16.)).pl(px(18.))))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(16.))
                    .px(px(22.))
                    .h(px(38.))
                    .border_t_1()
                    .border_color(p.line)
                    .text_size(px(12.)).line_height(px(15.))
                    .text_color(p.tx2)
                    .child(footer)
                    .child("保存时经 SFTP 原子写回（临时文件 + rename），保留原权限与属主"),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::Keystroke;

    use super::*;

    #[test]
    fn save_is_command_s_in_the_editor_context() {
        assert!(Keystroke::parse("cmd-s").is_ok());
        assert_eq!(CONTEXT, "Editor");
    }

    #[test]
    fn the_buffer_is_saved_with_the_line_ends_it_is_compared_with() {
        assert_eq!(normalize("a\r\nb\rc\n"), "a\nb\nc\n");
        assert_eq!(normalize("no breaks"), "no breaks");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn lines_are_counted_without_the_empty_one_after_a_final_newline() {
        assert_eq!(count_lines(0, 1, false), 0, "an empty file has no lines");
        assert_eq!(count_lines(5, 1, false), 1, "one line without a newline");
        assert_eq!(count_lines(6, 2, true), 1, "one line with its newline");
        assert_eq!(count_lines(11, 3, false), 3);
        assert_eq!(count_lines(12, 4, true), 3);
        assert_eq!(count_lines(1, 2, true), 1, "just a newline");
    }
}
