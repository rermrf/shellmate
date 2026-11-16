//! Credential dialogs: password, key passphrase, one-time code and keyboard-interactive challenges.

use std::path::PathBuf;

use gpui::component::checkbox::Checkbox;
use gpui::component::input::{Input, InputEvent, InputState, OtpEvent, OtpInput, OtpState};
use gpui::prelude::*;
use gpui::{App, Context, Entity, IntoElement, KeyDownEvent, Render, SharedString, Subscription, Window, div, px};
use shellmate_core::Secret;
use shellmate_ssh::{ChallengeRequest, PassphraseRequest, PasswordRequest};
use tokio::sync::oneshot;

use super::{DialogBody, Notice, Tone, actions, button, field, hint, notice, palette, stack};
use crate::connect::{Answer, looks_like_otp};

/// What a [`SecretDialog`] asks for.
enum SecretKind {
    Password { user: String, host: String, port: u16 },
    Passphrase { key: PathBuf },
}

/// Asks for a password or a private key passphrase (masked), with the keychain option.
pub struct SecretDialog {
    kind: SecretKind,
    attempt: u32,
    input: Entity<InputState>,
    save: bool,
    error: Option<SharedString>,
    reply: Option<oneshot::Sender<Option<Answer>>>,
    _subscription: Subscription,
}

impl SecretDialog {
    /// Opens the password dialog for `request`.
    pub fn open_password(
        request: PasswordRequest,
        reply: oneshot::Sender<Option<Answer>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let title = format!("连接 {}", request.host);
        let kind = SecretKind::Password { user: request.user, host: request.host, port: request.port };
        Self::open(kind, request.attempt, "输入密码", title, reply, window, cx);
    }

    /// Opens the passphrase dialog for `request`.
    pub fn open_passphrase(
        request: PassphraseRequest,
        reply: oneshot::Sender<Option<Answer>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let title = "解锁私钥".to_string();
        Self::open(
            SecretKind::Passphrase { key: request.key_path },
            request.attempt,
            "输入私钥口令",
            title,
            reply,
            window,
            cx,
        );
    }

    fn open(
        kind: SecretKind,
        attempt: u32,
        placeholder: &'static str,
        title: String,
        reply: oneshot::Sender<Option<Answer>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let body = cx.new(|cx| {
            let input = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(placeholder));
            let subscription = cx.subscribe_in(&input, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit(window, cx);
                }
            });
            Self { kind, attempt, input, save: true, error: None, reply: Some(reply), _subscription: subscription }
        });
        super::open(window, cx, title, 440., body);
    }

    fn finish(&mut self, answer: Option<Answer>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(answer);
        }
    }
}

impl DialogBody for SecretDialog {
    fn confirm(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        let value = self.input.read(cx).value().to_string();
        if value.is_empty() {
            self.error = Some(match self.kind {
                SecretKind::Password { .. } => "请输入密码".into(),
                SecretKind::Passphrase { .. } => "请输入口令".into(),
            });
            cx.notify();
            return false;
        }
        self.finish(Some(Answer { secret: Secret::new(value), save: self.save }));
        true
    }

    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.finish(None);
    }

    fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }
}

impl Render for SecretDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (subtitle, caption, retry_error, hint_text) = match &self.kind {
            SecretKind::Password { user, host, port } => {
                (format!("{user}@{host}:{port}"), "密码", "密码错误，请重试", "凭据存储中没有此主机的密码")
            }
            SecretKind::Passphrase { key } => {
                (key.display().to_string(), "私钥口令", "口令不正确，请重试", "私钥已加密，凭据存储中未找到口令")
            }
        };
        let error = self.error.clone().or_else(|| (self.attempt > 0).then(|| retry_error.into()));
        stack()
            .child(div().font_family(crate::theme::MONO_FONT).text_size(px(12.)).text_color(p.tx2).child(subtitle))
            .children((self.attempt == 0).then(|| hint(hint_text, &p)))
            .child(field(caption, &p, Input::new(&self.input).mask_toggle()))
            .children(error.map(|text| notice(Notice::Error, text, &p)))
            .child(Checkbox::new("save-secret").label("保存到当前凭据存储，下次自动填写").checked(self.save).on_click(
                cx.listener(|this, checked: &bool, _, cx| {
                    this.save = *checked;
                    cx.notify();
                }),
            ))
            .child(actions(vec![
                button("secret-cancel", "取消", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                button("secret-ok", "连接", Tone::Primary, true)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            ]))
    }
}

/// How a [`ChallengeDialog`] collects answers.
enum Mode {
    /// A 6-digit one-time code.
    Otp(Entity<OtpState>),
    /// One field per prompt, masked unless the server wants the echo.
    Fields(Vec<Entity<InputState>>),
}

/// Asks the questions of one keyboard-interactive round.
pub struct ChallengeDialog {
    request: ChallengeRequest,
    mode: Mode,
    error: Option<SharedString>,
    reply: Option<oneshot::Sender<Option<Vec<Secret>>>>,
    _subscriptions: Vec<Subscription>,
}

/// Length of the one-time codes the OTP input collects.
const OTP_LENGTH: usize = 6;

impl ChallengeDialog {
    /// Opens the dialog for `request`; a single verification-code prompt gets the 6-digit input.
    pub fn open(
        request: ChallengeRequest,
        reply: oneshot::Sender<Option<Vec<Secret>>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let otp = matches!(request.prompts.as_slice(), [only] if looks_like_otp(&only.text));
        let title = if otp {
            "两步验证".to_string()
        } else if request.name.trim().is_empty() {
            "身份验证".to_string()
        } else {
            request.name.trim().to_string()
        };
        let body = cx.new(|cx| {
            let mut subscriptions = Vec::new();
            let mode = if otp {
                let state = cx.new(|cx| OtpState::new(OTP_LENGTH, window, cx));
                subscriptions.push(cx.subscribe_in(&state, window, |this: &mut Self, _, event: &OtpEvent, _, cx| {
                    if matches!(event, OtpEvent::Change) {
                        this.error = None;
                        cx.notify();
                    }
                }));
                Mode::Otp(state)
            } else {
                let inputs: Vec<Entity<InputState>> = request
                    .prompts
                    .iter()
                    .map(|prompt| cx.new(|cx| InputState::new(window, cx).masked(!prompt.echo)))
                    .collect();
                for input in &inputs {
                    subscriptions.push(cx.subscribe_in(
                        input,
                        window,
                        |this: &mut Self, _, event: &InputEvent, window, cx| {
                            if let InputEvent::PressEnter { .. } = event {
                                this.submit(window, cx);
                            }
                        },
                    ));
                }
                Mode::Fields(inputs)
            };
            Self { request, mode, error: None, reply: Some(reply), _subscriptions: subscriptions }
        });
        super::open(window, cx, title, 440., body);
    }

    fn finish(&mut self, answers: Option<Vec<Secret>>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(answers);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "enter" {
            self.submit(window, cx);
            cx.stop_propagation();
        }
    }
}

impl DialogBody for ChallengeDialog {
    fn confirm(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        let answers = match &self.mode {
            Mode::Otp(state) => {
                let code = state.read(cx).value().to_string();
                if code.chars().count() != OTP_LENGTH || !code.chars().all(|c| c.is_ascii_digit()) {
                    self.error = Some(format!("验证码应为 {OTP_LENGTH} 位数字").into());
                    cx.notify();
                    return false;
                }
                vec![Secret::new(code)]
            }
            Mode::Fields(inputs) => {
                inputs.iter().map(|input| Secret::new(input.read(cx).value().to_string())).collect()
            }
        };
        self.finish(Some(answers));
        true
    }

    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.finish(None);
    }

    fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::Otp(state) => state.update(cx, |state, cx| state.focus(window, cx)),
            Mode::Fields(inputs) => {
                if let Some(first) = inputs.first() {
                    first.update(cx, |input, cx| input.focus(window, cx));
                }
            }
        }
    }
}

impl Render for ChallengeDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let request = &self.request;
        let location = format!("第 {} 项 · {}@{}:{}", request.round, request.user, request.host, request.port);
        let retry = (request.attempt > 0).then(|| SharedString::from("验证未通过，请重新输入"));
        let error = self.error.clone().or(retry);
        let mut body = stack()
            .on_key_down(cx.listener(Self::on_key_down))
            .child(div().text_size(px(12.)).text_color(p.tx2).child(location));
        if !request.instructions.trim().is_empty() {
            body = body.child(hint(request.instructions.trim().to_string(), &p));
        }
        body = match &self.mode {
            Mode::Otp(state) => body.child(hint("打开验证器 App 查看 6 位动态码", &p)).child(field(
                request.prompts[0].text.trim().trim_end_matches(':').to_string(),
                &p,
                div().flex().justify_center().py(px(4.)).child(OtpInput::new(state).groups(2)),
            )),
            Mode::Fields(inputs) => {
                let mut body = body;
                for (prompt, input) in request.prompts.iter().zip(inputs) {
                    let label = prompt.text.trim().trim_end_matches(':').trim().to_string();
                    let label = if label.is_empty() { "回答".to_string() } else { label };
                    let control = if prompt.echo { Input::new(input) } else { Input::new(input).mask_toggle() };
                    body = body.child(field(label, &p, control));
                }
                body
            }
        };
        body.children(error.map(|text| notice(Notice::Error, text, &p))).child(actions(vec![
            button("challenge-cancel", "取消", Tone::Plain, true)
                .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
            button("challenge-ok", "验证", Tone::Primary, true)
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
        ]))
    }
}
