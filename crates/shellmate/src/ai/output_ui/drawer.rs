use super::*;

impl Drawer {
    pub(in crate::ai::panel) fn prepare_output_views(&mut self, cx: &mut Context<Self>) {
        let mut sources = Vec::new();
        let mut initially_open = BTreeSet::new();
        let mut generic = Vec::new();
        for e in self.state.model.conversation.entries() {
            if let shellmate_ai::Entry::Step(step) = e
                && step.display.is_none()
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
            let key = super::super::tool_ui::key(&scope, &step.call.id);
            if self.state.tool_expanded.contains(&key) || step.result.is_error {
                initially_open.insert(key.clone());
            }
            sources.push(source(
                key,
                step.result.output_id.clone(),
                &step.result.inline,
                false,
            ));
        }
        for (turn, t) in self.state.model.conversation.turns().iter().enumerate() {
            if let Some(answer) = &t.answer {
                for (item, entry) in answer.items.iter().enumerate() {
                    if let shellmate_ai::AnswerItem::Step { output, .. } = entry {
                        let step = self.state.model.conversation.displayed_step(turn, item);
                        let id = step.and_then(|s| s.result.output_id.clone());
                        let key = id
                            .as_ref()
                            .map(|id| format!("output:{id}"))
                            .unwrap_or_else(|| format!("step:{turn}:{item}"));
                        if self.state.expanded.contains(&(turn, item)) {
                            initially_open.insert(key.clone());
                        }
                        sources.push(source(key, id, output, false));
                    }
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
                pane.update(cx, |s, cx| s.update_source(source, cx));
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
    pub(in crate::ai::panel) fn output_for_step(
        &self,
        key: (usize, usize),
    ) -> Option<Entity<OutputPane>> {
        let id = self
            .state
            .model
            .conversation
            .displayed_step(key.0, key.1)
            .and_then(|s| s.result.output_id.as_ref());
        self.state
            .output_panes
            .get(
                &id.map(|id| format!("output:{id}"))
                    .unwrap_or_else(|| format!("step:{}:{}", key.0, key.1)),
            )
            .cloned()
    }
    pub(in crate::ai::panel) fn reveal_output(
        &mut self,
        id: &str,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = format!("output:{id}");
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
