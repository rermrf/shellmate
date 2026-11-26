//! Pure presentation decisions and drawer-local expansion state. Never alters the log.
use super::subagent_view::View;
use shellmate_ai::SubagentStatus;
use std::collections::BTreeMap;

pub const EXPIRED: &str = "执行记录已过期，仅保留报告";
pub const EMPTY_REPORT: &str = "（没有报告）";

fn tokens(n: Option<u64>) -> String {
    n.map_or_else(
        || "未知".into(),
        |n| {
            if n >= 1000 {
                format!("{:.1}K", n as f64 / 1000.)
            } else {
                n.to_string()
            }
        },
    )
}
pub fn meta(view: &View) -> Option<String> {
    let mut parts = Vec::new();
    if view.model_overridden {
        parts.push(view.model.clone());
    }
    if view.reasoning_overridden {
        parts.push(format!("思考 {}", view.reasoning.label()));
    }
    if let SubagentStatus::Failed(error) = view.status {
        parts.push(if error == shellmate_ai::AiError::Disconnected {
            "连接已断开".into()
        } else {
            error.to_string()
        });
    }
    if view.usage.input_tokens.is_some() || view.usage.output_tokens.is_some() {
        parts.push(format!(
            "输入 {} · 输出 {} token",
            tokens(view.usage.input_tokens),
            tokens(view.usage.output_tokens)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}
pub fn preview(report: &str) -> String {
    let mut lines = report.lines().filter_map(|line| {
        let mut line = line.trim();
        loop {
            let stripped = line.trim_start_matches(['#', '-', '*', '>']).trim_start();
            if stripped == line {
                break;
            }
            line = stripped;
        }
        if let Some((number, rest)) = line.split_once('.')
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
            && rest.starts_with(char::is_whitespace)
        {
            line = rest.trim_start();
        }
        let clean = line.replace("**", "");
        (!clean.is_empty()).then_some(clean)
    });
    let mut first: Vec<_> = lines.by_ref().take(3).collect();
    if lines.next().is_some()
        && let Some(last) = first.last_mut()
    {
        last.push('…');
    }
    if first.is_empty() {
        EMPTY_REPORT.into()
    } else {
        // One paragraph lets the text renderer clamp the whole preview to three
        // visual lines; explicit newlines get a separate wrap budget in GPUI.
        first.join(" ")
    }
}
pub fn transcript_note(view: &View) -> Option<&'static str> {
    (view.history && view.transcript.is_none()).then_some(EXPIRED)
}
/// The report in View is the live full report or its loaded spool, never the preview.
pub fn copy_report(view: &View) -> String {
    view.report.clone()
}

/// Match live identities to persisted Steps, preserving the answer position.
pub fn projected(
    log: &shellmate_ai::Conversation,
    views: &mut BTreeMap<String, View>,
) -> BTreeMap<(usize, usize), String> {
    let mut ids = Vec::new();
    for entry in log.entries() {
        let shellmate_ai::Entry::Step(step) = entry else {
            continue;
        };
        let Some(display) = &step.result.subagent else {
            continue;
        };
        let id = display
            .transcript_id
            .clone()
            .or_else(|| display.output_id.clone())
            .or_else(|| {
                views
                    .values()
                    .find(|v| v.call_id == step.call.id)
                    .map(|v| v.id.clone())
            })
            .unwrap_or_else(|| step.call.id.clone());
        views.entry(id.clone()).or_insert_with(|| {
            let mut view = View::reopened(
                id.clone(),
                display,
                step.call.args_json["run_in_background"]
                    .as_bool()
                    .unwrap_or(false),
                step.result.inline.clone(),
            );
            view.call_id = step.call.id.clone();
            view.prompt = step.call.args_json["prompt"]
                .as_str()
                .unwrap_or_default()
                .into();
            view
        });
        ids.push(id);
    }
    let keys = log.turns().iter().enumerate().flat_map(|(turn, t)| {
        t.answer.iter().flat_map(move |a| {
            a.items.iter().enumerate().filter_map(move |(item, value)| {
                matches!(value, shellmate_ai::AnswerItem::Subagent { .. }).then_some((turn, item))
            })
        })
    });
    keys.zip(ids).collect()
}

#[cfg(test)]
#[path = "subagent_card_tests.rs"]
mod tests;
