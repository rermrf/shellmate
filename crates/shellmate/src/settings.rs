//! Preferences are persisted before the workspace applies their runtime effects.
use crate::security::{
    self, Purpose,
    password::{PasswordEvent, PasswordInput},
};
use crate::ui::Toggle as Switch;
use crate::{
    dialogs::{self, DialogBody},
    theme::ActivePalette,
    workspace::Workspace,
};
use gpui::{
    App, Context, Entity, FontWeight, WeakEntity, Window,
    component::{Sizable as _, button::Button},
    div,
    prelude::*,
    px,
};
use shellmate_store::{Backend, Preference as P, Preferences, TerminalFontSize};

// Re-enable the placeholder rows when remote sync is available.
const SHOW_REMOTE_SYNC: bool = false;
const LOCAL_TERMINAL: usize = 3;

pub struct SettingsWindow {
    workspace: WeakEntity<Workspace>,
    preferences: Preferences,
    ai: Entity<crate::ai::settings::AiSettings>,
    section: usize,
    finder_enabled: Option<bool>,
    scroll: gpui::ScrollHandle,
    error: Option<String>,
    credentials: std::sync::Arc<shellmate_store::CredentialStore>,
    selected_master: bool,
    master_input: Entity<PasswordInput>,
    _password_subscription: gpui::Subscription,
    #[cfg(debug_assertions)]
    demo_master: bool,
}
impl SettingsWindow {
    pub fn open(
        workspace: WeakEntity<Workspace>,
        preferences: Preferences,
        credentials: std::sync::Arc<shellmate_store::CredentialStore>,
        section: usize,
        db: shellmate_store::SharedStore,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let body = cx.new(|cx| {
            let master_input = cx.new(|cx| PasswordInput::new("输入主密码", crate::ui::Surface::Card, window, cx));
            let subscription = cx.subscribe_in(
                &master_input,
                window,
                |s: &mut Self, _, event: &PasswordEvent, w, cx| match event {
                    PasswordEvent::Changed => cx.notify(),
                    PasswordEvent::Submit => s.master_password(w, cx),
                },
            );
            if let Some(workspace) = workspace.upgrade() {
                cx.subscribe(&workspace, |settings: &mut Self, _, change: &crate::workspace::CloseWindowPreferenceChanged, cx| {
                    settings.preferences.close_window_with_sessions = change.0;
                    cx.notify();
                }).detach();
                cx.subscribe(&workspace, |settings: &mut Self, _, change: &crate::workspace::PreferenceChanged, cx| {
                    settings.preferences.set(change.0, change.1);
                    cx.notify();
                }).detach();
                cx.subscribe(&workspace, |settings: &mut Self, _, change: &crate::workspace::TerminalFontSizeChanged, cx| {
                    settings.preferences.terminal_font_size = change.0;
                    cx.notify();
                }).detach();
            }
            let ai=cx.new(|cx|crate::ai::settings::AiSettings::new(workspace.clone(),db,credentials.clone(),window,cx));
            ai.update(cx,|a,cx|a.refresh_key(cx));
            cx.observe_window_activation(window, |s: &mut Self, w, cx| {
                if s.section == LOCAL_TERMINAL && w.is_window_active() {
                    s.finder_enabled = crate::finder::enabled();
                    cx.notify();
                }
            }).detach();
            Self {
                ai, workspace,
                preferences,
                section,
                finder_enabled: if section == LOCAL_TERMINAL { crate::finder::enabled() } else { None },
                scroll: gpui::ScrollHandle::new(),
                error: None,
                selected_master: credentials.backend() == Backend::Master,
                credentials,
                master_input,
                _password_subscription: subscription,
                #[cfg(debug_assertions)]
                demo_master: false,
            }
        });
        body.update(cx, |s, cx| {
            let manager = s.credentials.clone();
            let task = cx
                .background_executor()
                .spawn(async move { manager.refresh_touch_availability() });
            cx.spawn(async move |this, cx| {
                task.await;
                this.update(cx, |_, cx| cx.notify()).ok();
            })
            .detach();
        });
        dialogs::open(
            window,
            cx,
            "设置",
            920_f32.min(f32::from(window.viewport_size().width) - 40.),
            body.clone(),
        );
        body
    }
    fn refresh_security(&mut self, cx: &mut Context<Self>) {
        self.selected_master = self.credentials.backend() == Backend::Master;
        cx.notify();
    }
    fn operation(
        &mut self,
        purpose: Purpose,
        needs_unlock: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let manager = self.credentials.clone();
        let this = cx.entity().downgrade();
        let next = move |ok: bool, w: &mut Window, cx: &mut App| {
            if !ok {
                return;
            }
            security::run_operation(
                manager,
                purpose,
                move |_, _, cx| {
                    this.update(cx, |s, cx| s.refresh_security(cx)).ok();
                },
                w,
                cx,
            );
        };
        if needs_unlock {
            security::ensure_unlocked(self.credentials.clone(), "更改凭据设置", window, cx, next)
        } else {
            next(true, window, cx)
        }
    }
    fn master_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !shellmate_vault::acceptable(self.master_input.read(cx).expose()) {
            self.error = Some(shellmate_vault::PASSWORD_MINIMUM.into());
            cx.notify();
            return;
        }
        let manager = self.credentials.clone();
        let purpose = if manager.backend() == Backend::Master {
            Purpose::Change
        } else {
            Purpose::Setup
        };
        let initial = self.master_input.update(cx, |i, cx| i.take(cx));
        let this = cx.entity().downgrade();
        security::ensure_unlocked(
            manager.clone(),
            "更改主密码",
            window,
            cx,
            move |ok, w, cx| {
                if !ok {
                    return;
                }
                security::PasswordDialog::open(
                    manager,
                    purpose,
                    Some(initial),
                    Box::new(move |_, _, cx| {
                        this.update(cx, |s, cx| s.refresh_security(cx)).ok();
                    }),
                    w,
                    cx,
                );
            },
        );
    }
    fn export_bundle(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| workspace.export_bundle(w, cx)).ok();
    }
    fn import_bundle(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| workspace.import_bundle(w, cx)).ok();
    }
    #[cfg(debug_assertions)]
    pub fn fixture_master(&mut self, value: &str, focused: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_master = true;
        self.demo_master = true;
        self.master_input.update(cx, |input, cx| input.set(value, cx));
        if focused {
            crate::proto::focus_after_layout(gpui::Focusable::focus_handle(&self.master_input, cx), window);
        }
        cx.notify();
    }
    #[cfg(debug_assertions)]
    pub fn fixture_ai_advanced(&self, cx: &mut Context<Self>) {
        self.ai.update(cx, crate::ai::settings::AiSettings::fixture_advanced);
    }
    #[cfg(debug_assertions)]
    pub fn fixture_ai_web(&self,cx:&mut Context<Self>){self.ai.update(cx,crate::ai::settings::AiSettings::fixture_web);}
    #[cfg(debug_assertions)]
    pub fn fixture_scroll_bottom(&self, window: &mut Window) {
        crate::proto::scroll_after_layout(self.scroll.clone(), |handle| f32::from(handle.max_offset().y), window);
    }
    #[cfg(debug_assertions)]
    pub fn fixture_scroll_web(&self,window:&mut Window){crate::proto::scroll_after_layout(self.scroll.clone(),|handle|(f32::from(handle.max_offset().y)-300.).max(0.),window);}
    #[cfg(debug_assertions)]
    pub fn fixture_replace_key(&self, cx: &mut Context<Self>) {
        self.ai.update(cx, crate::ai::settings::AiSettings::replace_key);
    }
    #[cfg(debug_assertions)]
    pub fn fixture_scroll(&mut self, offset: f32, cx: &mut Context<Self>) {
        self.scroll.set_offset(gpui::point(px(0.), px(-offset)));
        cx.notify();
    }
    /// One step of the default terminal font size (the − and + of 设置 › 终端).
    fn step_font_size(&mut self, step: i8, cx: &mut Context<Self>) {
        let (min, max) = (TerminalFontSize::MIN, TerminalFontSize::MAX);
        let current = self.preferences.terminal_font_size;
        let next = (i16::from(current) + i16::from(step)).clamp(i16::from(min), i16::from(max)) as u8;
        if next == current {
            return;
        }
        match self.workspace.update(cx, |w, cx| w.set_terminal_font_size(next, cx)) {
            Ok(Ok(())) => {
                self.preferences.terminal_font_size = next;
                self.error = None;
            }
            Ok(Err(e)) => self.error = Some(e),
            Err(_) => self.error = Some("窗口已关闭".into()),
        }
        cx.notify();
    }
    fn toggle(&mut self, key: P, value: bool, window: &mut Window, cx: &mut Context<Self>) {
        match self
            .workspace
            .update(cx, |w, cx| w.set_preference(key, value, window, cx))
        {
            Ok(Ok(())) => {
                self.preferences.set(key, value);
                self.error = None;
            }
            Ok(Err(e)) => self.error = Some(e),
            Err(_) => self.error = Some("窗口已关闭".into()),
        }
        cx.notify();
    }
}
impl DialogBody for SettingsWindow {
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        false
    }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}
impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let security_model = security::model::SettingsModel::new(self.credentials.status());
        let demo_master = {
            #[cfg(debug_assertions)]
            {
                self.demo_master
            }
            #[cfg(not(debug_assertions))]
            {
                false
            }
        };
        let names = ["凭据与安全", "同步与备份", "终端", "本地终端", "连接与保活", "AI 助手", "软件更新"];
        let nav = div()
            .w(px(186.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .h(px(47.))
                    .px(px(10.))
                    .text_size(px(24.))
                    .line_height(px(33.))
                    .font_weight(FontWeight::BOLD)
                    .child("设置"),
            )
            .children(names.into_iter().enumerate().map(|(index, name)| {
                div()
                    .id(("setting-section", index))
                    .px(px(12.))
                    .h(px(38.))
                    .flex()
                    .items_center()
                    .rounded(px(12.))
                    .cursor_pointer()
                    .text_size(px(13.5))
                    .line_height(px(16.))
                    .text_color(if self.section == index { p.tx } else { p.tx2 })
                    .when(self.section == index, |d| {
                        d.bg(p.card).font_weight(FontWeight::SEMIBOLD)
                    })
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = index;
                        if index == LOCAL_TERMINAL {
                            this.finder_enabled = crate::finder::enabled();
                        }
                        this.scroll.set_offset(gpui::point(px(0.), px(0.)));
                        cx.notify();
                    }))
            }));
        let mut content = div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(12.));
        if self.section == 0 {
            content = content
                .child(
                    div()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .font_weight(FontWeight::BOLD)
                        .child("凭据存储方式"),
                )
                .child(
                    div().w_full().min_w_0().flex().gap(px(10.)).children(
                        [
                            (
                                !self.selected_master,
                                "系统钥匙串",
                                "密码与私钥口令交给 macOS 钥匙串（keyring crate），随系统登录解锁",
                            ),
                            (
                                self.selected_master,
                                "主密码加密本地库",
                                "Argon2id 派生密钥 + XChaCha20-Poly1305 加密 SQLite，跨平台同步更方便",
                            ),
                        ]
                        .into_iter()
                        .enumerate().map(|(index,(selected, title, description))| {
                            div()
                                .id(("credential-backend",index)).cursor_pointer().on_click(cx.listener(move|s,_,w,cx|{if index==1{s.selected_master=true;cx.notify();}else if s.credentials.backend()==Backend::Master{s.operation(Purpose::SwitchKeychain,true,w,cx);}else{s.selected_master=false;cx.notify();}}))
                                .relative().flex_1().min_w_0()
                                .whitespace_normal()
                                .p(px(16.))
                                .min_h(px(100.))
                                .rounded(px(18.))
                                .bg(if selected { p.accs } else { p.card })
                                .child(
                                    div()
                                        .h(px(18.)).text_color(p.tx)
                                        .text_size(px(14.))
                                        .line_height(px(17.))
                                        .font_weight(FontWeight::BOLD)
                                        .flex().items_center().gap(px(8.))
                                        .child(div().size(px(16.)).flex_none().rounded_full().border_1().border_color(if selected{p.acc}else{p.tx3})
                                            .flex().items_center().justify_center().when(selected,|d|d.child(div().size(px(8.)).rounded_full().bg(p.acc))))
                                        .child(title)
,
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .whitespace_normal()
                                        .mt(px(10.))
                                        .text_size(px(12.5))
                                        .line_height(px(20.))
                                        .text_color(p.tx2)
                                        .child(description),
                                )
                                .child(crate::ui::surface_ring(18., 1.5, if selected { p.acc } else { p.line }))
                        }),
                    ),
                );
            if self.selected_master {
                content = content.child(
                    div()
                        .flex_none()
                        .h(px(66.))
                        .rounded(px(18.))
                        .bg(p.card)
                        .px(px(16.))
                        .py(px(14.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(div().flex_1().min_w_0().child(self.master_input.clone()))
                        // The slot keeps its width while empty, so the first keystroke doesn't shrink the field.
                        .child(div().flex_none().w(px(60.)).children(
                            security::password::visible_strength(self.master_input.read(cx).expose()).map(|strength| {
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(security::password::strength_color(strength, &p))
                                    .child(format!("强度：{}", strength.label()))
                            }),
                        ))
                        .child(
                            dialogs::button(
                                "master-set",
                                if security_model.can_change_password() {
                                    "更改主密码"
                                } else {
                                    "设置主密码"
                                },
                                dialogs::Tone::Primary,
                                shellmate_vault::acceptable(self.master_input.read(cx).expose()),
                            )
                            .h(px(38.))
                            .on_click(cx.listener(|s, _, w, cx| s.master_password(w, cx))),
                        ),
                );
            }
            let touch_reason = security_model.touch_reason.clone();
            let rows = [
                (
                    "用 Touch ID 解锁凭据",
                    if demo_master {
                        None
                    } else {
                        touch_reason.clone()
                    },
                    security_model.touch_enabled || demo_master,
                    touch_reason.is_some() && !security_model.touch_enabled && !demo_master,
                ),
                (
                    "闲置 15 分钟后锁定",
                    None,
                    security_model.idle_enabled,
                    self.credentials.backend() != Backend::Master && !demo_master,
                ),
            ];
            content = content.child(
                div()
                    .flex_none()
                    .rounded(px(18.))
                    .bg(p.card)
                    .px(px(18.))
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .children(rows.into_iter().enumerate().map(
                        |(index, (label, description, checked, disabled))| {
                            div()
                                .min_h(px(44.))
                                .py(px(12.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(12.))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(13.5))
                                        .line_height(px(16.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(p.tx)
                                        .child(label)
                                        .children(description.map(|description| {
                                            div()
                                                .mt(px(4.))
                                                .text_size(px(12.))
                                                .line_height(px(15.))
                                                .font_weight(FontWeight::NORMAL)
                                                .text_color(p.tx2)
                                                .child(description)
                                        })),
                                )
                                .child(
                                    Switch::new(("security-pref", index))
                                        .checked(checked)
                                        .disabled(disabled)
                                        .color(p.ok)
                                        .on_click(cx.listener(move |s, value: &bool, w, cx| {
                                            if index == 0 {
                                                s.operation(
                                                    if *value {
                                                        Purpose::EnableTouch
                                                    } else {
                                                        Purpose::DisableTouch
                                                    },
                                                    *value,
                                                    w,
                                                    cx,
                                                );
                                            } else {
                                                if let Err(e) = s.credentials.set_idle_lock(*value)
                                                {
                                                    s.error = Some(e.to_string());
                                                }
                                                cx.notify();
                                            }
                                        })),
                                )
                        },
                    )),
            );
            if security_model.master {
                content = content.child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            dialogs::button(
                                "vault-lock",
                                if security_model.can_lock() {
                                    "立即锁定"
                                } else {
                                    "解锁凭据库"
                                },
                                dialogs::Tone::Plain,
                                true,
                            )
                            .on_click(cx.listener(|s, _, w, cx| {
                                if s.credentials.status().locked {
                                    s.operation(Purpose::Unlock("读取凭据".into()), false, w, cx);
                                } else {
                                    s.credentials.lock();
                                    cx.notify();
                                }
                            })),
                        )
                        .child(
                            dialogs::button(
                                "vault-reset",
                                "重置凭据库",
                                dialogs::Tone::Danger,
                                true,
                            )
                            .on_click(
                                cx.listener(|s, _, w, cx| {
                                    s.operation(Purpose::Reset, false, w, cx)
                                }),
                            ),
                        ),
                );
            }
        } else if self.section == 1 {
            content=content.child(div().flex_none().p(px(18.)).py(px(16.)).rounded(px(18.)).bg(p.card).flex().items_center().gap(px(14.))
                .child(div().flex_1().min_w_0().text_size(px(14.)).line_height(px(20.)).font_weight(FontWeight::BOLD).child("导出加密配置包"))
                .child(dialogs::button("bundle-export","导出…",dialogs::Tone::Primary,true).h(px(36.)).on_click(cx.listener(|s,_,w,cx|s.export_bundle(w,cx))))
                .child(dialogs::button("bundle-import","导入…",dialogs::Tone::Plain,true).h(px(36.)).on_click(cx.listener(|s,_,w,cx|s.import_bundle(w,cx)))));
            content = content.when(SHOW_REMOTE_SYNC, |content| content
                .child(
                    div()
                        .flex_none()
                        .rounded(px(18.))
                        .bg(p.card)
                        .px(px(18.))
                        .py(px(4.))
                        .flex()
                        .flex_col()
                        .children(["WebDAV 同步", "Git 仓库同步"].into_iter().enumerate().map(
                            |(index, label)| {
                                div()
                                    .h(px(56.))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_size(px(13.5))
                                            .line_height(px(16.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(label)
                                            .child(
                                                div()
                                                    .mt(px(4.))
                                                    .text_size(px(12.))
                                                    .line_height(px(15.))
                                                    .font_weight(FontWeight::NORMAL)
                                                    .text_color(p.tx3)
                                                    .child("后续版本提供"),
                                            ),
                                    )
                                    .child(
                                        Switch::new(("sync-deferred", index))
                                            .checked(false)
                                            .disabled(true),
                                    )
                            },
                        )),
                )
                .child(div().flex().justify_end().child(
                    dialogs::button("sync-now", "立即同步", dialogs::Tone::Plain, false).h(px(36.)),
                )));
        } else if self.section == 6 {
            content = content.child(crate::updater::settings(cx));
        } else if self.section==5 {
            content=content.child(self.ai.clone());
        } else {
            if self.section == 2 {
                let style = cx.terminal_style();
                let color = shellmate_term::colors::to_hsla;
                let font = style.font_size;
                let height = (font * style.line_height).round();
                let row = |label: &'static str, value: String| {
                    div()
                        .flex()
                        .justify_between()
                        .text_size(px(13.))
                        .line_height(px(18.))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
                        .child(div().text_color(p.tx2).child(value))
                };
                content = content.child(
                    div()
                        .flex_none()
                        .px(px(18.))
                        .py(px(16.))
                        .rounded(px(18.))
                        .bg(p.card)
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .child(row(
                            "字体",
                            format!(
                                "{} · 中文回退 {} · 行高 {:.1}",
                                style.font_family,
                                crate::theme::CJK_FALLBACK,
                                style.line_height
                            ),
                        ))
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .items_center()
                                .text_size(px(13.))
                                .line_height(px(18.))
                                .child(div().font_weight(FontWeight::SEMIBOLD).child("字号"))
                                .child({
                                    let this = cx.weak_entity();
                                    crate::ui::stepper(
                                        "terminal-font-size",
                                        format!("{} pt", self.preferences.terminal_font_size),
                                        self.preferences.terminal_font_size > TerminalFontSize::MIN,
                                        self.preferences.terminal_font_size < TerminalFontSize::MAX,
                                        move |step, _, cx| {
                                            this.update(cx, |s, cx| s.step_font_size(step, cx)).ok();
                                        },
                                        cx,
                                    )
                                }),
                        )
                        .child(
                            div()
                                .px(px(14.))
                                .py(px(12.))
                                .rounded(px(12.))
                                .bg(color(style.palette.background))
                                .text_color(color(style.palette.foreground))
                                .font_family(style.font_family)
                                .text_size(font)
                                .line_height(height)
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .flex()
                                .child(
                                    div()
                                        .text_color(color(style.palette.ansi[2]))
                                        .font_weight(FontWeight::BOLD)
                                        .child("deploy@prod-web-01"),
                                )
                                .child(":")
                                .child(
                                    div()
                                        .text_color(color(style.palette.ansi[4]))
                                        .font_weight(FontWeight::BOLD)
                                        .child("~"),
                                )
                                .child("$ echo \"宽字符占两格 ✓\" | wc -m"),
                        )
                        .child(row(
                            "滚动缓冲",
                            if self.preferences.get(P::SessionLog) {
                                "10,000 行 · 会话日志已开启"
                            } else {
                                "10,000 行 · 会话日志未开启"
                            }
                            .into(),
                        )),
                );
            }
            let keys: &[P] = match self.section {
                2 => &[
                    P::CopySelection,
                    P::RightPaste,
                    P::BracketedPaste,
                    P::PromptHighlight,
                    P::History,
                    P::SessionLog,
                    P::AutoRecord,
                    P::TabAlerts,
                    P::SystemNotifications,
                    P::LongCommandAlert,
                ],
                LOCAL_TERMINAL => &[P::LoginShell],
                _ if crate::prototype_fixture() => &[P::ShowStatusIcon, P::QuitOnWindowClose, P::ConfirmQuit, P::RestoreOnLaunch, P::FollowDirectory],
                _ => &[P::ShowStatusIcon, P::QuitOnWindowClose, P::ConfirmQuit, P::RestoreOnLaunch, P::FollowDirectory, P::PreloadPanels],
            };
            let mut rows = div()
                .flex_none()
                .rounded(px(18.))
                .bg(p.card)
                .px(px(18.))
                .py(px(4.))
                .flex()
                .flex_col();
            let preference_row = |label: &'static str, description: &'static str| {
                div()
                    .min_h(px(56.))
                    .flex_none()
                    .py(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .text_size(px(13.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .line_height(px(16.))
                            .child(label)
                            .when(!description.is_empty(), |d| {
                                d.child(
                                    div()
                                        .mt(px(4.))
                                        .text_size(px(12.))
                                        .text_color(p.tx2)
                                        .child(description),
                                )
                            }),
                    )
            };
            for &key in keys {
                rows = rows.child(
                    preference_row(key.label(), key.description())
                        .when(key == P::History, |d| {
                            d.child(
                                Button::new("clear-history")
                                    .small()
                                    .label("清空历史")
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        let workspace = this.workspace.clone();
                                        crate::dialogs::confirm::ConfirmDialog::open(
                                            "清空命令历史？",
                                            "将删除本机保存的所有命令历史，无法撤销。",
                                            "清空历史",
                                            true,
                                            move |w, cx| {
                                                workspace
                                                    .update(cx, |s, cx| s.clear_history(w, cx))
                                                    .map_err(|_| "窗口已关闭".to_string())?
                                            },
                                            w,
                                            cx,
                                        );
                                    })),
                            )
                        })
                        .when_else(key == P::QuitOnWindowClose, |d| {
                            let quit = self.preferences.get(key);
                            let this = cx.weak_entity();
                            d.child(crate::ui::segmented(key.key(), vec![
                                crate::ui::SegmentItem::new("保持运行", !quit),
                                crate::ui::SegmentItem::new("退出 Shellmate", quit),
                            ], move |index, w, cx| {
                                this.update(cx, |s, cx| s.toggle(key, index == 1, w, cx)).ok();
                            }, cx))
                        }, |d| d.child(
                            Switch::new(key.key())
                                .checked(self.preferences.get(key))
                                // System notifications are part of the tab alerts: they go with them.
                                .disabled(key == P::SystemNotifications && !self.preferences.get(P::TabAlerts))
                                .color(p.ok)
                                .on_click(cx.listener(move |this, value: &bool, w, cx| {
                                    this.toggle(key, *value, w, cx)
                                })),
                        )),
                );
                // Keep established prototype fixtures stable; the new scene exercises this row.
                let show_close_policy = !crate::prototype_fixture()
                    || std::env::var("SHELLMATE_DEMO").is_ok_and(|s| s == "proto:set-conn-close-window");
                if key == P::QuitOnWindowClose && show_close_policy {
                    use shellmate_store::CloseWindowWithSessions as Policy;
                    let selected = self.preferences.close_window_with_sessions;
                    let owner = cx.weak_entity();
                    rows = rows.child(div().py(px(12.)).flex().flex_col().gap(px(4.))
                        .child(div().flex().items_center().gap(px(12.))
                            .child(div().flex_1().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).child("关闭含会话的窗口"))
                            .child(crate::ui::segmented(Policy::KEY,
                                Policy::ALL.iter().map(|value| crate::ui::SegmentItem::new(value.label(), *value == selected)).collect(),
                                move |index, _, cx| {
                                    owner.update(cx, |settings, cx| {
                                        let result = settings.workspace.update(cx, |ws, cx| ws.set_close_window_preference(Policy::ALL[index], cx));
                                        settings.error = match result {
                                            Ok(Ok(())) => { settings.preferences.close_window_with_sessions = Policy::ALL[index]; None },
                                            Ok(Err(error)) => Some(error),
                                            Err(_) => Some("窗口已关闭".into()),
                                        };
                                        cx.notify();
                                    }).ok();
                                }, cx)))
                        .child(div().text_size(px(12.)).line_height(px(16.)).text_color(p.tx2)
                            .child("「按需询问」会直接关闭空闲本机终端，有任务或连接时才提示。仅对非最后一个窗口生效；最后一个窗口仍按「关闭窗口时」处理。")));
                }
            }
            if self.section == LOCAL_TERMINAL {
                let status = match self.finder_enabled {
                    Some(true) => "已启用",
                    Some(false) => "未启用",
                    None => "开发版不可用",
                };
                rows = rows.child(
                    preference_row("访达右键菜单", "在访达的文件夹或空白处右键，选择「进入 Shellmate」，即可在该目录打开本机终端。")
                        .child(div().flex_none().text_size(px(12.)).text_color(p.tx2).child(status))
                        .child(dialogs::button("finder-settings", "在系统设置中打开", dialogs::Tone::Plain, true)
                            .on_click(|_, _, _| crate::finder::show_settings())),
                );
            }
            content = content.child(rows);
        }
        div()
            .relative()
            .h_full()
            .min_h_0()
            .overflow_hidden()
            .pt(px(24.))
            .pb(px(24.))
            .pl(px(12.))
            .pr(px(24.))
            .flex()
            .gap(px(20.))
            .child(nav)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    // 30 + 12 replaces the old 42 px scrolling top inset.
                    .gap(px(12.))
                    .child(dialogs::header(
                        "settings-close",
                        div(),
                        &p,
                        cx.listener(|s, _, w, cx| s.dismiss(w, cx)),
                    ))
                    .child(dialogs::scroll_body(
                        "settings-content",
                        content.children(self.error.clone().map(|e| {
                            dialogs::notice(dialogs::Notice::Error, e, &p)
                        })),
                    ).track_scroll(&self.scroll)),
            )
    }
}
