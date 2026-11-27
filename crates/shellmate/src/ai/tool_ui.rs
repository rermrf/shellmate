//! Shared tool shell, disclosure and actions for every thread and execution phase.
use super::*;
use crate::ai::{
    tasks::{Kind, Status as TaskStatus, TaskInfo, Tasks},
    tool_model as model,
};
use shellmate_ai::{LoggedCall, LoggedResult, StepDisplay, StepStatus};
pub(super) fn key(scope: &str, id: &str) -> String {
    format!("tool:{scope}:{id}")
}
pub(super) struct Card {
    pub key: String,
    pub title: String,
    pub status: model::Status,
    pub duration: Option<u64>,
    pub summary: String,
    pub detail: String,
}
impl Drawer {
    #[cfg(debug_assertions)]
    pub(super) fn unified_fixture(&mut self, scene: &str, cx: &mut Context<Self>) {
        use shellmate_ai::{
            Conversation, Entry, Step, Target, UserTurn,
            policy::Class,
            web::{Hit, ResultData},
        };
        self.state.model.conversation = Conversation::default();
        self.state.model.conversation.push_user(UserTurn::new(
            "检查 nginx 超时原因，并核对官方文档",
            Default::default(),
        ));
        self.state.model.busy = false;
        self.state.active_steps = Default::default();
        self.fixture_tasks.clear();
        self.state.subagents.clear();
        let call = LoggedCall {
            id: "command".into(),
            name: "run_command".into(),
            args_json: serde_json::json!({"command":"journalctl -u nginx --since '1 hour ago'","cwd":"/srv/app","target":"background"}),
        };
        let display = StepDisplay {
            command: call.args_json["command"].as_str().unwrap().into(),
            target: Target::Background,
            purpose: "检查最近一小时日志".into(),
            class: Class::ReadOnly,
            status: StepStatus::Completed,
            exit_code: Some(0),
            duration_ms: 800,
            automatic: true,
            output: "14:03:12 upstream timed out\nupstream: 127.0.0.1:8080".into(),
        };
        self.state.model.conversation.append(Entry::Step(Step {
            message_id: 1,
            call,
            result: LoggedResult {
                attachments: Vec::new(), inline: display.output.clone(),
                output_id: None,
                is_error: false,
                cleared: false,
                subagent: None,
            },
            display: Some(display),
        }));
        let search = ResultData::Search {
            query: "nginx upstream timed out 官方文档".into(),
            provider: "Tavily".into(),
            results: vec![Hit {
                title: "Module ngx_http_proxy_module".into(),
                url: "https://nginx.org/en/docs/http/ngx_http_proxy_module.html".into(),
                snippet: "proxy_read_timeout 指令定义与上游之间连续读取的超时时间。".into(),
            }],
            searched_at: crate::ai::now(),
            cached: false,
            note: None,
        };
        self.state.model.conversation.append(Entry::Step(Step{message_id:1,call:LoggedCall{id:"search".into(),name:"web_search".into(),args_json:serde_json::json!({"query":"nginx upstream timed out 官方文档","max_results":5})},result:LoggedResult{attachments: Vec::new(), inline:serde_json::to_string(&search).unwrap(),output_id:None,is_error:false,cleared:false,subagent:None},display:None}));
        let mut view = crate::proto::subagents::view("agent-1", "检查应用连接池");
        view.background = true;
        view.duration_ms = 138_000;
        view.steps = 3;
        view.status = shellmate_ai::SubagentStatus::Completed;
        view.report = "应用请求排队超过 30 秒，需要继续核实连接池配置。".into();
        view.prompt = "检查应用连接池与最近一小时错误日志。只读检查，返回关键证据。".into();
        self.state.model.conversation.append(Entry::Step(Step {
            message_id: 1,
            call: LoggedCall {
                id: view.call_id.clone(),
                name: "agent".into(),
                args_json: serde_json::json!({"description":view.description,"prompt":view.prompt}),
            },
            result: LoggedResult {
                attachments: Vec::new(), inline: view.report.clone(),
                output_id: view.output_id.clone(),
                is_error: false,
                cleared: false,
                subagent: Some(Box::new(view.display())),
            },
            display: None,
        }));
        self.state.subagents.insert(view.id.clone(), view);
        self.state.model.conversation.push_text("上游响应超时与应用请求排队的时间一致。下一步应检查连接池等待，先不要调整 nginx 超时。 ");
        if scene == "ai-tools-expanded" {
            self.state
                .tool_expanded
                .insert(self.tool_key("main", "command", false));
            self.state.follow_bottom = false;
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
        cx.notify();
    }
    pub(super) fn tool_key(&self, scope: &str, id: &str, active: bool) -> String {
        let identities = scope
            .strip_prefix("child:")
            .and_then(|id| self.state.subagents.get(id))
            .map(|v| &v.tool_ids)
            .unwrap_or(&self.state.tool_ids);
        key(
            &format!("{}:{scope}", self.state.id),
            &if active {
                identities.active(id)
            } else {
                identities.saved(id)
            },
        )
    }
    pub(super) fn tool_open(&self, key: &str, status: model::Status) -> bool {
        status == model::Status::Approval
            || self.state.tool_expanded.contains(key)
            || (status.attention() && !self.state.tool_collapsed.contains(key))
    }
    pub(super) fn tool_shell(&self, card: &Card, cx: &mut Context<Self>) -> Stateful<Div> {
        let p = cx.palette().clone();
        let open = self.tool_open(&card.key, card.status);
        let toggle = card.key.clone();
        let attention = card.status == model::Status::Approval;
        let status = if let Some(ms) = card.duration {
            format!("{} · {}", card.status.label(), model::elapsed(ms))
        } else {
            card.status.label().into()
        };
        agent_ui::step_surface(&p)
            .id(SharedString::from(card.key.clone()))
            .flex_none()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(7.))
            .child(
                div()
                    .id(SharedString::from(format!("{}:header", card.key)))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(90.))
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .child(format!("{} {}", if open { "▾" } else { "▸" }, card.title)),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(card.status.color(&p))
                            .child(status),
                    )
                    .on_click(cx.listener(move |s, _, _, cx| {
                        if attention {
                            return;
                        }
                        s.state.follow_bottom = false;
                        if open {
                            s.state.tool_expanded.remove(&toggle);
                            s.state.tool_collapsed.insert(toggle.clone());
                        } else {
                            s.state.tool_collapsed.remove(&toggle);
                            s.state.tool_expanded.insert(toggle.clone());
                            if let Some(p) = s.state.output_panes.get(&toggle) {
                                p.update(cx, |s, cx| s.open(cx));
                            }
                        }
                        cx.notify();
                    })),
            )
            .when(!card.summary.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(12.5))
                        .line_height(px(19.))
                        .whitespace_normal()
                        .child(model::compact(&card.summary, 120)),
                )
            })
            .when(!card.detail.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.tx3)
                        .whitespace_normal()
                        .child(model::compact(&card.detail, 150)),
                )
            })
    }
    pub(super) fn tool_tasks(&self, scope: &str) -> Option<Tasks> {
        if let Some(id) = scope.strip_prefix("child:") {
            self.tasks.as_ref()?.child_registry(id)
        } else {
            self.tasks.clone()
        }
    }
    pub(super) fn tool_task(
        &self,
        scope: &str,
        call: &str,
        output: Option<&str>,
    ) -> Option<TaskInfo> {
        let tasks = if self.fixture {
            self.fixture_tasks.iter().filter(|t| scope == "main" || scope.strip_prefix("child:") == t.owner.as_deref()).cloned().collect()
        } else {
            self.tool_tasks(scope).map(|t| t.list()).unwrap_or_default()
        };
        let found = tasks.into_iter().rev().find(|t| {
            t.kind == Kind::Command
                && match output {
                    Some(id) => t.output_id == id,
                    None => t.call_id == call,
                }
        });
        // A resumed child transcript also includes calls from earlier runs.
        // Their persistent services remain owned by the conversation registry.
        found.or_else(|| {
            let output = output?;
            self.tasks.as_ref()?.list().into_iter().find(|t| t.service.is_some() && t.output_id == output)
        })
    }
    pub(super) fn tool_card(
        &self,
        scope: &str,
        call: &LoggedCall,
        result: Option<&LoggedResult>,
        display: Option<&StepDisplay>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = cx.palette().clone();
        let id = self.tool_key(scope, &call.id, result.is_none());
        let task = self.tool_task(scope, &call.id, result.and_then(|r| r.output_id.as_deref()));
        let mut status = model::command_status(display, result);
        if result.is_none() && call.name == "wait_tasks" {
            status = model::Status::Waiting;
        }
        let mut duration = display.filter(|d| d.duration_ms > 0).map(|d| d.duration_ms);
        let mut detail = String::new();
        if call.name == "run_command" {
            if task.is_none() && display.is_some_and(|d| d.status == StepStatus::Background) {
                status = model::Status::Unknown;
                duration = None;
            }
            let target = display.map(|d| d.target.label()).unwrap_or_else(|| {
                if call.args_json["target"] == "terminal" {
                    "终端内"
                } else {
                    "独立通道"
                }
            });
            detail = format!("{target}执行");
            if let Some(code) = display.and_then(|d| d.exit_code) {
                detail.push_str(&format!(" · 退出码 {code}"));
            }
            if let Some(task) = &task {
                match task.status {
                    TaskStatus::Running => {
                        status = model::Status::Running;
                        duration =
                            Some(task.started.elapsed().as_millis().min(u64::MAX as u128) as u64);
                    }
                    TaskStatus::Exited(code) => {
                        status = if shellmate_ai::output::successful(
                            model::command(call, display),
                            code,
                        ) {
                            model::Status::Completed
                        } else {
                            model::Status::Failed
                        };
                        detail = format!(
                            "{target}执行 · 退出码 {}",
                            code.map(|n| n.to_string()).unwrap_or_else(|| "未知".into())
                        );
                        if display.is_some_and(|d| d.status == StepStatus::Background) {
                            duration = None;
                        }
                    }
                    TaskStatus::Stopped => {
                        status = model::Status::Stopped;
                        duration = None;
                    }
                    _ => {
                        status = model::Status::Failed;
                        duration = None;
                    }
                }
            }
        }
        if call.name == "run_command" && (task.as_ref().is_some_and(|t| t.background)
            || display.is_some_and(|d| d.status == StepStatus::Background)
            || call.args_json["run_in_background"] == true) {
            detail.push_str(" · 异步任务");
        }
        let web = result
            .filter(|r| !r.is_error)
            .and_then(|r| serde_json::from_str::<shellmate_ai::web::ResultData>(&r.inline).ok());
        let summary = if call.name == "view_image" { result.and_then(|r|r.attachments.first()).map(|a|a.name.clone()).unwrap_or_else(||"读取会话中的图片".into()) } else if call.name == "run_command" {
            model::command(call, display).to_owned()
        } else {
            model::summary(call)
        };
        if let Some(data) = &web {
            detail = match data {
                shellmate_ai::web::ResultData::Search {
                    provider,
                    results,
                    cached,
                    note,
                    ..
                } => format!(
                    "找到 {} 条来源 · {provider}{}{}",
                    results.len(),
                    if *cached { " · 缓存" } else { "" },
                    if note.is_some() {
                        " · 已使用备用搜索"
                    } else {
                        ""
                    }
                ),
                shellmate_ai::web::ResultData::Page {
                    title,
                    total_chars,
                    cached,
                    ..
                } => format!(
                    "{title} · 正文 {total_chars} 字符{}",
                    if *cached { " · 缓存" } else { "" }
                ),
            };
        } else if let Some(r) = result
            && call.name != "run_command"
        {
            detail = if r.is_error {
                "操作未完成，展开查看原因"
            } else {
                match call.name.as_str() {
                    "read_output" => "已读取执行输出",
                    "view_image" => "已读取图片",
                    "memory" => "记忆操作已完成",
                    "wait_tasks" => "等待已返回，展开查看任务结果",
                    "stop_task" => "停止请求已处理",
                    "send_agent_message" => "消息已送达",
                    _ => "已返回结果，展开查看",
                }
            }
            .into();
        }
        let card = Card {
            key: id.clone(),
            title: model::label(call),
            status,
            duration,
            summary,
            detail,
        };
        let mut card = card;
        if call.name == "wait_tasks" {
            let ids = call.args_json["ids"].as_array().map(|v| v.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()).unwrap_or_default();
            let tasks = if self.fixture && scope == "main" { self.fixture_tasks.clone() } else { self.tool_tasks(scope).map(|t| t.list()).unwrap_or_default() };
            let ended = tasks.iter().filter(|t| ids.contains(&t.id.as_str()) && t.status != TaskStatus::Running).count();
            let agents = !ids.is_empty() && ids.iter().all(|id| id.starts_with("agent-") && !id.contains('/'));
            card.title = if result.is_none() {
                format!("等待 {} 个{} · {} 个已结束", ids.len(), if agents {"子代理"} else {"任务"}, ended)
            } else if result.is_some_and(|r| r.is_error) { "等待未完成".into() } else { "已结束等待".into() };
            card.summary.clear();
            card.detail.clear();
        }
        if call.name == "run_command" && let Some(task) = &task {
            if let Some(detail) = model::service_detail(task) {
                card.detail.push_str(&format!(" · {detail}"));
            }
            if task.service == Some(crate::ai::tasks::ServiceState::Failed)
                && matches!(task.status, TaskStatus::Exited(_)) {
                card.status = model::Status::Failed; status = model::Status::Failed;
            }
        }
        if let Some(label) = call.args_json["resolved_target"]["label"].as_str() {
            let target = &call.args_json["resolved_target"];
            card.detail = format!(
                "{} · {} · {} · {}",
                label,
                target["protocol"].as_str().unwrap_or_default(),
                target["cwd"].as_str().unwrap_or("目录未知"),
                card.detail
            );
        }
        let open = self.tool_open(&id, status);
        let mut surface = self.tool_shell(&card, cx);
        if call.name == "wait_tasks" {
            surface = surface.bg(gpui::transparent_black()).py(px(3.)).px(px(2.));
        }
        if open {
            if call.name == "run_command" && let Some(history) = task.as_ref().and_then(model::service_history) {
                surface = surface.child(div().text_size(px(11.5)).text_color(p.tx3).whitespace_normal().child(history));
            }
            surface = surface.child(section("输入", &p));
            if call.name == "run_command" {
                surface = surface.child(agent_ui::command_block(
                    model::command(call, display),
                    format!("{id}:command").into(),
                    self.selection_scroll.clone(),
                    cx,
                ));
                if let Some(cwd) = call.args_json["cwd"].as_str() {
                    surface = surface.child(
                        div()
                            .text_size(px(11.5))
                            .text_color(p.tx3)
                            .whitespace_normal()
                            .child(format!("目录：{cwd}")),
                    );
                }
                if let Some(d) = display
                    && !d.purpose.is_empty()
                {
                    surface = surface.child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.tx2)
                            .child(d.purpose.clone()),
                    );
                }
            } else {
                surface = surface.child(
                    super::subagent_ui::selectable(
                        format!("{id}:input"),
                        &model::summary(call),
                        w,
                        cx,
                    )
                    .text_size(px(12.))
                    .whitespace_normal(),
                );
            }
            let raw = id.clone();
            surface = surface.child(
                agent_ui::step_action(
                    SharedString::from(format!("{id}:raw")),
                    if self.state.tool_raw.contains(&id) {
                        "收起原始参数"
                    } else {
                        "原始参数"
                    },
                    cx,
                )
                .on_click(cx.listener(move |s, _, _, cx| {
                    if !s.state.tool_raw.remove(&raw) {
                        s.state.tool_raw.insert(raw.clone());
                    }
                    cx.notify();
                })),
            );
            if self.state.tool_raw.contains(&id) {
                surface = surface.child(
                    super::subagent_ui::selectable(
                        format!("{id}:args"),
                        &serde_json::to_string_pretty(&call.args_json).unwrap_or_default(),
                        w,
                        cx,
                    )
                    .text_size(px(12.))
                    .font_family(MONO_FONT)
                    .whitespace_normal(),
                );
            }
            if let Some(r)=result && !r.attachments.is_empty() {
                surface=surface.children(self.message_images(&id,&r.attachments,cx));
            } else if let Some(data) = &web {
                surface = surface
                    .child(section("结果", &p))
                    .child(self.web_content(&id, data, true, w, cx));
            } else if let Some(r) = result {
                surface = surface.child(section("结果", &p));
                if call.name == "compare_directories"
                    && let Some(content) = self.comparison_content(&id, &r.inline, w, cx)
                {
                    surface = surface.child(content);
                    if let Some(pane)=self.state.output_panes.get(&id).cloned(){
                        let key=id.clone();let expanded=self.comparison_full.contains(&id);
                        surface=surface.child(agent_ui::step_action(SharedString::from(format!("{id}:full-comparison")),if expanded{"收起完整清单"}else{"展开完整清单"},cx).on_click(cx.listener(move|s,_,_,cx|{if !s.comparison_full.remove(&key){s.comparison_full.insert(key.clone());}cx.notify();})));
                        if expanded{surface=surface.child(pane);}
                    }
                } else if matches!(call.name.as_str(),"list_sessions"|"read_terminal") && let Some(content)=self.session_result_content(&id,&r.inline,w,cx){
                    surface=surface.child(content);
                } else if let Some(pane) = self.state.output_panes.get(&id) {
                    surface = surface.child(pane.clone());
                } else {
                    surface = surface.child(
                        super::subagent_ui::selectable(format!("{id}:result"), &r.inline, w, cx)
                            .text_size(px(12.))
                            .whitespace_normal(),
                    );
                }
            } else if let Some(pane) = self.state.output_panes.get(&id) {
                surface = surface.child(section("输出", &p)).child(pane.clone());
            }
        }
        let mut actions = agent_ui::step_actions().flex_wrap();
        let mut has_action = false;
        if open {
            let copy = if call.name == "run_command" {
                model::command(call, display).to_owned()
            } else {
                serde_json::to_string_pretty(&call.args_json).unwrap_or_default()
            };
            actions = actions.child(
                agent_ui::step_action(
                    SharedString::from(format!("{id}:copy-input")),
                    if call.name == "run_command" {
                        "复制命令"
                    } else {
                        "复制输入"
                    },
                    cx,
                )
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                }),
            );
            has_action = true;
            if let Some(pane) = self.state.output_panes.get(&id).cloned() {
                actions = actions.child(
                    agent_ui::step_action(
                        SharedString::from(format!("{id}:copy-result")),
                        "复制输出",
                        cx,
                    )
                    .on_click(move |_, _, cx| pane.update(cx, |s, cx| s.copy_all(cx))),
                );
            } else if let Some(result) = result {
                let text = match &web {
                    Some(shellmate_ai::web::ResultData::Search { results, .. }) => results
                        .iter()
                        .map(|h| format!("{}\n{}\n{}", h.title, h.url, h.snippet))
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                    _ => result.inline.clone(),
                };
                actions = actions.child(
                    agent_ui::step_action(
                        SharedString::from(format!("{id}:copy-result")),
                        "复制结果",
                        cx,
                    )
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                    }),
                );
            }
        }
        if let Some(task) = task
            && task.status == TaskStatus::Running
        {
            let scope = scope.to_owned();
            actions = actions.child(
                agent_ui::step_action(SharedString::from(format!("{id}:stop")), "停止任务", cx)
                    .on_click(cx.listener(move |s, _, _, cx| {
                        let tasks = if task.owner.is_some() { s.tasks.clone() } else { s.tool_tasks(&scope) };
                        if let Some(tasks) = tasks { tasks.stop(&task.id); }
                        cx.notify();
                    })),
            );
            has_action = true;
        }
        if result.is_none() && matches!(call.name.as_str(), "web_search" | "web_fetch") {
            let scope = scope.to_owned();
            actions = actions.child(
                agent_ui::step_action(SharedString::from(format!("{id}:cancel")), "取消请求", cx)
                    .on_click(cx.listener(move |s, _, _, cx| {
                        if let Some(tasks) = s.tool_tasks(&scope) {
                            tasks
                                .control()
                                .request(shellmate_ai::interaction::Intent::SendNow);
                        }
                        cx.notify();
                    })),
            );
            has_action = true;
        }
        if has_action {
            surface = surface.child(actions);
        }
        surface.into_any_element()
    }
}
pub(super) fn section(label: &str, p: &crate::theme::Palette) -> Div {
    div()
        .text_size(px(11.))
        .text_color(p.tx3)
        .child(label.to_owned())
}
