use crate::{security::password::PasswordInput, theme::ActivePalette};
use futures::StreamExt;
use gpui::{
    component::{
        Disableable as _,
        button::Button,
        input::{Input, InputState},
    },
    prelude::*,
    *,
};
use shellmate_ai::{Event, Provider, ReasoningLevel, RigClient};
use shellmate_store::{CredentialStore, SecretKey, SecretStore, SharedStore, ai::Settings};
use std::sync::Arc;
pub struct AiSettings {
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
        Self {
            workspace,
            db,
            credentials,
            provider: anthropic,
            base: cx.new(|cx| {
                InputState::new(w, cx)
                    .default_value(s.provider.base_url())
                    .placeholder(base_placeholder(anthropic))
            }),
            model: cx.new(|cx| {
                InputState::new(w, cx)
                    .default_value(s.model)
                    .placeholder("gpt-4o-mini / deepseek-chat / qwen…")
            }),
            reasoning: s.reasoning,
            key: cx.new(|cx| PasswordInput::new("输入 API Key", cx)),
            key_saved: false,
            replace: false,
            save: s.save_conversations,
            busy: false,
            result: None,
            abort: None,
            fixture: crate::prototype_fixture(),
        }
    }
    fn values(&self, cx: &App) -> Settings {
        let base_url = self
            .base
            .read(cx)
            .value()
            .trim()
            .trim_end_matches('/')
            .to_owned();
        Settings {
            provider: if self.provider {
                Provider::Anthropic { base_url }
            } else {
                Provider::OpenAiCompatible { base_url }
            },
            model: self.model.read(cx).value().trim().into(),
            reasoning: self.reasoning,
            save_conversations: self.save,
        }
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
        let card = div()
            .p(px(18.))
            .rounded(px(18.))
            .bg(p.card)
            .flex()
            .flex_col()
            .gap(px(14.));
        let mut form = card.child(crate::dialogs::field(
            "服务商",
            &p,
            div().flex().gap(px(8.)).children(
                [(false, "OpenAI 兼容"), (true, "Anthropic")]
                    .into_iter()
                    .enumerate()
                    .map(|(i, (anthropic, label))| {
                        Button::new(("ai-provider", i))
                            .label(label)
                            .disabled(self.busy)
                            .when(self.provider == anthropic, |b| {
                                b.bg(p.accs).text_color(p.acc)
                            })
                            .on_click(cx.listener(move |s, _, w, cx| {
                                if s.provider == anthropic {
                                    return;
                                }
                                s.provider = anthropic;
                                let saved = s.db.borrow().ai_settings_for_provider(anthropic);
                                let (base, reasoning) = saved
                                    .map(|saved| {
                                        (saved.provider.base_url().to_owned(), saved.reasoning)
                                    })
                                    .unwrap_or_else(|_| {
                                        (
                                            base_placeholder(anthropic).into(),
                                            ReasoningLevel::Default,
                                        )
                                    });
                                s.reasoning = reasoning;
                                s.base.update(cx, |i, cx| {
                                    i.set_value(base, w, cx);
                                    i.set_placeholder(base_placeholder(anthropic), w, cx);
                                });
                                s.key.update(cx, |i, cx| {
                                    i.take(cx);
                                });
                                s.replace = false;
                                s.model.update(cx, |i, cx| i.set_value("", w, cx));
                                s.refresh_key(cx);
                            }))
                    }),
            ),
        ));
        form = form.child(crate::dialogs::field(
            "Base URL",
            &p,
            Input::new(&self.base),
        ));
        form = form
            .child(crate::dialogs::field("模型", &p, Input::new(&self.model)))
            .child(crate::dialogs::field(
                "思考等级",
                &p,
                div().flex().flex_wrap().gap(px(8.)).children(
                    ReasoningLevel::options(&self.values(cx).provider)
                        .iter()
                        .copied()
                        .enumerate()
                        .map(|(index, level)| {
                            Button::new(("ai-reasoning", index))
                                .label(level.label())
                                .disabled(self.busy)
                                .when(self.reasoning == level, |b| b.bg(p.accs).text_color(p.acc))
                                .on_click(cx.listener(move |s, _, _, cx| {
                                    s.reasoning = level;
                                    cx.notify();
                                }))
                        }),
                ),
            ))
            .child(div().text_size(px(12.)).text_color(p.tx2).child(
                "等级越高，回答越慢、越耗 token",
            ));
        if self.key_saved && !self.replace {
            form = form.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().text_size(px(13.)).child("API Key · 已保存"))
                    .child(
                        Button::new("ai-key-replace")
                            .label("替换")
                            .on_click(cx.listener(|s, _, _, cx| {
                                s.replace = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("ai-key-remove")
                            .label("移除")
                            .on_click(cx.listener(|s, _, w, cx| s.remove_key(w, cx))),
                    ),
            );
        } else {
            form = form.child(crate::dialogs::field("API Key", &p, self.key.clone()));
        }
        form = form.child(
            div()
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
            .children(
                self.result
                    .clone()
                    .map(|text| crate::dialogs::hint(text, &p)),
            )
    }
}
