use super::*;
#[derive(Default)]
pub(super) struct SideQuestion {
    pub question: Option<String>,
    answer: String,
    error: Option<String>,
    pending: bool,
    usage: Option<shellmate_ai::Usage>,
    generation: u64,
    abort: Option<tokio::task::AbortHandle>,
}
impl Drop for SideQuestion {
    fn drop(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}
impl Drawer {
    #[cfg(debug_assertions)]
    pub(super) fn fixture_btw(&mut self) {
        self.state.btw.question = Some("刚才为什么选择后台执行？".into());
        self.state.btw.answer="后台执行使用独立通道，适合不需要交互输入的检查任务。主任务仍在运行，这条侧问不会执行命令。".into();
        self.state.btw.usage = Some(shellmate_ai::Usage {
            input_tokens: Some(1200),
            output_tokens: Some(86),
        });
    }
    pub(super) fn close_btw(&mut self, cx: &mut Context<Self>) {
        let generation = self.state.btw.generation + 1;
        self.state.btw = Default::default();
        self.state.btw.generation = generation;
        cx.notify();
    }
    pub(super) fn start_btw(&mut self, question: &str, w: &mut Window, cx: &mut Context<Self>) {
        if question.trim().is_empty() {
            self.local(slash_ui::LocalBody::Hint("用法：/btw <问题>".into()), cx);
            return;
        }
        if self.state.model.conversation.turns().is_empty() {
            self.local(
                slash_ui::LocalBody::Hint("当前还没有可供侧问的对话内容".into()),
                cx,
            );
            return;
        }
        self.close_btw(cx);
        self.state.btw.question = Some(question.into());
        self.state.btw.pending = true;
        let generation = self.state.btw.generation;
        if let Some(config) = self.state.run_config.clone() {
            self.run_btw(config, generation, w, cx);
        } else {
            let this = cx.entity().downgrade();
            let id = self.state.id.clone();
            super::super::authorize(
                self.db.clone(),
                self.credentials.clone(),
                w,
                cx,
                move |result, w, cx| {
                    this.update(cx, |s, cx| {
                        if s.state.id != id || s.state.btw.generation != generation {
                            return;
                        }
                        match result {
                            Ok(config) => s.run_btw(config, generation, w, cx),
                            Err(e) => {
                                s.state.btw.pending = false;
                                s.state.btw.error = Some(e.to_string());
                                cx.notify();
                            }
                        }
                    })
                    .ok();
                },
            );
        }
        cx.notify();
    }
    fn run_btw(
        &mut self,
        config: shellmate_ai::Config,
        generation: u64,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = crate::runtime::get(cx) else {
            self.state.btw.pending = false;
            self.state.btw.error = Some("运行环境不可用".into());
            return;
        };
        let mut entries = self.state.model.conversation.entries().to_vec();
        if self.state.model.busy
            && self.state.control.phase() == shellmate_ai::interaction::Phase::Generating
        {
            while matches!(entries.last(), Some(shellmate_ai::Entry::Assistant { .. })) {
                entries.pop();
            }
        }
        let log = shellmate_ai::Conversation::from_entries(entries);
        let question = self.state.btw.question.clone().unwrap();
        let id = self.state.id.clone();
        let alive = self.binding.as_ref().map(|b| b.alive.clone());
        let image_root=self.data_root.clone();
        let job = runtime.spawn(async move {
            let client = RigClient::new(&config)?.with_image_root(image_root);
            tokio::select!{biased;
                ()=async{match alive{Some(alive)=>crate::ai::exec::disconnected(&alive).await,None=>std::future::pending().await}}=>Err(AiError::Disconnected),
                result=client.side_question(&log,&question)=>result,
            }
        });
        self.state.btw.abort = Some(job.abort_handle());
        cx.spawn_in(w, async move |this, cx| {
            let result = job.await;
            this.update_in(cx, |s, _, cx| {
                if s.state.id != id || s.state.btw.generation != generation {
                    return;
                }
                s.state.btw.pending = false;
                s.state.btw.abort = None;
                match result {
                    Ok(Ok((answer, usage))) => {
                        s.state.btw.answer = answer;
                        s.state.btw.usage = Some(usage);
                    }
                    Ok(Err(e)) => s.state.btw.error = Some(e.to_string()),
                    Err(_) => s.state.btw.error = Some("侧问已停止".into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    pub(super) fn btw_card(&self, w: &mut Window, cx: &mut Context<Self>) -> Option<Div> {
        let side = &self.state.btw;
        let question = side.question.as_ref()?;
        let p = cx.palette().clone();
        let mut card = agent_ui::step_surface(&p)
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
                    .child(agent_ui::status_badge("侧问", p.acc, p.accs, false))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .text_size(px(12.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(p.tx)
                            .child(question.clone()),
                    )
                    .child(
                        agent_ui::step_action("btw-close", "关闭", cx)
                            .flex_none()
                            .on_click(cx.listener(|s, _, _, cx| s.close_btw(cx))),
                    ),
            );
        if side.pending {
            card = card.child(
                div()
                    .text_size(px(12.))
                    .text_color(p.tx3)
                    .child("正在回答…"),
            );
        }
        if let Some(error) = &side.error {
            card = card.child(
                div()
                    .text_size(px(12.))
                    .text_color(p.err)
                    .child(error.clone()),
            );
        }
        if !side.answer.is_empty() {
            card = card.child(
                div()
                    .id("btw-answer")
                    .p(px(10.))
                    .rounded(px(10.))
                    .bg(p.card)
                    .border_1()
                    .border_color(p.line)
                    .max_h(px(if w.viewport_size().height < px(720.) {
                        90.
                    } else {
                        220.
                    }))
                    .overflow_y_scroll()
                    .child(markdown::drawer_view(
                        format!("{}:btw:{}", self.state.id, side.generation).into(),
                        side.answer.clone().into(),
                        self.selection_scroll.clone(),
                        Default::default(),
                        w,
                        cx,
                    )),
            );
            let copy = side.answer.clone();
            card = card.child(agent_ui::step_actions().child(
                agent_ui::step_action("btw-copy", "复制回答", cx).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                }),
            ));
            if let Some(usage) = side.usage {
                card = card.child(div().text_size(px(11.)).text_color(p.tx3).child(format!(
                    "侧问用量 · 输入 {} · 输出 {} token",
                    usage.input_tokens.unwrap_or(0),
                    usage.output_tokens.unwrap_or(0)
                )));
            }
        }
        Some(card)
    }
}
