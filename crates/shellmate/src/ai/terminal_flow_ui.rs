//! Transcript-only grouping. Every original tool and thinking block stays ordered inside.
use super::*;
use crate::ai::tool_model::{self, Status};
use shellmate_ai::{Step, ThinkingBlock};
use std::collections::BTreeMap;
#[derive(Clone, Copy)]
pub(super) enum Piece<'a> {
    Step(&'a Step),
    Thinking(&'a ThinkingBlock),
    Break,
}
fn session(piece: Piece<'_>) -> Option<&str> {
    let Piece::Step(step) = piece else {
        return None;
    };
    let (s, _) = tool_model::terminal_status(&step.call, Some(&step.result))?;
    if s.attention()
        || matches!(s, Status::Stopped | Status::Unknown)
        || step.result.cleared
        || step.call.args_json["release"] == true
    {
        return None;
    }
    step.call.args_json["session_id"].as_str()
}
/// Adjacent tools on one terminal; only completed thinking may sit between them.
pub(super) fn ranges(pieces: &[Piece<'_>]) -> BTreeMap<usize, usize> {
    let mut out = BTreeMap::new();
    let mut i = 0;
    while i < pieces.len() {
        let Some(target) = session(pieces[i]) else {
            i += 1;
            continue;
        };
        let start = i;
        let mut end = i + 1;
        let mut count = 1;
        i += 1;
        while i < pieces.len() {
            match pieces[i] {
                Piece::Thinking(b) if b.complete => {
                    i += 1;
                }
                p if session(p) == Some(target) => {
                    count += 1;
                    i += 1;
                    end = i;
                }
                _ => break,
            }
        }
        if count >= 2 {
            out.insert(start, end);
        }
        if i == start {
            i += 1;
        }
    }
    out
}
impl Drawer {
    pub(super) fn terminal_flow(
        &self,
        scope: &str,
        pieces: &[Piece<'_>],
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let steps = pieces
            .iter()
            .filter_map(|p| {
                if let Piece::Step(s) = p {
                    Some(*s)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let first = steps[0];
        let last = steps.last().unwrap();
        let key = format!("{}:flow", self.tool_key(scope, &first.call.id, false));
        let (status, detail) = tool_model::terminal_status(&last.call, Some(&last.result))
            .unwrap_or((Status::Read, String::new()));
        let summary = steps
            .iter()
            .rev()
            .find(|s| s.call.name == "terminal_input")
            .map(|s| tool_model::summary(&s.call))
            .unwrap_or_else(|| "观察终端画面".into());
        let card = tool_ui::Card {
            key: key.clone(),
            title: format!("终端操作 · {} 步", steps.len()),
            status,
            duration: None,
            summary,
            detail,
        };
        // Preserve links that previously expanded an individual tool.
        let child_open = steps.iter().any(|s| {
            self.state
                .tool_expanded
                .contains(&self.tool_key(scope, &s.call.id, false))
        });
        let open = self.tool_open(&key, status)
            || (child_open && !self.state.tool_collapsed.contains(&key));
        let mut surface = self.tool_shell_open(&card, open, cx);
        if open {
            let mut body = div().flex().flex_col().gap(px(8.));
            for piece in pieces {
                body = match piece {
                    Piece::Step(s) => body.child(self.tool_card(
                        scope,
                        &s.call,
                        Some(&s.result),
                        s.display.as_ref(),
                        w,
                        cx,
                    )),
                    Piece::Thinking(b) => body.child(self.thinking_block(scope, b, false, w, cx)),
                    Piece::Break => body,
                };
            }
            surface = surface.child(body);
        }
        surface.into_any_element()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn step(id: &str, session: &str, error: bool) -> Step {
        Step {
            message_id: 1,
            call: shellmate_ai::LoggedCall {
                id: id.into(),
                name: "read_terminal".into(),
                args_json: serde_json::json!({"session_id":session,"screen":true}),
            },
            result: shellmate_ai::LoggedResult {
                inline: "{}".into(),
                is_error: error,
                cleared: false,
                output_id: None,
                attachments: vec![],
                subagent: None,
            },
            display: None,
        }
    }
    #[::core::prelude::v1::test]
    fn grouping_preserves_boundaries_and_exposes_errors() {
        let a = step("a", "one", false);
        let b = step("b", "one", false);
        let c = step("c", "two", false);
        let fail = step("bad", "one", true);
        let pieces = [
            Piece::Step(&a),
            Piece::Step(&b),
            Piece::Step(&fail),
            Piece::Step(&a),
            Piece::Break,
            Piece::Step(&b),
            Piece::Step(&c),
        ];
        assert_eq!(ranges(&pieces), BTreeMap::from([(0, 2)]));
    }
}
