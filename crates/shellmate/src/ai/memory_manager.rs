//! Modal memory browser/editor using the app's host-editor shell and field surfaces.
use super::memory_model::{self, Focus, Model};
use crate::{
    dialogs::{self, DialogBody, Tone},
    theme::{ActivePalette, MONO_FONT},
    ui,
};
use gpui::{
    component::{
        button::Button,
        input::{InputState, Textarea, TextareaState},
    },
    prelude::*,
    *,
};
use shellmate_store::SharedStore;
use std::{path::PathBuf, time::SystemTime};

pub struct MemoryManager {
    db: SharedStore,
    model: Model,
    path: Option<String>,
    editor: Entity<TextareaState>,
    filename: Entity<InputState>,
    creating: bool,
    error: Option<String>,
    saved: Option<String>,
    _subscription: Subscription,
}
impl MemoryManager {
    pub fn open(
        root: PathBuf,
        db: SharedStore,
        current: Option<&str>,
        focus: Focus,
        w: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>, String> {
        let model = Model::new(root, &db.borrow(), current, &focus)?;
        let body = cx.new(|cx| {
            let editor = cx.new(|cx| TextareaState::new(w, cx).auto_grow(8, 8).soft_wrap(true));
            let subscription = cx.subscribe(
                &editor,
                |s: &mut Self, _, _: &gpui::component::input::InputEvent, cx| {
                    s.saved = None;
                    cx.notify();
                },
            );
            Self {
                db,
                model,
                path: None,
                editor,
                filename: cx.new(|cx| InputState::new(w, cx).placeholder("deploy.md")),
                creating: false,
                error: None,
                saved: None,
                _subscription: subscription,
            }
        });
        if let Some(path) = focus.path {
            body.update(cx, |s, cx| s.select_file(path, w, cx));
        }
        let first = std::cell::Cell::new(true);
        let modal_body = body.clone();
        dialogs::open_modal(w, cx, move |dialog, w, cx| {
            if first.replace(false) {
                modal_body.update(cx, |s, cx| s.focus(w, cx));
            }
            let close = modal_body.clone();
            let height = px(dialogs::shell_height(
                f32::from(w.viewport_size().height),
                840.,
                30.,
            ));
            dialog
                .w(px(660.).min(w.viewport_size().width - px(40.)))
                .h(height)
                .margin_top(((w.viewport_size().height - height) / 2.).max(px(30.)))
                .rounded(px(26.))
                .p_0()
                .gap_0()
                .border_0()
                .bg(cx.palette().win)
                .shadow(dialogs::modal_shadow(None))
                .close_button(false)
                .overlay_closable(false)
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, w, cx| {
                    close.update(cx, |s, cx| s.cancel(w, cx));
                    true
                })
                .map(|d| dialogs::fixed_shell(d, modal_body.clone(), height))
        });
        Ok(body)
    }
    fn select_file(&mut self, path: String, w: &mut Window, cx: &mut Context<Self>) {
        match self.model.read(&path) {
            Ok(text) => {
                self.path = Some(path);
                self.creating = false;
                self.error = None;
                self.saved = None;
                self.editor.update(cx, |e, cx| e.set_value(text, w, cx));
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }
    fn scope(&mut self, index: usize, cx: &mut Context<Self>) {
        self.model.selected = index;
        self.path = None;
        self.creating = false;
        self.saved = None;
        self.error = self.model.reload().err();
        cx.notify();
    }
    fn save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let result = if self.creating {
            self.model
                .create(&self.filename.read(cx).value(), &text)
                .map(|path| {
                    self.path = Some(path);
                    self.creating = false;
                    "记忆已创建".into()
                })
        } else {
            self.path
                .as_ref()
                .ok_or_else(|| "请先选择文件".into())
                .and_then(|path| self.model.save(path, &text))
        };
        match result {
            Ok(message) => {
                if let Some(path) = self.path.clone() {
                    self.select_file(path, w, cx);
                }
                self.error = self.model.reload().err();
                self.saved = Some(message);
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }
    fn new_file(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.path = None;
        self.creating = true;
        self.error = None;
        self.saved = None;
        self.filename.update(cx, |e, cx| e.set_value("", w, cx));
        self.editor
            .update(cx, |e, cx| e.set_value(memory_model::TEMPLATE, w, cx));
        self.filename.focus_handle(cx).focus(w, cx);
        cx.notify();
    }
    fn delete(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let this = cx.weak_entity();
        dialogs::confirm::ConfirmDialog::open(
            "删除这份记忆？",
            format!("将删除 {path}，无法撤销。"),
            "删除",
            true,
            move |_, cx| {
                this.update(cx, |s, cx| {
                    s.model.delete(&path)?;
                    s.path = None;
                    s.saved = None;
                    s.model.reload()?;
                    cx.notify();
                    Ok(())
                })
                .map_err(|e| e.to_string())?
            },
            w,
            cx,
        );
    }
    fn file_row(
        &self,
        path: String,
        title: String,
        detail: String,
        kind: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = cx.palette();
        let active = self.path.as_ref() == Some(&path);
        div()
            .id(SharedString::from(format!("memory-file-{path}")))
            .flex_none()
            .min_h(px(56.))
            // The card's 18 px inset leaves a 6 px gutter around this neutral selection.
            .mx(px(-12.))
            .my(px(2.))
            .px(px(12.))
            .py(px(12.))
            .rounded(px(12.))
            .min_w_0()
            .when(active, |d| d.bg(p.card2))
            .cursor_pointer()
            .when(!active, |d| d.hover(|d| d.bg(p.card2.opacity(0.55))))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_ellipsis()
                            .text_size(px(13.5))
                            .line_height(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .children(kind.map(|kind| neutral_chip(kind, p))),
            )
            .when(!detail.is_empty(), |d| {
                d.child(
                    div()
                        .mt(px(4.))
                        .whitespace_normal()
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .text_color(p.tx2)
                        .child(detail),
                )
            })
            .on_click(cx.listener(move |s, _, w, cx| s.select_file(path.clone(), w, cx)))
    }
}
impl DialogBody for MemoryManager {
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        false
    }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
    fn focus(&self, w: &mut Window, cx: &mut Context<Self>) {
        if self.path.is_some() {
            self.editor.focus_handle(cx).focus(w, cx);
        }
    }
}
impl Render for MemoryManager {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let enabled = self
            .db
            .borrow()
            .ai_settings()
            .unwrap_or_default()
            .auto_memory;
        let header = dialogs::header(
            "memory-close",
            div()
                .flex()
                .items_center()
                .gap(px(14.))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(20.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("记忆管理"),
                )
                .child(div().text_size(px(12.)).text_color(p.tx2).child("自动记忆"))
                .child(
                    ui::Toggle::new("memory-auto")
                        .checked(enabled)
                        .color(p.ok)
                        .on_click(cx.listener(|s, on: &bool, _, cx| {
                            s.error =
                                s.db.borrow_mut()
                                    .set_auto_memory(*on)
                                    .err()
                                    .map(|e| e.to_string());
                            cx.notify();
                        })),
                ),
            &p,
            cx.listener(|s, _, w, cx| s.dismiss(w, cx)),
        );
        let mut scopes = div().flex().flex_col().gap(px(4.));
        for (index, scope) in self.model.scopes.iter().enumerate() {
            scopes = scopes.child(
                div()
                    .id(("memory-scope", index))
                    .px(px(12.))
                    .h(px(38.))
                    .flex()
                    .items_center()
                    .rounded(px(12.))
                    .cursor_pointer()
                    .text_size(px(13.5))
                    .line_height(px(16.))
                    .text_color(if index == self.model.selected {
                        p.tx
                    } else {
                        p.tx2
                    })
                    .when(index == self.model.selected, |d| {
                        d.bg(p.card).font_weight(FontWeight::SEMIBOLD)
                    })
                    .child(ui::ellipsis(
                        ("memory-scope-name", index),
                        scope.label.clone(),
                    ))
                    .on_click(cx.listener(move |s, _, _, cx| s.scope(index, cx))),
            );
        }
        let prefix = self.model.prefix();
        let mut files = div()
            .flex()
            .flex_col()
            .gap(px(7.))
            .child(section("说明", &p))
            .child(grouped_card(&p).child(self.file_row(
                format!("{prefix}/AGENTS.md"),
                "AGENTS.md".into(),
                "随 AI 请求发送的说明".into(),
                None,
                cx,
            )))
            .child(
                div()
                    .mt(px(8.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section("记忆", &p))
                    .child(
                        Button::new("memory-new")
                            .label("新建记忆")
                            .on_click(cx.listener(|s, _, w, cx| s.new_file(w, cx))),
                    ),
            );
        let mut memories = grouped_card(&p);
        let mut has_index = false;
        for file in &self.model.files {
            if file.path == format!("{prefix}/AGENTS.md") {
                continue;
            }
            let index = file.path == format!("{prefix}/MEMORY.md");
            has_index |= index;
            let modified = memory_model::relative_time(file.modified, SystemTime::now());
            let description = if file.description.is_empty() {
                modified
            } else {
                format!("{} · {modified}", file.description)
            };
            memories = memories.child(
                self.file_row(
                    file.path.clone(),
                    if index {
                        "MEMORY.md".into()
                    } else {
                        file.name.clone()
                    },
                    description,
                    if index {
                        Some("索引")
                    } else {
                        type_label(&file.kind)
                    }
                    .map(str::to_owned),
                    cx,
                ),
            );
        }
        if !has_index {
            memories = memories.child(self.file_row(
                format!("{prefix}/MEMORY.md"),
                "MEMORY.md".into(),
                "尚未创建".into(),
                Some("索引".into()),
                cx,
            ));
        }
        files = files.child(memories);
        let mut right = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                dialogs::scroll_body("memory-files", files)
                    .surface(p.win)
                    .bottom_padding(px(8.)),
            );
        if self.path.is_some() || self.creating {
            let title = self
                .path
                .as_deref()
                .and_then(|p| p.rsplit('/').next())
                .unwrap_or("新建记忆");
            let mut editor = div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(7.))
                .child(section(title, &p));
            if self.creating {
                editor = editor.child(ui::field_input(&self.filename, ui::Surface::Window, true));
            }
            editor = editor
                .child(
                    Textarea::new(&self.editor)
                        .bordered(false)
                        .h(px(190.))
                        .font_family(MONO_FONT)
                        .text_size(px(12.))
                        .bg(p.card)
                        .rounded(px(11.)),
                )
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            dialogs::button("memory-delete", "删除", Tone::Plain, true)
                                .text_color(p.err)
                                .when(self.creating, |b| b.invisible())
                                .on_click(cx.listener(|s, _, w, cx| s.delete(w, cx))),
                        )
                        .child(
                            dialogs::button("memory-save", "保存", Tone::Primary, true)
                                .on_click(cx.listener(|s, _, w, cx| s.save(w, cx))),
                        ),
                );
            right = right.child(editor);
        }
        right = right
            .children(
                self.error
                    .clone()
                    .map(|e| dialogs::notice(dialogs::Notice::Error, e, &p)),
            )
            .children(self.saved.clone().map(|s| {
                div()
                    .text_size(px(11.))
                    .text_color(p.tx2)
                    .whitespace_normal()
                    .child(s)
            }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .overflow_hidden()
            .bg(p.win)
            .text_color(p.tx)
            .child(div().flex_none().px(px(24.)).py(px(18.)).child(header))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(24.))
                    .pb(px(20.))
                    .flex()
                    .gap(px(16.))
                    .child(
                        dialogs::scroll_body("memory-scopes", scopes)
                            .flex_none()
                            .w(px(128.)),
                    )
                    .child(right),
            )
    }
}
fn section(text: &str, p: &crate::theme::Palette) -> Div {
    div()
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.tx)
        .child(text.to_owned())
}

fn grouped_card(p: &crate::theme::Palette) -> Div {
    div()
        .flex_none()
        .px(px(18.))
        .py(px(4.))
        .rounded(px(18.))
        .bg(p.card)
        .flex()
        .flex_col()
}
fn neutral_chip(label: impl Into<SharedString>, p: &crate::theme::Palette) -> Div {
    div()
        .flex_none()
        .px(px(5.))
        .py(px(2.))
        .rounded(px(5.))
        .bg(p.card3)
        .text_size(px(10.))
        .text_color(p.tx2)
        .child(label.into())
}
fn type_label(kind: &str) -> Option<&'static str> {
    match kind {
        "user" => Some("用户"),
        "feedback" => Some("反馈"),
        "project" => Some("项目"),
        "reference" => Some("参考"),
        _ => None,
    }
}
#[cfg(test)]
mod style_tests {
    use super::type_label;
    #[test]
    fn memory_type_labels_are_chinese_and_unknown_types_have_no_chip() {
        for (kind, label) in [
            ("user", "用户"),
            ("feedback", "反馈"),
            ("project", "项目"),
            ("reference", "参考"),
        ] {
            assert_eq!(type_label(kind), Some(label));
        }
        for kind in ["", "unknown", "User", "项目"] {
            assert_eq!(type_label(kind), None);
        }
    }
}
