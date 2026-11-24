use super::{AiSettings, segment, settings_card, settings_row};
use crate::{
    theme::ActivePalette,
    ui::{Surface, field_input},
};
use gpui::{component::input::InputState, prelude::*, *};
use shellmate_ai::{ContextWindow, OutputTokens, WindowSource, resolve_window};
use shellmate_store::ai::Settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Choice {
    Auto,
    Fixed(u32),
    Custom,
}
pub(super) struct Fields {
    pub choices: [Choice; 4],
    pub inputs: [Entity<InputState>; 7],
    pub advanced: bool,
}
fn choices(s: &Settings) -> [Choice; 4] {
    [
        match s.context_window {
            ContextWindow::Auto => Choice::Auto,
            ContextWindow::Tokens(n) if [128000, 200000, 256000, 400000, 1000000].contains(&n) => {
                Choice::Fixed(n)
            }
            _ => Choice::Custom,
        },
        if s.autocompact_window == ContextWindow::Auto {
            Choice::Auto
        } else {
            Choice::Custom
        },
        match s.max_steps {
            None => Choice::Auto,
            Some(n) if [50, 100, 200].contains(&n) => Choice::Fixed(n as u32),
            _ => Choice::Custom,
        },
        if s.max_output_tokens == OutputTokens::Auto {
            Choice::Auto
        } else {
            Choice::Custom
        },
    ]
}
fn numbers(s: &Settings) -> [String; 7] {
    let window = resolve_window(&s.provider, &s.model, s.context_window).0;
    [
        (window / 1000).to_string(),
        (s.context_budget().auto / 1000).to_string(),
        s.max_steps.unwrap_or(50).to_string(),
        match s.max_output_tokens {
            OutputTokens::Auto => s.max_output_tokens.resolve(&s.provider, &s.model),
            OutputTokens::Tokens(n) => n,
        }
        .to_string(),
        s.exec.tool_output_chars.to_string(),
        s.exec.default_timeout_secs.to_string(),
        s.exec.max_timeout_secs.to_string(),
    ]
}
impl Fields {
    pub fn new(s: &Settings, w: &mut Window, cx: &mut Context<AiSettings>) -> Self {
        Self {
            choices: choices(s),
            inputs: numbers(s).map(|n| {
                cx.new(|cx| {
                    InputState::new(w, cx)
                        .default_value(n)
                        .validate(|value, _| value.bytes().all(|c| c.is_ascii_digit()))
                })
            }),
            advanced: false,
        }
    }
    pub fn load_model(&mut self, s: &Settings, w: &mut Window, cx: &mut Context<AiSettings>) {
        let choices = choices(s);
        let values = numbers(s);
        for i in [0, 1, 3] {
            self.choices[i] = choices[i];
            self.inputs[i].update(cx, |input, cx| input.set_value(values[i].clone(), w, cx));
        }
    }
    pub fn normalize_inputs(&mut self, s: &Settings, w: &mut Window, cx: &mut Context<AiSettings>) {
        let choices = choices(s);
        for (i, choice) in choices.into_iter().enumerate() {
            if self.choices[i] != Choice::Custom || choice == Choice::Auto {
                self.choices[i] = choice;
            }
        }
        for (input, value) in self.inputs.iter().zip(numbers(s)) {
            input.update(cx, |input, cx| input.set_value(value, w, cx));
        }
    }
    pub fn apply(&self, mut s: Settings, cx: &App) -> Settings {
        let n = |i: usize| self.inputs[i].read(cx).value().parse::<u32>().unwrap_or(0);
        let window = |i: usize| match self.choices[i] {
            Choice::Auto => ContextWindow::Auto,
            Choice::Fixed(n) => ContextWindow::Tokens(n),
            Choice::Custom => ContextWindow::Tokens(n(i).saturating_mul(1000)),
        };
        s.context_window = window(0);
        s.autocompact_window = window(1);
        s.max_steps = match self.choices[2] {
            Choice::Auto => None,
            Choice::Fixed(n) => Some(n as usize),
            Choice::Custom => Some(n(2) as usize),
        };
        s.max_output_tokens = if self.choices[3] == Choice::Auto {
            OutputTokens::Auto
        } else {
            OutputTokens::Tokens(n(3))
        };
        s.exec.tool_output_chars = n(4) as usize;
        s.exec.default_timeout_secs = n(5).into();
        s.exec.max_timeout_secs = n(6).into();
        s.normalized()
    }
}
pub(super) fn window_hint(s: &Settings) -> String {
    let (w, source) = resolve_window(&s.provider, &s.model, s.context_window);
    let w = crate::ai::model::tokens(w as usize);
    match source {
        WindowSource::Setting => format!("按 {w} 计算；超出模型实际上限会导致请求失败"),
        WindowSource::Catalog => format!("自动：已识别为 {w}"),
        WindowSource::Fallback => "自动：未识别该模型，按 128K 计算，可手动设置".into(),
    }
}
pub(super) fn compact_hint(s: &Settings) -> String {
    format!(
        "将在约 {} 时自动压缩",
        crate::ai::model::tokens(s.context_budget().trigger as usize)
    )
}
/// Steppers use the same persisted validation as typed values, including the
/// coupled default/max timeout constraint when the maximum is lowered.
fn stepped(settings: &Settings, index: usize, direction: i8) -> Settings {
    let mut s = settings.normalized();
    match index {
        4 => {
            s.exec.tool_output_chars = (s.exec.tool_output_chars as i64
                + i64::from(direction) * 5000)
                .clamp(1000, 150000) as usize
        }
        5 => {
            s.exec.default_timeout_secs =
                (s.exec.default_timeout_secs as i64 + i64::from(direction) * 30)
                    .clamp(1, s.exec.max_timeout_secs as i64) as u64
        }
        6 => {
            s.exec.max_timeout_secs =
                (s.exec.max_timeout_secs as i64 + i64::from(direction) * 30).clamp(1, 600) as u64
        }
        _ => unreachable!("numeric stepper index"),
    }
    s.normalized()
}
impl AiSettings {
    fn limit_row(
        &self,
        index: usize,
        label: &'static str,
        options: &[(Choice, &'static str)],
        unit: &'static str,
        hint: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let choices = options.iter().map(|(value, _)| *value).collect::<Vec<_>>();
        let items = options
            .iter()
            .map(|(value, label)| segment(*label, self.limits.choices[index] == *value, self.busy))
            .collect();
        let this = cx.weak_entity();
        let segmented = crate::ui::segmented(
            SharedString::from(format!("ai-limit-{index}")),
            items,
            move |i, _, cx| {
                this.update(cx, |s, cx| {
                    if !s.busy {
                        s.limits.choices[index] = choices[i];
                        cx.notify();
                    }
                })
                .ok();
            },
            cx,
        )
        .max_w_full()
        .flex_wrap();
        let controls = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .child(segmented)
            .when(self.limits.choices[index] == Choice::Custom, |d| {
                d.child(div().flex_none().w(px(100.)).child(field_input(
                    &self.limits.inputs[index],
                    Surface::Card,
                    true,
                )))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.palette().tx2)
                        .child(unit),
                )
            });
        settings_row(label, hint, controls, cx).into_any_element()
    }
    fn numeric_row(
        &self,
        index: usize,
        label: &'static str,
        hint: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = self.values(cx);
        let (value, min, max, unit) = match index {
            4 => (s.exec.tool_output_chars as u64, 1000, 150000, "字符"),
            5 => (
                s.exec.default_timeout_secs,
                1,
                s.exec.max_timeout_secs,
                "秒",
            ),
            6 => (s.exec.max_timeout_secs, 1, 600, "秒"),
            _ => unreachable!("numeric stepper index"),
        };
        let this = cx.weak_entity();
        let control = crate::ui::stepper(
            SharedString::from(format!("ai-number-{index}")),
            format!("{value} {unit}"),
            !self.busy && value > min,
            !self.busy && value < max,
            move |direction, w, cx| {
                this.update(cx, |s, cx| {
                    if s.busy {
                        return;
                    }
                    let values = numbers(&stepped(&s.values(cx), index, direction));
                    for (i, value) in values.into_iter().enumerate().skip(4) {
                        s.limits.inputs[i].update(cx, |input, cx| input.set_value(value, w, cx));
                    }
                    cx.notify();
                })
                .ok();
            },
            cx,
        );
        settings_row(label, hint, control, cx).into_any_element()
    }
    pub(super) fn limit_fields(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.values(cx);
        let p = cx.palette().clone();
        let context = settings_card(cx)
            .child(
                div()
                    .pt(px(12.))
                    .pb(px(4.))
                    .text_size(px(13.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("上下文与执行"),
            )
            .child(self.limit_row(
                0,
                "上下文窗口",
                &[
                    (Choice::Auto, "自动"),
                    (Choice::Fixed(128000), "128K"),
                    (Choice::Fixed(200000), "200K"),
                    (Choice::Fixed(256000), "256K"),
                    (Choice::Fixed(400000), "400K"),
                    (Choice::Fixed(1000000), "1M"),
                    (Choice::Custom, "自定义"),
                ],
                "K",
                if self.limits.choices[0] == Choice::Custom {
                    format!("{}；自定义 8–10000K", window_hint(&s))
                } else {
                    window_hint(&s)
                },
                cx,
            ))
            .child(self.limit_row(
                1,
                "自动压缩窗口",
                &[(Choice::Auto, "自动"), (Choice::Custom, "自定义")],
                "K",
                compact_hint(&s),
                cx,
            ))
            .child(self.limit_row(
                2,
                "单轮步数上限",
                &[
                    (Choice::Auto, "不限"),
                    (Choice::Fixed(50), "50"),
                    (Choice::Fixed(100), "100"),
                    (Choice::Fixed(200), "200"),
                    (Choice::Custom, "自定义"),
                ],
                "步",
                "达到上限后暂停，可继续任务；自定义 1–10000".into(),
                cx,
            ));
        let mut advanced =
            settings_card(cx).child(
                div()
                    .id("ai-advanced")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .py(px(12.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .line_height(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.tx2)
                    .child(
                        div().w(px(10.)).h(px(15.)).flex_none().child(
                            crate::icons::group_chevron(self.limits.advanced).text_color(p.tx2),
                        ),
                    )
                    .child("高级")
                    .on_click(cx.listener(|s, _, _, cx| {
                        s.limits.advanced = !s.limits.advanced;
                        cx.notify();
                    })),
            );
        if self.limits.advanced {
            let output = s.max_output_tokens.resolve(&s.provider, &s.model);
            advanced = advanced
                .child(self.limit_row(
                    3,
                    "单次输出 token",
                    &[(Choice::Auto, "自动"), (Choice::Custom, "自定义")],
                    "token",
                    if self.limits.choices[3] == Choice::Auto {
                        format!("自动：{output}")
                    } else {
                        format!("实际使用：{output}；自定义 1024–128000")
                    },
                    cx,
                ))
                .child(self.numeric_row(4, "工具输出长度", "1000–150000 字符".into(), cx))
                .child(self.numeric_row(
                    5,
                    "命令默认超时",
                    format!("1–{} 秒，不超过最长超时", s.exec.max_timeout_secs),
                    cx,
                ))
                .child(self.numeric_row(6, "命令最长超时", "1–600 秒".into(), cx));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(context)
            .child(advanced)
            .into_any_element()
    }
}
#[cfg(test)]
mod tests {
    use super::{Choice, ContextWindow, Settings, choices, compact_hint, stepped, window_hint};
    #[test]
    fn numeric_steppers_keep_bounds_and_default_below_maximum() {
        let mut s = Settings::default();
        assert_eq!(stepped(&s, 4, 1).exec.tool_output_chars, 35000);
        assert_eq!(stepped(&s, 5, 1).exec.default_timeout_secs, 150);
        assert_eq!(stepped(&s, 6, 1).exec.max_timeout_secs, 600);
        s.exec.tool_output_chars = 1000;
        assert_eq!(stepped(&s, 4, -1).exec.tool_output_chars, 1000);
        s.exec.tool_output_chars = 149000;
        assert_eq!(stepped(&s, 4, 1).exec.tool_output_chars, 150000);
        s.exec.max_timeout_secs = 31;
        s.exec.default_timeout_secs = 31;
        let s = stepped(&s, 6, -1);
        assert_eq!(s.exec.max_timeout_secs, 1);
        assert_eq!(s.exec.default_timeout_secs, 1);
        assert_eq!(stepped(&s, 5, -1).exec.default_timeout_secs, 1);
        assert_eq!(stepped(&s, 5, 1).exec.default_timeout_secs, 1);
    }
    #[test]
    fn hints_setting_catalog_fallback_and_trigger() {
        let s = Settings::default();
        assert_eq!(
            window_hint(&s),
            "自动：未识别该模型，按 128K 计算，可手动设置"
        );
        let s = Settings {
            model: "claude-sonnet-4-5".into(),
            ..s
        };
        assert_eq!(window_hint(&s), "自动：已识别为 200K");
        assert_eq!(compact_hint(&s), "将在约 167K 时自动压缩");
        let s = Settings {
            context_window: ContextWindow::Tokens(1000000),
            ..s
        };
        assert_eq!(
            window_hint(&s),
            "按 1M 计算；超出模型实际上限会导致请求失败"
        );
        assert_eq!(compact_hint(&s), "将在约 967K 时自动压缩");
        assert_eq!(choices(&Settings::default()), [Choice::Auto; 4]);
    }
}
