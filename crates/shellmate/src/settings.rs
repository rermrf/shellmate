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
use shellmate_store::{Backend, Preference as P, Preferences};
pub struct SettingsWindow {
    workspace: WeakEntity<Workspace>,
    preferences: Preferences,
    section: usize,
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
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let body = cx.new(|cx| {
            let master_input = cx.new(|cx| PasswordInput::new("输入主密码", cx));
            let subscription = cx.subscribe_in(
                &master_input,
                window,
                |s: &mut Self, _, event: &PasswordEvent, w, cx| match event {
                    PasswordEvent::Changed => cx.notify(),
                    PasswordEvent::Submit => s.master_password(w, cx),
                },
            );
            Self {
                workspace,
                preferences,
                section,
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
        security::PasswordDialog::open(
            self.credentials.clone(),
            Purpose::Export,
            None,
            Box::new(|_, _, _| {}),
            w,
            cx,
        );
    }
    fn import_bundle(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        security::open_import(
            self.credentials.clone(),
            move |ok, w, cx| {
                if ok {
                    workspace.update(cx, |s, cx| s.refresh_bundle(w, cx)).ok();
                }
            },
            w,
            cx,
        );
    }
    #[cfg(debug_assertions)]
    pub fn fixture_master(&mut self, cx: &mut Context<Self>) {
        self.selected_master = true;
        self.demo_master = true;
        self.master_input
            .update(cx, |i, cx| i.set("meadow river stone !", cx));
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
        let names = ["凭据与安全", "同步与备份", "终端", "本地终端", "连接与保活"];
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
                        cx.notify();
                    }))
            }));
        let mut content = div()
            .id("settings-content")
            .pt(px(42.))
            .pr(px(0.))
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
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
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(security::password::strength_color(
                                    self.master_input.read(cx).expose(),
                                    &p,
                                ))
                                .child(format!(
                                    "强度：{}",
                                    shellmate_vault::strength(self.master_input.read(cx).expose())
                                        .label()
                                )),
                        )
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
                    (if demo_master {
                        None
                    } else {
                        touch_reason.clone()
                    })
                    .unwrap_or_else(|| "需要密码或口令时验证指纹".into()),
                    security_model.touch_enabled || demo_master,
                    touch_reason.is_some() && !security_model.touch_enabled && !demo_master,
                ),
                (
                    "闲置 15 分钟后锁定",
                    "锁定后需重新解锁才能读取凭据".into(),
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
                                .h(px(56.))
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
                                        .child(
                                            div()
                                                .mt(px(4.))
                                                .text_size(px(12.))
                                                .line_height(px(15.))
                                                .font_weight(FontWeight::NORMAL)
                                                .text_color(p.tx2)
                                                .child(description),
                                        ),
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
            content=content.child(div().flex_none().min_h(px(75.)).p(px(18.)).py(px(16.)).rounded(px(18.)).bg(p.card).flex().items_center().gap(px(14.))
                .child(div().flex_1().min_w_0().text_size(px(14.)).line_height(px(20.)).font_weight(FontWeight::BOLD).child("导出加密配置包")
                    .child(div().mt(px(3.)).text_size(px(12.5)).line_height(px(20.)).font_weight(FontWeight::NORMAL).text_color(p.tx2).whitespace_normal().child("主机、分组、快捷命令、转发规则和凭据打包为 .smbundle，用导出口令加密")))
                .child(dialogs::button("bundle-export","导出…",dialogs::Tone::Primary,true).h(px(36.)).on_click(cx.listener(|s,_,w,cx|s.export_bundle(w,cx))))
                .child(dialogs::button("bundle-import","导入…",dialogs::Tone::Plain,true).h(px(36.)).on_click(cx.listener(|s,_,w,cx|s.import_bundle(w,cx)))));
            content = content
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
                ));
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
                                "{} {} · 中文回退 {} · 行高 {:.1}",
                                style.font_family,
                                f32::from(font),
                                crate::theme::CJK_FALLBACK,
                                style.line_height
                            ),
                        ))
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
                    P::History,
                    P::HistoryShortcut,
                    P::SessionLog,
                    P::AutoRecord,
                ],
                3 => &[P::LoginShell],
                _ => &[P::FollowDirectory],
            };
            let mut rows = div()
                .flex_none()
                .rounded(px(18.))
                .bg(p.card)
                .px(px(18.))
                .py(px(4.))
                .flex()
                .flex_col();
            for key in keys {
                let key = *key;
                rows = rows.child(
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
                                .child(key.label())
                                .when(!key.description().is_empty(), |d| {
                                    d.child(
                                        div()
                                            .mt(px(4.))
                                            .text_size(px(12.))
                                            .text_color(p.tx2)
                                            .child(key.description()),
                                    )
                                }),
                        )
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
                        .child(
                            Switch::new(key.key())
                                .checked(self.preferences.get(key))
                                .color(p.ok)
                                .on_click(cx.listener(move |this, value: &bool, w, cx| {
                                    this.toggle(key, *value, w, cx)
                                })),
                        ),
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
                content.children(
                    self.error
                        .clone()
                        .map(|e| dialogs::notice(dialogs::Notice::Error, e, &p)),
                ),
            )
            .child(
                div()
                    .id("settings-close")
                    .absolute()
                    .right(px(24.))
                    .top(px(24.))
                    .size(px(30.))
                    .rounded(px(10.))
                    .bg(p.card)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_size(px(16.))
                    .line_height(px(16.))
                    .text_color(p.tx2)
                    .child("×")
                    .on_click(cx.listener(|s, _, w, cx| s.dismiss(w, cx))),
            )
    }
}
