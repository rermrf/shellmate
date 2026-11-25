//! Fixed-shell memory list and detail pages, using the app's existing settings/editor controls.
use super::{
    memory_edit::{self, Content, Navigation, NavigationResult, Page},
    memory_model::{self, Focus, Model},
};
use crate::{
    dialogs::{self, DialogBody, Tone},
    theme::{ActivePalette, MONO_FONT},
    ui,
};
use gpui::{
    component::{
        Sizable, Size,
        button::Button,
        input::{InputEvent, InputState, Textarea, TextareaState},
    },
    prelude::*,
    *,
};
use shellmate_store::SharedStore;
use std::{path::PathBuf, time::SystemTime};

pub struct MemoryManager {
    db: SharedStore,
    model: Model,
    page: Page,
    editor: Entity<TextareaState>,
    filename: Entity<InputState>,
    name: Entity<InputState>,
    description: Entity<InputState>,
    error: Option<String>,
    saved: Option<String>,
    modal: Option<dialogs::modal::Handle>,
    _subscriptions: Vec<Subscription>,
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
        let page = Page::from_focus(&model, focus.path.as_deref())?;
        let body = cx.new(|cx| {
            let editor = cx.new(|cx| TextareaState::new(w, cx).soft_wrap(true));
            let filename = cx.new(|cx| InputState::new(w, cx).placeholder("deploy.md"));
            let name = cx.new(|cx| InputState::new(w, cx).placeholder("部署约定"));
            let description =
                cx.new(|cx| InputState::new(w, cx).placeholder("用一句话概括这条记忆"));
            let mut subscriptions = Vec::new();
            for field in [&filename, &name, &description] {
                subscriptions.push(cx.subscribe(
                    field,
                    |s: &mut Self, _, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change) {
                            s.sync_draft(cx);
                            s.saved = None;
                            cx.notify();
                        }
                    },
                ));
            }
            subscriptions.push(
                cx.subscribe(&editor, |s: &mut Self, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        s.sync_draft(cx);
                        s.saved = None;
                        cx.notify();
                    }
                }),
            );
            Self {
                db,
                model,
                page,
                editor,
                filename,
                name,
                description,
                error: None,
                saved: None,
                modal: None,
                _subscriptions: subscriptions,
            }
        });
        body.update(cx, |s, cx| s.load_fields(w, cx));
        let first = std::cell::Cell::new(true);
        let modal_body = body.clone();
        let handle = dialogs::open_modal(w, cx, move |dialog, w, cx| {
            if first.replace(false) {
                modal_body.update(cx, |s, cx| s.focus(w, cx));
            }
            let close = modal_body.clone();
            let height = px(dialogs::shell_height(
                f32::from(w.viewport_size().height),
                640.,
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
                    close.update(cx, |s, cx| s.navigate(Navigation::Close, w, cx));
                    false
                })
                .map(|d| dialogs::fixed_shell(d, modal_body.clone(), height))
        });
        body.update(cx, |s, _| s.modal = Some(handle));
        Ok(body)
    }
    fn sync_draft(&mut self, cx: &App) {
        let Some(detail) = &mut self.page.detail else {
            return;
        };
        if detail.path.is_none() {
            detail.draft.filename = self.filename.read(cx).value().to_string();
        }
        if let Content::Topic(topic) = &mut detail.draft.content {
            topic.name = self.name.read(cx).value().to_string();
            topic.description = self.description.read(cx).value().to_string();
        }
        *detail.draft.content.body_mut() = self.editor.read(cx).value().to_string();
    }
    fn load_fields(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = &self.page.detail else {
            return;
        };
        let (name, description) = match &detail.draft.content {
            Content::Topic(t) => (t.name.clone(), t.description.clone()),
            _ => (String::new(), String::new()),
        };
        let text = detail.draft.content.body().to_owned();
        let filename = detail.draft.filename.clone();
        let placeholder = if matches!(detail.draft.content, Content::Instructions(_)) {
            memory_edit::AGENTS_PLACEHOLDER
        } else {
            ""
        };
        self.filename
            .update(cx, |e, cx| e.set_value(filename, w, cx));
        self.name.update(cx, |e, cx| e.set_value(name, w, cx));
        self.description
            .update(cx, |e, cx| e.set_value(description, w, cx));
        self.editor.update(cx, |e, cx| {
            e.set_placeholder(placeholder, w, cx);
            e.set_value(text, w, cx);
        });
    }
    fn select_file(&mut self, path: &str, w: &mut Window, cx: &mut Context<Self>) {
        match self.page.open(&self.model, path) {
            Ok(()) => {
                self.error = None;
                self.saved = None;
                self.load_fields(w, cx);
                self.focus(w, cx);
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }
    pub(crate) fn new_file(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.page.create();
        self.error = None;
        self.saved = None;
        self.load_fields(w, cx);
        self.filename.focus_handle(cx).focus(w, cx);
        cx.notify();
    }
    fn navigate(&mut self, action: Navigation, w: &mut Window, cx: &mut Context<Self>) {
        self.sync_draft(cx);
        match self.page.navigate(&mut self.model, action, false) {
            Ok(NavigationResult::NeedsConfirmation) => {
                let this = cx.weak_entity();
                dialogs::confirm::ConfirmDialog::open(
                    "放弃未保存的修改？",
                    "修改尚未保存，离开后将丢失。",
                    "放弃修改",
                    true,
                    move |w, cx| {
                        // The confirm dialog closes itself first; then close/switch the underlying page.
                        let this = this.clone();
                        w.defer(cx, move |w, cx| {
                            this.update(cx, |s, cx| {
                                let result = s.page.navigate(&mut s.model, action, true);
                                s.finish_navigation(result, w, cx);
                            })
                            .ok();
                        });
                        Ok(())
                    },
                    w,
                    cx,
                );
            }
            result => self.finish_navigation(result, w, cx),
        }
    }
    fn finish_navigation(
        &mut self,
        result: Result<NavigationResult, String>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(NavigationResult::Close) => {
                if let Some(handle) = &self.modal {
                    handle.close(w, cx);
                }
            }
            Ok(_) => {
                self.error = None;
                self.saved = None;
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }
    fn save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.sync_draft(cx);
        if !self.page.dirty() {
            return;
        }
        match self.page.save(&mut self.model) {
            Ok(message) => {
                self.error = None;
                self.load_fields(w, cx);
                self.saved = Some(message.split_once('\n').map_or_else(
                    || "已保存".into(),
                    |(_, warning)| format!("已保存\n{warning}"),
                ));
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }
    fn delete(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = self.page.detail.as_ref().filter(|d| d.exists) else {
            return;
        };
        let path = detail.path.as_ref().unwrap().clone();
        let this = cx.weak_entity();
        dialogs::confirm::ConfirmDialog::open(
            "删除这份记忆？",
            format!("将删除 {path}，无法撤销。"),
            "删除",
            true,
            move |_, cx| {
                this.update(cx, |s, cx| {
                    s.page.delete(&mut s.model)?;
                    s.error = None;
                    s.saved = None;
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
        missing: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = cx.palette();
        div()
            .id(SharedString::from(format!("memory-file-{path}")))
            .flex_none()
            .min_h(px(56.))
            .mx(px(-12.))
            .my(px(2.))
            .px(px(12.))
            .py(px(12.))
            .rounded(px(12.))
            .min_w_0()
            .cursor_pointer()
            .hover(|d| d.bg(p.card2.opacity(0.55)))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_ellipsis()
                            .text_size(px(13.5))
                            .line_height(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .mt(px(4.))
                            .whitespace_normal()
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(if missing { p.tx3 } else { p.tx2 })
                            .child(detail),
                    ),
            )
            .children(kind.map(|kind| neutral_chip(kind, p)))
            .child(crate::icons::group_chevron(false).text_color(p.tx2))
            .on_click(cx.listener(move |s, _, w, cx| s.select_file(&path, w, cx)))
    }
    fn list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = cx.palette().clone();
        let p = &palette;
        let prefix = self.model.prefix();
        let missing = !self.model.instructions_exist();
        let description = if missing {
            "未填写 · 点击编写"
        } else if self.model.scopes[self.model.selected].key.is_some() {
            "仅对此主机生效 · AGENTS.md"
        } else {
            "对所有主机生效 · AGENTS.md"
        };
        let mut list = div()
            .flex()
            .flex_col()
            .gap(px(7.))
            .child(section("说明", p))
            .child(grouped_card(p).child(self.file_row(
                format!("{prefix}/AGENTS.md"),
                "AI 说明".into(),
                description.into(),
                None,
                missing,
                cx,
            )))
            .child(
                div()
                    .mt(px(8.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section("记忆", p))
                    .child(
                        Button::new("memory-new")
                            .label("新建记忆")
                            .on_click(cx.listener(|s, _, w, cx| s.new_file(w, cx))),
                    ),
            );
        let mut memories = grouped_card(p);
        for file in self.model.topics() {
            let modified = memory_model::relative_time(file.modified, SystemTime::now());
            let description = if file.description.is_empty() {
                modified
            } else {
                format!("{} · {modified}", file.description)
            };
            memories = memories.child(self.file_row(
                file.path.clone(),
                file.name.clone(),
                description,
                type_label(&file.kind).map(str::to_owned),
                false,
                cx,
            ));
        }
        if let Some(index) = self.model.index() {
            memories = memories.child(self.file_row(
                index.path.clone(),
                "索引".into(),
                "MEMORY.md · AI 自动维护".into(),
                None,
                false,
                cx,
            ));
        }
        if self.model.memories_empty() {
            memories = memories.child(
                div()
                    .py(px(12.))
                    .min_h(px(56.))
                    .whitespace_normal()
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(p.tx3)
                    .child("还没有记忆。对话中让 AI「记住…」，或点「新建记忆」手动添加。"),
            );
        }
        list = list.child(memories);
        dialogs::scroll_body("memory-files", list)
            .surface(p.win)
            .bottom_padding(px(8.))
    }
    fn detail(&self, w: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let detail = self.page.detail.as_ref().unwrap();
        let creating = detail.path.is_none();
        let mut body = div().flex_1().min_h_0().flex().flex_col().gap(px(10.));
        if let Content::Topic(topic) = &detail.draft.content {
            let name = form_field(
                "名称",
                ui::field_input(&self.name, ui::Surface::Window, false),
                p,
            );
            // Creating: 文件名 | 名称 share one row, like the host editor's 名称 | 分组.
            body = if creating {
                body.child(
                    div()
                        .flex_none()
                        .flex()
                        .gap(px(12.))
                        .child(
                            form_field(
                                "文件名",
                                ui::field_input(&self.filename, ui::Surface::Window, true),
                                p,
                            )
                            .flex_1()
                            .min_w_0(),
                        )
                        .child(name.flex_1().min_w_0()),
                )
            } else {
                body.child(name)
            };
            body = body.child(form_field(
                "描述",
                ui::field_input(&self.description, ui::Surface::Window, false),
                p,
            ));
            let this = cx.weak_entity();
            let picker = ui::segmented(
                "memory-type",
                memory_edit::TYPES
                    .iter()
                    .enumerate()
                    .map(|(i, (_, label))| ui::SegmentItem::new(*label, i == topic.kind))
                    .collect(),
                move |index, _, cx| {
                    this.update(cx, |s, cx| {
                        if let Some(detail) = &mut s.page.detail
                            && let Content::Topic(topic) = &mut detail.draft.content
                        {
                            topic.kind = index;
                            s.saved = None;
                            cx.notify();
                        }
                    })
                    .ok();
                },
                cx,
            );
            body = body.child(form_field("类型", picker, p));
        }
        let hint = match detail.draft.content {
            Content::Raw(_) => Some(memory_edit::RAW_HINT),
            Content::Index(_) => Some(memory_edit::INDEX_HINT),
            _ => None,
        };
        body = body.children(hint.map(|hint| {
            div()
                .flex_none()
                .text_size(px(12.))
                .line_height(px(18.))
                .text_color(p.tx3)
                .whitespace_normal()
                .child(hint)
        }));
        let mut content = div().flex_1().min_h_0().flex().flex_col().gap(px(6.));
        if matches!(detail.draft.content, Content::Topic(_)) {
            content = content.child(caption("内容", p));
        }
        body = body.child(
            content.child(
                div().relative().flex_1().min_h_0().child(
                    div().absolute().inset_0().child(
                        ui::field_box(
                            Textarea::new(&self.editor)
                                .xsmall()
                                .appearance(false)
                                .bordered(false),
                            ui::Surface::Window,
                            p,
                            self.editor.focus_handle(cx).is_focused(w),
                        )
                        // Textarea adds 4 px internally at XSmall. Keep the same total
                        // 12 px horizontal inset as field_input; multi-line text also
                        // needs a vertical inset so the first line doesn't touch the edge.
                        .px(px(12.) - Size::XSmall.input_px())
                        .py(px(8.))
                        .h(relative(1.))
                        .w_full()
                        .min_h_0()
                        .max_h(relative(1.))
                        .font_family(MONO_FONT),
                    ),
                ),
            ),
        );
        // Sub-page header: ‹ back chevron beside the title, like a settings sub-page.
        let back = div()
            .id("memory-back")
            .flex_none()
            .size(px(30.))
            .rounded(px(10.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|d| d.bg(p.card))
            .child(
                crate::icons::group_chevron(false)
                    .rotate(gpui::radians(std::f32::consts::PI))
                    .text_color(p.tx2),
            )
            .on_click(cx.listener(|s, _, w, cx| s.navigate(Navigation::Back, w, cx)));
        let heading = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .ml(px(-6.))
            .child(back)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        ui::ellipsis("memory-detail-title", detail.title().to_owned())
                            .text_size(px(16.))
                            .line_height(px(22.))
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        ui::ellipsis("memory-detail-path", self.page.subtitle(&self.model))
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(p.tx3),
                    ),
            );
        let footer = div()
            .h(px(75.))
            .flex_none()
            .border_t_1()
            .border_color(p.line)
            .pt(px(14.))
            .pb(px(20.))
            .flex()
            .items_center()
            .gap(px(10.))
            .when(detail.exists, |d| {
                d.child(
                    dialogs::button("memory-delete", "删除", Tone::Plain, true)
                        .h(px(40.))
                        .rounded(px(13.))
                        .text_color(p.err)
                        .on_click(cx.listener(|s, _, w, cx| s.delete(w, cx))),
                )
            })
            .child(div().flex_1())
            .child(
                dialogs::button("memory-save", "保存", Tone::Primary, detail.dirty())
                    .h(px(40.))
                    .w(px(70.))
                    .rounded(px(13.))
                    .on_click(cx.listener(|s, _, w, cx| s.save(w, cx))),
            );
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(heading)
            .child(body)
            .children(self.saved.clone().map(|text| {
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(p.tx2)
                    .whitespace_normal()
                    .child(text)
            }))
            .children(
                self.error
                    .clone()
                    .map(|e| dialogs::notice(dialogs::Notice::Error, e, p)),
            )
            .child(footer)
    }
}
impl DialogBody for MemoryManager {
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        false
    }
    fn cancel(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Navigation::Close, w, cx);
    }
    fn dismiss(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.navigate(Navigation::Close, w, cx);
    }
    fn focus(&self, w: &mut Window, cx: &mut Context<Self>) {
        if self.page.detail.is_some() {
            self.editor.focus_handle(cx).focus(w, cx);
        }
    }
}
impl Render for MemoryManager {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .on_click(
                        cx.listener(move |s, _, w, cx| s.navigate(Navigation::Scope(index), w, cx)),
                    ),
            );
        }
        let right = if self.page.detail.is_some() {
            self.detail(w, cx).into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(self.list(cx))
                .children(
                    self.error
                        .clone()
                        .map(|e| dialogs::notice(dialogs::Notice::Error, e, &p)),
                )
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .overflow_hidden()
            // No fill here: the rounded dialog already paints `win`; a square
            // fill on this root would cover its corners.
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
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(right),
                    ),
            )
    }
}
fn section(text: &str, p: &crate::theme::Palette) -> Div {
    div()
        .flex_none()
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.tx)
        .child(text.to_owned())
}
fn form_field(label: &str, control: impl IntoElement, p: &crate::theme::Palette) -> Div {
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(caption(label, p))
        .child(control)
}
/// Field caption, same as `dialogs::field` in the host editor (名称 / 分组 / 端口).
fn caption(text: &str, p: &crate::theme::Palette) -> Div {
    div()
        .flex_none()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(p.tx2)
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
    memory_edit::TYPES
        .iter()
        .find_map(|(value, label)| (*value == kind).then_some(*label))
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
