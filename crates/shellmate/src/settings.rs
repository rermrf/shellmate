//! Preferences are persisted before the workspace applies their runtime effects.
use crate::{
    dialogs::{self, DialogBody},
    theme::ActivePalette,
    workspace::Workspace,
};
use gpui::{
    App, Context, Entity, FontWeight, WeakEntity, Window,
    component::{
        Disableable as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        switch::Switch,
    },
    div,
    prelude::*,
    px,
};
use shellmate_store::{Preference as P, Preferences};
pub struct SettingsWindow {
    workspace: WeakEntity<Workspace>,
    preferences: Preferences,
    section: usize,
    error: Option<String>,
}
impl SettingsWindow {
    pub fn open(
        workspace: WeakEntity<Workspace>,
        preferences: Preferences,
        section: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let body = cx.new(|_| Self {
            workspace,
            preferences,
            section,
            error: None,
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let names = ["凭据与安全", "同步与备份", "终端", "本地终端", "连接与保活"];
        let nav = div()
            .w(px(185.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.))
            .children(names.into_iter().enumerate().map(|(index, name)| {
                div()
                    .id(("setting-section", index))
                    .px(px(12.))
                    .py(px(12.))
                    .rounded(px(12.))
                    .cursor_pointer()
                    .text_size(px(13.))
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
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("凭据存储方式"),
                )
                .child(
                    div().w_full().min_w_0().flex().gap(px(10.)).children(
                        [
                            (
                                true,
                                "系统钥匙串",
                                "密码与私钥口令交给 macOS 钥匙串（keyring crate），随系统登录解锁",
                            ),
                            (
                                false,
                                "主密码加密本地库",
                                "Argon2id 派生密钥 + XChaCha20-Poly1305 加密 SQLite，跨平台同步更方便",
                            ),
                        ]
                        .into_iter()
                        .map(|(selected, title, description)| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .p(px(16.))
                                .rounded(px(18.))
                                .border_1()
                                .border_color(if selected { p.acc } else { p.line })
                                .bg(if selected { p.accs } else { p.card })
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(format!("{}  {title}", if selected { "◉" } else { "○" })),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .whitespace_normal()
                                        .mt(px(8.))
                                        .text_size(px(12.))
                                        .text_color(p.tx2)
                                        .child(description),
                                )
                                .when(!selected, |d| {
                                    d.child(
                                        div()
                                            .mt(px(8.))
                                            .text_size(px(11.))
                                            .text_color(p.tx3)
                                            .child("后续版本提供"),
                                    )
                                })
                        }),
                    ),
                );
            for (i, label) in ["用 Touch ID 解锁凭据", "闲置 15 分钟后锁定"]
                .into_iter()
                .enumerate()
            {
                content = content.child(
                    div()
                        .p(px(16.))
                        .rounded(px(14.))
                        .bg(p.card)
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .text_size(px(13.))
                                .child(label)
                                .child(
                                    div()
                                        .mt(px(4.))
                                        .text_size(px(12.))
                                        .text_color(p.tx3)
                                        .child("后续版本提供"),
                                ),
                        )
                        .child(Switch::new(("future-pref", i)).checked(false).disabled(true)),
                );
            }
        } else if self.section == 1 {
            content = content.child(
                div()
                    .w_full()
                    .min_w_0()
                    .whitespace_normal()
                    .p(px(24.))
                    .rounded(px(18.))
                    .bg(p.card)
                    .text_size(px(14.))
                    .child("同步与备份")
                    .child(
                        div()
                            .mt(px(10.))
                            .text_size(px(12.))
                            .text_color(p.tx2)
                            .child("WebDAV、Git 同步与加密配置备份 · 后续版本提供"),
                    ),
            );
        } else {
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
            for key in keys {
                let key = *key;
                content = content.child(
                    div()
                        .px(px(18.))
                        .py(px(12.))
                        .rounded(px(14.))
                        .bg(p.card)
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .text_size(px(13.))
                                .font_weight(FontWeight::MEDIUM)
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
                            d.child(Button::new("clear-history").small().label("清空历史").on_click(
                                cx.listener(|this, _, w, cx| {
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
                                }),
                            ))
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
        }
        div()
            .h((window.viewport_size().height - px(250.)).clamp(px(260.), px(560.)))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .w_full()
                    .flex()
                    .gap(px(20.))
                    .child(nav)
                    .child(content),
            )
            .children(
                self.error
                    .clone()
                    .map(|e| dialogs::notice(dialogs::Notice::Error, e, &p)),
            )
            .child(
                div().flex().justify_end().child(
                    Button::new("settings-close")
                        .primary()
                        .label("完成")
                        .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                ),
            )
    }
}
