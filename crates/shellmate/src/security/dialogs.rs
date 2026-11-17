use super::{
    model,
    password::{PasswordEvent, PasswordInput},
};
use crate::{
    dialogs::{self, DialogBody, Notice, Tone, actions, button, field, hint, notice, stack},
    theme::ActivePalette,
};
use gpui::{
    component::{Disableable as _, WindowExt as _, checkbox::Checkbox, notification::Notification},
    prelude::*,
    *,
};
use shellmate_store::{
    CredentialStore, SecretStore,
    bundle::{Bundle, Choice, Classification, Contents, PREVIEW_FOOTER, Preview},
};
use std::{path::PathBuf, sync::Arc};
type Completion = Box<dyn FnOnce(bool, &mut Window, &mut App)>;
#[derive(Clone)]
pub enum Purpose {
    Unlock(String),
    Setup,
    Change,
    Export,
    Import(Arc<Vec<u8>>),
    SwitchKeychain,
    EnableTouch,
    DisableTouch,
    Reset,
}
impl Purpose {
    fn title(&self) -> &'static str {
        match self {
            Self::Unlock(_) => "解锁凭据库",
            Self::Setup => "设置主密码",
            Self::Change => "更改主密码",
            Self::Export => "导出加密配置包",
            Self::Import(_) => "打开加密配置包",
            Self::SwitchKeychain => "切换到系统钥匙串",
            Self::EnableTouch => "启用 Touch ID",
            Self::DisableTouch => "关闭 Touch ID",
            Self::Reset => "重置凭据库",
        }
    }
    fn password(&self) -> bool {
        matches!(
            self,
            Self::Unlock(_) | Self::Setup | Self::Change | Self::Export | Self::Import(_)
        )
    }
    fn confirmation(&self) -> bool {
        matches!(self, Self::Setup | Self::Change | Self::Export)
    }
}
pub fn ensure_unlocked(
    manager: Arc<CredentialStore>,
    reason: &str,
    window: &mut Window,
    cx: &mut App,
    done: impl FnOnce(bool, &mut Window, &mut App) + 'static,
) {
    if !manager.is_locked() {
        window.defer(cx, move |w, cx| done(true, w, cx));
    } else {
        PasswordDialog::open(
            manager,
            Purpose::Unlock(reason.into()),
            None,
            Box::new(done),
            window,
            cx,
        );
    }
}
pub fn run_operation(
    manager: Arc<CredentialStore>,
    purpose: Purpose,
    done: impl FnOnce(bool, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    PasswordDialog::open(manager, purpose, None, Box::new(done), window, cx);
}
pub struct PasswordDialog {
    manager: Arc<CredentialStore>,
    purpose: Purpose,
    input: Entity<PasswordInput>,
    current: Entity<PasswordInput>,
    confirm: Entity<PasswordInput>,
    contents: Contents,
    error: Option<String>,
    busy: bool,
    done: Option<Completion>,
    _subscriptions: Vec<Subscription>,
}
enum Output {
    Done,
    Encrypted(Vec<u8>),
    Imported(Bundle),
}
impl PasswordDialog {
    pub fn open(
        manager: Arc<CredentialStore>,
        purpose: Purpose,
        initial: Option<zeroize::Zeroizing<String>>,
        done: Completion,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let body = cx.new(|cx| {
            let input = cx.new(|cx| PasswordInput::new("输入口令", cx));
            if let Some(initial) = initial {
                input.update(cx, |i, cx| i.set(&initial, cx));
            }
            let current = cx.new(|cx| PasswordInput::new("输入当前主密码", cx));
            let confirm = cx.new(|cx| PasswordInput::new("再次输入口令", cx));
            let subscriptions = [&current, &input, &confirm]
                .into_iter()
                .map(|input| {
                    cx.subscribe_in(
                        input,
                        window,
                        |this: &mut Self, _, event: &PasswordEvent, w, cx| match event {
                            PasswordEvent::Changed => cx.notify(),
                            PasswordEvent::Submit => this.submit(w, cx),
                        },
                    )
                })
                .collect();
            Self {
                manager,
                purpose,
                input,
                current,
                confirm,
                contents: Contents::default(),
                error: None,
                busy: false,
                done: Some(done),
                _subscriptions: subscriptions,
            }
        });
        body.update(cx, |s, cx| {
            let manager = s.manager.clone();
            let task = cx
                .background_executor()
                .spawn(async move { manager.refresh_touch_availability() });
            cx.spawn(async move |this, cx| {
                task.await;
                this.update(cx, |_, cx| cx.notify()).ok();
            })
            .detach();
        });
        open_secure(window, cx, 540., Self::preferred_height, body.clone());
        body
    }
    fn preferred_height(&self) -> f32 {
        let base = match self.purpose {
            Purpose::Unlock(_) => 280.,
            Purpose::Export => 548.,
            Purpose::Change => 520.,
            Purpose::Setup => 440.,
            _ => 360.,
        };
        // Unlock reserves one inline error line; the other forms grow only when
        // they need a notice, capped by open_secure at the available viewport.
        base + if self.error.is_some() && !matches!(self.purpose, Purpose::Unlock(_)) {
            44.
        } else {
            0.
        }
    }
    fn forgot_password(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let this = cx.entity().downgrade();
        run_operation(
            self.manager.clone(),
            Purpose::Reset,
            move |ok, w, cx| {
                if ok {
                    this.update(cx, |s, cx| {
                        w.close_dialog(cx);
                        w.refresh();
                        w.push_notification(Notification::info("凭据库已重置，原操作已中止"), cx);
                        s.finish(false, w, cx);
                    })
                    .ok();
                }
            },
            w,
            cx,
        );
    }
    fn finish(&mut self, ok: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| {
            i.take(cx);
        });
        self.confirm.update(cx, |i, cx| {
            i.take(cx);
        });
        self.current.update(cx, |i, cx| {
            i.take(cx);
        });
        if let Some(done) = self.done.take() {
            window.defer(cx, move |w, cx| done(ok, w, cx));
        }
    }
    fn run(&mut self, touch: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if !touch
            && self.purpose.confirmation()
            && let Err(e) = model::validate_password(
                self.input.read(cx).expose(),
                self.confirm.read(cx).expose(),
            )
        {
            self.error = Some(e);
            cx.notify();
            return;
        }
        if matches!(self.purpose, Purpose::Export)
            && self.contents.credentials
            && self.manager.is_locked()
        {
            let this = cx.entity().downgrade();
            ensure_unlocked(
                self.manager.clone(),
                "导出凭据",
                window,
                cx,
                move |ok, w, cx| {
                    this.update(cx, |s, cx| {
                        if ok {
                            s.run(false, w, cx)
                        } else {
                            s.error = Some("已取消导出凭据".into());
                            cx.notify();
                        }
                    })
                    .ok();
                },
            );
            return;
        }
        let current = self.current.update(cx, |i, cx| i.take(cx));
        let password = self.input.update(cx, |i, cx| i.take(cx));
        self.confirm.update(cx, |i, cx| {
            i.take(cx);
        });
        let manager = self.manager.clone();
        let purpose = self.purpose.clone();
        let contents = self.contents;
        self.busy = true;
        self.error = None;
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            if touch {
                manager.unlock_touch_id().map_err(|e| e.to_string())?;
                return Ok(Output::Done);
            }
            let params = if matches!(purpose, Purpose::Setup | Purpose::Change | Purpose::Export) {
                shellmate_vault::calibrate().map_err(|e| e.to_string())?.0
            } else {
                Default::default()
            };
            match purpose {
                Purpose::Unlock(_) => manager
                    .unlock(&password)
                    .map(|_| Output::Done)
                    .map_err(|e| e.to_string()),
                Purpose::Setup => manager
                    .switch_to_master(&password, params)
                    .map(|_| Output::Done)
                    .map_err(|e| e.to_string()),
                Purpose::Change => manager
                    .change_password(&current, &password, params)
                    .map(|_| Output::Done)
                    .map_err(|e| e.0),
                Purpose::Export => manager
                    .export_bundle(contents)?
                    .encrypt(&password, params)
                    .map(Output::Encrypted),
                Purpose::Import(bytes) => Bundle::decrypt(&password, &bytes).map(Output::Imported),
                Purpose::SwitchKeychain => manager
                    .switch_to_keychain()
                    .map(|_| Output::Done)
                    .map_err(|e| e.to_string()),
                Purpose::EnableTouch => manager
                    .set_touch_id(true)
                    .map(|_| Output::Done)
                    .map_err(|e| e.to_string()),
                Purpose::DisableTouch => manager
                    .set_touch_id(false)
                    .map(|_| Output::Done)
                    .map_err(|e| e.to_string()),
                Purpose::Reset => manager
                    .reset()
                    .map(|_| Output::Done)
                    .map_err(|e| e.to_string()),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<Output, String> = task.await;
            this.update_in(cx, |s, w, cx| {
                s.busy = false;
                match result {
                    Err(e) => {
                        s.error = Some(e);
                        s.focus(w, cx);
                        cx.notify();
                    }
                    Ok(Output::Done) => {
                        w.close_dialog(cx);
                        w.refresh();
                        s.finish(true, w, cx);
                    }
                    Ok(Output::Encrypted(bytes)) => s.save_file(bytes, w, cx),
                    Ok(Output::Imported(bundle)) => {
                        let manager = s.manager.clone();
                        let unlock_manager = manager.clone();
                        let done = s.done.take();
                        w.close_dialog(cx);
                        w.refresh();
                        w.defer(cx, move |w, cx| {
                            let needs_unlock = bundle
                                .items
                                .iter()
                                .any(|i| i.kind == shellmate_store::bundle::Kind::Credential);
                            let next = move |ok: bool, w: &mut Window, cx: &mut App| {
                                if !ok {
                                    if let Some(done) = done {
                                        done(false, w, cx)
                                    }
                                    return;
                                }
                                let preview_manager = manager.clone();
                                let task = cx
                                    .background_executor()
                                    .spawn(async move { manager.preview_bundle(bundle) });
                                let handle = w.window_handle();
                                cx.spawn(async move |cx| {
                                    let result = task.await;
                                    cx.update_window(handle, |_, w, cx| match result {
                                        Ok(preview) => {
                                            PreviewDialog::open_with_manager(
                                                preview,
                                                preview_manager,
                                                done,
                                                w,
                                                cx,
                                            );
                                        }
                                        Err(e) => {
                                            w.push_notification(Notification::error(e), cx);
                                            if let Some(done) = done {
                                                done(false, w, cx)
                                            }
                                        }
                                    })
                                    .ok();
                                })
                                .detach();
                            };
                            if needs_unlock {
                                ensure_unlocked(unlock_manager, "导入凭据", w, cx, next)
                            } else {
                                next(true, w, cx)
                            }
                        });
                    }
                }
            })
            .ok();
        })
        .detach();
    }
    fn save_file(&mut self, bytes: Vec<u8>, window: &mut Window, cx: &mut Context<Self>) {
        let name = default_name();
        let picked = cx.prompt_for_new_path(std::path::Path::new("."), Some(&name));
        self.busy = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let selected = picked.await;
            let result = match selected {
                Ok(Ok(Some(mut path))) => {
                    if path.extension().is_none() {
                        path.set_extension("smbundle");
                    }
                    cx.background_executor()
                        .spawn(async move { write_bundle_file(&path, &bytes).map(|_| true) })
                        .await
                }
                Ok(Ok(None)) => Ok(false),
                _ => Err("无法打开保存对话框".into()),
            };
            this.update_in(cx, |s, w, cx| {
                s.busy = false;
                match result {
                    Ok(ok) => {
                        w.close_dialog(cx);
                        w.refresh();
                        if ok {
                            w.push_notification(Notification::success("已导出加密配置包"), cx);
                        } else {
                            w.push_notification(Notification::info("已取消导出"), cx);
                        }
                        s.finish(ok, w, cx);
                    }
                    Err(e) => {
                        s.error = Some(e);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }
}
impl DialogBody for PasswordDialog {
    fn can_cancel(&self) -> bool {
        !self.busy
    }
    fn confirm(&mut self, w: &mut Window, cx: &mut Context<Self>) -> bool {
        self.run(false, w, cx);
        false
    }
    fn cancel(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if let Purpose::Unlock(reason) = &self.purpose {
            w.push_notification(
                Notification::info(format!("已取消解锁，{reason}已中止")),
                cx,
            );
        }
        self.finish(false, w, cx);
    }
    fn focus(&self, w: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.purpose, Purpose::Change) {
            self.current.update(cx, |i, cx| i.focus(w, cx));
        } else if self.purpose.password() {
            self.input.update(cx, |i, cx| i.focus(w, cx));
        }
    }
}
impl Render for PasswordDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let touch = matches!(self.purpose, Purpose::Unlock(_))
            && self.manager.status().touch_id
            && self.manager.status().touch_available.is_ok();
        let mut body = stack();
        let unlocking = matches!(self.purpose, Purpose::Unlock(_));
        if let Purpose::Unlock(reason) = &self.purpose {
            body = body.child(
                div()
                    .text_size(px(12.5))
                    .line_height(px(19.))
                    .text_color(p.tx2)
                    .text_ellipsis()
                    .child(format!("{reason} 需要读取凭据，解锁后继续。")),
            );
        }
        if matches!(self.purpose, Purpose::Setup | Purpose::Change) {
            body = body.child(hint(
                "请牢记主密码。没有找回方式；忘记后只能重置凭据库。",
                &p,
            ));
        }
        if matches!(self.purpose, Purpose::EnableTouch) {
            body = body.child(notice(Notice::Warn, model::TOUCH_TRADEOFF, &p));
        }
        if matches!(self.purpose, Purpose::Reset) {
            body = body.child(notice(Notice::Error, model::RESET_WARNING, &p));
        }
        if matches!(self.purpose, Purpose::DisableTouch) {
            body = body.child(hint("删除本机钥匙串中的解锁密钥，之后使用主密码解锁。", &p));
        }
        if matches!(self.purpose, Purpose::Export) {
            body = body.child(hint(
                "导出口令独立于主密码，请单独妥善保存。",
                &p,
            ));
            for (index, label, checked) in [
                (0, "主机", self.contents.hosts),
                (1, "分组", self.contents.groups),
                (2, "快捷命令", self.contents.commands),
                (3, "转发规则", self.contents.rules),
                (4, "凭据", self.contents.credentials),
            ] {
                body = body.child(
                    Checkbox::new(("bundle-content", index as usize))
                        .label(label)
                        .checked(checked)
                        .disabled(self.busy)
                        .on_click(cx.listener(move |s, v: &bool, _, cx| {
                            match index {
                                0 => s.contents.hosts = *v,
                                1 => s.contents.groups = *v,
                                2 => s.contents.commands = *v,
                                3 => s.contents.rules = *v,
                                _ => s.contents.credentials = *v,
                            }
                            cx.notify();
                        })),
                );
            }
        }
        if matches!(self.purpose, Purpose::Change) {
            body = body.child(field("当前主密码", &p, self.current.clone()));
        }
        if unlocking {
            // Field and error form one unit. Empty errors keep their line so a
            // failed unlock never moves the input or footer.
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.tx2)
                            .child("主密码"),
                    )
                    .child(div().mt(px(6.)).child(self.input.clone()))
                    .child(
                        div()
                            .mt(px(6.))
                            .h(px(20.))
                            .text_size(px(12.))
                            .line_height(px(20.))
                            .text_color(p.err)
                            .text_ellipsis()
                            .child(self.error.clone().unwrap_or_default()),
                    ),
            );
        } else if self.purpose.password() {
            body = body.child(field(
                match self.purpose {
                    Purpose::Export | Purpose::Import(_) => "配置包口令",
                    Purpose::Change => "新主密码",
                    _ => "主密码",
                },
                &p,
                self.input.clone(),
            ));
        }
        if self.purpose.confirmation() {
            body = body
                .child(
                    div()
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .text_color(super::password::strength_color(
                            self.input.read(cx).expose(),
                            &p,
                        ))
                        .child(format!(
                            "强度：{}",
                            shellmate_vault::strength(self.input.read(cx).expose()).label()
                        )),
                )
                .child(field(
                    if matches!(self.purpose, Purpose::Change) {
                        "确认新主密码"
                    } else {
                        "确认口令"
                    },
                    &p,
                    self.confirm.clone(),
                ));
        }
        if !unlocking && let Some(e) = &self.error {
            body = body.child(notice(Notice::Error, e.clone(), &p));
        }
        let mut buttons = vec![
            button("security-cancel", "取消", Tone::Plain, !self.busy)
                .on_click(cx.listener(|s, _, w, cx| s.dismiss(w, cx))),
        ];
        if touch {
            buttons.push(
                button("security-touch", "使用 Touch ID", Tone::Plain, !self.busy)
                    .on_click(cx.listener(|s, _, w, cx| s.run(true, w, cx))),
            );
        }
        buttons.push(
            button(
                "security-submit",
                if self.busy {
                    "处理中…"
                } else {
                    match self.purpose {
                        Purpose::Unlock(_) => "解锁",
                        Purpose::Import(_) => "预览",
                        Purpose::Export => "导出…",
                        Purpose::Reset => "永久删除全部凭据",
                        _ => "确认",
                    }
                },
                if matches!(self.purpose, Purpose::Reset) {
                    Tone::Danger
                } else {
                    Tone::Primary
                },
                model::password_submit_enabled(
                    self.input.read(cx).expose(),
                    self.purpose.confirmation(),
                    self.busy,
                ),
            )
            .on_click(cx.listener(|s, _, w, cx| s.submit(w, cx))),
        );
        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .when(unlocking, |d| {
                d.child(
                    div()
                        .id("forgot-master")
                        .flex_none()
                        .pt(px(6.))
                        .text_size(px(12.5))
                        .text_color(p.tx2)
                        .when(!self.busy, |d| {
                            d.cursor_pointer().hover(|d| d.text_color(p.acc))
                        })
                        .child("忘记主密码？")
                        .on_click(cx.listener(|s, _, w, cx| s.forgot_password(w, cx))),
                )
            })
            .child(div().ml_auto().child(actions(buttons)));
        secure_shell(
            self.purpose.title(),
            div()
                .id("security-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(px(24.))
                .pb(px(16.))
                .child(body),
            footer,
            &p,
        )
    }
}
fn default_name() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    format!("shellmate-{}.smbundle", date(seconds))
}
fn write_bundle_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let temporary = parent.join(format!(
        ".shellmate-{}.tmp",
        shellmate_core::HostId::generate()
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| "无法创建配置包临时文件")?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "配置包写入失败，原文件保留")?;
        std::fs::rename(&temporary, path).map_err(|_| "无法完成配置包保存，原文件保留")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
pub fn open_import(
    manager: Arc<CredentialStore>,
    done: impl FnOnce(bool, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("导入 .smbundle".into()),
    });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let path = match picked.await {
            Ok(Ok(Some(paths))) => paths.into_iter().next(),
            _ => None,
        };
        let Some(path) = path else { return };
        let result = cx
            .background_executor()
            .spawn(async move { read_bundle_file(path) })
            .await;
        cx.update_window(handle, |_, w, cx| match result {
            Ok(bytes) => {
                PasswordDialog::open(
                    manager,
                    Purpose::Import(Arc::new(bytes)),
                    None,
                    Box::new(done),
                    w,
                    cx,
                );
            }
            Err(e) => w.push_notification(Notification::error(e), cx),
        })
        .ok();
    })
    .detach();
}
fn read_bundle_file(path: PathBuf) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let f = std::fs::File::open(path).map_err(|_| "无法读取配置包")?;
    let mut bytes = Vec::new();
    f.take(64 * 1024 * 1024 + 81)
        .read_to_end(&mut bytes)
        .map_err(|_| "配置包读取失败")?;
    if bytes.len() > 64 * 1024 * 1024 + 80 {
        return Err("配置包过大".into());
    }
    Ok(bytes)
}
pub struct PreviewDialog {
    preview: Option<Preview>,
    identical_expanded: bool,
    manager: Arc<CredentialStore>,
    done: Option<Completion>,
    busy: bool,
    error: Option<String>,
}
impl PreviewDialog {
    pub fn open_with_manager(
        preview: Preview,
        manager: Arc<CredentialStore>,
        done: Option<Completion>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let body = cx.new(|_| Self {
            preview: Some(preview),
            identical_expanded: false,
            manager,
            done,
            busy: false,
            error: None,
        });
        open_secure(window, cx, 760., |_| 640., body);
    }
}
impl DialogBody for PreviewDialog {
    fn can_cancel(&self) -> bool {
        !self.busy
    }
    fn cancel(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if let Some(done) = self.done.take() {
            w.defer(cx, move |w, cx| done(false, w, cx));
        }
    }
    fn confirm(&mut self, w: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.busy {
            return false;
        }
        if self.manager.is_locked()
            && self.preview.as_ref().is_some_and(|p| {
                p.rows
                    .iter()
                    .any(|r| r.incoming.kind == shellmate_store::bundle::Kind::Credential)
            })
        {
            let this = cx.entity().downgrade();
            ensure_unlocked(
                self.manager.clone(),
                "导入凭据",
                w,
                cx,
                move |ok, w, cx| {
                    if ok {
                        this.update(cx, |s, cx| {
                            s.confirm(w, cx);
                        })
                        .ok();
                    }
                },
            );
            return false;
        }
        let Some(preview) = self.preview.take() else {
            return false;
        };
        let manager = self.manager.clone();
        self.busy = true;
        let task = cx.background_executor().spawn(async move {
            let result = manager.apply_bundle(&preview);
            (preview, result)
        });
        cx.spawn_in(w, async move |this, cx| {
            let (preview, result) = task.await;
            this.update_in(cx, |s, w, cx| {
                s.busy = false;
                s.preview = Some(preview);
                match result {
                    Ok(count) => {
                        w.close_dialog(cx);
                        w.refresh();
                        w.push_notification(
                            Notification::success(format!("已导入 {count} 项")),
                            cx,
                        );
                        if let Some(done) = s.done.take() {
                            w.defer(cx, move |w, cx| done(true, w, cx));
                        }
                    }
                    Err(e) => {
                        s.error = Some(e);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
        false
    }
}
impl Render for PreviewDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let mut toolbar = div()
            .flex_none()
            .px(px(24.))
            .pb(px(12.))
            .flex()
            .flex_col()
            .gap(px(12.));
        let mut list = div()
            .id("bundle-preview-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(24.))
            .flex()
            .flex_col();
        if let Some(preview) = &self.preview {
            let counts = preview.counts();
            toolbar = toolbar
                .child(
                    div()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!(
                            "冲突 {} · 新增 {} · 相同 {}",
                            counts[2], counts[0], counts[1]
                        )),
                )
                .child(
                    div().flex().gap(px(8.)).children(
                        [Choice::Skip, Choice::Overwrite, Choice::KeepBoth]
                            .into_iter()
                            .enumerate()
                            .map(|(index, choice)| {
                                gpui::component::button::Button::new(("bulk-choice", index))
                                    .label(format!("全部{}", choice.label()))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(move |s, _, _, cx| {
                                        if let Some(p) = &mut s.preview {
                                            p.choose_all(choice);
                                        }
                                        cx.notify();
                                    }))
                            }),
                    ),
                );
            let order = model::preview_order(preview.rows.iter().map(|r| r.classification));
            let mut summary_shown = false;
            for index in order {
                let row = &preview.rows[index];
                if row.classification == Classification::Identical {
                    if !summary_shown {
                        summary_shown = true;
                        list = list.child(
                            div()
                                .id("identical-summary")
                                .flex_none()
                                .h(px(34.))
                                .px(px(12.))
                                .border_b_1()
                                .border_color(p.line)
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .cursor_pointer()
                                .text_size(px(12.5))
                                .text_color(p.tx2)
                                .hover(|d| d.bg(p.card2))
                                .child(div().w(px(12.)).text_size(px(14.)).child(
                                    if self.identical_expanded {
                                        "⌄"
                                    } else {
                                        "›"
                                    },
                                ))
                                .child(format!("相同 {} 项 · 已自动跳过", counts[1]))
                                .on_click(cx.listener(|s, _, _, cx| {
                                    s.identical_expanded = !s.identical_expanded;
                                    cx.notify();
                                })),
                        );
                    }
                    if !self.identical_expanded {
                        continue;
                    }
                }
                let conflict = row.classification == Classification::Conflict;
                let choices = div().flex().items_center().gap(px(4.)).children(
                    conflict
                        .then(|| {
                            [Choice::Skip, Choice::Overwrite, Choice::KeepBoth]
                                .into_iter()
                                .enumerate()
                                .map(|(n, choice)| {
                                    gpui::component::button::Button::new((
                                        "conflict-choice",
                                        index * 3 + n,
                                    ))
                                    .h(px(26.))
                                    .text_size(px(12.))
                                    .rounded(px(8.))
                                    .label(if row.choice == choice {
                                        format!("✓ {}", choice.label())
                                    } else {
                                        choice.label().into()
                                    })
                                    .disabled(self.busy)
                                    .on_click(cx.listener(
                                        move |s, _, _, cx| {
                                            if let Some(p) = &mut s.preview {
                                                p.rows[index].choice = choice;
                                            }
                                            cx.notify();
                                        },
                                    ))
                                })
                                .collect::<Vec<_>>()
                        })
                        .into_iter()
                        .flatten(),
                );
                list = list.child(
                    div()
                        .flex_none()
                        .min_h(px(34.))
                        .when(!conflict, |d| d.h(px(34.)))
                        .px(px(12.))
                        .border_b_1()
                        .border_color(p.line)
                        .child(
                            div()
                                .h(px(33.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .text_size(px(13.))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(match row.classification {
                                            Classification::Conflict => p.warn,
                                            Classification::New => p.ok,
                                            Classification::Identical => p.tx3,
                                        })
                                        .child(row.classification.label()),
                                )
                                .child(div().flex_1().min_w_0().text_ellipsis().child(format!(
                                    "{} · {}",
                                    row.incoming.kind.label(),
                                    row.incoming.name()
                                )))
                                .child(choices),
                        )
                        .when(conflict, |d| {
                            d.child(div().pb(px(8.)).flex().flex_col().gap(px(4.)).children(
                                row.diff().into_iter().map(|line| {
                                    div()
                                        .text_size(px(12.))
                                        .line_height(px(18.))
                                        .text_color(p.tx2)
                                        .whitespace_normal()
                                        .child(line)
                                }),
                            ))
                        }),
                );
            }
        }
        toolbar = toolbar.children(self.error.clone().map(|e| notice(Notice::Error, e, &p)));
        let body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(list);
        secure_shell(
            "导入配置包预览",
            body,
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(hint(PREVIEW_FOOTER, &p))
                .child(actions(vec![
                    button("preview-cancel", "取消", Tone::Plain, !self.busy)
                        .on_click(cx.listener(|s, _, w, cx| s.dismiss(w, cx))),
                    button(
                        "preview-apply",
                        if self.busy {
                            "正在导入…"
                        } else {
                            "应用导入"
                        },
                        Tone::Primary,
                        !self.busy,
                    )
                    .on_click(cx.listener(|s, _, w, cx| s.submit(w, cx))),
                ])),
            &p,
        )
    }
}

fn date(seconds: i64) -> String {
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    let t = seconds as libc::time_t;
    unsafe {
        if libc::localtime_r(&t, tm.as_mut_ptr()).is_null() {
            return "backup".into();
        }
        let tm = tm.assume_init();
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    }
}

// Shared fixed header/footer; only the middle pane scrolls at 1100×700.
fn open_secure<V: DialogBody>(
    window: &mut Window,
    cx: &mut App,
    width: f32,
    preferred: impl Fn(&V) -> f32 + 'static,
    body: Entity<V>,
) {
    let first = std::cell::Cell::new(true);
    dialogs::open_modal(window, cx, move |dialog, w, cx| {
        if first.replace(false) {
            body.update(cx, |s, cx| s.focus(w, cx));
        }
        let ok = body.clone();
        let cancel = body.clone();
        let height = px(dialogs::shell_height(
            f32::from(w.viewport_size().height),
            preferred(body.read(cx)),
            20.,
        ));
        dialogs::fixed_shell(
            dialog
                .w(px(width.min(f32::from(w.viewport_size().width) - 40.)))
                .margin_top(((w.viewport_size().height - height) / 2.).max(px(20.)))
                .p(px(0.))
                .gap(px(0.))
                .rounded(px(22.))
                .shadow(dialogs::modal_shadow(None))
                .bg(cx.palette().win)
                .close_button(false)
                .overlay_closable(false)
                .on_ok(move |_, w, cx| ok.update(cx, |s, cx| s.confirm(w, cx)))
                .on_cancel(move |_, w, cx| {
                    cancel.update(cx, |s, cx| {
                        if !s.can_cancel() {
                            return false;
                        }
                        s.cancel(w, cx);
                        true
                    })
                }),
            body.clone(),
            height,
        )
    });
}
fn secure_shell(
    title: &'static str,
    body: impl IntoElement,
    footer: impl IntoElement,
    p: &crate::theme::Palette,
) -> gpui::Div {
    div()
        .h_full()
        .min_h_0()
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            div()
                .flex_none()
                .px(px(24.))
                .py(px(20.))
                .text_size(px(22.))
                .line_height(px(28.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(body)
        .child(
            div()
                .flex_none()
                .px(px(24.))
                .pt(px(14.))
                .pb(px(20.))
                .border_t_1()
                .border_color(p.line)
                .child(footer),
        )
}
