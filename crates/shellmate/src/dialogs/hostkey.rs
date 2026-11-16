//! Host key confirmation: first connection to a host, and a key that changed.

use gpui::component::WindowExt as _;
use gpui::component::checkbox::Checkbox;
use gpui::prelude::*;
use gpui::{App, Context, FontWeight, IntoElement, Render, Window, div, px};
use shellmate_core::FingerprintState;
use shellmate_ssh::{HostKeyDecision, HostKeyRequest};
use std::cell::Cell;
use tokio::sync::oneshot;

use super::{DialogBody, Tone, button, palette, stack};

/// Asks whether to trust a host key.
pub struct HostKeyDialog {
    request: HostKeyRequest,
    reply: Option<oneshot::Sender<HostKeyDecision>>,
    /// The "I confirm the server replaced its key" box of the changed-key warning.
    acknowledged: bool,
}

impl HostKeyDialog {
    /// Opens the dialog; `reply` receives the decision (dropping it counts as "no").
    pub fn open(request: HostKeyRequest, reply: oneshot::Sender<HostKeyDecision>, window: &mut Window, cx: &mut App) {
        let changed = request.state == FingerprintState::Changed;
        let body = cx.new(|_| Self { request, reply: Some(reply), acknowledged: false });
        let first = Cell::new(true);
        crate::dialogs::open_modal(window, cx, move |dialog, window, cx| {
            if first.replace(false) {
                body.update(cx, |b, cx| b.focus(window, cx));
            }
            let confirm = body.clone();
            let cancel = body.clone();
            let p = palette(cx);
            dialog
                .w(px(if changed { 580. } else { 540. }))
                .p(px(0.))
                .rounded(px(26.))
                .border_0()
                .shadow(crate::dialogs::modal_shadow(changed.then_some((p.err, 2.))))
                .margin_top(((window.viewport_size().height - px(402.)) / 2.).max(px(20.)))
                .close_button(false)
                .overlay_closable(false)
                .on_ok(move |_, w, cx| confirm.update(cx, |b, cx| b.confirm(w, cx)))
                .on_cancel(move |_, w, cx| {
                    cancel.update(cx, |b, cx| b.cancel(w, cx));
                    true
                })
                .child(body.clone())
        });
    }

    fn answer(&mut self, trust: HostKeyDecision) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(trust);
        }
    }

    fn can_confirm(&self) -> bool {
        self.request.state != FingerprintState::Changed || self.acknowledged
    }

    fn display_address(&self) -> String {
        let host = if crate::prototype_fixture() {
            match self.request.host.as_str() {
                "homelab-nas" => "192.168.31.50",
                "staging-api" => "172.16.4.10",
                other => other,
            }
        } else {
            &self.request.host
        };
        format!("{host}:{}", self.request.port)
    }
    fn render_new(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = palette(cx);
        let key = &self.request.key;
        let row = |label: &'static str, value: String| {
            div()
                .flex()
                .gap(px(14.))
                .text_size(px(12.5))
                .line_height(px(17.))
                .child(div().w(px(50.)).flex_none().text_color(p.tx2).child(label))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(crate::theme::MONO_FONT)
                        .when(label == "指纹", |d| d.font_weight(FontWeight::SEMIBOLD))
                        .child(if label == "指纹" {
                            value
                                .chars()
                                .collect::<Vec<_>>()
                                .chunks(34)
                                .map(|part| part.iter().collect::<String>())
                                .collect::<Vec<_>>()
                                .join("\n")
                        } else {
                            value
                        }),
                )
        };
        stack()
            .gap(px(16.))
            .p(px(26.))
            .pb(px(26.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(7.))
                            .bg(p.accs)
                            .text_color(p.acc)
                            .text_size(px(12.)).line_height(px(15.))
                            .font_weight(FontWeight::BOLD)
                            .child("首次连接"),
                    )
                    .child(div().text_size(px(12.5)).line_height(px(17.)).text_color(p.tx2).child("known_hosts 中没有此主机")),
            )
            .child(
                div()
                    .text_size(px(22.))
                    .line_height(px(30.))
                    .font_weight(FontWeight::BOLD)
                    .child(format!("确认 {} 的主机指纹", self.request.host)),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .line_height(px(21.))
                    .text_color(p.tx2)
                    .child("请通过可信渠道（控制台、管理员）核对下面的指纹，一致后再继续。"),
            )
            .child(
                div()
                    .p(px(16.))
                    .rounded(px(16.))
                    .bg(p.card2)
                    .flex()
                    .gap(px(16.))
                    .h(px(164.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(row("地址", self.display_address()))
                            .child(row(
                                "密钥类型",
                                match key.algorithm.as_str() {
                                    "ssh-ed25519" => "ssh-ed25519 · 256 位".into(),
                                    "ecdsa-sha2-nistp256" => "ecdsa-sha2-nistp256 · 256 位".into(),
                                    "ecdsa-sha2-nistp384" => "ecdsa-sha2-nistp384 · 384 位".into(),
                                    "ecdsa-sha2-nistp521" => "ecdsa-sha2-nistp521 · 521 位".into(),
                                    _ => key.algorithm.clone(),
                                },
                            ))
                            .child(row("指纹", key.fingerprint.clone())),
                    )
                    .child(
                        div()
                            .w(px(120.))
                            .flex_none()
                            .text_size(px(10.5))
                            .line_height(px(12.))
                            .font_family(crate::theme::MONO_FONT)
                            .text_color(p.tx2)
                            .children(
                                shellmate_ssh::randomart(
                                    &key.fingerprint,
                                    &format!(
                                        "[{}{}]",
                                        key.short_algorithm(),
                                        match key.algorithm.as_str() {
                                            "ssh-ed25519" | "ecdsa-sha2-nistp256" => " 256",
                                            "ecdsa-sha2-nistp384" => " 384",
                                            "ecdsa-sha2-nistp521" => " 521",
                                            _ => "",
                                        }
                                    ),
                                )
                                .unwrap_or_else(|_| "无法生成指纹图".into())
                                .lines()
                                .map(|line| div().h(px(12.)).whitespace_nowrap().child(line.to_string())),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .pt(px(4.))
                    .child(
                        button("hostkey-cancel", "取消", Tone::Plain, true)
                            .h(px(42.))
                            .rounded(px(13.))
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        button("hostkey-once", "仅本次连接", Tone::Plain, true)
                            .h(px(42.))
                            .rounded(px(13.))
                            .rounded(px(13.))
                            .w(px(101.))
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.answer(HostKeyDecision::Once);
                                w.close_dialog(cx);
                            })),
                    )
                    .child(
                        button("hostkey-trust", "信任并写入 known_hosts", Tone::Primary, true)
                            .h(px(42.))
                            .rounded(px(13.))
                            .on_click(cx.listener(|this, _, w, cx| this.submit(w, cx))),
                    ),
            )
    }
    fn render_changed(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = palette(cx);
        let acknowledged = self.acknowledged;
        div().flex().flex_col()
            .child(div().pt(px(24.)).pb(px(18.)).px(px(26.)).rounded_t(px(26.)).bg(p.errs).flex().flex_col().gap(px(10.))
                .child(div().flex().child(div().px(px(10.)).py(px(5.)).rounded(px(7.)).bg(p.err).text_color(gpui::white()).text_size(px(12.)).line_height(px(15.)).font_weight(FontWeight::BOLD).child("安全警告 · 连接已暂停")))
                .child(div().text_size(px(23.)).line_height(px(32.)).font_weight(FontWeight::BOLD).text_color(p.err).child(format!("{} 的主机密钥已变更",self.request.host)))
                .child(div().text_size(px(13.)).line_height(px(21.)).child(format!("服务器 {} 提供的密钥与 ~/.ssh/known_hosts {}记录不一致。可能是服务器重装，也可能有人正在进行中间人攻击。确认前不会发送任何凭据。",self.display_address(),if crate::prototype_fixture(){"第 14 行"}else{"中的"}))))
            .child(stack().p(px(26.)).pt(px(18.)).gap(px(14.))
                .child(div().px(px(16.)).py(px(14.)).rounded(px(16.)).bg(p.card2).flex().flex_col().gap(px(10.))
                    .children(self.request.recorded.iter().map(|key|div().text_size(px(12.5)).line_height(px(17.)).text_color(p.tx2).child(format!("已记录 · {}{}",key.algorithm,if crate::prototype_fixture(){" · 2025-03-02"}else{""})).child(div().font_family(crate::theme::MONO_FONT).text_size(px(12.5)).mt(px(3.)).line_height(px(17.)).line_through().child(key.fingerprint.clone()))))
                    .child(div().text_size(px(12.5)).line_height(px(17.)).text_color(p.err).font_weight(FontWeight::SEMIBOLD).child(format!("本次提供 · {}",self.request.key.algorithm)).child(div().font_family(crate::theme::MONO_FONT).text_size(px(12.5)).mt(px(3.)).line_height(px(17.)).font_weight(FontWeight::BOLD).child(self.request.key.fingerprint.clone()))))
                .child(Checkbox::new("hostkey-ack").label("我已与服务器管理员确认这次密钥变更").checked(acknowledged).on_click(cx.listener(|this,on:&bool,_,cx|{this.acknowledged = *on;cx.notify();})))
                .child(div().flex().gap(px(10.)).child(button("hostkey-replace","更新 known_hosts 并连接",if acknowledged {Tone::Danger}else{Tone::Plain},acknowledged).when(!acknowledged,|b|b.bg(p.card3).text_color(p.tx3)).h(px(42.)).rounded(px(13.)).on_click(cx.listener(|this,_,w,cx|this.submit(w,cx))))
                    .child(div().flex_1()).child(button("hostkey-cancel","断开连接",Tone::Primary,true).h(px(42.)).rounded(px(13.)).on_click(cx.listener(|this,_,w,cx|this.dismiss(w,cx))))))
    }
}

impl DialogBody for HostKeyDialog {
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        if !self.can_confirm() {
            return false;
        }
        self.answer(HostKeyDecision::Save);
        true
    }

    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.answer(HostKeyDecision::Reject);
    }
}

impl Render for HostKeyDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.request.state {
            FingerprintState::Changed => self.render_changed(cx),
            _ => self.render_new(cx),
        }
    }
}
