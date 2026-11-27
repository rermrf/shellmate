use super::*;
use shellmate_ai::Config;
use std::collections::BTreeSet;
impl Drawer {
    pub(super) fn session_result_content(
        &self,
        key: &str,
        result: &str,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let value: serde_json::Value = serde_json::from_str(result).ok()?;
        let p = cx.palette().clone();
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .text_size(px(12.))
            .whitespace_normal();
        if let Some(rows) = value["sessions"].as_array() {
            for (index, row) in rows.iter().enumerate() {
                body = body.child(div().py(px(4.)).border_b_1().border_color(p.line).child(
                    format!(
                        "{}. {} · {} · {}{}\n{}",
                        row["number"].as_u64().unwrap_or(index as u64 + 1),
                        row["name"].as_str().unwrap_or("终端"),
                        row["protocol"].as_str().unwrap_or_default(),
                        if row["connected"] == true {
                            "已连接"
                        } else {
                            "已断开"
                        },
                        if row["default"] == true { " · 对话默认" } else { "" },
                        row["cwd"].as_str().unwrap_or("目录未知")
                    ),
                ));
            }
            if rows.is_empty() {
                body = body.child("没有匹配的终端");
            }
            if value["next_offset"].is_number() {
                body = body.child("还有更多终端，可继续查询下一页");
            }
        } else {
            let text = value["text"].as_str()?;
            body = body
                .child(format!(
                    "{} · {}",
                    value["target"]["label"].as_str().unwrap_or("终端"),
                    value["target"]["cwd"].as_str().unwrap_or("目录未知")
                ))
                .child(super::subagent_ui::selectable(
                    format!("{key}:terminal-snapshot"),
                    text,
                    w,
                    cx,
                ));
        }
        Some(body)
    }
    pub(super) fn message_context(
        &mut self,
        mut context: shellmate_ai::ContextSnapshot,
    ) -> shellmate_ai::ContextSnapshot {
        if !self.composer_targets.is_empty() {
            context.text.push_str(&format!(
                "\n本条消息明确选择的终端引用：{}。同名终端以这些引用为准。",
                serde_json::to_string(&std::mem::take(&mut self.composer_targets)).unwrap()
            ));
        }
        context
    }
    pub(super) fn composer_target_chips(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.composer_targets.is_empty() {
            return None;
        }
        let catalog = self
            .runtime
            .as_ref()
            .map(|r| r.router.catalog.clone())
            .unwrap_or_else(|| crate::workspace::hub::Hub::ai_services(cx).0);
        let mut row = div().flex().flex_wrap().gap(px(6.));
        for id in &self.composer_targets {
            let label = catalog
                .resolve(id)
                .map(|e| e.target.label)
                .unwrap_or_else(|_| "终端已失效".into());
            let id = id.clone();
            row = row.child(
                agent_ui::pill_action(
                    SharedString::from(format!("mention-{id}")),
                    SharedString::from(format!("@{label} ×")),
                    agent_ui::ActionTone::Accent,
                    cx,
                )
                .h(px(24.))
                .px(px(8.))
                .on_click(cx.listener(move |s, _, _, cx| {
                    s.composer_targets.remove(&id);
                    cx.notify();
                })),
            );
        }
        Some(row)
    }
    #[cfg(debug_assertions)]
    pub(super) fn multi_fixture(&mut self, scene: &str, cx: &mut Context<Self>) {
        use crate::ai::{exec::Binding, session_catalog::Catalog, target_router::Router};
        use shellmate_ai::{Conversation, Entry, LoggedCall, LoggedResult, Step, UserTurn};
        use std::sync::atomic::AtomicBool;
        let Some(rt) = crate::runtime::get(cx) else {
            return;
        };
        let handle = rt.handle();
        let _entered = handle.enter();
        let (tx, _) = async_channel::unbounded();
        let catalog = Catalog::new(tx.clone());
        catalog.update(
            [
                (1, "本地终端 1", "本机终端", "/Users/demo/project", true),
                (2, "prod-web-01", "SSH", "/srv/app", false),
                (3, "prod-db-01", "SSH", "/srv/db", false),
            ]
            .into_iter()
            .map(|(session, label, protocol, cwd, local)| {
                (
                    Binding {
                        session,
                        local,
                        connection: None,
                        alive: Arc::new(AtomicBool::new(true)),
                        capabilities: Capabilities {
                            background: true,
                            cwd: Some(cwd.into()),
                            ..Default::default()
                        },
                    },
                    label.into(),
                    protocol.into(),
                    (!local).then(|| label.into()),
                    "窗口 1".into(),
                )
            })
            .collect(),
        );
        let manager = crate::workspace::hub::Hub::ai_services(cx).1;
        let mut current = None;
        for (session, title, busy) in [
            (1, "分析本地项目", true),
            (2, "比较本地与远程目录", false),
            (3, "检查数据库连接", true),
        ] {
            let router = Router::new(
                catalog.clone(),
                self.db.clone(),
                self.data_root.clone(),
                tx.clone(),
            );
            let target = catalog.session(session).unwrap().target;
            router.add(&target.session_id, true).unwrap();
            if session == 2 {
                router
                    .add(&catalog.session(1).unwrap().target.session_id, false)
                    .unwrap();
            }
            let mut log = Conversation::default();
            log.push_user(UserTurn::new(title, Default::default()));
            log.push_text(if session == 2 {
                "本地与远程目录清单已读取，正在整理差异。"
            } else {
                "任务正在处理，切换到其他对话不会停止本任务。"
            });
            // Historical fixture: rendering this retired tool must never read live files.
            if session == 2 && scene == "ai-multi-compare" {
                log.append(Entry::Step(Step{message_id:1,call:LoggedCall{id:"compare".into(),name:"compare_directories".into(),args_json:serde_json::json!({"left":{"session_id":"local","path":"/Users/demo/project"},"right":{"session_id":"remote","path":"/srv/app"}})},result:LoggedResult{attachments: Vec::new(), inline:serde_json::json!({"left":{"label":"本地终端 1"},"right":{"label":"prod-web-01"},"left_path":"/Users/demo/project","right_path":"/srv/app","scan_complete":true,"summary":{"内容不同":2,"仅左侧存在":1,"内容相同":126},"differences":[{"path":"config/app.toml","status":"内容不同"},{"path":"src/main.rs","status":"内容不同"},{"path":"README.local.md","status":"仅左侧存在"}]}).to_string(),output_id:None,subagent:None,cleared:false,is_error:false},display:None}));
            }
            let r = conversation_runtime::Runtime::new(
                format!("multi-fixture-{session}"),
                session as i64,
                log,
                Default::default(),
                router,
                self.db.clone(),
                self.data_root.clone(),
                Arc::new(shellmate_store::MemoryStore::default()),
            );
            {
                let mut d = r.data.lock().unwrap();
                d.model.busy = busy;
                d.revision = 1;
            }
            let r = manager.insert(r);
            if session == 2 {
                current = Some(r);
            }
        }
        self.multi_fixture = true;
        self.attach_runtime(current.unwrap(), cx);
        self.chooser = scene == "ai-multi-conversations";
        self.target_picker = scene == "ai-multi-targets";
        self.next_context = Some(SessionContext {
            session: Some(1),
            host: None,
            label: "本地终端 1".into(),
            protocol: "本机终端".into(),
            binding: None,
        });
        if scene == "ai-multi-compare" {
            self.state
                .tool_expanded
                .insert(self.tool_key("main", "compare", false));
        }
        cx.notify();
    }
    pub(crate) fn draft_title(&self, cx: &App) -> String {
        self.state
            .model
            .conversation
            .turns()
            .first()
            .map(|t| t.user.title(28))
            .unwrap_or_else(|| {
                let text = self.input.read(cx).value();
                if text.trim().is_empty() {
                    self.draft_images().first().map(|a|a.name.clone()).unwrap_or_else(||"新对话".into())
                } else {
                    text.chars().take(28).collect()
                }
            })
    }
    pub(crate) fn reuse_draft(&mut self) {
        self.context_bound = false;
        self.composer_targets.clear();
        self.next_context = None;
    }
    pub(crate) fn empty_draft(&self, cx: &App) -> bool {
        self.runtime.is_none()
            && self.state.media.values().all(|d| d.images.is_empty() && !d.loading)
            && self.input.read(cx).value().trim().is_empty()
            && self.state.model.conversation.turns().is_empty()
            && !self.state.pending
    }
    pub(crate) fn runtime_retired(&self) -> bool {
        self.runtime.as_ref().is_some_and(|r| r.is_retired())
    }
    pub(crate) fn conversation_id(&self) -> &str {
        &self.state.id
    }
    pub(crate) fn attach_runtime(
        &mut self,
        runtime: Arc<conversation_runtime::Runtime>,
        cx: &mut Context<Self>,
    ) {
        self.state.id = runtime.id.clone();
        self.state.created = runtime.created;
        self.state.control = runtime.control.clone();
        self.tasks = Some(runtime.tasks.clone());
        self.runtime = Some(runtime);
        self.runtime_revision = u64::MAX;
        self.sync_runtime(cx);
    }
    pub(super) fn start_runtime(&mut self, config: Config, cx: &mut Context<Self>) -> bool {
        let Some(runtime) = self.prepare_runtime(cx) else {
            return false;
        };
        let settings = self.db.borrow().ai_settings().unwrap_or_default();
        runtime.launch(config, settings.limits(), settings);
        self.sync_runtime(cx);
        true
    }
    pub(super) fn prepare_runtime(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<Arc<conversation_runtime::Runtime>> {
        let runtime_handle = crate::runtime::get(cx)?;
        let (catalog, manager) = crate::workspace::hub::Hub::ai_services(cx);
        if self.runtime.is_none()
            && let Some(existing) = manager.get(&self.state.id)
        {
            self.attach_runtime(existing, cx);
        }
        if self.runtime.is_none() {
            let (tx, _) = async_channel::unbounded();
            let router = crate::ai::target_router::Router::new(
                catalog.clone(),
                self.db.clone(),
                self.data_root.clone(),
                tx,
            );
            let restored = self
                .db
                .borrow()
                .ai_runtime(&self.state.id)
                .ok()
                .flatten()
                .and_then(|r| {
                    serde_json::from_value::<crate::ai::target_router::Scope>(r["targets"].clone())
                        .ok()
                });
            if let Some(scope) = restored {
                *router.scope.lock().unwrap() = scope;
            } else if self.state.readonly {
                let id = format!("legacy-{}", self.state.id);
                let target = shellmate_ai::session_tools::Destination {
                    session_id: id.clone(),
                    label: self
                        .state
                        .model
                        .conversation
                        .latest_user()
                        .map_or("原终端", |u| u.context.label.as_str())
                        .into(),
                    protocol: self
                        .state
                        .model
                        .conversation
                        .latest_user()
                        .map_or("", |u| u.context.protocol.as_str())
                        .into(),
                    host_id: self.state.history_memory_host.clone(),
                    cwd: None,
                };
                let mut scope = router.scope.lock().unwrap();
                scope.default = Some(id.clone());
                scope.targets.insert(id, target);
            } else if let Some(session) = self.session
                && let Some(e) = catalog.session(session)
            {
                let _ = router.add(&e.target.session_id, true);
            }
            let id = self.state.id.clone();
            let created = self.state.created;
            let log = self.state.model.conversation.clone();
            let control = self.state.control.clone();
            let db = self.db.clone();
            let root = self.data_root.clone();
            let credentials = self.credentials.clone();
            // Construct the independent worker's bridge on the application Tokio runtime.
            let handle = runtime_handle.handle();
            let _entered = handle.enter();
            let runtime = conversation_runtime::Runtime::new(
                id,
                created,
                log,
                control,
                router,
                db,
                root,
                credentials,
            );
            {
                let mut data = runtime.data.lock().unwrap();
                data.timing = self.state.timing.clone();
            }
            self.attach_runtime(manager.insert(runtime), cx);
        }
        self.runtime.clone()
    }
    pub(super) fn sync_runtime(&mut self, _cx: &mut Context<Self>) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        if runtime.is_retired() {
            self.runtime = None;
            self.tasks = None;
            self.state.reset_conversation();
            return;
        }
        let snapshot = runtime.snapshot();
        if snapshot.revision == self.runtime_revision && !runtime.active() {
            return;
        }
        self.subagent_service = runtime.service.lock().unwrap().clone();
        self.runtime_revision = snapshot.revision;
        self.state.model = snapshot.model;
        self.state.timing = snapshot.timing;
        if snapshot.config.is_some() {
            self.state.readonly = false;
        }
        self.state.run_config = snapshot.config;
        self.state.run_limits = Some(snapshot.limits);
        self.state.run_subagent_settings = Some(snapshot.settings);
        self.state.save_error = snapshot.error;
        self.state.tool_ids = snapshot.tool_ids;
        self.state.active_tools = snapshot.tools;
        let keys: Vec<_> = self
            .state
            .active_steps
            .iter()
            .filter(|(key, _)| !snapshot.approvals.contains_key(*key))
            .map(|(key, _)| key.to_owned())
            .collect();
        for key in keys {
            self.state.active_steps.remove(&key);
        }
        for (key, a) in snapshot.approvals {
            if a.reply.as_ref().is_some_and(|r| r.is_closed()) {
                continue;
            }
            if self
                .state
                .active_steps
                .get(&key)
                .is_none_or(|s| s.started.is_none() != !a.started)
            {
                let mut step = agent_ui::ActiveStep::new(a.call);
                step.source = a.source.clone();
                step.redline = a.redline;
                step.reply = a.reply;
                step.automatic = a.automatic;
                if a.started {
                    step.started = Some(std::time::Instant::now());
                }
                self.state
                    .active_steps
                    .insert(key, a.source.map(|s| s.id), step);
            }
        }
        for view in runtime.tasks.agent_views() {
            if let Some(old) = self.state.subagents.get_mut(&view.id) {
                old.refresh(view);
            } else {
                self.state.subagents.insert(view.id.clone(), view);
            }
        }
        if let Ok(endpoint) = runtime.router.resolve(None) {
            self.session = Some(endpoint.binding.session);
            self.host = endpoint.target.host_id.clone();
            self.label = endpoint.target.label.clone();
            self.protocol = endpoint.target.protocol.clone();
            *self.mode.lock().unwrap() = runtime.router.policy(&endpoint);
            self.binding = Some(endpoint.binding.clone());
            self.capabilities = endpoint.binding.capabilities;
        } else {
            let scope = runtime.router.scope.lock().unwrap();
            let target = scope.default.as_ref().and_then(|id| scope.targets.get(id));
            self.session = None;
            self.binding = None;
            self.host = target.and_then(|t| t.host_id.clone());
            self.label = target.map_or("无默认终端", |t| t.label.as_str()).into();
            self.protocol = target.map_or("", |t| t.protocol.as_str()).into();
            self.capabilities.terminal = false;
            self.capabilities.background = false;
        }
        self.capabilities.sessions = true;
        self.capabilities.web = runtime.tasks.web_enabled();
        let has_default = runtime.router.scope.lock().unwrap().default.is_some();
        if has_default && runtime.router.resolve(None).is_err() && self.state.save_error.is_none() {
            self.state.save_error = Some("默认终端不可用，请重新选择；已获得的结果仍保留".into());
        }
    }
    pub(super) fn conversation_controls(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.fixture && !self.multi_fixture {
            return None;
        }
        let p = cx.palette().clone();
        let all = crate::workspace::hub::Hub::ai_services(cx).1.list();
        let pending = all
            .iter()
            .filter(|r| r.snapshot().approvals.values().any(|a| a.reply.is_some()))
            .count();
        let count = self.runtime.as_ref().map(|r| r.router.catalog.list().len())
            .unwrap_or_else(|| crate::workspace::hub::Hub::ai_services(cx).0.list().len());
        let conversations_btn = agent_ui::pill_action(
            "ai-conversations",
            if pending > 0 {
                "对话 · 待处理"
            } else {
                "切换对话"
            },
            if pending > 0 {
                agent_ui::ActionTone::Warn
            } else if self.chooser {
                agent_ui::ActionTone::Accent
            } else {
                agent_ui::ActionTone::Surface
            },
            cx,
        );
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .mt(px(8.))
            .child(
                conversations_btn
                .on_click(cx.listener(|s, _, _, cx| {
                    s.chooser = !s.chooser;
                    s.target_picker = false;
                    s.history = false;
                    s.state.follow_bottom = false;
                    s.scroll.set_offset(gpui::point(px(0.), px(0.)));
                    cx.notify();
                })),
            )
            .child(
                agent_ui::pill_action(
                    "ai-targets",
                    SharedString::from(format!("可用终端 · {count}")),
                    if self.target_picker {
                        agent_ui::ActionTone::Accent
                    } else {
                        agent_ui::ActionTone::Surface
                    },
                    cx,
                )
                .on_click(cx.listener(|s, _, _, cx| {
                    s.target_picker = !s.target_picker;
                    s.chooser = false;
                    s.history = false;
                    s.state.follow_bottom = false;
                    s.scroll.set_offset(gpui::point(px(0.), px(0.)));
                    cx.notify();
                })),
            );
        if let Some(session) = self.session {
            row = row.child(
                agent_ui::pill_action("locate-default", "定位", agent_ui::ActionTone::Surface, cx)
                    .on_click(cx.listener(move |s, _, w, cx| {
                        let ws = s.workspace.clone();
                        w.defer(cx, move |w, cx| {
                            ws.update(cx, |s, cx| s.locate_ai_terminal(session, w, cx))
                                .ok();
                        });
                    })),
            );
        }
        if let Some(next) = &self.next_context
            && next.session != self.session
        {
            row = row.child(
                div()
                    .w_full()
                    .min_w_0()
                    .mt(px(4.))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(10.))
                    .bg(p.card2)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_ellipsis()
                            .text_size(px(11.5))
                            .text_color(p.tx2)
                            .child(format!("当前标签：{}", next.label)),
                    )
                    .child(
                        agent_ui::pill_action(
                            "new-current-target",
                            "为此新建",
                            agent_ui::ActionTone::Accent,
                            cx,
                        )
                        .h(px(24.))
                        .px(px(8.))
                        .on_click(cx.listener(|s, _, w, cx| s.new_conversation(w, cx))),
                    ),
            );
        }
        Some(row)
    }
    pub(super) fn conversation_picker(&self, cx: &mut Context<Self>) -> Div {
        let p = cx.palette().clone();
        let mut list = div()
            .p(px(12.))
            .rounded(px(14.))
            .bg(p.card2)
            .border_1()
            .border_color(p.line)
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.tx)
                            .child("切换对话"),
                    )
                    .child(
                        agent_ui::pill_action("all-history", "历史对话", agent_ui::ActionTone::Card, cx)
                            .h(px(24.))
                            .px(px(8.))
                            .on_click(cx.listener(|s, _, _, cx| {
                                s.chooser = false;
                                s.history(cx);
                            })),
                    )
                    .child(
                        div()
                            .id("chooser-close")
                            .size(px(24.))
                            .rounded(px(8.))
                            .bg(p.card)
                            .border_1()
                            .border_color(p.line)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .text_size(px(14.))
                            .text_color(p.tx2)
                            .hover(|d| d.bg(p.card3).text_color(p.tx))
                            .child("×")
                            .on_click(cx.listener(|s, _, _, cx| {
                                s.chooser = false;
                                cx.notify();
                            })),
                    ),
            );
        let mut all = crate::workspace::hub::Hub::ai_services(cx).1.list();
        all.sort_by_key(|r| {
            let s = r.snapshot();
            (
                s.approvals.is_empty(),
                !s.model.busy,
                std::cmp::Reverse(r.created),
            )
        });
        for runtime in all {
            let snap = runtime.snapshot();
            let title = snap
                .model
                .conversation
                .turns()
                .first()
                .map(|t| t.user.question.chars().take(28).collect::<String>())
                .unwrap_or_else(|| "新对话".into());
            let scope = runtime.router.scope.lock().unwrap();
            let target = scope
                .default
                .as_ref()
                .and_then(|id| scope.targets.get(id))
                .map(|t| t.label.as_str())
                .unwrap_or("无默认终端");
            let (status, badge_fg, badge_bg, has_dot) = if snap.approvals.values().any(|a| a.reply.is_some()) {
                ("等待批准", p.warn, p.warns, true)
            } else if snap.model.busy || runtime.tasks.has_running() {
                ("运行中", p.acc, p.accs, true)
            } else if snap.paused {
                ("已暂停", p.warn, p.warns, true)
            } else {
                ("已结束", p.tx3, p.card2, false)
            };
            let unread = snap.revision > runtime.seen.load(std::sync::atomic::Ordering::Acquire);
            let current = self.runtime.as_ref().is_some_and(|r| r.id == runtime.id);
            let id = runtime.id.clone();
            let close_id = id.clone();
            let stop = runtime.clone();
            let active = runtime.active();
            let secs = snap
                .timing
                .snapshot(timing::Moment::now())
                .last()
                .map_or(0, |t| t.duration_ms / 1000);
            list = list.child(
                div()
                    .p(px(10.))
                    .rounded(px(12.))
                    .bg(p.card)
                    .border_1()
                    .border_color(if current { p.acc.opacity(0.32) } else { p.line })
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .id(SharedString::from(format!("choose-{id}")))
                            .cursor_pointer()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .when(unread, |d| d.child(crate::ui::dot(p.acc, 6.)))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(12.5))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(p.tx)
                                            .child(title),
                                    )
                                    .child(agent_ui::status_badge(status, badge_fg, badge_bg, has_dot)),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(p.tx2)
                                    .truncate()
                                    .child(format!("{target} · {secs}s")),
                            )
                            .on_click(cx.listener(move |s, _, w, cx| {
                                s.chooser = false;
                                let ws = s.workspace.clone();
                                let id = id.clone();
                                w.defer(cx, move |w, cx| {
                                    ws.update(cx, |s, cx| s.select_ai_conversation(&id, w, cx))
                                        .ok();
                                });
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap(px(6.))
                            .when(active, |d| {
                                d.child(
                                    agent_ui::pill_action(
                                        SharedString::from(format!("stop-{close_id}")),
                                        "停止回复",
                                        agent_ui::ActionTone::Surface,
                                        cx,
                                    )
                                    .h(px(24.))
                                    .px(px(8.))
                                    .on_click(move |_, _, _| stop.stop_reply()),
                                )
                            })
                            .child(
                                agent_ui::pill_action(
                                    SharedString::from(format!("close-{close_id}")),
                                    if active { "停止并关闭" } else { "关闭此对话" },
                                    if active {
                                        agent_ui::ActionTone::Danger
                                    } else {
                                        agent_ui::ActionTone::Surface
                                    },
                                    cx,
                                )
                                .h(px(24.))
                                .px(px(8.))
                                .on_click(cx.listener(move |s, _, w, cx| {
                                    let ws = s.workspace.clone();
                                    let id = close_id.clone();
                                    w.defer(cx, move |w, cx| {
                                        ws.update(cx, |s, cx| s.close_ai_conversation(&id, w, cx))
                                            .ok();
                                    });
                                })),
                            ),
                    ),
            );
        }
        if let Some(ws) = self.workspace.upgrade() {
            for (id, title) in ws.read(cx).ai_drafts(cx) {
                list = list.child(
                    div()
                        .id(SharedString::from(format!("draft-{id}")))
                        .p(px(10.))
                        .rounded(px(12.))
                        .bg(p.card)
                        .border_1()
                        .border_color(p.line)
                        .cursor_pointer()
                        .hover(|d| d.bg(p.card3.opacity(0.4)))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(agent_ui::status_badge("草稿", p.tx2, p.card2, false))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.5))
                                .text_color(p.tx)
                                .child(title),
                        )
                        .on_click(cx.listener(move |s, _, w, cx| {
                            let ws = s.workspace.clone();
                            let id = id.clone();
                            w.defer(cx, move |w, cx| {
                                ws.update(cx, |s, cx| s.select_ai_conversation(&id, w, cx))
                                    .ok();
                            });
                        })),
                );
            }
        }
        list
    }
    pub(super) fn targets_panel(&self, cx: &mut Context<Self>) -> Div {
        let p = cx.palette().clone();
        let mut list = div()
            .p(px(12.))
            .rounded(px(14.))
            .bg(p.card2)
            .border_1()
            .border_color(p.line)
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.tx)
                            .child("可用终端"),
                    )
                    .child(
                        div()
                            .id("targets-close")
                            .size(px(24.))
                            .rounded(px(8.))
                            .bg(p.card)
                            .border_1()
                            .border_color(p.line)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .text_size(px(14.))
                            .text_color(p.tx2)
                            .hover(|d| d.bg(p.card3).text_color(p.tx))
                            .child("×")
                            .on_click(cx.listener(|s, _, _, cx| {
                                s.target_picker = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .line_height(px(16.))
                    .text_color(p.tx2)
                    .whitespace_normal()
                    .child("AI 会按任务选择终端。引用可明确指向某个终端，默认终端用于未指定目标的操作。"),
            );
        let entries = self
            .runtime
            .as_ref()
            .map(|r| r.router.catalog.list())
            .unwrap_or_else(|| crate::workspace::hub::Hub::ai_services(cx).0.list());
        if let Some(runtime) = &self.runtime {
            let scope = runtime.router.scope.lock().unwrap().clone();
            for (id, target) in &scope.targets {
                if runtime.router.catalog.resolve(id).is_ok() {
                    continue;
                }
                let old = id.clone();
                let remove = id.clone();
                list = list.child(
                    div()
                        .p(px(10.))
                        .rounded(px(12.))
                        .bg(p.card)
                        .border_1()
                        .border_color(p.line)
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(crate::ui::dot(p.tx3, 6.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.))
                                .text_ellipsis()
                                .text_color(p.tx2)
                                .child(format!("{} · 已失效", target.label)),
                        )
                        .child(
                            agent_ui::pill_action(
                                SharedString::from(format!("rebind-{id}")),
                                "重新关联",
                                agent_ui::ActionTone::Accent,
                                cx,
                            )
                            .h(px(24.))
                            .px(px(8.))
                            .on_click(cx.listener(move |s, _, _, cx| {
                                s.rebind_target = Some(old.clone());
                                cx.notify();
                            })),
                        )
                        .child(
                            agent_ui::pill_action(
                                SharedString::from(format!("forget-{id}")),
                                "忘记关联",
                                agent_ui::ActionTone::Surface,
                                cx,
                            )
                            .h(px(24.))
                            .px(px(8.))
                            .on_click(cx.listener(move |s, _, _, cx| {
                                if let Some(r) = &s.runtime {
                                    r.router.remove(&remove);
                                    r.data.lock().unwrap().revision += 1;
                                    r.save(&[], &[]);
                                }
                                cx.notify();
                            })),
                        ),
                );
            }
        }
        if self.rebind_target.is_some() {
            list = list.child(
                div()
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(10.))
                    .bg(p.accs)
                    .text_size(px(12.))
                    .text_color(p.acc)
                    .child("请选择下方终端作为原目标的新关联"),
            );
        }
        let multiple_windows = entries.iter().map(|e| &e.window).collect::<BTreeSet<_>>().len() > 1;
        for endpoint in entries {
            let id = endpoint.target.session_id.clone();
            let default_id = id.clone();
            let default_endpoint = endpoint.clone();
            let is_default = self.runtime.as_ref().map(|r| {
                r.router.scope.lock().unwrap().default.as_ref() == Some(&id)
            }).unwrap_or(self.session == Some(endpoint.binding.session));
            let label = endpoint.target.label.clone();
            let permission = self.db.borrow().ai_permission(if endpoint.binding.local {
                "local"
            } else {
                endpoint.target.host_id.as_deref().unwrap_or("temporary")
            }).unwrap_or_default();
            let (state, dot_color) = if !endpoint.connected {
                ("已断开", p.tx3)
            } else if endpoint.binding.capabilities.terminal_busy {
                ("终端占用中", p.warn)
            } else {
                ("已连接", p.ok)
            };
            let metadata = format!("{} · {} · {}{}", endpoint.target.protocol, state, permission.label(),
                if multiple_windows {format!(" · {}", endpoint.window)} else {String::new()});
            let session = endpoint.binding.session;
            let mut actions = agent_ui::step_actions().flex_wrap()
                .child(
                    agent_ui::pill_action(
                        SharedString::from(format!("locate-{id}")),
                        "定位",
                        agent_ui::ActionTone::Surface,
                        cx,
                    )
                    .h(px(24.))
                    .px(px(8.))
                    .on_click(cx.listener(move |s, _, w, cx| {
                        let ws = s.workspace.clone();
                        w.defer(cx, move |w, cx| { ws.update(cx, |s, cx| s.locate_ai_terminal(session, w, cx)).ok(); });
                    })),
                );
            if endpoint.connected {
                actions = actions.child(
                    agent_ui::pill_action(
                        SharedString::from(format!("default-{id}")),
                        if is_default { "默认 ✓" } else { "设为默认" },
                        if is_default {
                            agent_ui::ActionTone::Accent
                        } else {
                            agent_ui::ActionTone::Surface
                        },
                        cx,
                    )
                    .h(px(24.))
                    .px(px(8.))
                    .on_click(cx.listener(move |s, _, _, cx| {
                        if let Some(r) = &s.runtime {
                            if r.active() {
                                s.state.save_error = Some("当前任务的默认终端已固定；可引用其他终端指定后续操作".into());
                                cx.notify();
                                return;
                            }
                            if let Err(e) = r.router.add(&default_id, true) {s.state.save_error = Some(e);}
                            r.data.lock().unwrap().revision += 1;
                            r.save(&[], &[]);
                        } else {
                            s.session = Some(default_endpoint.binding.session);
                            s.binding = Some(default_endpoint.binding.clone());
                            s.capabilities = default_endpoint.binding.capabilities.clone();
                            *s.mode.lock().unwrap() = permission;
                            s.label = default_endpoint.target.label.clone();
                            s.protocol = default_endpoint.target.protocol.clone();
                            s.host = default_endpoint.target.host_id.clone();
                            s.context_bound = true;
                        }
                        cx.notify();
                    })),
                )
                .child(
                    agent_ui::pill_action(
                        SharedString::from(format!("mention-{id}")),
                        if self.rebind_target.is_some() { "关联到此" } else { "引用" },
                        if self.rebind_target.is_some() {
                            agent_ui::ActionTone::Accent
                        } else {
                            agent_ui::ActionTone::Surface
                        },
                        cx,
                    )
                    .h(px(24.))
                    .px(px(8.))
                    .on_click(cx.listener(move |s, _, w, cx| {
                        if let Some(old) = s.rebind_target.take() {
                            if let Some(r) = &s.runtime {
                                if let Err(e) = r.router.rebind(&old, &id) {s.state.save_error = Some(e);}
                                r.data.lock().unwrap().revision += 1;
                                r.save(&[], &[]);
                            }
                        } else {
                            s.composer_targets.insert(id.clone());
                            let draft = s.input.read(cx).value().to_string();
                            let text = if draft.ends_with('@') {format!("{}@{label} ", draft.trim_end_matches('@'))}
                                else {format!("{}{separator}@{label} ", draft, separator=if draft.is_empty()||draft.ends_with(char::is_whitespace){""}else{" "})};
                            s.set_composer_text(text, w, cx);
                            s.input.focus_handle(cx).focus(w, cx);
                            s.target_picker = false;
                        }
                        cx.notify();
                    })),
                );
            }
            list = list.child(
                div()
                    .p(px(10.))
                    .rounded(px(12.))
                    .bg(p.card)
                    .border_1()
                    .border_color(if is_default { p.acc.opacity(0.28) } else { p.line })
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(crate::ui::dot(dot_color, 6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.tx)
                                    .whitespace_normal()
                                    .child(endpoint.target.label),
                            )
                            .when(is_default, |d| {
                                d.child(agent_ui::status_badge("默认", p.acc, p.accs, false))
                            }),
                    )
                    .child(div().text_size(px(11.)).text_color(p.tx3).whitespace_normal().child(metadata))
                    .child(div().text_size(px(11.)).font_family(crate::theme::MONO_FONT).text_color(p.tx2).whitespace_normal()
                        .child(endpoint.target.cwd.unwrap_or_else(|| "目录未知".into())))
                    .child(actions),
            );
        }
        list
    }
}
