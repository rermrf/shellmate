//! Shared tool presentation for main and child transcripts, including unknown tools.
use super::*;
use shellmate_ai::{LoggedCall, LoggedResult, StepDisplay};
pub(super) fn label(call: &LoggedCall) -> String {
    match call.name.as_str() {
        "run_command" => "执行命令".into(),
        "read_output" => "读取输出".into(),
        "wait_tasks" => "等待任务".into(),
        "stop_task" => "停止任务".into(),
        "memory" => format!(
            "记忆操作 · {}",
            call.args_json["command"]
                .as_str()
                .or_else(|| call.args_json["action"].as_str())
                .unwrap_or("查看")
        ),
        "send_agent_message" => "向子任务追加消息".into(),
        _ => format!("工具 · {}", call.name),
    }
}
pub(super) fn key(scope: &str, id: &str) -> String {
    format!("tool:{scope}:{id}")
}
impl Drawer {
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
        let key = key(scope, &call.id);
        let error = result.is_some_and(|r| r.is_error);
        let expanded = self.state.tool_expanded.contains(&key)
            || (error && !self.state.tool_collapsed.contains(&key));
        let status = if let Some(d) = display {
            crate::ai::subagent_card::step_status(&crate::ai::subagent_view::StepView {
                id: call.id.clone(),
                command: d.command.clone(),
                exit_code: d.exit_code,
                duration_ms: d.duration_ms,
                output_id: None,
                status: d.status.clone(),
                running: false,
                output: String::new(),
            })
            .0
        } else if result.is_none() {
            "运行中".into()
        } else if error {
            "未完成".into()
        } else {
            "已完成".into()
        };
        let output_key = key.clone();
        let toggle = key.clone();
        let mut card = agent_ui::step_surface(&p)
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .id(SharedString::from(format!("{key}:toggle")))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(80.))
                            .text_size(px(12.5))
                            .child(format!(
                                "{} {}",
                                if expanded { "▾" } else { "▸" },
                                label(call)
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(if error { p.err } else { p.tx3 })
                            .child(status),
                    )
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.state.follow_bottom = false;
                        if expanded {
                            s.state.tool_expanded.remove(&toggle);
                            s.state.tool_collapsed.insert(toggle.clone());
                        } else {
                            s.state.tool_collapsed.remove(&toggle);
                            s.state.tool_expanded.insert(toggle.clone());
                            if let Some(p) = s.state.output_panes.get(&output_key) {
                                p.update(cx, |s, cx| s.open(cx));
                            }
                        }
                        cx.notify();
                    })),
            );
        if call.name == "run_command" {
            let command = call.args_json["command"].as_str().unwrap_or_default();
            card = card.child(
                super::subagent_ui::selectable(format!("{key}:command"), command, w, cx)
                    .font_family(MONO_FONT)
                    .text_size(px(12.))
                    .when(!expanded, |d| {
                        d.whitespace_nowrap().text_ellipsis().overflow_hidden()
                    }),
            );
        }
        if expanded {
            let args = serde_json::to_string_pretty(&call.args_json).unwrap_or_default();
            card = card
                .child(div().text_size(px(11.)).text_color(p.tx3).child("调用参数"))
                .child(
                    super::subagent_ui::selectable(format!("{key}:args"), &args, w, cx)
                        .text_size(px(12.))
                        .font_family(MONO_FONT)
                        .whitespace_normal(),
                );
            if let Some(pane) = self.state.output_panes.get(&key) {
                card = card.child(pane.clone());
            } else if let Some(result) = result {
                card = card.child(
                    super::subagent_ui::selectable(format!("{key}:result"), &result.inline, w, cx)
                        .text_size(px(12.))
                        .whitespace_normal(),
                );
            }
        }
        card.into_any_element()
    }
}
