use super::AiSettings;
use crate::{
    theme::ActivePalette,
    ui::{Surface, field_input},
};
use gpui::{
    component::{Disableable, button::Button, input::InputState},
    prelude::*,
    *,
};
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
        let p = cx.palette().clone();
        let controls = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .children(options.iter().enumerate().map(|(i, (choice, label))| {
                let choice = *choice;
                Button::new((SharedString::from(format!("ai-limit-{index}")), i))
                    .label(*label)
                    .disabled(self.busy)
                    .when(self.limits.choices[index] == choice, |b| {
                        b.bg(p.accs).text_color(p.acc)
                    })
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.limits.choices[index] = choice;
                        cx.notify();
                    }))
            }))
            .when(self.limits.choices[index] == Choice::Custom, |d| {
                d.child(div().w(px(100.)).child(field_input(
                    &self.limits.inputs[index],
                    Surface::Card,
                    true,
                )))
                .child(div().text_size(px(12.)).text_color(p.tx2).child(unit))
            });
        div()
            .flex()
            .flex_col()
            .gap(px(7.))
            .child(crate::dialogs::field(label, &p, controls))
            .child(
                div()
                    .text_size(px(11.5))
                    .line_height(px(18.))
                    .text_color(p.tx2)
                    .child(hint),
            )
            .into_any_element()
    }
    pub(super) fn limit_fields(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.values(cx);
        let p = cx.palette().clone();
        let mut form = div()
            .flex()
            .flex_col()
            .gap(px(14.))
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
                "K · 8–10000",
                window_hint(&s),
                cx,
            ))
            .child(self.limit_row(
                1,
                "自动压缩窗口",
                &[
                    (Choice::Auto, "自动（= 上下文窗口）"),
                    (Choice::Custom, "自定义"),
                ],
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
                "步 · 1–10000",
                "达到上限后暂停，可继续任务".into(),
                cx,
            ))
            .child(
                Button::new("ai-advanced")
                    .label(if self.limits.advanced {
                        "▾ 高级"
                    } else {
                        "▸ 高级"
                    })
                    .on_click(cx.listener(|s, _, _, cx| {
                        s.limits.advanced = !s.limits.advanced;
                        cx.notify();
                    })),
            );
        if self.limits.advanced {
            let output = s.max_output_tokens.resolve(&s.provider, &s.model);
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .pt(px(12.))
                    .border_t_1()
                    .border_color(p.line)
                    .child(self.limit_row(
                        3,
                        "单次输出 token",
                        &[(Choice::Auto, "自动"), (Choice::Custom, "自定义")],
                        "1024–128000",
                        format!(
                            "{}：{output}",
                            if self.limits.choices[3] == Choice::Auto {
                                "自动"
                            } else {
                                "实际使用"
                            }
                        ),
                        cx,
                    ))
                    .children(
                        [
                            (4, "工具输出长度", "字符 · 1000–150000"),
                            (5, "命令默认超时", "秒 · 不超过最长超时"),
                            (6, "命令最长超时", "秒 · 1–600"),
                        ]
                        .map(|(i, label, unit)| {
                            crate::dialogs::field(
                                label,
                                &p,
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(div().w(px(120.)).child(field_input(
                                        &self.limits.inputs[i],
                                        Surface::Card,
                                        true,
                                    )))
                                    .child(div().text_size(px(11.5)).text_color(p.tx2).child(unit)),
                            )
                        }),
                    ),
            );
        }
        form.into_any_element()
    }
}
#[cfg(test)]
mod tests {
    use super::{Choice, ContextWindow, Settings, choices, compact_hint, window_hint};
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
