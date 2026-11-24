#[path = "settings_memory.rs"]
mod memory;
#[path = "settings_limits.rs"]
mod limits;
use crate::{security::password::PasswordInput, theme::ActivePalette};
use crate::ui::{Surface, field_input};
use futures::StreamExt;
use gpui::{
    component::{
        button::Button,
        input::InputState,
    },
    prelude::*,
    *,
};
use shellmate_ai::{Event, Provider, ReasoningLevel, RigClient};
use shellmate_store::{CredentialStore, SecretKey, SecretStore, SharedStore, ai::Settings};
use std::sync::Arc;
pub struct AiSettings {
    limits: limits::Fields,
    _subscriptions: Vec<Subscription>,
    workspace: WeakEntity<crate::workspace::Workspace>,
    db: SharedStore,
    credentials: Arc<CredentialStore>,
    provider: bool,
    base: Entity<InputState>,
    model: Entity<InputState>,
    reasoning: ReasoningLevel,
    key: Entity<PasswordInput>,
    key_saved: bool,
    replace: bool,
    save: bool,
    busy: bool,
    result: Option<String>,
    abort: Option<tokio::task::AbortHandle>,
    fixture: bool,
}
impl AiSettings {
    pub fn new(
        workspace: WeakEntity<crate::workspace::Workspace>,
        db: SharedStore,
        credentials: Arc<CredentialStore>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let s = db.borrow().ai_settings().unwrap_or_default();
        let anthropic = matches!(s.provider, Provider::Anthropic { .. });
        let model = cx.new(|cx| InputState::new(w, cx).default_value(s.model.clone()).placeholder("gpt-4o-mini / deepseek-chat / qwen…"));
        let limits = limits::Fields::new(&s, w, cx);
        let mut subscriptions = vec![cx.subscribe_in(&model, w, |s: &mut Self, _, event: &gpui::component::input::InputEvent, w, cx| {
            if matches!(event, gpui::component::input::InputEvent::Change) {
                s.reload_model_limits(w, cx);
            }
        })];
        for input in &limits.inputs { subscriptions.push(cx.subscribe(input, |_: &mut Self, _, _: &gpui::component::input::InputEvent, cx| cx.notify())); }
        Self {
            limits,
            _subscriptions: subscriptions,
            workspace,
            db,
            credentials,
            provider: anthropic,
            base: cx.new(|cx| {
                InputState::new(w, cx)
                    .default_value(s.provider.base_url())
                    .placeholder(base_placeholder(anthropic))
            }),
            model,
            reasoning: s.reasoning,
            key: cx.new(|cx| PasswordInput::new("输入 API Key", Surface::Card, w, cx)),
            key_saved: false,
            replace: false,
            save: s.save_conversations,
            busy: false,
            result: None,
            abort: None,
            fixture: crate::prototype_fixture(),
        }
    }
    #[cfg(debug_assertions)]
    pub(crate) fn fixture_advanced(&mut self, cx: &mut Context<Self>) {
        self.limits.advanced = true;
        self.limits.choices[0] = limits::Choice::Custom;
        cx.notify();
    }
    pub(crate) fn replace_key(&mut self, cx: &mut Context<Self>) {
        self.replace = true;
        cx.notify();
    }
    fn select_provider(&mut self, anthropic: bool, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.provider == anthropic { return; }
        self.provider = anthropic;
        let saved = self.db.borrow().ai_settings_for_provider(anthropic);
        let (base, reasoning) = saved.map(|saved| (saved.provider.base_url().to_owned(), saved.reasoning))
            .unwrap_or_else(|_| (base_placeholder(anthropic).into(), ReasoningLevel::Default));
        self.reasoning = reasoning;
        self.base.update(cx, |i, cx| {
            i.set_value(base, w, cx);
            i.set_placeholder(base_placeholder(anthropic), w, cx);
        });
        self.key.update(cx, |i, cx| { i.take(cx); });
        self.replace = false;
        self.model.update(cx, |i, cx| i.set_value("", w, cx));
        self.reload_model_limits(w, cx);
        self.refresh_key(cx);
    }
    fn reload_model_limits(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let values = self.values(cx);
        let saved = self.db.borrow().ai_model_settings(&values).unwrap_or(values);
        self.limits.load_model(&saved, w, cx);
        cx.notify();
    }
    fn values(&self, cx: &App) -> Settings {
        let base_url = self
            .base
            .read(cx)
            .value()
            .trim()
            .trim_end_matches('/')
            .to_owned();
        self.limits.apply(Settings {
            provider: if self.provider {
                Provider::Anthropic { base_url }
            } else {
                Provider::OpenAiCompatible { base_url }
            },
            model: self.model.read(cx).value().trim().into(),
            reasoning: self.reasoning,
            save_conversations: self.save,
            auto_memory: self.db.borrow().ai_settings().unwrap_or_default().auto_memory,
            ..Default::default()
        }, cx)
    }
    pub fn refresh_key(&mut self, cx: &mut Context<Self>) {
        if self.fixture {
            self.key_saved = true;
            cx.notify();
            return;
        }
        let account = self.values(cx).key_account();
        let expected_account = account.clone();
        let credentials = self.credentials.clone();
        let task = cx.background_executor().spawn(async move {
            credentials
                .keys()
                .map(|keys| keys.iter().any(|k| k.account() == account))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |s, cx| {
                if s.values(cx).key_account() != expected_account {
                    return;
                }
                if let Ok(saved) = result {
                    s.key_saved = saved;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    fn notify_configuration(&self, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            workspace.update(cx, |s, cx| s.sync_ai_context(cx)).ok();
        });
    }
    fn save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let settings = self.values(cx);
        self.limits.normalize_inputs(&settings, w, cx);
        if settings.provider.destination().is_err() {
            self.result = Some("Base URL 无效，请使用完整的 http(s) 地址".into());
            cx.notify();
            return;
        }
        if self.key.read(cx).expose().is_empty() {
            self.result = Some(
                if self.db.borrow_mut().save_ai_settings(&settings).is_ok() {
                    "设置已保存".into()
                } else {
                    "设置保存失败".into()
                },
            );
            self.notify_configuration(cx);
            cx.notify();
            return;
        }
        self.busy = true;
        let this = cx.entity().downgrade();
        let key = self.key.update(cx, |i, cx| i.take(cx));
        let credentials = self.credentials.clone();
        let db = self.db.clone();
        crate::security::ensure_unlocked(
            self.credentials.clone(),
            "保存 AI API Key",
            w,
            cx,
            move |ok, _, cx| {
                if !ok {
                    this.update(cx, |s, cx| {
                        s.busy = false;
                        s.result = Some("已取消，API Key 未保存".into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                let task = cx.background_executor().spawn(async move {
                    credentials
                        .set(
                            &SecretKey::Account(settings.key_account()),
                            &shellmate_core::Secret::new(key.as_str()),
                        )
                        .map_err(|_| "API Key 保存失败")?;
                    db.borrow_mut()
                        .save_ai_settings(&settings)
                        .map_err(|_| "设置保存失败")?;
                    Ok::<_, &str>(())
                });
                cx.spawn(async move |cx| {
                    let result = task.await;
                    this.update(cx, |s, cx| {
                        s.busy = false;
                        s.result = Some(
                            result.map_or_else(str::to_owned, |_| "设置与 API Key 已保存".into()),
                        );
                        s.replace = false;
                        s.notify_configuration(cx);
                        s.refresh_key(cx);
                    })
                    .ok();
                })
                .detach();
            },
        );
        cx.notify();
    }
    fn remove_key(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let account = self.values(cx).key_account();
        let credentials = self.credentials.clone();
        crate::security::ensure_unlocked(
            credentials.clone(),
            "移除 AI API Key",
            w,
            cx,
            move |ok, _, cx| {
                if !ok {
                    return;
                }
                let task = cx
                    .background_executor()
                    .spawn(async move { credentials.delete(&SecretKey::Account(account)) });
                cx.spawn(async move |cx| {
                    let result = task.await;
                    this.update(cx, |s, cx| {
                        s.result = Some(
                            if result.is_ok() {
                                "API Key 已移除"
                            } else {
                                "API Key 移除失败"
                            }
                            .into(),
                        );
                        s.refresh_key(cx);
                    })
                    .ok();
                })
                .detach();
            },
        );
    }
    fn test(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.fixture {
            self.result = Some("测试成功 · 演示响应（未连接网络）".into());
            cx.notify();
            return;
        }
        if !self.key.read(cx).expose().is_empty() {
            self.result = Some("请先保存 API Key，再测试连接".into());
            cx.notify();
            return;
        }
        let settings = self.values(cx);
        if !settings.configured() {
            self.result = Some("请先填写模型与有效的 Base URL".into());
            cx.notify();
            return;
        }
        self.busy = true;
        let this = cx.entity().downgrade();
        super::authorize_settings(
            self.db.clone(),
            self.credentials.clone(),
            settings,
            w,
            cx,
            move |result, w, cx| {
                this.update(cx, |s, cx| {
                    let config = match result {
                        Ok(c) => c,
                        Err(e) => {
                            s.busy = false;
                            s.result = Some(e.to_string());
                            cx.notify();
                            return;
                        }
                    };
                    let Some(runtime) = crate::runtime::get(cx) else {
                        s.busy = false;
                        return;
                    };
                    let join = runtime.spawn(async move {
                        let client = RigClient::new(&config)?;
                        let mut stream = client.test_connection()?;
                        while let Some(event) = stream.next().await {
                            match event {
                                Event::Done(_) => return Ok(()),
                                Event::Error(e) => return Err(e),
                                _ => {}
                            }
                        }
                        Err(shellmate_ai::AiError::InvalidResponse)
                    });
                    s.abort = Some(join.abort_handle());
                    cx.spawn_in(w, async move |this, cx| {
                        let result = join.await;
                        this.update(cx, |s, cx| {
                            s.busy = false;
                            s.abort = None;
                            s.result = Some(match result {
                                Ok(Ok(())) => "测试成功，服务商已响应".into(),
                                Ok(Err(e)) => e.to_string(),
                                Err(_) => "测试已停止".into(),
                            });
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                    cx.notify();
                })
                .ok();
            },
        );
        cx.notify();
    }
    fn clear_all(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let db = self.db.clone();
        let workspace = self.workspace.clone();
        crate::dialogs::confirm::ConfirmDialog::open(
            "清除全部 AI 对话？",
            "将永久删除本机保存的所有 AI 对话及消息，无法撤销。",
            "清除全部对话",
            true,
            move |_, cx| {
                workspace.update(cx, |s, cx| s.clear_ai_memory(cx)).ok();
                db.borrow_mut()
                    .clear_ai_conversations()
                    .map_err(|_| "无法清除对话".into())
            },
            w,
            cx,
        );
    }
}
fn base_placeholder(anthropic: bool) -> &'static str {
    if anthropic {
        "https://api.anthropic.com"
    } else {
        "https://api.openai.com/v1"
    }
}
impl Drop for AiSettings {
    fn drop(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}
impl Render for AiSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let this = cx.weak_entity();
        let provider = crate::ui::segmented("ai-provider", [(false, "OpenAI 兼容"), (true, "Anthropic")]
            .into_iter().map(|(value, label)| segment(label, self.provider == value, self.busy)).collect(),
            move |index, w, cx| { this.update(cx, |s, cx| s.select_provider(index == 1, w, cx)).ok(); }, cx);
        let mut form = settings_card(cx)
            .child(settings_row("服务商", "选择 API 协议", provider, cx))
            .child(div().py(px(12.)).child(crate::dialogs::field("Base URL", &p, field_input(&self.base, Surface::Card, true))))
            .child(div().py(px(12.)).child(crate::dialogs::field("模型", &p, field_input(&self.model, Surface::Card, true))));
        if self.key_saved && !self.replace {
            form = form.child(
                div()
                    .py(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().text_size(px(13.)).child("API Key · 已保存"))
                    .child(
                        Button::new("ai-key-replace")
                            .label("替换")
                            .on_click(cx.listener(|s, _, _, cx| s.replace_key(cx))),
                    )
                    .child(
                        Button::new("ai-key-remove")
                            .label("移除")
                            .on_click(cx.listener(|s, _, w, cx| s.remove_key(w, cx))),
                    ),
            );
        } else {
            form = form.child(div().py(px(12.)).child(crate::dialogs::field("API Key", &p, self.key.clone())));
        }
        let levels = ReasoningLevel::options(&self.values(cx).provider).to_vec();
        let items = levels.iter().map(|level| segment(level.label(), self.reasoning == *level, self.busy)).collect();
        let this = cx.weak_entity();
        let reasoning = crate::ui::segmented("ai-reasoning", items, move |index, _, cx| {
            this.update(cx, |s, cx| { if !s.busy { s.reasoning = levels[index]; cx.notify(); } }).ok();
        }, cx).max_w_full().flex_wrap();
        form = form.child(settings_row("思考等级", "等级越高，回答越慢、越耗 token", reasoning, cx));
        form = form.child(
            div()
                .py(px(12.))
                .flex()
                .justify_end()
                .gap(px(8.))
                .child(
                    crate::dialogs::button(
                        "ai-test",
                        if self.busy {
                            "处理中…"
                        } else {
                            "测试连接"
                        },
                        crate::dialogs::Tone::Plain,
                        !self.busy,
                    )
                    .on_click(cx.listener(|s, _, w, cx| s.test(w, cx))),
                )
                .child(
                    crate::dialogs::button(
                        "ai-save",
                        "保存设置",
                        crate::dialogs::Tone::Primary,
                        !self.busy,
                    )
                    .on_click(cx.listener(|s, _, w, cx| s.save(w, cx))),
                ),
        );
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(form)
            .child(self.limit_fields(cx))
            .child(
                div()
                    .p(px(18.))
                    .rounded(px(18.))
                    .bg(p.card)
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("保存对话"),
                    )
                    .child(
                        crate::ui::Toggle::new("ai-save-history")
                            .checked(self.save)
                            .color(p.ok)
                            .on_click(cx.listener(|s, v: &bool, _, cx| {
                                if s.db
                                    .borrow_mut()
                                    .set_setting(
                                        shellmate_store::ai::SAVE_CONVERSATIONS,
                                        if *v { "1" } else { "0" },
                                    )
                                    .is_ok()
                                {
                                    s.save = *v;
                                    s.notify_configuration(cx);
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div().flex().justify_end().child(
                    crate::dialogs::button(
                        "ai-clear-all",
                        "清除全部对话",
                        crate::dialogs::Tone::Plain,
                        true,
                    )
                    .text_color(p.err)
                    .on_click(cx.listener(|s, _, w, cx| s.clear_all(w, cx))),
                ),
            )
            .child(self.memory_card(cx))
            .children(
                self.result
                    .clone()
                    .map(|text| crate::dialogs::hint(text, &p)),
            )
    }
}

fn segment(label: impl Into<SharedString>, active: bool, busy: bool) -> crate::ui::SegmentItem {
    let item = crate::ui::SegmentItem::new(label, active);
    if busy { item.disabled("正在处理，请稍候") } else { item }
}
fn settings_card(cx: &App) -> Div {
    div().flex_none().px(px(18.)).py(px(4.)).rounded(px(18.)).bg(cx.palette().card).flex().flex_col()
}
/// Same label/description typography as the other settings pages. Each complete
/// control moves below its label if the row cannot fit both at the current width.
fn settings_row(label: &'static str, hint: impl Into<SharedString>, control: impl IntoElement, cx: &App) -> Div {
    let p = cx.palette();
    div().min_h(px(56.)).flex_none().py(px(12.)).flex().flex_wrap().items_center().gap(px(12.))
        .child(div().flex_1().min_w(px(180.)).max_w_full().whitespace_normal()
            .text_size(px(13.5)).line_height(px(16.)).font_weight(FontWeight::SEMIBOLD).child(label)
            .child(div().mt(px(4.)).text_size(px(12.)).line_height(px(18.)).font_weight(FontWeight::NORMAL).text_color(p.tx2).child(hint.into())))
        .child(div().flex_none().min_w_0().max_w_full().child(control))
}
