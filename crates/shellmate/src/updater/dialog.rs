use super::{Phase, Updater, check_now, percent, restart, start_download};
use crate::{
    ai::markdown,
    dialogs::{self, DialogBody, Notice, Tone},
    files::format::{local_broken, mtime_text, now_secs},
    theme::ActivePalette,
    ui::Toggle,
    update,
};
use gpui::{
    App, Context, FontWeight, Render, Window,
    component::WindowExt as _,
    div, img,
    prelude::*,
    px, relative,
};

pub fn manual(window: &mut Window, cx: &mut App) {
    check_now(true, cx);
    open(window, cx);
}

pub fn open(window: &mut Window, cx: &mut App) {
    cx.global_mut::<Updater>().check_target();
    let body = cx.new(|_| UpdateDialog {
        notes: Default::default(),
    });
    dialogs::open(window, cx, "软件更新", 460., body);
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1_000_000.
}

struct UpdateDialog {
    notes: markdown::MarkdownCache,
}

fn page(cx: &mut App) {
    if crate::prototype_fixture() {
        return;
    }
    let page = cx
        .global::<Updater>()
        .release
        .as_ref()
        .map_or(update::DOWNLOAD_PAGE, |r| r.page.as_str())
        .to_owned();
    cx.open_url(&page);
}

fn primary(cx: &mut App) -> bool {
    match cx.global::<Updater>().phase {
        Phase::Available => start_download(cx),
        Phase::Ready => restart(cx),
        Phase::Fallback(_) => {
            page(cx);
            return true;
        }
        Phase::Idle | Phase::CheckFailed(_) => check_now(true, cx),
        Phase::Error(_) => start_download(cx),
        Phase::Current => return true,
        _ => {}
    }
    false
}

impl DialogBody for UpdateDialog {
    fn confirm(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        primary(cx)
    }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}

impl Render for UpdateDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = cx.global::<Updater>();
        let p = cx.palette();
        let mut body = dialogs::stack();
        let mut buttons = Vec::new();
        let action = |id, label, tone| dialogs::button(id, label, tone, true);
        let submit = |label| {
            action("update-submit", label, Tone::Primary)
                .on_click(cx.listener(|s, _, w, cx| s.submit(w, cx)))
        };
        let close = |label| {
            action("update-later", label, Tone::Plain)
                .on_click(cx.listener(|s, _, w, cx| s.dismiss(w, cx)))
        };
        if let Some(r) = s
            .release
            .as_ref()
            .filter(|_| !matches!(s.phase, Phase::Checking | Phase::CheckFailed(_)))
        {
            let mut details = format!(
                "当前版本 {} · 发布于 {}",
                update::VERSION,
                r.published.split('T').next().unwrap_or(&r.published)
            );
            if let Some(asset) = r.asset(cfg!(target_arch = "aarch64")) {
                let arch = if asset.name.ends_with("-arm64.dmg") {
                    "arm64"
                } else {
                    "通用"
                };
                details.push_str(&format!(" · {arch} 版 {:.1} MB", mb(asset.size)));
            }
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        img(if cx.is_dark() {
                            "dock-dark.png"
                        } else {
                            "dock-light.png"
                        })
                        .size(px(44.)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(18.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("Shellmate {} 可用", r.version)),
                            )
                            .child(dialogs::hint(details, p)),
                    ),
            );
            if !r.body.is_empty() {
                let notes = self.notes.text(&r.name, (0, 0), &r.body, &[]);
                body = body.child(
                    div()
                        .id("update-release-notes")
                        .max_h(px(280.))
                        .overflow_y_scroll()
                        .child(markdown::view("update-markdown".into(), notes, window, cx)),
                );
            }
        }
        match &s.phase {
            Phase::Idle => {
                body = body.child(dialogs::hint("尚无可用更新", p));
                buttons.push(submit("检查更新"));
            }
            Phase::Current => {
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(
                            img(if cx.is_dark() {
                                "dock-dark.png"
                            } else {
                                "dock-light.png"
                            })
                            .size(px(44.)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(px(18.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("已是最新版本"),
                                )
                                .child(dialogs::hint(
                                    format!("Shellmate {} · {}", update::VERSION, last_check(s.prefs.last)),
                                    p,
                                )),
                        ),
                );
                buttons.push(submit("关闭"));
            }
            Phase::Checking => body = body.child(dialogs::hint("正在检查更新…", p)),
            Phase::Available => {
                buttons.push(action("update-skip", "跳过这个版本", Tone::Plain).on_click(
                    |_, w, cx| {
                        let s = cx.global_mut::<Updater>();
                        if let Some(r) = &s.release
                            && let Err(e) = s.prefs.skip(r.version.to_string())
                        {
                            log::warn!("skip update: {e}");
                            w.push_notification(
                                gpui::component::notification::Notification::error(
                                    "保存跳过版本失败",
                                ),
                                cx,
                            );
                        } else {
                            s.release = None;
                            s.phase = Phase::Idle;
                            w.close_dialog(cx);
                        }
                        cx.refresh_windows();
                    },
                ));
                buttons.push(close("稍后"));
                buttons.push(submit("下载并更新"));
            }
            Phase::Downloading(n, total) => {
                body = body
                    .child(
                        div()
                            .w_full()
                            .h(px(6.))
                            .rounded(px(3.))
                            .bg(p.card3)
                            .overflow_hidden()
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(percent(*n, *total) as f32 / 100.))
                                    .rounded(px(3.))
                                    .bg(p.acc),
                            ),
                    )
                    .child(dialogs::hint(
                        format!("已下载 {:.1} / {:.1} MB", mb(*n), mb(*total)),
                        p,
                    ));
                buttons.push(
                    action("update-cancel", "取消", Tone::Plain).on_click(|_, _, cx| {
                        if let Some(stop) = cx.global_mut::<Updater>().cancel.take() {
                            let _ = stop.send(());
                        }
                    }),
                );
            }
            Phase::Verifying => body = body.child(dialogs::hint("正在校验安装包…", p)),
            Phase::Ready => {
                body = body.child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("更新已准备好"),
                );
                let count: usize = crate::workspace::Hub::windows(cx)
                    .iter()
                    .filter_map(|entry| entry.workspace.upgrade().map(|w| w.read(cx).live_session_count()))
                    .sum();
                #[cfg(debug_assertions)]
                let count = if crate::prototype_fixture() { 2 } else { count };
                if count > 0 {
                    body = body.child(dialogs::notice(
                        Notice::Warn,
                        format!("重启会断开 {count} 个会话"),
                        p,
                    ));
                }
                buttons.push(
                    action("update-on-quit", "退出时安装", Tone::Plain).on_click(|_, w, cx| {
                        cx.global_mut::<Updater>().at_quit = true;
                        w.close_dialog(cx);
                        cx.refresh_windows();
                    }),
                );
                buttons.push(submit("立即重启"));
            }
            Phase::Error(message) | Phase::CheckFailed(message) => {
                body = body.child(dialogs::notice(Notice::Error, message.clone(), p));
                buttons.push(
                    action("update-page", "前往下载页", Tone::Plain).on_click(|_, _, cx| page(cx)),
                );
                buttons.push(submit("重试"));
            }
            Phase::Fallback(message) => {
                body = body.child(dialogs::notice(Notice::Info, message.clone(), p));
                buttons.push(submit("前往下载页"));
            }
        }
        body.when(!buttons.is_empty(), |d| d.child(dialogs::actions(buttons)))
    }
}

fn last_check(last: Option<i64>) -> String {
    let Some(last) = last else {
        return "从未检查".into();
    };
    if let (Some(a), Some(b)) = (local_broken(last), local_broken(now_secs()))
        && (a.year, a.month, a.day) == (b.year, b.month, b.day)
    {
        format!("上次检查：今天 {:02}:{:02}", a.hour, a.minute)
    } else {
        format!("上次检查：{}", mtime_text(Some(last)))
    }
}

pub fn settings(cx: &App) -> impl IntoElement {
    let s = cx.global::<Updater>();
    let p = cx.palette();
    let row = || {
        div()
            .min_h(px(56.))
            .py(px(12.))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .text_size(px(13.5))
    };
    let available = s.release.as_ref().map(|r| r.version.to_string());
    let checked = if matches!(s.phase, Phase::Current) {
        format!("{} · 已是最新版本", last_check(s.prefs.last))
    } else {
        last_check(s.prefs.last)
    };
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(
            div()
                .text_size(px(18.))
                .font_weight(FontWeight::SEMIBOLD)
                .child("软件更新"),
        )
        .child(
            div()
                .rounded(px(18.))
                .bg(p.card)
                .px(px(18.))
                .py(px(4.))
                .child(row().child("当前版本").child(update::VERSION))
                .child(
                    row().child("自动检查更新").child(
                        Toggle::new("update-auto")
                            .checked(s.prefs.auto)
                            .color(p.ok)
                            .on_click(|value, w, cx| {
                                if let Err(e) = cx.global_mut::<Updater>().prefs.set_auto(*value) {
                                    log::warn!("setting automatic updates: {e}");
                                    w.push_notification(
                                        gpui::component::notification::Notification::error(
                                            "保存更新设置失败",
                                        ),
                                        cx,
                                    );
                                }
                                cx.refresh_windows();
                            }),
                    ),
                )
                .child(
                    row().child(dialogs::hint(checked, p)).child(
                        dialogs::button("settings-check", "检查更新", Tone::Plain, true)
                            .on_click(|_, w, cx| manual(w, cx)),
                    ),
                )
                .when_some(available, |card, version| {
                    card.child(
                        row()
                            .child(
                                div()
                                    .text_color(p.acc)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("新版本 {version} 可用")),
                            )
                            .child(
                                dialogs::button("settings-open-update", "查看更新", Tone::Primary, true)
                                    .on_click(|_, w, cx| open(w, cx)),
                            ),
                    )
                }),
        )
}
