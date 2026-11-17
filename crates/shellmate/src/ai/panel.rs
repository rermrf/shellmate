#[path = "agent_ui.rs"]
pub(super) mod agent_ui;
use super::model::{DrawerModel, SUGGESTIONS};
use crate::{
    theme::{ActivePalette, MONO_FONT},
    workspace::Workspace,
};
use agent_ui::ActiveStep;
use futures::StreamExt;
use gpui::{
    component::{
        button::Button,
        input::{Input, InputEvent, InputState},
    },
    prelude::*,
    *,
};
use shellmate_ai::agent::{self, AgentEvent, Capabilities};
use shellmate_ai::policy::PermissionMode;
use shellmate_ai::{
    AiError, CommandCard, ContextSnapshot, Event, ProviderClient, RigClient, Risk, Turn, UserTurn,
};
use shellmate_store::{
    CredentialStore, SharedStore,
    ai::{ConversationSummary, SavedConversation},
};
use std::sync::Arc;
use std::{collections::HashSet, sync::Mutex};
pub struct SessionContext {
    pub session: Option<usize>,
    pub host: Option<String>,
    pub label: String,
    pub protocol: String,
    pub binding: Option<super::exec::Binding>,
}
pub struct Drawer {
    pub model: DrawerModel,
    workspace: WeakEntity<Workspace>,
    db: SharedStore,
    credentials: Arc<CredentialStore>,
    input: Entity<InputState>,
    _subscription: Subscription,
    id: String,
    created: i64,
    pub label: String,
    pub protocol: String,
    pub host: Option<String>,
    pub session: Option<usize>,
    provider: String,
    model_id: String,
    save_error: Option<String>,
    pending: bool,
    queued: Option<UserTurn>,
    epoch: u64,
    abort: Option<tokio::task::AbortHandle>,
    pub history: bool,
    history_rows: Vec<ConversationSummary>,
    pub configured: bool,
    pub fixture: bool,
    scroll: ScrollHandle,
    follow_bottom: bool,
    capabilities: Capabilities,
    mode: Arc<Mutex<PermissionMode>>,
    permission_menu: bool,
    binding: Option<super::exec::Binding>,
    active_step: Option<ActiveStep>,
    expanded: HashSet<(usize, usize)>,
    overflowing_output: HashSet<(usize, usize)>,
    readonly: bool,
    continuation: Option<Box<agent::Resume>>,
    resume_steps: usize,
    run_config: Option<shellmate_ai::Config>,
    next_context: Option<SessionContext>,
}
impl Drawer {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        db: SharedStore,
        credentials: Arc<CredentialStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("描述要做的事，例如：找出最占内存的 5 个进程")
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |s: &mut Self, _, event: &InputEvent, w, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    s.send_input(w, cx)
                }
            },
        );
        let configured = db.borrow().ai_settings().is_ok_and(|s| s.configured());
        Self {
            model: Default::default(),
            workspace,
            db,
            credentials,
            input,
            _subscription: subscription,
            id: shellmate_core::HostId::generate().to_string(),
            created: super::now(),
            label: "无会话".into(),
            protocol: String::new(),
            host: None,
            session: None,
            provider: String::new(),
            model_id: String::new(),
            save_error: None,
            pending: false,
            queued: None,
            epoch: 0,
            abort: None,
            history: false,
            history_rows: vec![],
            configured,
            fixture: false,
            scroll: ScrollHandle::new(),
            follow_bottom: true,
            capabilities: Capabilities::default(),
            mode: Arc::new(Mutex::new(PermissionMode::Ask)),
            permission_menu: false,
            binding: None,
            active_step: None,
            expanded: HashSet::new(),
            overflowing_output: HashSet::new(),
            readonly: false,
            continuation: None,
            resume_steps: 0,
            run_config: None,
            next_context: None,
        }
    }
    pub fn sync_context(&mut self, context: SessionContext, cx: &mut Context<Self>) {
        if self.fixture {
            return;
        }
        if self.model.busy || self.pending || self.continuation.is_some() {
            self.next_context = Some(context);
            return;
        }
        self.session = context.session;
        self.host = context.host;
        self.label = context.label;
        self.protocol = context.protocol;
        self.binding = context.binding;
        self.capabilities = self
            .binding
            .as_ref()
            .map(|b| b.capabilities.clone())
            .unwrap_or_default();
        *self.mode.lock().unwrap() = self
            .db
            .borrow()
            .ai_permission(self.permission_key())
            .unwrap_or_default();
        if !self.fixture {
            self.configured = self.db.borrow().ai_settings().is_ok_and(|s| s.configured());
        }
        cx.notify();
    }
    fn send_input(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let question = self.input.read(cx).value().to_string();
        if !question.trim().is_empty() {
            self.ask(question, w, cx);
        }
    }
    pub fn ask(&mut self, question: String, w: &mut Window, cx: &mut Context<Self>) {
        if self.pending || self.model.busy || question.trim().is_empty() {
            return;
        }
        self.continuation = None;
        self.run_config = None;
        self.release_context(cx);
        let context = self
            .workspace
            .update(cx, |s, cx| s.ai_context(self.session, cx))
            .unwrap_or_default();
        let user = UserTurn::redacted(&question, context);
        if user.question.trim().is_empty() {
            return;
        }
        if !self.fixture {
            self.binding = self
                .workspace
                .update(cx, |s, cx| s.ai_binding(self.session, cx))
                .ok()
                .flatten();
            self.capabilities = self
                .binding
                .as_ref()
                .map(|b| b.capabilities.clone())
                .unwrap_or_default();
        }
        self.input.update(cx, |i, cx| i.set_value("", w, cx));
        self.begin(Some(user), w, cx);
    }
    fn begin(&mut self, user: Option<UserTurn>, w: &mut Window, cx: &mut Context<Self>) {
        if self.model.busy || self.pending {
            return;
        }
        if let Some(user) = user {
            self.queued = Some(user);
            self.follow_bottom = true;
        }
        self.readonly = false;
        self.continuation = None;
        self.active_step = None;
        self.pending = true;
        self.history = false;
        self.model.error = None;
        cx.notify();
        let epoch = self.epoch;
        let this = cx.entity().downgrade();
        if self.fixture {
            self.pending = false;
            if !self.configured {
                self.model.error = Some(AiError::NotConfigured);
                cx.notify();
                return;
            }
            let queued = self.queued.take();
            self.start_fixture(queued, w, cx);
            return;
        }
        super::authorize(
            self.db.clone(),
            self.credentials.clone(),
            w,
            cx,
            move |result, w, cx| {
                this.update(cx, |s, cx| {
                    if s.epoch != epoch {
                        return;
                    }
                    s.pending = false;
                    match result {
                        Ok(config) => {
                            if shellmate_store::SecretStore::is_locked(s.credentials.as_ref()) {
                                s.begin(None, w, cx);
                                return;
                            }
                            s.configured = true;
                            s.provider = config.provider.id().into();
                            s.model_id = config.model.clone();
                            if let Some(user) = s.queued.take() {
                                s.model.conversation.turns.push(Turn { user, answer: None });
                                s.model.busy = true;
                            } else if !s.model.retry() {
                                return;
                            }
                            s.start(config, w, cx);
                        }
                        Err(e) => {
                            s.model.error = Some(e);
                            if e == AiError::NotConfigured {
                                s.configured = false;
                            }
                            cx.notify();
                        }
                    }
                })
                .ok();
            },
        );
    }
    fn start(&mut self, config: shellmate_ai::Config, w: &mut Window, cx: &mut Context<Self>) {
        self.epoch += 1;
        if self.continuation.is_none() && !self.fixture {
            self.binding = self
                .workspace
                .update(cx, |s, cx| s.ai_binding(self.session, cx))
                .ok()
                .flatten();
            *self.mode.lock().unwrap() = self
                .db
                .borrow()
                .ai_permission(self.permission_key())
                .unwrap_or_default();
        }
        let Some(runtime) = crate::runtime::get(cx) else {
            self.model.event(Event::Error(AiError::Unavailable));
            cx.notify();
            return;
        };
        let mut conversation = self.model.conversation.clone();
        let (tx, rx) = async_channel::bounded(64);
        let epoch = self.epoch;
        let (ui, messages) = async_channel::unbounded();
        let binding = self.binding.clone();
        let capabilities = binding
            .as_ref()
            .map(|b| b.capabilities.clone())
            .unwrap_or_default();
        self.capabilities = capabilities.clone();
        let alive = binding
            .as_ref()
            .map(|b| b.alive.clone())
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(true)));
        let executor = super::exec::AppExecutor {
            binding: binding.unwrap_or(super::exec::Binding {
                session: usize::MAX,
                capabilities: Capabilities::default(),
                connection: None,
                local: false,
                alive: alive.clone(),
            }),
            ui: ui.clone(),
        };
        let host = super::host::Host {
            mode: self.mode.clone(),
            ui,
            executor: Box::new(executor),
            alive,
            resolved: Mutex::new(None),
        };
        self.bridge(messages, epoch, w, cx);
        let continuation = self.continuation.take();
        self.run_config = Some(config.clone());
        let join = runtime.spawn(async move {
            let client = match RigClient::new(&config) {
                Ok(client) => client,
                Err(e) => {
                    let _ = worker_event(&tx, AgentEvent::Error(e)).await;
                    return;
                }
            };
            let result = match continuation {
                Some(resume) => agent::resume(
                    &client,
                    &mut conversation,
                    &host,
                    *resume,
                    agent::Limits::default(),
                ),
                None => agent::run(
                    &client,
                    &mut conversation,
                    capabilities,
                    &host,
                    agent::Limits::default(),
                ),
            };
            match result {
                Ok(mut stream) => {
                    let _ = tx.send(Worker::Started(stream.dropped_turns)).await;
                    while let Some(event) = stream.next().await {
                        if worker_event(&tx, event).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = worker_event(&tx, AgentEvent::Error(e)).await;
                }
            }
        });
        self.abort = Some(join.abort_handle());
        cx.spawn_in(w, async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if this
                    .update_in(cx, |s, _, cx| {
                        if s.epoch != epoch {
                            return;
                        }
                        match event {
                            Worker::Started(n) => {
                                s.model.dropped = n;
                                s.persist();
                            }
                            Worker::Event(event, ready) => {
                                s.agent_event(event);
                                let _ = ready.send(());
                                if !s.model.busy {
                                    s.abort = None;
                                    s.persist();
                                    if s.continuation.is_none() {
                                        s.binding = None;
                                        s.release_context(cx);
                                    }
                                }
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }
    fn persist(&mut self) {
        if self.fixture {
            return;
        }
        let title = self
            .model
            .conversation
            .turns
            .first()
            .map(|t| t.user.question.chars().take(48).collect::<String>())
            .unwrap_or_default();
        let saved = SavedConversation {
            id: self.id.clone(),
            host_id: self.host.clone(),
            label: self
                .model
                .conversation
                .turns
                .last()
                .map(|t| t.user.context.label.clone())
                .unwrap_or_default(),
            title,
            provider: self.provider.clone(),
            model: self.model_id.clone(),
            created_at: self.created,
            updated_at: super::now(),
            conversation: self.model.conversation.clone(),
        };
        if self.db.borrow_mut().save_ai_conversation(&saved).is_err() {
            self.save_error = Some("对话未能保存到本机，请检查主机库".into());
        }
    }
    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        self.epoch += 1;
        self.continuation = None;
        self.run_config = None;
        if let Some(step) = self.active_step.take()
            && let Some(turn) = self.model.conversation.turns.last_mut()
        {
            turn.answer
                .get_or_insert_with(Default::default)
                .items
                .push(shellmate_ai::agent::ToolOutcome::Failed("已停止".into()).item(&step.call));
        }
        self.pending = false;
        self.binding = None;
        if self.model.busy {
            self.model.event(Event::Error(AiError::Cancelled));
            self.persist();
        }
        self.release_context(cx);
        cx.notify();
    }
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        self.model.clear();
        self.queued = None;
        self.id = shellmate_core::HostId::generate().to_string();
        self.created = super::now();
        self.history_rows.clear();
        self.history = false;
        self.save_error = None;
        self.expanded.clear();
        self.overflowing_output.clear();
        self.follow_bottom = true;
        self.release_context(cx);
        cx.notify();
    }
    fn history(&mut self, cx: &mut Context<Self>) {
        self.history = !self.history;
        if !self.fixture {
            self.history_rows = self.db.borrow().ai_conversations().unwrap_or_default();
        }
        cx.notify();
    }
    fn load(&mut self, id: &str, cx: &mut Context<Self>) {
        let loaded = self.db.borrow().ai_conversation(id);
        match loaded {
            Ok(Some(c)) => {
                self.stop(cx);
                self.id = c.id;
                self.created = c.created_at;
                self.provider = c.provider;
                self.model_id = c.model;
                self.queued = None;
                self.model.conversation = c.conversation;
                self.model.error = None;
                self.history = false;
                self.readonly = true;
                self.expanded.clear();
                self.overflowing_output.clear();
                self.follow_bottom = true;
                cx.notify();
            }
            _ => {
                self.save_error = Some("无法打开这段对话".into());
                cx.notify();
            }
        }
    }
    fn delete(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.db.borrow_mut().delete_ai_conversation(id).is_err() {
            self.save_error = Some("无法删除对话".into());
        } else {
            self.history_rows.retain(|r| r.id != id);
            if self.id == id {
                self.stop(cx);
                self.model.clear();
                self.id = shellmate_core::HostId::generate().to_string();
            }
        }
        cx.notify();
    }
    fn settings(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let ws = self.workspace.clone();
        w.defer(cx, move |w, cx| {
            ws.update(cx, |s, cx| s.open_ai_settings(w, cx)).ok();
        });
    }
    fn insert(&mut self, card: CommandCard, w: &mut Window, cx: &mut Context<Self>) {
        let ws = self.workspace.clone();
        let target = self.session;
        w.defer(cx, move |w, cx| {
            ws.update(cx, |s, cx| s.ai_insert(card, target, w, cx)).ok();
        });
    }
    fn start_fixture(&mut self, user: Option<UserTurn>, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(user) = user {
            self.model
                .conversation
                .turns
                .push(Turn { user, answer: None });
        } else {
            self.model.retry();
        }
        self.model.busy = true;
        let mut stream = FixtureProvider
            .start(&self.model.conversation)
            .expect("fixture provider");
        // The fixture stream is immediately ready; no timer, runtime or HTTP client.
        while let Some(event) = futures::executor::block_on(stream.next()) {
            self.model.event(event);
        }

        cx.notify();
    }
    #[cfg(debug_assertions)]
    pub fn scene(&mut self, scene: &str, cx: &mut Context<Self>) {
        self.follow_bottom = true;
        self.fixture = true;
        self.configured = scene != "ai-unconfigured";
        if scene.starts_with("ai-agent") {
            self.agent_scene(scene);
        }
        if matches!(scene, "ai-chat" | "ai-error") {
            self.model.start(
                "找出占用 3000 端口的进程\nTOKEN=fixture-sensitive",
                ContextSnapshot {
                    label: "prod-web-01".into(),
                    protocol: "SSH".into(),
                    text: "[error] bind: address already in use".into(),
                },
            );
            if scene == "ai-error" {
                self.model.event(Event::Error(AiError::RateLimited));
            } else {
                for e in fixture_events() {
                    self.model.event(e);
                }
                self.model
                    .start("这个命令会修改系统吗？", ContextSnapshot::default());
                self.model.event(Event::TextDelta(
                    "不会。它只查询监听 3000 端口的进程；请先预览命令并确认目标会话。".into(),
                ));
                self.model.event(Event::Done(None));
            }
        }
        if scene == "ai-history" {
            self.history = true;
            self.history_rows = vec![
                ConversationSummary {
                    id: "demo1".into(),
                    label: "prod-web-01".into(),
                    title: "找出占用 3000 端口的进程".into(),
                    updated_at: super::now() - 120,
                },
                ConversationSummary {
                    id: "demo2".into(),
                    label: "ESP32-DevKit".into(),
                    title: "解释 ESP_ERR_TIMEOUT".into(),
                    updated_at: super::now() - 3600,
                },
            ];
            for row in &self.history_rows {
                let saved = SavedConversation {
                    id: row.id.clone(),
                    host_id: None,
                    label: row.label.clone(),
                    title: row.title.clone(),
                    provider: "openai".into(),
                    model: "fixture".into(),
                    created_at: row.updated_at,
                    updated_at: row.updated_at,
                    conversation: shellmate_ai::Conversation {
                        turns: vec![Turn {
                            user: UserTurn::redacted(
                                &row.title,
                                ContextSnapshot {
                                    label: row.label.clone(),
                                    protocol: "SSH".into(),
                                    text: String::new(),
                                },
                            ),
                            answer: Some(shellmate_ai::Answer {
                                items: vec![shellmate_ai::AnswerItem::Text(
                                    "已为你整理建议，请先预览后执行。".into(),
                                )],
                            }),
                        }],
                    },
                };
                self.db
                    .borrow_mut()
                    .save_ai_conversation(&saved)
                    .expect("fixture history");
            }
        }
        cx.notify();
    }
}
enum Worker {
    Started(usize),
    Event(AgentEvent, tokio::sync::oneshot::Sender<()>),
}
impl Drop for Drawer {
    fn drop(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}
struct FixtureProvider;
impl ProviderClient for FixtureProvider {
    fn start(
        &self,
        _: &shellmate_ai::Conversation,
    ) -> Result<shellmate_ai::ResponseStream, AiError> {
        Ok(shellmate_ai::ResponseStream::fixture(fixture_events()))
    }
}
fn fixture_events() -> Vec<Event> {
    vec![
        Event::TextDelta("可以先查看哪个进程正在监听 3000 端口。".into()),
        Event::Command(CommandCard {
            command: "lsof -nP -iTCP:3000 -sTCP:LISTEN".into(),
            explanation: "列出监听该端口的进程及 PID".into(),
            risk: Risk::Medium,
        }),
        Event::Done(None),
    ]
}
fn ago(time: i64) -> String {
    let n = super::now().saturating_sub(time);
    if n < 60 {
        "刚刚".into()
    } else if n < 3600 {
        format!("{} 分钟前", n / 60)
    } else if n < 86400 {
        format!("{} 小时前", n / 3600)
    } else {
        format!("{} 天前", n / 86400)
    }
}
impl Render for Drawer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        // Only the session label may shrink; the context prefix and line count
        // keep their intrinsic widths below the title/actions row.
        let context = div()
            .flex()
            .items_center()
            .min_w_0()
            .mt(px(2.))
            .text_size(px(11.5))
            .line_height(px(15.))
            .text_color(p.tx2)
            .child(div().flex_none().child("上下文："))
            .when(self.session.is_some(), |d| {
                d.child(
                    div()
                        .min_w_0()
                        .flex_shrink_1()
                        .text_ellipsis()
                        .child(self.label.clone()),
                )
                .child(div().flex_none().child(" · 最近 40 行输出"))
            })
            .when(self.session.is_none(), |d| d.child("无会话"));
        let header = div()
            .flex_none()
            .flex()
            .flex_col()
            .pt(px(14.))
            .pb(px(10.))
            .pl(px(16.))
            .pr(px(12.))
            .child(
                div()
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(15.))
                            .line_height(px(20.))
                            .font_weight(FontWeight::BOLD)
                            .child("AI 助手"),
                    )
                    .child(
                        Button::new("ai-history")
                            .icon(crate::icons::AppIcon::History.at(14.))
                            .w(px(26.))
                            .h(px(26.))
                            .p_0()
                            .rounded(px(8.))
                            .tooltip("历史对话")
                            .on_click(cx.listener(|s, _, _, cx| s.history(cx))),
                    )
                    .child(
                        Button::new("ai-clear")
                            .label("清空")
                            .h(px(26.))
                            .px(px(8.))
                            .rounded(px(8.))
                            .text_size(px(12.))
                            .on_click(cx.listener(|s, _, _, cx| s.clear(cx))),
                    )
                    .child(
                        Button::new("ai-close")
                            .label("×")
                            .w(px(26.))
                            .h(px(26.))
                            .rounded(px(8.))
                            .on_click(cx.listener(|s, _, w, cx| {
                                let ws = s.workspace.clone();
                                w.defer(cx, move |_, cx| {
                                    ws.update(cx, |s, cx| s.close_ai(cx)).ok();
                                });
                            })),
                    ),
            )
            .child(context);
        // GPUI applies this request during layout, using the new content height.
        if self.follow_bottom && !self.history {
            self.scroll.scroll_to_bottom();
        }
        let mut body = div()
            .id("ai-messages")
            .track_scroll(&self.scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .on_scroll(cx.listener(|_, _, _, cx| {
                // Read the offset after GPUI has applied the user's scroll delta.
                // Nested output scrolling leaves the message offset unchanged.
                let drawer = cx.entity().downgrade();
                cx.defer(move |cx| {
                    drawer
                        .update(cx, |s, cx| {
                            s.follow_bottom =
                                s.scroll.offset().y + s.scroll.max_offset().y <= px(4.);
                            cx.notify();
                        })
                        .ok();
                });
            }))
            .px(px(14.))
            .pt(px(4.))
            .pb(px(10.))
            .flex()
            .flex_col()
            .gap(px(12.));
        if self.history {
            for row in self.history_rows.clone() {
                let id = row.id.clone();
                let del = row.id;
                body = body.child(
                    div()
                        .flex_none()
                        .py(px(10.))
                        .border_b_1()
                        .border_color(p.line)
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .id(SharedString::from(format!("conversation-{id}")))
                                .flex_1()
                                .min_w_0()
                                .cursor_pointer()
                                .child(div().text_size(px(13.)).text_ellipsis().child(row.title))
                                .child(
                                    div()
                                        .mt(px(4.))
                                        .text_size(px(11.5))
                                        .text_color(p.tx2)
                                        .child(format!("{} · {}", row.label, ago(row.updated_at))),
                                )
                                .on_click(cx.listener(move |s, _, _, cx| s.load(&id, cx))),
                        )
                        .child(
                            Button::new(SharedString::from(format!("delete-{del}")))
                                .label("删除")
                                .h(px(26.))
                                .text_size(px(11.5))
                                .on_click(cx.listener(move |s, _, _, cx| s.delete(&del, cx))),
                        ),
                );
            }
        } else if !self.configured {
            body = body
                .child(
                    div()
                        .text_size(px(13.))
                        .line_height(px(21.))
                        .text_color(p.tx2)
                        .child("尚未配置 AI 助手。设置服务商、模型和 API Key 后即可提问。"),
                )
                .child(
                    Button::new("ai-settings")
                        .label("去设置")
                        .on_click(cx.listener(|s, _, w, cx| s.settings(w, cx))),
                );
        } else if self.model.conversation.turns.is_empty() {
            body=body.child(div().flex_none().pt(px(6.)).flex().flex_col().gap(px(8.)).child(div().text_size(px(13.)).line_height(px(20.8)).text_color(p.tx2).child("用自然语言描述想做的事，或让我解释终端里的报错。执行命令遵循这台主机的审批设置。"))
        .children(SUGGESTIONS.into_iter().enumerate().map(|(i,label)|div().id(("ai-suggestion",i)).py(px(10.)).px(px(12.)).rounded(px(12.)).bg(p.card2).cursor_pointer().hover(|d|d.bg(p.card3)).text_size(px(13.)).line_height(px(17.)).child(label).on_click(cx.listener(move|s,_,w,cx|s.ask(label.into(),w,cx))))));
        }
        if !self.history {
            for (index, turn) in self
                .model
                .conversation
                .turns
                .clone()
                .into_iter()
                .enumerate()
            {
                body = body.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap(px(5.))
                        .child(
                            div()
                                .max_w_full()
                                .px(px(12.))
                                .py(px(9.))
                                .rounded(px(13.))
                                .bg(p.accs)
                                .text_size(px(13.))
                                .line_height(px(20.8))
                                .whitespace_normal()
                                .child(turn.user.question),
                        )
                        .when(turn.user.redactions > 0, |d| {
                            d.child(
                                div()
                                    .px(px(8.))
                                    .py(px(3.))
                                    .rounded(px(8.))
                                    .bg(p.card2)
                                    .text_color(p.tx2)
                                    .text_size(px(11.))
                                    .child(format!("已去除 {} 处敏感内容", turn.user.redactions)),
                            )
                        }),
                );
                if let Some(answer) = turn.answer {
                    let mut reply = div().flex_none().flex().flex_col().gap(px(8.));
                    for (n, item) in answer.items.into_iter().enumerate() {
                        let card = match item {
                            item @ shellmate_ai::AnswerItem::Step { .. } => {
                                reply = reply.child(self.step_card(&item, (index, n), cx));
                                continue;
                            }
                            shellmate_ai::AnswerItem::Command(card) => card,
                            item => {
                                reply = reply.child(
                                    div()
                                        .text_size(px(13.))
                                        .line_height(px(21.45))
                                        .whitespace_normal()
                                        .child(item.wire_text()),
                                );
                                continue;
                            }
                        };
                        let copy = card.command.clone();
                        let insert = card.clone();
                        let risk = card.risk;
                        reply = reply.child(
                            div()
                                .rounded(px(12.))
                                .bg(p.card2)
                                .overflow_hidden()
                                .child(
                                    div()
                                        .px(px(11.))
                                        .py(px(9.))
                                        .bg(shellmate_term::colors::to_hsla(
                                            cx.terminal_style().palette.background,
                                        ))
                                        .text_color(shellmate_term::colors::to_hsla(
                                            cx.terminal_style().palette.foreground,
                                        ))
                                        .font_family(MONO_FONT)
                                        .text_size(px(12.))
                                        .line_height(px(18.))
                                        .whitespace_normal()
                                        .child(card.command),
                                )
                                .child(
                                    div()
                                        .px(px(10.))
                                        .py(px(7.))
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap(px(6.))
                                        .child(
                                            div()
                                                .w_full()
                                                .text_size(px(12.))
                                                .text_color(p.tx2)
                                                .child(card.explanation),
                                        )
                                        .child(
                                            div()
                                                .px(px(6.))
                                                .py(px(1.))
                                                .rounded(px(5.))
                                                .text_size(px(10.5))
                                                .text_color(if risk == Risk::High {
                                                    p.err
                                                } else {
                                                    p.warn
                                                })
                                                .bg(if risk == Risk::High {
                                                    p.errs
                                                } else {
                                                    p.warns
                                                })
                                                .child(risk.label()),
                                        )
                                        .child(
                                            Button::new(("ai-copy", index * 1000 + n))
                                                .label("复制")
                                                .h(px(24.))
                                                .text_size(px(11.5))
                                                .on_click(move |_, _, cx| {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(copy.clone()),
                                                    )
                                                }),
                                        )
                                        .child(
                                            Button::new(("ai-insert", index * 1000 + n))
                                                .label("插入")
                                                .h(px(24.))
                                                .text_size(px(11.5))
                                                .on_click(cx.listener(move |s, _, w, cx| {
                                                    s.insert(insert.clone(), w, cx)
                                                })),
                                        ),
                                ),
                        );
                    }
                    body = body.child(reply);
                }
            }
            if self.continuation.is_some() {
                body = body.child(
                    crate::dialogs::button(
                        "ai-continue",
                        format!("已执行 {} 步 · 继续", self.resume_steps),
                        crate::dialogs::Tone::Primary,
                        true,
                    )
                    .on_click(cx.listener(|s, _, w, cx| s.resume(w, cx))),
                );
            }
            if self.pending
                || (self.model.busy
                    && self.active_step.is_none()
                    && self
                        .model
                        .conversation
                        .turns
                        .last()
                        .and_then(|t| t.answer.as_ref())
                        .is_none_or(|a| a.text().is_empty()))
            {
                body = body.child(div().text_size(px(12.5)).text_color(p.tx2).child(
                    if self.pending {
                        "正在准备请求…"
                    } else if self.model.thinking {
                        "正在思考…"
                    } else {
                        "正在分析当前会话输出…"
                    },
                ));
            }
            if self.model.dropped > 0 {
                body = body.child(
                    div()
                        .text_size(px(11.))
                        .text_color(p.tx3)
                        .child(format!("本次请求已省略最早 {} 轮对话", self.model.dropped)),
                );
            }
            if let Some(error) = self.model.error {
                body = body.child(
                    div()
                        .flex_none()
                        .p(px(10.))
                        .rounded(px(12.))
                        .bg(p.errs)
                        .text_color(p.err)
                        .text_size(px(12.5))
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .line_height(px(19.))
                                .child(error.to_string()),
                        )
                        .child(
                            Button::new("ai-retry")
                                .label("重试")
                                .flex_none()
                                .h(px(34.))
                                .px(px(12.))
                                .rounded(px(10.))
                                .on_click(cx.listener(|s, _, w, cx| s.begin(None, w, cx))),
                        ),
                );
            }
        }
        if let Some(e) = &self.save_error {
            body = body.child(div().text_size(px(12.)).text_color(p.err).child(e.clone()));
        }
        // Keep the blocking card last so following the bottom reveals all its actions.
        if !self.history && self.active_step.is_some() {
            body = body.child(self.active_card(cx));
        }
        let footer = div()
            .flex_none()
            .pt(px(10.))
            .px(px(12.))
            .pb(px(12.))
            .border_t_1()
            .border_color(p.line)
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(self.permission_chip(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .pl(px(12.))
                    .pr(px(4.))
                    .py(px(4.))
                    .rounded(px(13.))
                    .bg(p.card2)
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.input)
                                .appearance(false)
                                .p_0()
                                .h(px(32.))
                                .text_size(px(13.)),
                        ),
                    )
                    .child(
                        crate::dialogs::button(
                            "ai-send",
                            if self.model.busy || self.pending {
                                "停止"
                            } else {
                                "发送"
                            },
                            crate::dialogs::Tone::Primary,
                            true,
                        )
                        .h(px(30.))
                        .on_click(cx.listener(|s, _, w, cx| {
                            if s.model.busy || s.pending {
                                s.stop(cx)
                            } else {
                                s.send_input(w, cx)
                            }
                        })),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .line_height(px(16.))
                    .text_color(p.tx2)
                    .child("发送前会自动去掉 .env、私钥、密码等敏感内容"),
            );
        div()
            .w(px(340.))
            .flex_none()
            .h_full()
            .min_h_0()
            .rounded(px(18.))
            .bg(p.card)
            .shadow(crate::ui::card_shadow(&p))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .child(footer)
    }
}

async fn worker_event(tx: &async_channel::Sender<Worker>, event: AgentEvent) -> Result<(), ()> {
    let (ready, receive) = tokio::sync::oneshot::channel();
    tx.send(Worker::Event(event, ready)).await.map_err(|_| ())?;
    receive.await.map_err(|_| ())
}
