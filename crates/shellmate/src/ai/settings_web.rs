use super::*;
use crate::ai::web;
pub(super) struct Fields {
    key: Entity<PasswordInput>,
    saved: bool,
    pub(super) replace: bool,
}
impl Fields {
    pub fn new(w: &mut Window, cx: &mut App) -> Self {
        Self {
            key: cx.new(|cx| PasswordInput::new("Tavily API Key", Surface::Card, w, cx)),
            saved: false,
            replace: false,
        }
    }
}
impl AiSettings {
    pub(super) fn refresh_web_key(&mut self, cx: &mut Context<Self>) {
        if self.fixture {
            self.web.saved = true;
            return;
        }
        let credentials = self.credentials.clone();
        let job = cx.background_executor().spawn(async move {
            credentials
                .keys()
                .map(|keys| keys.iter().any(|k| k.account() == web::KEY))
        });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |s, cx| {
                if let Ok(saved) = result {
                    s.web.saved = saved;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    fn save_web_key(&mut self, remove: bool, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let key = self.web.key.update(cx, |i, cx| i.take(cx));
        if !remove && key.trim().is_empty() {
            self.result = Some("请输入 Tavily API Key".into());
            cx.notify();
            return;
        }
        self.busy = true;
        let credentials = self.credentials.clone();
        let this = cx.entity().downgrade();
        crate::security::ensure_unlocked(
            credentials.clone(),
            if remove {
                "移除搜索 API Key"
            } else {
                "保存搜索 API Key"
            },
            w,
            cx,
            move |ok, _, cx| {
                if !ok {
                    this.update(cx, |s, cx| {
                        s.busy = false;
                        s.result = Some("搜索密钥操作已取消".into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                let job = cx.background_executor().spawn(async move {
                    let account = SecretKey::Account(web::KEY.into());
                    if remove {
                        credentials.delete(&account)
                    } else {
                        credentials.set(&account, &shellmate_core::Secret::new(key.trim()))
                    }
                });
                cx.spawn(async move |cx| {
                    let result = job.await;
                    this.update(cx, |s, cx| {
                        s.busy = false;
                        s.web.replace = false;
                        s.result = Some(
                            if result.is_ok() {
                                if remove {
                                    "搜索 API Key 已移除"
                                } else {
                                    "搜索 API Key 已保存"
                                }
                            } else {
                                "搜索 API Key 操作失败"
                            }
                            .into(),
                        );
                        s.refresh_web_key(cx);
                        s.notify_configuration(cx);
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            },
        );
        cx.notify();
    }
    pub(super) fn web_card(&self, cx: &mut Context<Self>) -> Div {
        let p = cx.palette().clone();
        let enabled = self
            .db
            .borrow()
            .setting(web::ENABLED)
            .ok()
            .flatten()
            .as_deref()
            == Some("1");
        let fallback = self
            .db
            .borrow()
            .setting(web::FALLBACK)
            .ok()
            .flatten()
            .as_deref()
            != Some("0");
        let mut card=settings_card(cx).child(settings_row("网页搜索与读取","主代理和子代理可查询公开资料，读取网页正文",crate::ui::Toggle::new("ai-web-enabled").checked(enabled).color(p.ok).on_click(cx.listener(|s,on:&bool,_,cx|{if s.db.borrow_mut().set_setting(web::ENABLED,if *on{"1"}else{"0"}).is_err(){s.result=Some("网页设置保存失败".into());}s.notify_configuration(cx);cx.notify();})),cx))
            .child(div().py(px(8.)).text_size(px(12.)).text_color(p.tx2).whitespace_normal().child("搜索词发送给 Tavily；启用备用搜索后，Tavily 暂不可用时会发送给 DuckDuckGo。网页由本机直接读取，不发送整段对话。"));
        if self.web.saved && !self.web.replace {
            card = card.child(
                div()
                    .py(px(10.))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .child("Tavily API Key · 已保存"),
                    )
                    .child(
                        crate::dialogs::button(
                            "web-key-replace",
                            "替换",
                            crate::dialogs::Tone::Plain,
                            true,
                        )
                        .on_click(cx.listener(|s, _, _, cx| {
                            s.web.replace = true;
                            cx.notify();
                        })),
                    )
                    .child(
                        crate::dialogs::button(
                            "web-key-remove",
                            "移除",
                            crate::dialogs::Tone::Danger,
                            true,
                        )
                        .on_click(cx.listener(|s, _, w, cx| s.save_web_key(true, w, cx))),
                    ),
            );
        } else {
            card = card
                .child(div().py(px(10.)).child(crate::dialogs::field(
                    "Tavily API Key",
                    &p,
                    self.web.key.clone(),
                )))
                .child(
                    div().flex().justify_end().child(
                        crate::dialogs::button(
                            "web-key-save",
                            "保存搜索密钥",
                            crate::dialogs::Tone::Plain,
                            !self.busy,
                        )
                        .on_click(cx.listener(|s, _, w, cx| s.save_web_key(false, w, cx))),
                    ),
                );
        }
        let card = card.child(settings_row("DuckDuckGo 备用搜索","Tavily 限流、额度受限或网络失败时使用；可能受验证码限制",crate::ui::Toggle::new("ai-web-fallback").checked(fallback).color(p.ok).on_click(cx.listener(|s,on:&bool,_,cx|{if s.db.borrow_mut().set_setting(web::FALLBACK,if *on{"1"}else{"0"}).is_err(){s.result=Some("备用搜索设置保存失败".into());}cx.notify();})),cx))
            .child(div().py(px(8.)).text_size(px(11.5)).text_color(p.tx3).child("使用 Tavily Basic Search。额度与计费由 Tavily 账户决定，Shellmate 不自动充值。"));
        div()
            .flex_none()
            .mt(px(4.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(section_title("网页搜索", &p))
            .child(card)
    }
}
