//! Question bodies use the same tool shell in main and child transcripts.
use super::*;
use gpui::component::button::ButtonVariants as _;
use shellmate_ai::questions::{Answer, Pending, Response, Status};
use std::collections::{BTreeMap, BTreeSet};
pub(super) struct Draft {
    selected: BTreeMap<String, BTreeSet<String>>,
    text: BTreeMap<String, Entity<TextareaState>>,
    error: Option<String>,
    submitting: bool,
    save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}
impl Draft {
    fn new(question: &Pending, w: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut text = BTreeMap::new();
        let mut subscriptions = vec![];
        for q in &question.request.questions {
            let input = cx.new(|cx| {
                TextareaState::new(w, cx)
                    .auto_grow(1, 4)
                    .soft_wrap(true)
                    .placeholder(if q.options.is_empty() {
                        "填写你的回答…"
                    } else {
                        "自定义答案或补充说明…"
                    })
            });
            if let Some(answer) = question.draft.get(&q.id) {
                input.update(cx, |i, cx| i.set_value(answer.text.clone(), w, cx));
            }
            subscriptions.push(cx.observe(&input, |_, _, cx| cx.notify()));
            text.insert(q.id.clone(), input);
        }
        Self {
            selected: question
                .draft
                .iter()
                .map(|(id, a)| (id.clone(), a.selected.iter().cloned().collect()))
                .collect(),
            text,
            error: None,
            submitting: false,
            save_task: None,
            _subscriptions: subscriptions,
        }
    }
    fn response(&self, status: Status, cx: &App) -> Response {
        let answers = if status == Status::Answered {
            self.text
                .iter()
                .map(|(id, input)| {
                    (
                        id.clone(),
                        Answer {
                            selected: self
                                .selected
                                .get(id)
                                .map(|s| s.iter().cloned().collect())
                                .unwrap_or_default(),
                            text: input.read(cx).value().to_string(),
                        },
                    )
                })
                .collect()
        } else {
            BTreeMap::new()
        };
        Response { status, answers }
    }
}
impl Drawer {
    pub(super) fn question_input_focused(&self, w: &Window, cx: &App) -> bool {
        self.state
            .question_drafts
            .borrow()
            .values()
            .any(|(draft, _)| {
                draft
                    .read(cx)
                    .text
                    .values()
                    .any(|input| input.focus_handle(cx).contains_focused(w, cx))
            })
    }
    fn question_draft_changed(
        &mut self,
        scope: &str,
        id: &str,
        draft: Entity<Draft>,
        cx: &mut Context<Self>,
    ) {
        let answers = draft.read(cx).response(Status::Answered, cx).answers;
        let control = if let Some(child) = scope.strip_prefix("child:") {
            self.tasks
                .as_ref()
                .and_then(|t| t.latest_agent(child))
                .and_then(|(_, _, t)| t)
                .map(|t| t.control())
        } else {
            Some(self.state.control.clone())
        };
        let changed = if let Some(c) = control {
            if let Some(q) = self.scope_questions(scope).into_iter().find(|q| q.id == id) {
                c.register_question(q).ok();
            }
            c.update_question_draft(id, answers.clone())
        } else if let Some(child) = scope.strip_prefix("child:") {
            if let Some(view) = self.state.subagents.get_mut(child)
                && let Some(q) = view
                    .questions
                    .iter_mut()
                    .find(|q| q.id == id && q.response.is_none())
                && q.draft != answers
            {
                q.draft = answers.clone();
                true
            } else {
                false
            }
        } else {
            false
        };
        if !changed || self.fixture {
            return;
        }
        let scope = scope.to_owned();
        let id = id.to_owned();
        let conversation = self.state.id.clone();
        let source = draft.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(350))
                .await;
            this.update(cx, |s, cx| {
                if s.state.id != conversation {
                    return;
                }
                if let Some(child) = scope.strip_prefix("child:") {
                    let child = s
                        .tasks
                        .as_ref()
                        .and_then(|t| t.latest_agent(child))
                        .map(|(v, _, _)| v.id)
                        .unwrap_or_else(|| child.into());
                    let save = || {
                        super::super::subagent_files::save_question_draft(
                            &s.data_root,
                            &s.state.id,
                            &child,
                            &id,
                            answers,
                        )
                    };
                    let result = if let Some(tasks) = &s.tasks {
                        tasks.while_retained(save).unwrap_or(Ok(()))
                    } else {
                        save()
                    };
                    if result.is_err() {
                        source.update(cx, |d, cx| {
                            d.error = Some("草稿保存失败，提交前请检查存储".into());
                            cx.notify();
                        });
                    }
                } else {
                    s.persist();
                }
                cx.notify();
            })
            .ok();
        });
        draft.update(cx, |d, _| d.save_task = Some(task));
    }
    pub(super) fn scope_questions(&self, scope: &str) -> Vec<Pending> {
        let (entries, snapshot) = if let Some(child) = scope.strip_prefix("child:") {
            let Some(view) = self.state.subagents.get(child) else {
                return vec![];
            };
            (
                view.transcript.as_deref().unwrap_or(&[]),
                self.tasks
                    .as_ref()
                    .and_then(|t| t.latest_agent(child))
                    .map(|(latest, _, tasks)| {
                        tasks
                            .map(|t| t.control().snapshot().questions)
                            .unwrap_or_else(|| {
                                if latest.questions.is_empty() {
                                    view.questions.clone()
                                } else {
                                    latest.questions
                                }
                            })
                    })
                    .unwrap_or_else(|| view.questions.clone()),
            )
        } else {
            (
                self.state.model.conversation.entries(),
                self.state.control.snapshot().questions,
            )
        };
        let mut questions = shellmate_ai::questions::from_entries(entries);
        for q in snapshot {
            if let Some(old) = questions.iter_mut().find(|old| old.id == q.id) {
                *old = q;
            } else {
                questions.push(q);
            }
        }
        questions
    }
    pub(super) fn question_card(
        &self,
        scope: &str,
        call: &shellmate_ai::LoggedCall,
        result: Option<&shellmate_ai::LoggedResult>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let result = result.filter(|r| !r.is_error)?;
        let receipt: serde_json::Value = serde_json::from_str(&result.inline).ok()?;
        let request_id = receipt["request_id"].as_str()?;
        let q = self
            .scope_questions(scope)
            .into_iter()
            .find(|q| q.id == request_id)?;
        let id = self.tool_key(scope, &call.id, false);
        let pending = q.response.is_none();
        let summary = if let Some(response) = &q.response {
            q.request
                .questions
                .iter()
                .map(|question| {
                    let answer = response
                        .answers
                        .get(&question.id)
                        .map(|a| {
                            let mut values = a
                                .selected
                                .iter()
                                .filter_map(|id| {
                                    question
                                        .options
                                        .iter()
                                        .find(|o| &o.id == id)
                                        .map(|o| o.label.clone())
                                })
                                .collect::<Vec<_>>();
                            if !a.text.is_empty() {
                                values.push(a.text.clone());
                            }
                            values.join("、")
                        })
                        .unwrap_or_else(|| response.label().into());
                    format!("{} → {}", question.question, answer)
                })
                .collect::<Vec<_>>()
                .join("；")
        } else {
            q.request.questions[0].question.clone()
        };
        let mut card = tool_ui::Card {
            key: id.clone(),
            title: "向你提问".into(),
            status: match q.response.as_ref().map(|r| r.status) {
                None => crate::ai::tool_model::Status::Question,
                Some(Status::Answered) => crate::ai::tool_model::Status::Answered,
                Some(Status::Skipped) => crate::ai::tool_model::Status::Skipped,
                Some(Status::Cancelled) => crate::ai::tool_model::Status::Cancelled,
                Some(Status::Superseded) => crate::ai::tool_model::Status::Updated,
            },
            duration: None,
            summary,
            detail: if pending {
                if q.request.mode == shellmate_ai::questions::Mode::Async {
                    "可先继续其他工作，等待你的回答"
                } else {
                    "回答后继续这个任务"
                }
            } else {
                ""
            }
            .into(),
        };
        let open = self.tool_open(&id, card.status);
        if open {
            card.summary.clear();
        }
        let mut surface = self.tool_shell(&card, cx);
        if !open {
            return Some(surface.into_any_element());
        }
        let draft = {
            let mut drafts = self.state.question_drafts.borrow_mut();
            drafts
                .entry(q.id.clone())
                .or_insert_with(|| {
                    let draft = cx.new(|cx| Draft::new(&q, w, cx));
                    let scope = scope.to_owned();
                    let id = q.id.clone();
                    let subscription = cx.observe(&draft, move |s, entity, cx| {
                        s.question_draft_changed(&scope, &id, entity, cx);
                        cx.notify();
                    });
                    (draft, subscription)
                })
                .0
                .clone()
        };
        let p = cx.palette().clone();
        let mut content = div().flex().flex_col().gap(px(12.)).min_w_0().w_full();
        for (index, question) in q.request.questions.iter().enumerate() {
            let mut section = div().min_w_0().flex().flex_col().gap(px(6.)).child(
                subagent_ui::selectable(
                    format!("{id}:question:{index}"),
                    &question.question,
                    w,
                    cx,
                )
                .text_size(px(13.))
                .line_height(px(20.))
                .text_color(p.tx)
                .whitespace_normal(),
            );
            if let Some(response) = &q.response {
                if let Some(answer) = response.answers.get(&question.id) {
                    let mut pieces = answer
                        .selected
                        .iter()
                        .filter_map(|id| {
                            question
                                .options
                                .iter()
                                .find(|o| &o.id == id)
                                .map(|o| o.label.clone())
                        })
                        .collect::<Vec<_>>();
                    if !answer.text.is_empty() {
                        pieces.push(answer.text.clone());
                    }
                    section = section.child(
                        subagent_ui::selectable(
                            format!("{id}:answer:{index}"),
                            &pieces.join("\n"),
                            w,
                            cx,
                        )
                        .text_size(px(12.5))
                        .text_color(p.tx2)
                        .whitespace_normal(),
                    );
                }
            } else {
                for (n, option) in question.options.iter().enumerate() {
                    let selected = draft
                        .read(cx)
                        .selected
                        .get(&question.id)
                        .is_some_and(|s| s.contains(&option.id));
                    let option_id = option.id.clone();
                    let question_id = question.id.clone();
                    let state = draft.clone();
                    let multi = question.multi_select;
                    let button =
                        Button::new(SharedString::from(format!("{id}:option:{index}:{n}")))
                            .ghost()
                            .label(SharedString::from(format!(
                                "{} {}",
                                if selected {
                                    "✓"
                                } else if multi {
                                    "□"
                                } else {
                                    "○"
                                },
                                option.label
                            )))
                            .bg(if selected { p.accs } else { p.card })
                            .text_color(if selected { p.acc } else { p.tx2 })
                            .disabled(draft.read(cx).submitting)
                            .w_full()
                            .h_auto()
                            .min_h(px(30.))
                            .py(px(7.))
                            .rounded(px(10.))
                            .text_size(px(12.))
                            .whitespace_normal()
                            .on_click(move |_, _, cx| {
                                state.update(cx, |s, cx| {
                                    let options =
                                        s.selected.entry(question_id.clone()).or_default();
                                    if !options.remove(&option_id) {
                                        if !multi {
                                            options.clear();
                                        }
                                        options.insert(option_id.clone());
                                    }
                                    s.error = None;
                                    cx.notify();
                                })
                            });
                    section = section.child(button);
                    if !option.description.is_empty() {
                        section = section.child(
                            div()
                                .px(px(8.))
                                .text_size(px(11.))
                                .line_height(px(17.))
                                .text_color(p.tx3)
                                .whitespace_normal()
                                .child(option.description.clone()),
                        );
                    }
                }
                let input = draft.read(cx).text[&question.id].clone();
                section = section.child(
                    Textarea::new(&input)
                        .xsmall()
                        .min_h(px(34.))
                        .text_size(px(12.))
                        .disabled(draft.read(cx).submitting),
                );
            }
            content = content.child(section);
        }
        if pending {
            if let Some(error) = &draft.read(cx).error {
                content = content.child(
                    div()
                        .text_size(px(12.))
                        .text_color(p.err)
                        .whitespace_normal()
                        .child(error.clone()),
                );
            }
            let mut actions = div().flex().flex_wrap().gap(px(6.));
            for (suffix, label, status, tone) in [
                (
                    "submit",
                    "提交回答",
                    Status::Answered,
                    crate::dialogs::Tone::Primary,
                ),
                ("skip", "跳过", Status::Skipped, crate::dialogs::Tone::Plain),
                (
                    "cancel",
                    "取消问题",
                    Status::Cancelled,
                    crate::dialogs::Tone::Plain,
                ),
            ] {
                let state = draft.clone();
                let request = q.clone();
                let scope = scope.to_owned();
                actions = actions.child(
                    Button::new(SharedString::from(format!("{id}:{suffix}")))
                        .ghost()
                        .label(label)
                        .bg(if matches!(tone, crate::dialogs::Tone::Primary) {
                            p.accs
                        } else {
                            p.card
                        })
                        .text_color(if matches!(tone, crate::dialogs::Tone::Primary) {
                            p.acc
                        } else {
                            p.tx2
                        })
                        .disabled(draft.read(cx).submitting)
                        .h(px(28.))
                        .text_size(px(12.))
                        .on_click(cx.listener(move |s, _, w, cx| {
                            let response = state.read(cx).response(status, cx);
                            if let Err(e) = response.validate(&request.request) {
                                state.update(cx, |s, cx| {
                                    s.error = Some(e);
                                    cx.notify();
                                });
                                return;
                            }
                            s.submit_question(
                                scope.clone(),
                                request.id.clone(),
                                response,
                                state.clone(),
                                w,
                                cx,
                            );
                        })),
                );
            }
            content = content.child(actions);
        }
        surface = surface.child(content);
        Some(surface.into_any_element())
    }
    fn submit_question(
        &mut self,
        scope: String,
        id: String,
        response: Response,
        draft: Entity<Draft>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if draft.read(cx).submitting {
            return;
        }
        if self.fixture {
            let control = shellmate_ai::interaction::Control::default();
            for q in self.scope_questions(&scope) {
                control.register_question(q).ok();
            }
            let result = control.answer_question(&id, response, |_| Ok(()));
            if let Err(e) = result {
                draft.update(cx, |s, cx| {
                    s.error = Some(e);
                    cx.notify();
                });
                return;
            }
            control.resume();
            let inputs = control.claim();
            if let Some(child) = scope.strip_prefix("child:") {
                if let Some(view) = self.state.subagents.get_mut(child) {
                    view.questions = control.snapshot().questions;
                    for input in inputs {
                        view.transcript
                            .get_or_insert_default()
                            .push(control.input_entry(&input));
                    }
                }
            } else {
                for input in inputs {
                    self.state
                        .model
                        .conversation
                        .append(control.input_entry(&input));
                }
                self.state.control.restore(control.snapshot());
                self.state.model.busy = false;
            }
            cx.notify();
            return;
        }
        if let Some(child) = scope.strip_prefix("child:").map(str::to_owned) {
            let Some(service) = self.subagent_service.clone() else {
                let this = cx.entity().downgrade();
                let conversation = self.state.id.clone();
                super::super::authorize(
                    self.db.clone(),
                    self.credentials.clone(),
                    w,
                    cx,
                    move |result, w, cx| {
                        this.update(cx, |s, cx| {
                            if s.state.id != conversation {
                                return;
                            }
                            match result {
                                Ok(config) => {
                                    if s.install_subagent_service(config, w, cx) {
                                        s.submit_question(scope, id, response, draft, w, cx);
                                    }
                                }
                                Err(e) => draft.update(cx, |s, cx| {
                                    s.error = Some(e.to_string());
                                    cx.notify();
                                }),
                            }
                        })
                        .ok();
                    },
                );
                return;
            };
            let Some(runtime) = crate::runtime::get(cx) else {
                return;
            };
            let conversation = self.state.id.clone();
            let target = child.clone();
            draft.update(cx, |s, cx| {
                s.submitting = true;
                s.error = None;
                cx.notify();
            });
            let job =
                runtime.spawn(async move { service.answer_question(&child, &id, response).await });
            cx.spawn_in(w, async move |this, cx| {
                let result = job.await;
                this.update_in(cx, |s, _, cx| {
                    draft.update(cx, |d, cx| {
                        d.submitting = false;
                        d.error = match &result {
                            Ok(r) if !r.is_error => None,
                            Ok(r) => Some(r.inline.clone()),
                            Err(_) => Some("提交中断，请检查任务状态后重试".into()),
                        };
                        cx.notify();
                    });
                    if s.state.id != conversation {
                        return;
                    }
                    if let Some(tasks) = &s.tasks
                        && let Some((view, _, _)) = tasks.latest_agent(&target)
                    {
                        s.state.subagents.insert(view.id.clone(), view);
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        } else {
            let Some(runtime) = self.prepare_runtime(cx) else {
                return;
            };
            match runtime.answer_question(&id, response) {
                Ok(()) => {
                    let snap = runtime.snapshot();
                    if let Some(config) = snap.config {
                        runtime.launch(config, snap.limits, snap.settings);
                        self.sync_runtime(cx);
                    } else {
                        self.begin(None, w, cx);
                    }
                }
                Err(e) => draft.update(cx, |s, cx| {
                    s.error = Some(e);
                    cx.notify();
                }),
            }
        }
        cx.notify();
    }
    pub(super) fn child_question_notices(&self, cx: &mut Context<Self>) -> Option<Div> {
        let mut rows = div().flex().flex_col().gap(px(6.));
        let mut count = 0;
        let mut seen = BTreeSet::new();
        for view in self.state.subagents.values() {
            let scope = format!("child:{}", view.id);
            let pending = self
                .scope_questions(&scope)
                .iter()
                .filter(|q| q.response.is_none() && seen.insert(q.id.clone()))
                .count();
            if pending == 0 {
                continue;
            }
            count += pending;
            let id = view.id.clone();
            let text = format!("{} · {} 个问题待回答", view.description, pending);
            rows = rows.child(
                agent_ui::step_action(
                    SharedString::from(format!("question-notice-{id}")),
                    SharedString::from(text),
                    cx,
                )
                .on_click(cx.listener(move |s, _, w, cx| s.open_child(&id, w, cx))),
            );
        }
        (count > 0).then_some(rows)
    }
}

#[cfg(debug_assertions)]
impl Drawer {
    pub(super) fn questions_fixture(
        &mut self,
        scene: &str,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use shellmate_ai::{
            Conversation, Entry, LoggedCall, LoggedResult, Step, UserTurn,
            questions::{Mode, Request},
        };
        let request=Request::parse(serde_json::json!({"mode":if scene=="ai-question-async"{"async"}else{"wait"},"questions":[{"id":"environment","question":"使用哪个环境继续检查？","options":[{"id":"dev","label":"开发环境（推荐）","description":"先在开发环境核实配置。"},{"id":"prod","label":"生产环境","description":"仅按你指定的范围检查。"}]},{"id":"scope","question":"还需要检查哪些内容？","multi_select":true,"options":[{"id":"logs","label":"错误日志"},{"id":"config","label":"启动配置"}]}]})).unwrap();
        let call = LoggedCall {
            id: "question".into(),
            name: "ask_user".into(),
            args_json: serde_json::to_value(&request).unwrap(),
        };
        let mut question = Pending::new(
            "fixture-question".into(),
            call.clone(),
            request,
            Default::default(),
        );
        if scene == "ai-question-restored" {
            question.draft.insert(
                "environment".into(),
                Answer {
                    selected: vec!["dev".into()],
                    text: "先保持只读".into(),
                },
            );
        }
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("帮我检查项目启动问题。", Default::default()).sent_now());
        log.append(Entry::Step(Step {
            message_id: 1,
            call,
            result: LoggedResult {
                attachments: vec![],
                subagent: None,
                inline: question.receipt(),
                output_id: None,
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
        self.state.model.conversation = log.clone();
        self.state.model.busy = scene != "ai-question-restored";
        self.state.control = Default::default();
        self.state
            .control
            .register_question(question.clone())
            .unwrap();
        self.state.control.resume();
        self.state
            .control
            .set_phase(if question.request.mode == Mode::Wait {
                shellmate_ai::interaction::Phase::UserInput
            } else {
                shellmate_ai::interaction::Phase::Generating
            });
        if scene == "ai-question-child" {
            self.state.model.busy = false;
            self.state.control = Default::default();
            let mut child = crate::proto::subagents::view("question-child", "检查项目配置");
            child.questions = vec![question];
            child.transcript = Some(log.entries().to_vec());
            child.status = shellmate_ai::SubagentStatus::Running;
            self.state.subagents.insert(child.id.clone(), child);
            self.open_child("question-child", w, cx);
        }
        self.state.follow_bottom = false;
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }
}
