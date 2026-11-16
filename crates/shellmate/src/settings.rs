//! Preferences are persisted before the workspace applies their runtime effects.
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
                                .min_h(px(100.))
                                .rounded(px(18.))
                                .shadow(vec![gpui::BoxShadow {color:if selected{p.acc}else{p.line},offset:gpui::point(px(0.),px(0.)),blur_radius:px(0.),spread_radius:px(1.5),inset:false}])
                                .bg(if selected { p.accs } else { p.card })
                                .child(
                                    div()
                                        .text_size(px(14.))
                                        .line_height(px(17.))
                                        .font_weight(FontWeight::BOLD)
                                        .flex().items_center().gap(px(8.))
                                        .child(div().size(px(16.)).flex_none().rounded_full().border_1().border_color(if selected{p.acc}else{p.tx3})
                                            .flex().items_center().justify_center().when(selected,|d|d.child(div().size(px(8.)).rounded_full().bg(p.acc))))
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .whitespace_normal()
                                        .mt(px(12.))
                                        .text_size(px(12.5))
                                        .line_height(px(20.))
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
                                .text_size(px(13.5))
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
            if self.section==2 {
                let style=cx.terminal_style();
                let color=shellmate_term::colors::to_hsla;
                let font=style.font_size;
                let height=(font*style.line_height).round();
                let row=|label:&'static str,value:String| div().flex().justify_between().text_size(px(13.)).line_height(px(18.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(label)).child(div().text_color(p.tx2).child(value));
                content=content.child(div().flex_none().px(px(18.)).py(px(16.)).rounded(px(18.)).bg(p.card).flex().flex_col().gap(px(10.))
                    .child(row("字体",format!("{} {} · 中文回退 {} · 行高 {:.1}",style.font_family,f32::from(font),crate::theme::CJK_FALLBACK,style.line_height)))
                    .child(div().px(px(14.)).py(px(12.)).rounded(px(12.)).bg(color(style.palette.background)).text_color(color(style.palette.foreground))
                        .font_family(style.font_family).text_size(font).line_height(height).whitespace_nowrap().overflow_hidden().flex()
                        .child(div().text_color(color(style.palette.ansi[2])).font_weight(FontWeight::BOLD).child("deploy@prod-web-01"))
                        .child(":").child(div().text_color(color(style.palette.ansi[4])).font_weight(FontWeight::BOLD).child("~"))
                        .child("$ echo \"宽字符占两格 ✓\" | wc -m"))
                    .child(row("滚动缓冲",if self.preferences.get(P::SessionLog){"10,000 行 · 会话日志已开启"}else{"10,000 行 · 会话日志未开启"}.into())));
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
            let mut rows=div().flex_none().rounded(px(18.)).bg(p.card).px(px(18.)).py(px(4.)).flex().flex_col();
            for key in keys {
                let key = *key;
                rows = rows.child(
                    div()
                        .min_h(px(56.)).flex_none()
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
            content=content.child(rows);
        }
        div()
            .relative()
            .h((window.viewport_size().height - px(40.)).min(px(640.)))
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
