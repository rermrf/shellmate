//! A shared, quiet disclosure for provider-exposed reasoning in either transcript.
use super::*;
use gpui::component::button::ButtonVariants as _;
use shellmate_ai::ThinkingBlock;
#[derive(Default)]
struct Disclosure {
    expanded: bool,
}
impl Drawer {
    pub(super) fn thinking_block(
        &self,
        scope: &str,
        block: &ThinkingBlock,
        live: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = SharedString::from(format!("{}:{scope}:thinking:{}", self.state.id, block.id));
        let state = w.use_keyed_state(id.clone(), cx, |_, _| Disclosure::default());
        let expanded = state.read(cx).expanded;
        let p = cx.palette().clone();
        let label = if live {
            "正在思考…"
        } else {
            block.kind.label()
        };
        let mut body = div()
            .id(id.clone())
            .flex_none()
            .min_w_0()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(6.));
        let header = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(
                Button::new(SharedString::from(format!("{id}:toggle")))
                    .ghost()
                    .h(px(28.))
                    .px(px(6.))
                    .rounded(px(8.))
                    .text_size(px(12.))
                    .text_color(p.tx3)
                    .icon(crate::icons::group_chevron(expanded))
                    .label(label)
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.state.follow_bottom = false;
                        state.update(cx, |state, cx| {
                            state.expanded = !state.expanded;
                            cx.notify();
                        });
                        cx.notify();
                    })),
            );
        body = body.child(header);
        if expanded {
            body = body.child(
                div()
                    .flex_none()
                    .min_w_0()
                    .w_full()
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(10.))
                    .bg(p.card2)
                    .child(
                        super::subagent_ui::selectable(format!("{id}:body"), &block.text, w, cx)
                            .text_size(px(12.))
                            .line_height(px(19.))
                            .text_color(p.tx2)
                            .whitespace_normal(),
                    ),
            );
        }
        body
    }
}

#[cfg(debug_assertions)]
impl Drawer {
    pub(super) fn thinking_fixture(&mut self, scene: &str, w: &mut Window, cx: &mut Context<Self>) {
        use shellmate_ai::{Conversation, Entry, ThinkingBlock, ThinkingKind, UserTurn};
        let mut log = Conversation::default();
        log.push_user(
            UserTurn::new(
                "检查前端为什么无法启动，先不要修改文件。",
                Default::default(),
            )
            .sent_now(),
        );
        log.append(Entry::Thinking{message_id:1,block:ThinkingBlock{id:"1:0".into(),kind:ThinkingKind::Process,text:"先读取项目的启动脚本，确认需要的端口与环境变量。随后检查端口是否被占用，保留当前配置，不进行修改。\n\n如果发现已有服务，先核实它是否属于这个项目。".into(),complete:true}});
        log.append(Entry::Step(shellmate_ai::Step {
            message_id: 1,
            call: shellmate_ai::LoggedCall {
                id: "inspect".into(),
                name: "read_output".into(),
                args_json: serde_json::json!({"id":"fixture-output"}),
            },
            result: shellmate_ai::LoggedResult {
                attachments: vec![],
                inline: "端口 5173 已被另一个开发服务占用。".into(),
                subagent: None,
                output_id: None,
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
        log.append(Entry::Thinking {
            message_id: 3,
            block: ThinkingBlock {
                id: "3:0".into(),
                kind: ThinkingKind::Summary,
                text:
                    "检查结果表明端口冲突。可以先说明占用情况，再由用户决定停止旧服务或更换端口。"
                        .into(),
                complete: scene != "ai-thinking-live",
            },
        });
        if scene != "ai-thinking-live" {
            log.push_text(
                "启动失败是因为 **5173 端口被占用**。已确认原因，尚未修改文件或停止服务。",
            );
        }
        self.state.model.conversation = log.clone();
        self.state.model.busy = scene == "ai-thinking-live";
        self.state.model.thinking = self.state.model.busy;
        self.state
            .control
            .set_phase(shellmate_ai::interaction::Phase::Generating);
        self.state.follow_bottom = false;
        self.scroll.set_offset(point(px(0.), px(0.)));
        if scene == "ai-thinking-child" {
            let mut child = crate::proto::subagents::view("thinking-child", "检查前端启动问题");
            child.transcript = Some(log.entries().to_vec());
            child.report = log.turns()[0].answer.as_ref().unwrap().text();
            child.status = shellmate_ai::SubagentStatus::Completed;
            child.history = true;
            self.state.subagents.insert(child.id.clone(), child);
            self.open_child("thinking-child", w, cx);
            self.state.follow_bottom = false;
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use shellmate_ai::{AnswerItem, Entry, Event, ThinkingBlock, ThinkingKind, UserTurn};
    #[test]
    fn stopped_partial_thinking_remains_readable_and_copy_stays_answer_only() {
        let mut model = crate::ai::model::DrawerModel::default();
        model
            .conversation
            .push_user(UserTurn::new("检查", Default::default()));
        model.busy = true;
        model.event(Event::Reasoning {
            message_id: 1,
            block: ThinkingBlock {
                id: "1:0".into(),
                kind: ThinkingKind::Process,
                text: "已读取配置，接下来".into(),
                complete: false,
            },
        });
        assert!(model.thinking);
        model.event(Event::Error(shellmate_ai::AiError::Cancelled));
        assert!(!model.thinking && !model.busy);
        let answer = model.conversation.turns()[0].answer.as_ref().unwrap();
        assert!(answer.text().is_empty());
        assert!(
            matches!(&answer.items[0],AnswerItem::Thinking(b)if b.text=="已读取配置，接下来" && !b.complete)
        );
        let child = super::super::thread_ui::grouped(model.conversation.entries());
        assert!(
            matches!(&child[1],super::super::thread_ui::Item::Thinking(b)if b.text=="已读取配置，接下来")
        );
    }
    #[test]
    fn child_groups_fragments_without_splitting_an_interleaved_answer() {
        let b = |text: &str, complete| Entry::Thinking {
            message_id: 1,
            block: ThinkingBlock {
                id: "1:0".into(),
                kind: ThinkingKind::Summary,
                text: text.into(),
                complete,
            },
        };
        let entries = vec![
            b("思考", false),
            Entry::Assistant {
                message_id: 1,
                text: "回复".into(),
                replay: None,
            },
            b("结束", true),
            Entry::Assistant {
                message_id: 1,
                text: "继续".into(),
                replay: None,
            },
        ];
        let grouped = super::super::thread_ui::grouped(&entries);
        assert!(
            matches!(&grouped[..],[super::super::thread_ui::Item::Thinking(b),super::super::thread_ui::Item::Text{text,..}]if b.text=="思考结束" && b.complete && text=="回复继续")
        );
    }
}
