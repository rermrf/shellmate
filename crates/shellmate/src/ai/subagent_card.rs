//! Pure presentation decisions and drawer-local expansion state. Never alters the log.
use super::subagent_view::{StepView, View};
use shellmate_ai::{StepStatus, SubagentStatus};
use std::collections::BTreeMap;

pub const EXPIRED: &str = "执行记录已过期，仅保留报告";
pub const EMPTY_REPORT: &str = "（没有报告）";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Accent,
    Ok,
    Warning,
    Error,
    Quiet,
}
impl Color {
    pub fn resolve(self, p: &crate::theme::Palette) -> gpui::Hsla {
        match self {
            Self::Accent => p.acc,
            Self::Ok => p.ok,
            Self::Warning => p.warn,
            Self::Error => p.err,
            Self::Quiet => p.tx2,
        }
    }
}
pub fn duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60)
    }
}
pub fn status(status: &SubagentStatus, steps: usize, seconds: u64) -> (String, Color) {
    let (label, color, time) = match status {
        SubagentStatus::Running => ("运行中", Color::Accent, true),
        SubagentStatus::Completed => ("完成", Color::Ok, true),
        SubagentStatus::StepLimit => ("达到步数上限", Color::Warning, false),
        SubagentStatus::Failed(_) => ("失败", Color::Error, false),
        SubagentStatus::Stopped => ("已停止", Color::Quiet, false),
    };
    (
        format!(
            "{label} · {steps} 步{}",
            if time {
                format!(" · {}", duration(seconds))
            } else {
                String::new()
            }
        ),
        color,
    )
}
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
pub fn command_line(command: &str) -> String {
    command.replace(['\r', '\n'], " ")
}

pub fn step_status(step: &StepView) -> (String, Color) {
    if step.running {
        return ("运行中".into(), Color::Accent);
    }
    match &step.status {
        StepStatus::Completed => (
            format!(
                "{} · {:.1}s",
                step.exit_code
                    .map_or_else(|| "未知".into(), |n| n.to_string()),
                step.duration_ms as f64 / 1000.
            ),
            if shellmate_ai::output::successful(&step.command, step.exit_code) {
                Color::Ok
            } else {
                Color::Error
            },
        ),
        StepStatus::Background => ("已转后台".into(), Color::Accent),
        StepStatus::Denied { .. } => ("已拒绝".into(), Color::Error),
        StepStatus::TimedOut => ("超时".into(), Color::Error),
        StepStatus::Busy => ("终端正忙".into(), Color::Error),
        StepStatus::Disconnected => ("会话已断开".into(), Color::Error),
        StepStatus::Failed(_) => ("失败".into(), Color::Error),
    }
}
/// Kept outside View: the one-second registry refresh cannot reset UI choices.
#[derive(Default)]
pub struct Expansion {
    pub open: bool,
    pub step: Option<String>,
    pub outputs: BTreeMap<String, String>,
}
impl Expansion {
    pub fn toggle_step(&mut self, id: &str) {
        self.step = if self.step.as_deref() == Some(id) {
            None
        } else {
            Some(id.into())
        };
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
