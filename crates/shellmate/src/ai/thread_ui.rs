//! Navigation is presentation-only; it never starts, stops or restores a worker.
use super::*;
use shellmate_ai::{Entry, MessageOrigin};
use std::collections::{BTreeMap, HashSet};
pub(super) struct Page {
    pub error: Option<String>,
    pub unsent: Vec<String>,
    pub draft: String,
    pub scroll: ScrollHandle,
    pub follow: bool,
    pub seen: u64,
    pub expanded: HashSet<usize>,
    pub details: bool,
}
impl Default for Page {
    fn default() -> Self {
        Self {
            error: None,
            unsent: vec![],
            draft: String::new(),
            scroll: Default::default(),
            follow: true,
            seen: 0,
            expanded: Default::default(),
            details: false,
        }
    }
}
#[derive(Default)]
pub(super) struct Navigation {
    pub selected: Option<String>,
    pub pages: BTreeMap<String, Page>,
    pub main_draft: String,
    pub main_target: Option<String>,
    pub main_follow: bool,
    pub main_scroll: Option<ScrollHandle>,
}
impl Navigation {
    fn enter(
        &mut self,
        id: &str,
        draft: String,
        scroll: ScrollHandle,
        follow: bool,
        target: Option<String>,
    ) -> (String, ScrollHandle, bool) {
        if let Some(current) = &self.selected {
            let page = self.pages.entry(current.clone()).or_default();
            page.draft = draft;
            page.follow = follow;
            page.scroll = scroll;
        } else {
            self.main_draft = draft;
            self.main_scroll = Some(scroll);
            self.main_follow = follow;
            self.main_target = target;
        }
        self.selected = Some(id.into());
        let page = self.pages.entry(id.into()).or_default();
        (page.draft.clone(), page.scroll.clone(), page.follow)
    }
    fn leave(
        &mut self,
        draft: String,
        follow: bool,
    ) -> Option<(String, ScrollHandle, bool, Option<String>)> {
        let id = self.selected.take()?;
        let page = self.pages.entry(id).or_default();
        page.draft = draft;
        page.follow = follow;
        Some((
            std::mem::take(&mut self.main_draft),
            self.main_scroll.take().unwrap_or_default(),
            self.main_follow,
            self.main_target.take(),
        ))
    }
}
impl Drawer {
    #[cfg(debug_assertions)]
    pub(super) fn child_fixture(&mut self, scene: &str, w: &mut Window, cx: &mut Context<Self>) {
        use shellmate_ai::{
            LoggedCall, LoggedResult, Step, SubagentStatus, subagent::SubagentEvent,
        };
        let mut view = crate::proto::subagents::view("agent-1", "检查 nginx 错误日志");
        let prompt = "检查 nginx 最近一小时的错误日志，确认 502 的主要原因。\n先关联错误发生时间与上游服务状态，不要修改配置或重启服务。\n返回关键证据、结论以及尚未确认的事项。";
        let prompt = if scene == "ai-child-long-prompt" {
            format!(
                "{prompt}\n{}",
                "检查部署后的错误变化，保留原始证据并说明时间范围。\n".repeat(30)
            )
        } else {
            prompt.into()
        };
        let mut log = shellmate_ai::Conversation::default();
        log.push_user_from(
            shellmate_ai::UserTurn::new(&prompt, Default::default()),
            Some(MessageOrigin::Delegation),
        );
        log.push_text("我先确认错误发生时间，再检查对应的上游连接。");
        let step = |id: &str, name: &str, args: serde_json::Value, result: &str, error: bool| {
            Entry::Step(Step {
                message_id: 2,
                call: LoggedCall {
                    id: id.into(),
                    name: name.into(),
                    args_json: args,
                },
                result: LoggedResult {
                    inline: result.into(),
                    output_id: None,
                    is_error: error,
                    cleared: false,
                    subagent: None,
                },
                display: None,
            })
        };
        log.append(step("command-1","run_command",serde_json::json!({"command":"journalctl -u nginx --since '1 hour ago'","cwd":"/srv/app"}),"14:03:12 upstream timed out\nupstream: http://127.0.0.1:8080/api/orders",false));
        log.append(step(
            "read-1",
            "read_output",
            serde_json::json!({"id":"output-1","grep":"upstream","limit_lines":80}),
            "共发现 23 次 upstream timeout，集中在 14:03–14:06。",
            false,
        ));
        log.push_text("超时集中在部署后的三分钟内，接下来检查应用日志。");
        log.push_user_from(
            shellmate_ai::UserTurn::new("也检查一下应用连接池是否耗尽。", Default::default()),
            Some(MessageOrigin::User),
        );
        log.append(step(
            "memory-1",
            "memory",
            serde_json::json!({"command":"view","path":"/memories/services.md"}),
            "应用服务使用 PostgreSQL 连接池，最大连接数 20。",
            false,
        ));
        if scene == "ai-child-thread" {
            log.append(step(
                "custom-1",
                "inspect_service",
                serde_json::json!({"service":"orders"}),
                "应用请求排队 31 秒，连接池等待数为 18。",
                false,
            ));
        }
        log.push_text("nginx 的 502 与上游响应超时一致，应用连接池等待是下一步需要验证的线索。未修改任何配置。");
        view.event(SubagentEvent::Snapshot(log.entries()));
        view.steps = log
            .entries()
            .iter()
            .filter(|e| matches!(e, Entry::Step(_)))
            .count();
        view.report = "nginx 的 502 与上游响应超时一致，尚需验证连接池等待。".into();
        view.cwd = Some("/srv/app".into());
        view.prompt = prompt;
        view.status = SubagentStatus::Completed;
        if matches!(scene, "ai-child-running" | "ai-child-approval") {
            view.status = SubagentStatus::Running;
            view.active_tools.push(LoggedCall {
                id: "waiting-1".into(),
                name: "wait_tasks".into(),
                args_json: serde_json::json!({"ids":["task-1"],"mode":"all"}),
            });
        }
        if scene == "ai-child-approval" {
            let (source, call) = crate::proto::subagents::approvals().remove(0);
            view.active_tools.clear();
            view.event(SubagentEvent::Agent(&AgentEvent::StepProposed(
                call.clone(),
            )));
            let key = crate::ai::host::approval_key(Some(&source), &call.id);
            let mut active = agent_ui::ActiveStep::new(call);
            active.source = Some(source.clone());
            self.state.active_steps.insert(key, Some(source.id), active);
        }
        if scene == "ai-child-restored" {
            view.history = true;
            view.transcript = None;
            view.transcript_id = None;
            view.output_id = None;
        }
        self.state.subagents.insert(view.id.clone(), view);
        self.set_composer_text("主对话中尚未发送的草稿", w, cx);
        self.open_child("agent-1", w, cx);
        if scene == "ai-child-long-prompt" {
            self.state.follow_bottom = false;
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
    }
    pub(super) fn child_error(&mut self, text: String, cx: &mut Context<Self>) {
        if let Some(id) = self.state.child_page.selected.clone() {
            self.state.child_page.pages.entry(id).or_default().error = Some(text);
            cx.notify();
        } else {
            self.local(slash_ui::LocalBody::Hint(text), cx);
        }
    }
    pub(super) fn open_child(&mut self, id: &str, w: &mut Window, cx: &mut Context<Self>) {
        let (draft, scroll, follow) = self.state.child_page.enter(
            id,
            self.input.read(cx).value().to_string(),
            self.scroll.clone(),
            self.state.follow_bottom,
            self.state.message_target.take(),
        );
        self.scroll = scroll;
        self.state.follow_bottom = follow;
        self.input.update(cx, |i, cx| {
            i.set_placeholder("给这个子任务补充要求…", w, cx)
        });
        self.state.message_target = Some(id.into());
        if let Some(view) = self.state.subagents.get_mut(id) {
            view.load_transcript(&self.data_root, &self.state.id);
        }
        self.selection_scroll
            .update(cx, |s, _| s.set_conversation(self.scroll.clone()));
        self.set_composer_text(draft, w, cx);
        self.popup.escape();
        self.permission_menu = false;
        self.state.context_popover.close();
        cx.notify();
    }
    pub(super) fn back_to_main(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some((draft, scroll, follow, target)) = self.state.child_page.leave(
            self.input.read(cx).value().to_string(),
            self.state.follow_bottom,
        ) else {
            return;
        };
        self.input.update(cx, |i, cx| {
            i.set_placeholder("描述要做的事，例如：找出最占内存的 5 个进程", w, cx)
        });
        self.scroll = scroll;
        self.state.follow_bottom = follow;
        self.state.message_target = target;
        self.selection_scroll
            .update(cx, |s, _| s.set_conversation(self.scroll.clone()));
        self.set_composer_text(draft, w, cx);
        self.release_context(cx);
        cx.notify();
    }
    fn child_message(
        &self,
        id: &str,
        index: usize,
        text: &str,
        origin: Option<MessageOrigin>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = cx.palette();
        let user = origin == Some(MessageOrigin::User);
        let origin_label = match origin {
            Some(MessageOrigin::Delegation) => "主代理委派",
            Some(MessageOrigin::Parent) => "主代理补充",
            Some(MessageOrigin::User) => "你追加的要求",
            None => "任务输入",
        };
        let expanded = self
            .state
            .child_page
            .pages
            .get(id)
            .is_some_and(|p| p.expanded.contains(&index));
        let long = text.chars().count() > 240 || text.lines().count() > 6;
        let preview = if long && !expanded {
            format!(
                "{}…",
                text.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(100)
                    .collect::<String>()
            )
        } else {
            text.to_owned()
        };
        let scope = format!("{}:{id}:user:{index}", self.state.id);
        let mut message = div()
            .min_w_0()
            .w_full()
            .p(px(10.))
            .rounded(px(13.))
            .bg(if user { p.accs } else { p.card2 })
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(p.tx3)
                    .child(origin_label),
            )
            .child(
                super::subagent_ui::selectable(format!("{scope}:text"), &preview, w, cx)
                    .text_size(px(13.))
                    .line_height(px(20.8))
                    .whitespace_normal()
                    .when(long && !expanded, |d| d.line_clamp(5).overflow_hidden()),
            );
        let copy = text.to_owned();
        let target = id.to_owned();
        let mut actions = agent_ui::step_actions();
        if long {
            actions = actions.child(
                agent_ui::step_action(
                    SharedString::from(format!("{scope}:expand")),
                    if expanded {
                        "收起说明"
                    } else {
                        "展开完整说明"
                    },
                    cx,
                )
                .on_click(cx.listener(move |s, _, _, cx| {
                    let p = s.state.child_page.pages.entry(target.clone()).or_default();
                    if !p.expanded.remove(&index) {
                        p.expanded.insert(index);
                    }
                    s.state.follow_bottom = false;
                    cx.notify();
                })),
            );
        }
        actions = actions.child(
            agent_ui::step_action(SharedString::from(format!("{scope}:copy")), "复制", cx)
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                }),
        );
        message = message.child(actions);
        message.into_any_element()
    }
    pub(super) fn render_child(&mut self, w: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let id = self.state.child_page.selected.clone().unwrap();
        let resolved = self
            .tasks
            .as_ref()
            .and_then(|t| t.latest_agent(&id))
            .map(|(v, _, _)| v.id)
            .unwrap_or_else(|| id.clone());
        let view = self
            .state
            .subagents
            .get(&resolved)
            .or_else(|| self.state.subagents.get(&id))
            .cloned();
        let Some(view) = view else {
            return div()
                .child("子任务记录不可用")
                .child(
                    agent_ui::step_action("child-back-missing", "返回主对话", cx)
                        .on_click(cx.listener(|s, _, w, cx| s.back_to_main(w, cx))),
                )
                .into_any_element();
        };
        let p = cx.palette().clone();
        let running = view.status == shellmate_ai::SubagentStatus::Running;
        let page = self.state.child_page.pages.entry(id.clone()).or_default();
        let unseen = !self.state.follow_bottom && page.seen != view.revision;
        if self.state.follow_bottom {
            page.seen = view.revision;
            self.scroll.scroll_to_bottom();
        }
        let details = page.details;
        let error = page.error.clone();
        let unsent = page.unsent.clone();
        let secs = if running && !self.fixture {
            view.started.elapsed().as_secs()
        } else {
            view.duration_ms / 1000
        };
        let status = format!(
            "{} · {} 步 · {}",
            crate::ai::tool_model::child_status(&view.status).label(),
            view.steps,
            crate::ai::tool_model::elapsed(secs * 1000)
        );
        let mut header = div()
            .flex_none()
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        agent_ui::step_action("child-back", "← 主对话", cx)
                            .on_click(cx.listener(|s, _, w, cx| s.back_to_main(w, cx))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(80.))
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(view.description.clone()),
                    ),
            )
            .child(div().text_size(px(11.5)).text_color(p.tx3).child(status))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(12.))
                    .child(
                        agent_ui::step_action("child-prompt", "任务说明", cx).on_click(
                            cx.listener(|s, _, _, cx| {
                                s.state.follow_bottom = false;
                                s.scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        agent_ui::step_action("child-details", "运行信息", cx).on_click(
                            cx.listener(|s, _, _, cx| {
                                if let Some(id) = s.state.child_page.selected.clone() {
                                    let p = s.state.child_page.pages.entry(id).or_default();
                                    p.details = !p.details;
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .when(!running, |d| {
                        let report = crate::ai::subagent_card::copy_report(&view);
                        d.child(
                            agent_ui::step_action("child-copy-report", "复制报告", cx).on_click(
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(report.clone()))
                                },
                            ),
                        )
                    }),
            );
        if details {
            let context = view.transcript.as_ref().and_then(|entries| {
                entries.iter().find_map(|e| {
                    if let Entry::User { context, .. } = e {
                        Some(context)
                    } else {
                        None
                    }
                })
            });
            header = header
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.tx2)
                        .whitespace_normal()
                        .child(format!(
                            "{} · {}\n目录：{}\n模型：{} · 思考 {}",
                            context
                                .map(|c| c.label.as_str())
                                .filter(|s| !s.is_empty())
                                .unwrap_or(&self.label),
                            context
                                .map(|c| c.protocol.as_str())
                                .filter(|s| !s.is_empty())
                                .unwrap_or(&self.protocol),
                            view.cwd.as_deref().unwrap_or("历史记录未保存"),
                            view.model,
                            view.reasoning.label()
                        )),
                )
                .children(
                    crate::ai::subagent_card::meta(&view)
                        .map(|meta| div().text_size(px(11.)).text_color(p.tx3).child(meta)),
                );
        }
        let scope = format!("child:{}", view.id);
        let mut body = div()
            .id("child-transcript")
            .track_scroll(&self.scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .px(px(14.))
            .pb(px(12.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .on_scroll(cx.listener(|_, _, _, cx| {
                let drawer = cx.entity().downgrade();
                cx.defer(move |cx| {
                    drawer
                        .update(cx, |s, cx| {
                            s.state.follow_bottom =
                                s.scroll.offset().y + s.scroll.max_offset().y <= px(4.);
                            cx.notify();
                        })
                        .ok();
                });
            }));
        if let Some(entries) = view
            .transcript
            .as_ref()
            .filter(|entries| !entries.is_empty())
        {
            for item in grouped(entries) {
                match item {
                    Item::User {
                        index,
                        text,
                        origin,
                    } => body = body.child(self.child_message(&id, index, &text, origin, w, cx)),
                    Item::Text { index, text } => {
                        body = body.child(markdown::drawer_view(
                            SharedString::from(format!("{}:{scope}:text:{index}", self.state.id))
                                .into(),
                            text.into(),
                            self.selection_scroll.clone(),
                            self.fixture_copy,
                            w,
                            cx,
                        ))
                    }
                    Item::Step(step) => {
                        body = body.child(self.tool_card(
                            &scope,
                            &step.call,
                            Some(&step.result),
                            step.display.as_ref(),
                            w,
                            cx,
                        ))
                    }
                    Item::Notice(text) => {
                        body = body.child(
                            div()
                                .text_size(px(11.5))
                                .text_color(p.tx3)
                                .whitespace_normal()
                                .child(text),
                        )
                    }
                }
            }
        } else {
            if !view.prompt.is_empty() {
                body = body.child(self.child_message(&id, 0, &view.prompt, None, w, cx));
            }
            for step in &view.step_list {
                body = body.child(
                    super::subagent_ui::selectable(
                        format!("{scope}:legacy:{}", step.id),
                        &format!("{}\n{}", step.command, step.output),
                        w,
                        cx,
                    )
                    .text_size(px(12.))
                    .whitespace_normal(),
                );
            }
            body = body.child(div().text_size(px(12.)).text_color(p.tx3).child(
                crate::ai::subagent_card::transcript_note(&view).unwrap_or(if running {
                    "正在载入执行记录…"
                } else {
                    "执行记录不可用，以下为保留的报告"
                }),
            ));
            body = body.child(markdown::drawer_view(
                SharedString::from(format!("{}:{scope}:fallback", self.state.id)).into(),
                view.report.clone().into(),
                self.selection_scroll.clone(),
                self.fixture_copy,
                w,
                cx,
            ));
        }
        for call in &view.active_tools {
            let approval = self
                .state
                .active_steps
                .iter()
                .find(|(_, s)| {
                    s.source.as_ref().is_some_and(|source| source.id == view.id)
                        && s.call.id == call.id
                })
                .map(|(key, _)| key.to_owned());
            body = body.child(if let Some(key) = approval {
                self.active_card(&key, cx)
            } else {
                self.tool_card(&scope, call, None, None, w, cx)
            });
        }
        for (key, step) in self
            .state
            .active_steps
            .iter()
            .filter(|(_, s)| s.source.as_ref().is_some_and(|source| source.id == view.id))
        {
            if !view.active_tools.iter().any(|c| c.id == step.call.id) {
                body = body.child(self.active_card(key, cx));
            }
        }
        if view.saving {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(p.err)
                    .child("执行记录保存失败，已暂停后续步骤，正在重试"),
            );
        }
        if !running {
            body = body.child(div().text_size(px(11.5)).text_color(p.tx3).child(format!(
                "{} · 运行 {}",
                crate::ai::subagent_view::status_label(&view.status),
                timing::duration(view.duration_ms)
            )));
        }
        let queued = self
            .tasks
            .as_ref()
            .and_then(|t| t.latest_agent(&id))
            .and_then(|(_, _, t)| t)
            .map(|t| t.control().snapshot().inputs)
            .unwrap_or_else(|| view.pending_inputs.clone().into());
        for (n, input) in queued.iter().filter(|i| !i.claimed).enumerate() {
            body = body
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(p.tx3)
                        .child("待处理消息"),
                )
                .child(self.child_message(&id, usize::MAX - n, &input.text, input.origin, w, cx));
        }
        let mut footer = div()
            .flex_none()
            .p(px(12.))
            .border_t_1()
            .border_color(p.line)
            .flex()
            .flex_col()
            .gap(px(6.));
        let other_approvals = self
            .state
            .active_steps
            .iter()
            .filter(|(_, s)| s.source.as_ref().is_none_or(|source| source.id != view.id))
            .count();
        if other_approvals > 0 {
            footer = footer.child(
                div()
                    .text_size(px(11.5))
                    .text_color(p.warn)
                    .child(format!("其他任务有 {other_approvals} 项待批准"))
                    .child(
                        agent_ui::step_action("child-other-approvals", "返回查看", cx)
                            .on_click(cx.listener(|s, _, w, cx| s.back_to_main(w, cx))),
                    ),
            );
        }
        if let Some(error) = error {
            footer = footer.child(
                div()
                    .text_size(px(12.))
                    .text_color(p.err)
                    .whitespace_normal()
                    .child(error),
            );
        }
        for (index, text) in unsent.into_iter().enumerate() {
            let target = id.clone();
            footer = footer.child(
                div()
                    .text_size(px(11.5))
                    .text_color(p.tx2)
                    .child(format!(
                        "未发送：{}",
                        text.chars().take(70).collect::<String>()
                    ))
                    .child(
                        agent_ui::step_action(
                            SharedString::from(format!("child-restore-{index}")),
                            "恢复消息",
                            cx,
                        )
                        .on_click(cx.listener(move |s, _, w, cx| {
                            let draft = s.input.read(cx).value().to_string();
                            let page = s.state.child_page.pages.entry(target.clone()).or_default();
                            if index >= page.unsent.len() {
                                return;
                            }
                            let text = page.unsent.remove(index);
                            if !draft.is_empty() {
                                page.unsent.push(draft);
                            }
                            s.set_composer_text(text, w, cx);
                            cx.notify();
                        })),
                    ),
            );
        }
        if unseen {
            footer = footer.child(
                agent_ui::step_action("child-latest", "有新进展 · 回到最新", cx).on_click(
                    cx.listener(|s, _, _, cx| {
                        s.state.follow_bottom = true;
                        cx.notify();
                    }),
                ),
            );
        }
        if !queued.is_empty() {
            footer = footer.child(
                div()
                    .text_size(px(11.5))
                    .text_color(p.tx3)
                    .child(format!("{} 条消息待处理", queued.len())),
            );
        }
        footer = footer.child(
            div()
                .text_size(px(11.5))
                .text_color(p.tx2)
                .child(format!("发送给：{}", view.description)),
        );
        let mut actions = agent_ui::step_actions();
        if running {
            let stop = view.id.clone();
            actions =
                actions
                    .child(agent_ui::step_action("child-now", "立即处理", cx).on_click(
                        cx.listener(|s, _, w, cx| {
                            let text = s.input.read(cx).value().to_string();
                            s.send_to_child(text, true, w, cx);
                        }),
                    ))
                    .child(
                        agent_ui::step_action("child-stop", "停止任务", cx).on_click(cx.listener(
                            move |s, _, _, cx| {
                                if let Some(t) = &s.tasks {
                                    t.stop(&stop);
                                }
                                cx.notify();
                            },
                        )),
                    );
        }
        footer = footer.child(actions).child(
            div()
                .flex()
                .items_end()
                .gap(px(6.))
                .p(px(8.))
                .rounded(px(13.))
                .bg(p.card2)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .max_h(px(120.))
                        .overflow_hidden()
                        .child(
                            Textarea::new(&self.input)
                                .xsmall()
                                .appearance(false)
                                .text_size(px(13.)),
                        ),
                )
                .child(
                    crate::dialogs::button(
                        "child-send",
                        if running { "排队" } else { "继续执行" },
                        crate::dialogs::Tone::Primary,
                        true,
                    )
                    .on_click(cx.listener(|s, _, w, cx| s.send_input(w, cx))),
                ),
        );
        div()
            .key_context("AiChildThread")
            .capture_action(cx.listener(Self::send_now_action))
            .capture_action(cx.listener(Self::slash_action::<input::Escape>))
            .w_full()
            .h_full()
            .min_h_0()
            .rounded(px(18.))
            .bg(p.card)
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(crate::ai::selection_scroll::layer(
                        self.selection_scroll.clone(),
                    ))
                    .child(body),
            )
            .child(footer)
            .into_any_element()
    }
}
pub(super) enum Item {
    User {
        index: usize,
        text: String,
        origin: Option<MessageOrigin>,
    },
    Text {
        index: usize,
        text: String,
    },
    Step(Box<shellmate_ai::Step>),
    Notice(String),
}
pub(super) fn grouped(entries: &[Entry]) -> Vec<Item> {
    let mut out = vec![];
    let mut last_message = None;
    for (index, e) in entries.iter().enumerate() {
        match e {
            Entry::User { text, origin, .. } => {
                out.push(Item::User {
                    index,
                    text: text.clone(),
                    origin: *origin,
                });
                last_message = None;
            }
            Entry::Assistant { text, message_id } => {
                if last_message == Some(*message_id)
                    && let Some(Item::Text { text: previous, .. }) = out.last_mut()
                {
                    previous.push_str(text);
                } else {
                    out.push(Item::Text {
                        index,
                        text: text.clone(),
                    });
                }
                last_message = Some(*message_id);
            }
            Entry::Step(s) => {
                out.push(Item::Step(Box::new(s.clone())));
                last_message = None;
            }
            Entry::Notice { text, .. } => {
                out.push(Item::Notice(text.clone()));
                last_message = None;
            }
            Entry::Compaction(c) => {
                out.push(Item::Notice(crate::ai::model::compaction_label(c)));
                last_message = None;
            }
            Entry::CommandCard { .. } => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Item, Navigation, grouped};
    use shellmate_ai::{Entry, MessageOrigin};
    #[test]
    fn navigation_restores_independent_drafts_scroll_and_target_without_touching_workers() {
        let mut nav = Navigation::default();
        let main = gpui::ScrollHandle::default();
        main.set_offset(gpui::point(gpui::px(0.), gpui::px(-80.)));
        let (first, scroll, follow) = nav.enter(
            "a",
            "main draft".into(),
            main,
            false,
            Some("original-target".into()),
        );
        assert!(first.is_empty() && follow);
        scroll.set_offset(gpui::point(gpui::px(0.), gpui::px(-120.)));
        nav.enter("b", "child a draft".into(), scroll, false, Some("a".into()));
        let (main, scroll, follow, target) = nav.leave("child b draft".into(), true).unwrap();
        assert_eq!(main, "main draft");
        assert_eq!(scroll.offset().y, gpui::px(-80.));
        assert!(!follow);
        assert_eq!(target.as_deref(), Some("original-target"));
        let (draft, scroll, follow) = nav.enter("a", main, scroll, follow, target);
        assert_eq!(draft, "child a draft");
        assert_eq!(scroll.offset().y, gpui::px(-120.));
        assert!(!follow);
        assert_eq!(nav.pages["b"].draft, "child b draft");
    }
    #[test]
    fn timeline_keeps_all_tools_and_sender_order_and_only_merges_adjacent_text() {
        let step = shellmate_ai::Step {
            message_id: 1,
            call: shellmate_ai::LoggedCall {
                id: "tool".into(),
                name: "new_tool".into(),
                args_json: serde_json::json!({"path":"/srv/app"}),
            },
            result: shellmate_ai::LoggedResult {
                inline: "complete result".into(),
                output_id: None,
                is_error: false,
                cleared: false,
                subagent: None,
            },
            display: None,
        };
        let entries = vec![
            Entry::User {
                text: "delegate".into(),
                context: Default::default(),
                origin: Some(MessageOrigin::Delegation),
            },
            Entry::Assistant {
                text: "a".into(),
                message_id: 1,
            },
            Entry::Assistant {
                text: "b".into(),
                message_id: 1,
            },
            Entry::Step(step),
            Entry::Assistant {
                text: "c".into(),
                message_id: 1,
            },
            Entry::User {
                text: "followup".into(),
                context: Default::default(),
                origin: Some(MessageOrigin::User),
            },
        ];
        let items = grouped(&entries);
        assert_eq!(items.len(), 5);
        assert!(
            matches!(&items[0],Item::User{origin:Some(MessageOrigin::Delegation),text,..} if text=="delegate")
        );
        assert!(matches!(&items[1],Item::Text{text,..} if text=="ab"));
        assert!(
            matches!(&items[2],Item::Step(s) if s.call.name=="new_tool"&&s.result.inline=="complete result")
        );
        assert!(matches!(&items[3],Item::Text{text,..} if text=="c"));
        assert!(matches!(
            &items[4],
            Item::User {
                origin: Some(MessageOrigin::User),
                ..
            }
        ));
    }
}
