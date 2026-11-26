use super::*;

impl Drawer {
    pub(in crate::ai::panel) fn prepare_output_views(&mut self, cx: &mut Context<Self>) {
        let mut sources = Vec::new();
        let mut initially_open = BTreeSet::new();
        let mut generic = Vec::new();
        for (_, step) in self.state.active_steps.iter() {
            if step.started.is_none() {
                let scope = step
                    .source
                    .as_ref()
                    .map(|s| format!("child:{}", s.id))
                    .unwrap_or_else(|| "main".into());
                self.state
                    .tool_expanded
                    .insert(self.tool_key(&scope, &step.call.id, true));
            }
        }
        for e in self.state.model.conversation.entries() {
            if let shellmate_ai::Entry::Step(step) = e
                && step.result.subagent.is_none()
            {
                generic.push(("main".to_owned(), step));
            }
        }
        for view in self.state.subagents.values() {
            if let Some(entries) = &view.transcript {
                for e in entries {
                    if let shellmate_ai::Entry::Step(step) = e {
                        generic.push((format!("child:{}", view.id), step));
                    }
                }
            }
        }
        for (scope, step) in generic {
            if step.call.name == "web_search" {
                continue;
            }
            let key = self.tool_key(&scope, &step.call.id, false);
            if self.tool_open(
                &key,
                crate::ai::tool_model::command_status(step.display.as_ref(), Some(&step.result)),
            ) {
                initially_open.insert(key.clone());
            }
            let preview = if step.call.name == "web_fetch" {
                serde_json::from_str::<shellmate_ai::web::ResultData>(&step.result.inline)
                    .ok()
                    .and_then(|r| {
                        if let shellmate_ai::web::ResultData::Page { content, .. } = r {
                            Some(content)
                        } else {
                            None
                        }
                    })
            } else {
                None
            };
            let running = self
                .tool_task(&scope, &step.call.id, step.result.output_id.as_deref())
                .is_some_and(|t| t.status == crate::ai::tasks::Status::Running);
            sources.push(source(
                key,
                step.result.output_id.clone(),
                preview.as_deref().unwrap_or(&step.result.inline),
                running,
            ));
        }
        let mut active = vec![];
        for (_, step) in self.state.active_steps.iter() {
            let scope = step
                .source
                .as_ref()
                .map(|s| format!("child:{}", s.id))
                .unwrap_or_else(|| "main".into());
            active.push((scope, step.call.id.clone()));
        }
        for view in self.state.subagents.values() {
            for call in &view.active_tools {
                if call.name == "run_command" {
                    active.push((format!("child:{}", view.id), call.id.clone()));
                }
            }
        }
        for (scope, id) in active {
            if let Some(task) = self.tool_task(&scope, &id, None) {
                let key = self.tool_key(&scope, &id, true);
                if self.state.tool_expanded.contains(&key) {
                    initially_open.insert(key.clone());
                }
                if !sources.iter().any(|s| s.key == key) {
                    sources.push(source(
                        key,
                        Some(task.output_id),
                        "",
                        task.status == crate::ai::tasks::Status::Running,
                    ));
                }
            }
        }
        {
            let tasks = if self.fixture {
                self.fixture_tasks.clone()
            } else {
                self.tasks.as_ref().map(|t| t.list()).unwrap_or_default()
            };
            for task in tasks
                .into_iter()
                .filter(|t| t.kind == crate::ai::tasks::Kind::Command)
            {
                let key = format!("output:{}", task.output_id);
                if let Some(s) = sources.iter_mut().find(|s| s.key == key) {
                    s.running = task.status == crate::ai::tasks::Status::Running;
                } else {
                    sources.push(source(
                        key,
                        Some(task.output_id),
                        "",
                        task.status == crate::ai::tasks::Status::Running,
                    ));
                }
            }
        }
        for source in sources {
            let key = source.key.clone();
            if let Some(pane) = self.state.output_panes.get(&key) {
                pane.update(cx, |s, cx| {
                    s.update_source(source, cx);
                    if initially_open.contains(&key) && !s.expanded {
                        s.open(cx);
                    }
                });
            } else {
                let pane = cx.new(|cx| {
                    OutputPane::new(
                        source,
                        self.data_root.clone(),
                        self.state.id.clone(),
                        self.state.output_lifetime.0.clone(),
                        self.selection_scroll.clone(),
                        cx,
                    )
                });
                if initially_open.contains(&key) {
                    pane.update(cx, |s, cx| s.open(cx));
                }
                let subscription = cx.subscribe(&pane, |s, _, _: &Reading, cx| {
                    s.state.follow_bottom = false;
                    cx.notify();
                });
                self.state.output_subscriptions.push(subscription);
                self.state.output_panes.insert(key, pane);
            }
        }
    }
    pub(in crate::ai::panel) fn reveal_output(
        &mut self,
        id: &str,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self
            .state
            .output_panes
            .iter()
            .find(|(key, p)| {
                key.starts_with(&format!("tool:{}:main:", self.state.id))
                    && p.read(cx).source.output.as_deref() == Some(id)
            })
            .map(|(key, _)| key.clone())
            .unwrap_or_else(|| format!("output:{id}"));
        self.state.tool_expanded.insert(key.clone());
        self.state.tool_collapsed.remove(&key);
        self.state.follow_bottom = false;
        if let Some(pane) = self.state.output_panes.get(&key) {
            pane.update(cx, |s, cx| s.open(cx));
        }
        let drawer = cx.entity().downgrade();
        let conversation = self.state.id.clone();
        w.on_next_frame(move |w, _| {
            w.on_next_frame(move |_, cx| {
                drawer
                    .update(cx, |s, cx| {
                        if s.state.id != conversation {
                            return;
                        }
                        if let Some(pane) = s.state.output_panes.get(&key) {
                            let y = pane.read(cx).anchor.borrow().top();
                            let visible = s.scroll.bounds();
                            let current = s.scroll.offset();
                            let max = s.scroll.max_offset();
                            s.scroll.set_offset(point(
                                current.x,
                                (current.y + visible.top() - y).clamp(-max.y, px(0.)),
                            ));
                            cx.notify();
                        }
                    })
                    .ok();
            })
        });
        cx.notify();
    }
}
