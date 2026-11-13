//! Host key confirmation: first connection to a host, and a key that changed.

use gpui::component::checkbox::Checkbox;
use gpui::prelude::*;
use gpui::{App, Context, FontWeight, IntoElement, Render, Window, div, px};
use shellmate_core::FingerprintState;
use shellmate_ssh::HostKeyRequest;
use tokio::sync::oneshot;

use super::{DialogBody, Notice, Tone, actions, button, hint, mono, notice, palette, stack};

/// Asks whether to trust a host key.
pub struct HostKeyDialog {
    request: HostKeyRequest,
    reply: Option<oneshot::Sender<bool>>,
    /// The "I confirm the server replaced its key" box of the changed-key warning.
    acknowledged: bool,
}

impl HostKeyDialog {
    /// Opens the dialog; `reply` receives the decision (dropping it counts as "no").
    pub fn open(request: HostKeyRequest, reply: oneshot::Sender<bool>, window: &mut Window, cx: &mut App) {
        let title = match request.state {
            FingerprintState::Changed => format!("{} 的主机密钥已变更", request.host),
            _ => format!("确认 {} 的主机指纹", request.host),
        };
        let body = cx.new(|_| Self { request, reply: Some(reply), acknowledged: false });
        super::open(window, cx, title, 500., body);
    }

    fn answer(&mut self, trust: bool) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(trust);
        }
    }

    fn can_confirm(&self) -> bool {
        self.request.state != FingerprintState::Changed || self.acknowledged
    }

    fn render_new(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = palette(cx);
        let key = &self.request.key;
        let row = |label: &'static str, value: String, strong: bool| {
            div()
                .flex()
                .gap(px(14.))
                .child(div().w(px(64.)).flex_none().text_size(px(12.5)).text_color(p.tx2).child(label))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(crate::theme::MONO_FONT)
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .when(strong, |d| d.font_weight(FontWeight::SEMIBOLD))
                        .text_color(p.tx)
                        .child(value),
                )
        };
        let mut details = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(14.))
            .rounded(px(12.))
            .bg(p.card2)
            .child(row("地址", format!("{}:{}", self.request.host, self.request.port), false))
            .child(row("密钥类型", format!("{} · {}", key.algorithm, key.short_algorithm()), false))
            .child(row("指纹", key.fingerprint.clone(), true));
        for other in &self.request.recorded {
            details = details.child(row("已有记录", format!("{} {}", other.algorithm, other.fingerprint), false));
        }
        stack()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .px(px(8.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .bg(p.accs)
                            .text_color(p.acc)
                            .text_size(px(11.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("首次连接"),
                    )
                    .child(div().text_size(px(12.)).text_color(p.tx3).child("known_hosts 中没有此主机")),
            )
            .child(hint("请通过可信渠道（控制台、管理员）核对下面的指纹，一致后再继续。", &p))
            .child(details)
            .child(notice(Notice::Info, "信任后会写入 ~/.ssh/known_hosts，与 OpenSSH 共用。", &p))
            .child(actions(vec![
                button("hostkey-cancel", "取消", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                button("hostkey-trust", "信任并连接", Tone::Primary, true)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            ]))
    }

    fn render_changed(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = palette(cx);
        let key = &self.request.key;
        let recorded: Vec<String> =
            self.request.recorded.iter().map(|k| format!("{}  {}", k.algorithm, k.fingerprint)).collect();
        let labelled = |label: String, color, lines: Vec<String>| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(p.tx2).child(label))
                .children(lines.into_iter().map(|line| mono(line, &p, color)))
        };
        let acknowledged = self.acknowledged;
        stack()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(14.))
                    .rounded(px(12.))
                    .bg(p.errs)
                    .child(div().text_size(px(12.5)).font_weight(FontWeight::BOLD).text_color(p.err).child("安全警告 · 连接已暂停"))
                    .child(
                        div().text_size(px(13.)).line_height(px(20.)).text_color(p.tx).child(format!(
                            "服务器 {}:{} 提供的密钥与 ~/.ssh/known_hosts 中的记录不一致。可能是服务器重装，也可能有人正在进行中间人攻击。确认前不会发送任何凭据。",
                            self.request.host, self.request.port
                        )),
                    ),
            )
            .child(labelled("已记录".to_string(), p.err, recorded))
            .child(labelled(format!("本次提供 · {}", key.algorithm), p.ok, vec![key.fingerprint.clone()]))
            .child(
                Checkbox::new("hostkey-ack")
                    .label("我确认服务器已更换密钥")
                    .checked(acknowledged)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.acknowledged = *checked;
                        cx.notify();
                    })),
            )
            .child(notice(Notice::Warn, "只有在向服务器管理员核实过之后才应继续。确认后，known_hosts 中旧的记录会被替换。", &p))
            .child(actions(vec![
                button("hostkey-cancel", "断开连接", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                button("hostkey-replace", "替换并连接", Tone::Danger, acknowledged)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            ]))
    }
}

impl DialogBody for HostKeyDialog {
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        if !self.can_confirm() {
            return false;
        }
        self.answer(true);
        true
    }

    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.answer(false);
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
