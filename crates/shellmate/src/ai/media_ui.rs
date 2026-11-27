//! Composer-owned media, imported away from the UI thread and never into another draft.
use super::*;
use crate::dialogs::DialogBody as _;
use crate::icons::AppIcon;
use gpui::component::button::ButtonVariants as _;
use shellmate_ai::Attachment;
use std::{collections::BTreeMap, path::PathBuf};
#[derive(Default)]
pub(super) struct Draft {
    pub images: Vec<Attachment>,
    pub loading: bool,
    pub error: Option<String>,
}
pub(super) struct Drafts {
    entries: BTreeMap<String, Draft>,
    live: Arc<Mutex<bool>>,
}
impl Default for Drafts {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            live: Arc::new(Mutex::new(true)),
        }
    }
}
impl std::ops::Deref for Drafts {
    type Target = BTreeMap<String, Draft>;
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}
impl std::ops::DerefMut for Drafts {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entries
    }
}
impl Drafts {
    pub fn retire(&self) {
        *self.live.lock().unwrap() = false;
    }
}
impl Drop for Drafts {
    fn drop(&mut self) {
        self.retire();
    }
}
pub(super) enum Source {
    File(PathBuf),
    Bytes(Vec<u8>),
}
impl Drawer {
    pub(super) fn media_key(&self) -> String {
        self.state
            .child_page
            .selected
            .as_ref()
            .or(self.state.message_target.as_ref())
            .cloned()
            .unwrap_or_default()
    }
    pub(super) fn draft_images(&self) -> Vec<Attachment> {
        self.state
            .media
            .get(&self.media_key())
            .map(|d| d.images.clone())
            .unwrap_or_default()
    }
    pub(super) fn take_images(&mut self) -> Vec<Attachment> {
        let key = self.media_key();
        let draft = self.state.media.entry(key).or_default();
        draft.error = None;
        std::mem::take(&mut draft.images)
    }
    pub(super) fn validate_draft_images(&mut self, cx: &mut Context<Self>) -> bool {
        if self.media_loading() {
            return false;
        }
        if self.draft_images().is_empty() {
            return true;
        }
        let settings = self.db.borrow().ai_settings().ok();
        let config = self.state.run_config.as_ref();
        let provider = config
            .map(|c| &c.provider)
            .or_else(|| settings.as_ref().map(|s| &s.provider));
        let model = self
            .state
            .child_page
            .selected
            .as_ref()
            .or(self.state.message_target.as_ref())
            .and_then(|id| self.state.subagents.get(id))
            .map(|v| v.model.as_str())
            .or_else(|| config.map(|c| c.model.as_str()))
            .or_else(|| settings.as_ref().map(|s| s.model.as_str()));
        if let Some((p, m)) = provider.zip(model)
            && !shellmate_ai::media::supports_images(p, m)
        {
            let key = self.media_key();
            self.state.media.entry(key).or_default().error =
                Some("当前模型不支持图片，请先选择支持视觉的模型".into());
            cx.notify();
            return false;
        }
        true
    }
    pub(super) fn media_loading(&self) -> bool {
        self.state
            .media
            .get(&self.media_key())
            .is_some_and(|d| d.loading)
    }
    pub(super) fn restore_images(&mut self, images: Vec<Attachment>) {
        let key = self.media_key();
        self.state.media.entry(key).or_default().images = images;
    }
    fn import_images(&mut self, sources: Vec<Source>, cx: &mut Context<Self>) {
        if sources.is_empty() {
            return;
        }
        let key = self.media_key();
        let draft = self.state.media.entry(key.clone()).or_default();
        if draft.loading {
            draft.error = Some("正在处理图片，请稍后再次添加".into());
            cx.notify();
            return;
        }
        if sources.len() + draft.images.len() > shellmate_ai::media::MAX_IMAGES {
            draft.error = Some("每条消息最多添加 8 张图片".into());
            cx.notify();
            return;
        }
        draft.loading = true;
        draft.error = None;
        let root = self.data_root.clone();
        let conversation = self.state.id.clone();
        let guard = cx.entity().downgrade();
        let live = self.state.media.live.clone();
        let retained = self.tasks.as_ref().map(|t| t.retained());
        let job = cx.background_executor().spawn({
            let conversation = conversation.clone();
            async move {
                sources
                    .into_iter()
                    .map(|source| {
                        let prepared = match source {
                            Source::File(path) => shellmate_ai::media::prepare_file(&path),
                            Source::Bytes(bytes) => {
                                shellmate_ai::media::prepare("截图.png", &bytes)
                            }
                        }?;
                        let active = live.lock().unwrap();
                        if !*active {
                            return Err("会话已关闭".into());
                        }
                        let write = || prepared.persist(&root, &conversation);
                        match &retained {
                            Some(r) => r
                                .while_retained(write)
                                .unwrap_or_else(|| Err("会话已关闭".into())),
                            None => write(),
                        }
                    })
                    .collect::<Vec<_>>()
            }
        });
        cx.spawn(async move |_, cx| {
            let results = job.await;
            guard
                .update(cx, |s, cx| {
                    if s.state.id != conversation {
                        return;
                    }
                    let draft = s.state.media.entry(key).or_default();
                    draft.loading = false;
                    for result in results {
                        match result {
                            Ok(a) => draft.images.push(a),
                            Err(e) => draft.error = Some(e),
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
        cx.notify();
    }
    pub(super) fn paste_images(
        &mut self,
        clipboard: &ClipboardItem,
        cx: &mut Context<Self>,
    ) -> bool {
        let sources = clipboard
            .entries
            .iter()
            .flat_map(|entry| match entry {
                ClipboardEntry::Image(image) => vec![Source::Bytes(image.bytes.clone())],
                ClipboardEntry::ExternalPaths(paths) => {
                    paths.paths().iter().cloned().map(Source::File).collect()
                }
                _ => vec![],
            })
            .collect::<Vec<_>>();
        if sources.is_empty() {
            return false;
        }
        self.import_images(sources, cx);
        true
    }
    pub(super) fn media_textarea(&self, cx: &Context<Self>) -> Textarea {
        let this = cx.entity().downgrade();
        Textarea::new(&self.input).on_paste(move |item, _, cx| {
            this.update(cx, |s, cx| s.paste_images(item, cx))
                .unwrap_or(false)
        })
    }
    pub(super) fn drop_images(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        self.import_images(
            paths.paths().iter().cloned().map(Source::File).collect(),
            cx,
        );
    }
    pub(super) fn image_picker(&self, cx: &mut Context<Self>) -> Button {
        Button::new("image-picker")
            .ghost()
            .size(px(30.))
            .p_0()
            .rounded(px(10.))
            .icon(AppIcon::Plus.at(15.))
            .text_color(cx.palette().tx2)
            .tooltip("添加图片")
            .disabled(self.media_loading())
            .on_click(cx.listener(|s, _, _, cx| {
                let conversation = s.state.id.clone();
                let key = s.media_key();
                let picked = cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: true,
                    prompt: Some("添加图片".into()),
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = picked.await {
                        this.update(cx, |s, cx| {
                            if s.state.id == conversation && s.media_key() == key {
                                s.import_images(paths.into_iter().map(Source::File).collect(), cx);
                            }
                        })
                        .ok();
                    }
                })
                .detach();
            }))
    }
    pub(super) fn media_composer(&self, cx: &mut Context<Self>) -> Option<Div> {
        let key = self.media_key();
        let draft = self.state.media.get(&key)?;
        if draft.images.is_empty() && !draft.loading && draft.error.is_none() {
            return None;
        }
        let p = cx.palette().clone();
        let mut row = div().flex().flex_wrap().gap(px(6.));
        for (index, a) in draft.images.iter().enumerate() {
            let key = key.clone();
            let a = a.clone();
            row = row.child(
                div()
                    .relative()
                    .child(thumbnail(
                        format!("draft-image-{index}"),
                        a,
                        self.data_root.clone(),
                        true,
                        cx,
                    ))
                    .child(
                        Button::new(SharedString::from(format!("remove-image-{index}")))
                            .ghost()
                            .label("×")
                            .size(px(20.))
                            .p_0()
                            .rounded(px(8.))
                            .bg(p.card)
                            .border_1()
                            .border_color(p.line)
                            .text_color(p.tx2)
                            .absolute()
                            .top(px(4.))
                            .right(px(4.))
                            .tooltip("移除图片")
                            .on_mouse_down(MouseButton::Left, |_, w, cx| {
                                w.prevent_default();
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(move |s, _, _, cx| {
                                cx.stop_propagation();
                                let d = s.state.media.entry(key.clone()).or_default();
                                if index < d.images.len() {
                                    d.images.remove(index);
                                }
                                cx.notify();
                            })),
                    ),
            );
        }
        let mut content = div().flex().flex_col().gap(px(4.)).child(row);
        if draft.loading {
            content = content.child(
                div()
                    .text_size(px(11.))
                    .text_color(p.tx3)
                    .child("正在添加图片…"),
            );
        }
        if let Some(e) = &draft.error {
            content = content.child(div().text_size(px(11.)).text_color(p.err).child(e.clone()));
        }
        Some(content)
    }
    pub(super) fn message_images(
        &self,
        scope: &str,
        images: &[Attachment],
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        if images.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .children(images.iter().enumerate().map(|(n, a)| {
                    thumbnail(
                        format!("{scope}:image:{n}"),
                        a.clone(),
                        self.data_root.clone(),
                        false,
                        cx,
                    )
                })),
        )
    }
}
fn thumbnail(id: String, a: Attachment, root: PathBuf, small: bool, cx: &App) -> Stateful<Div> {
    let p = cx.palette();
    let path = a.path(&root, true).ok();
    let box_width: f32 = if small { 80. } else { 150. };
    let box_height: f32 = if small { 60. } else { 108. };
    let scale = (box_width / a.width.max(1) as f32).min(box_height / a.height.max(1) as f32);
    let image_width = a.width as f32 * scale;
    let image_height = a.height as f32 * scale;
    div()
        .id(SharedString::from(id))
        .w(px(if small { 80. } else { 150. }))
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(3.))
        .cursor_pointer()
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(12.))
                .bg(p.card2)
                .border_1()
                .border_color(p.line)
                .overflow_hidden()
                .h(px(if small { 60. } else { 108. }))
                .when_some(path, |d, path| {
                    d.child(
                        img(path)
                            .flex_none()
                            .w(px(image_width))
                            .h(px(image_height))
                            .object_fit(ObjectFit::Contain)
                            .with_loading(|| image_status("载入中…"))
                            .with_fallback(|| image_status("图片已不可用")),
                    )
                }),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(p.tx3)
                .truncate()
                .child(a.name.clone()),
        )
        .on_click(move |_, w, cx| {
            let title = a.name.clone();
            let root = root.clone();
            let a = a.clone();
            let body = cx.new(|_| Preview {
                a,
                root,
                error: None,
                copied: false,
            });
            crate::dialogs::open(w, cx, title, 720., body);
        })
}
fn image_status(text: &'static str) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.))
        .child(text)
        .into_any_element()
}
struct Preview {
    a: Attachment,
    root: PathBuf,
    error: Option<String>,
    copied: bool,
}
impl crate::dialogs::DialogBody for Preview {
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        true
    }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}
impl Render for Preview {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let height = (w.viewport_size().height * 0.55).min(px(640.));
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .when_some(self.a.path(&self.root, false).ok(), |d, path| {
                d.child(
                    img(path)
                        .w_full()
                        .h(height)
                        .object_fit(ObjectFit::Contain)
                        .with_loading(|| image_status("载入中…"))
                        .with_fallback(|| image_status("图片已不可用，请重新添加")),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(12.)).text_color(p.tx3).child(format!(
                        "{} × {} · {:.1} MB",
                        self.a.width,
                        self.a.height,
                        self.a.bytes as f64 / (1024. * 1024.)
                    )))
                    .child(
                        Button::new("copy-image")
                            .ghost()
                            .size(px(30.))
                            .p_0()
                            .rounded(px(10.))
                            .icon(if self.copied {
                                AppIcon::Check.at(14.)
                            } else {
                                AppIcon::Copy.at(14.)
                            })
                            .tooltip("复制图片")
                            .on_click(cx.listener(|s, _, _, cx| {
                                let root = s.root.clone();
                                let a = s.a.clone();
                                let format = if a.mime == "image/jpeg" {
                                    gpui::ImageFormat::Jpeg
                                } else {
                                    gpui::ImageFormat::Png
                                };
                                let job = cx
                                    .background_executor()
                                    .spawn(async move { shellmate_ai::media::read(&root, &a) });
                                cx.spawn(async move |this, cx| {
                                    let result = job.await;
                                    this.update(cx, |s, cx| {
                                        match result {
                                            Ok(bytes) => {
                                                cx.write_to_clipboard(ClipboardItem::new_image(
                                                    &gpui::Image::from_bytes(format, bytes),
                                                ));
                                                s.copied = true;
                                            }
                                            Err(e) => s.error = Some(e),
                                        }
                                        cx.notify();
                                    })
                                    .ok();
                                })
                                .detach();
                            })),
                    ),
            );
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(p.err)
                    .child(error.clone()),
            );
        }
        body.child(
            div().flex().justify_end().child(
                crate::dialogs::button("image-close", "关闭", crate::dialogs::Tone::Plain, true)
                    .on_click(cx.listener(|s, _, w, cx| s.dismiss(w, cx))),
            ),
        )
    }
}
