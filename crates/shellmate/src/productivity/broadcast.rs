//! Broadcast input bar and completion popup. All input goes through the workspace send path.
use super::multi::{self, Broadcast, SessionRow, SuggestAction, SuggestKey, SuggestionSelection};
use crate::{
    theme::{ActivePalette, MONO_FONT},
    ui::card_shadow,
};
use gpui::component::{
    Disableable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
};
use gpui::{
    App, Context, Entity, EventEmitter, Focusable, KeyBinding, Subscription, Window, actions, div,
    prelude::*, px,
};

#[derive(Clone, Default, PartialEq, Eq)]
pub struct BroadcastView {
    pub state: Broadcast,
    pub sessions: Vec<SessionRow>,
}
actions!(broadcast, [Up, Down, Accept, Submit]);
pub fn init(cx: &mut App) {
    for context in ["BroadcastInput", "BroadcastInput > Input"] {
        cx.bind_keys([
            KeyBinding::new("up", Up, Some(context)),
            KeyBinding::new("down", Down, Some(context)),
            KeyBinding::new("tab", Accept, Some(context)),
            KeyBinding::new("enter", Submit, Some(context)),
        ]);
    }
}
pub struct SubmitCommand(pub String);
pub struct BroadcastBar {
    input: Entity<InputState>,
    history: Vec<String>,
    snippets: Vec<String>,
    suggestions: Vec<String>,
    selection: SuggestionSelection,
    targets: usize,
    chip: String,
    _subscription: Subscription,
}
impl EventEmitter<SubmitCommand> for BroadcastBar {}
impl BroadcastBar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("输入命令，回车同时发送到所有选中会话"));
        let subscription = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.selection.selected = None;
                this.refresh(cx);
            }
        });
        Self {
            input,
            history: vec![],
            snippets: vec![],
            suggestions: vec![],
            selection: SuggestionSelection::default(),
            targets: 0,
            chip: "广播 → 0 个会话".into(),
            _subscription: subscription,
        }
    }
    pub fn sources(&mut self, history: Vec<String>, snippets: Vec<String>, cx: &mut Context<Self>) {
        self.history = history;
        self.snippets = snippets;
        self.selection.selected = None;
        self.refresh(cx);
    }
    pub fn targets(&mut self, count: usize, chip: String, cx: &mut Context<Self>) {
        if self.targets != count {
            self.targets = count;
            self.chip = chip;
            cx.notify();
        }
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.suggestions = multi::suggestions(
            &self.input.read(cx).value(),
            self.history.iter().map(String::as_str),
            self.snippets.iter().map(String::as_str),
        );
        cx.notify();
    }
    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_text("", window, cx);
    }
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |i, cx| i.set_value(text.to_string(), window, cx));
        self.selection.selected = None;
        self.refresh(cx);
    }
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
    }
    fn submit(&self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if self.targets > 0 && !text.trim().is_empty() {
            cx.emit(SubmitCommand(text));
        }
    }
    fn key(&mut self, key: SuggestKey, window: &mut Window, cx: &mut Context<Self>) {
        match self.selection.key(key, &self.suggestions) {
            SuggestAction::Accept(value) => {
                self.set_text(&value, window, cx);
                self.focus(window, cx);
            }
            SuggestAction::Send => self.submit(cx),
            SuggestAction::None => cx.notify(),
        }
    }
}
impl Render for BroadcastBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let mut popup = div()
            .id("broadcast-suggestions")
            .occlude()
            .absolute()
            .left(px(0.))
            .right(px(0.))
            .bottom(px(48.))
            .p(px(6.))
            .rounded(px(14.))
            .bg(p.card)
            .border_1()
            .border_color(p.line)
            .shadow(card_shadow(&p));
        for (i, text) in self.suggestions.iter().enumerate() {
            let value = text.clone();
            popup = popup.child(
                div()
                    .id(("bc-suggestion", i))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .font_family(MONO_FONT)
                    .text_size(px(12.))
                    .text_color(p.tx)
                    .when(self.selection.selected == Some(i), |d| {
                        d.bg(p.accs).text_color(p.acc)
                    })
                    .on_hover(cx.listener(move |this, on, _, cx| {
                        if *on {
                            this.selection.selected = Some(i);
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.set_text(&value, w, cx);
                        this.focus(w, cx);
                    }))
                    .child(text.clone()),
            );
        }
        popup = popup.child(
            div()
                .px(px(10.))
                .pt(px(4.))
                .text_size(px(11.))
                .text_color(p.tx2)
                .child("Tab 补全 · ↑↓ 选择 · 来自历史和快捷命令"),
        );
        div()
            .id("broadcast-input")
            .relative()
            .key_context("BroadcastInput")
            .mx(px(14.))
            .my(px(10.))
            .flex_none()
            .on_action(cx.listener(|this, _: &Up, w, cx| this.key(SuggestKey::Up, w, cx)))
            .on_action(cx.listener(|this, _: &Down, w, cx| this.key(SuggestKey::Down, w, cx)))
            .on_action(cx.listener(|this, _: &Accept, w, cx| this.key(SuggestKey::Tab, w, cx)))
            .on_action(cx.listener(|this, _: &Submit, w, cx| this.key(SuggestKey::Enter, w, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .p(px(6.))
                    .rounded(px(14.))
                    .bg(p.card)
                    .shadow(card_shadow(&p))
                    .child(
                        div()
                            .flex_none()
                            .px(px(9.))
                            .py(px(5.))
                            .rounded(px(8.))
                            .bg(p.warns)
                            .text_color(p.warn)
                            .text_size(px(11.5))
                            .child(self.chip.clone()),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.input)
                                .appearance(false)
                                .h(px(32.))
                                .text_size(px(12.5))
                                .p_0(),
                        ),
                    )
                    .child(
                        Button::new("broadcast-send")
                            .h(px(32.))
                            .px(px(14.))
                            .rounded(px(10.))
                            .text_size(px(12.5))
                            .primary()
                            .label("发送")
                            .disabled(self.targets == 0 || self.input.read(cx).value().trim().is_empty())
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
            .when(self.targets == 0, |d| {
                d.child(
                    div()
                        .pt(px(5.))
                        .text_size(px(11.5))
                        .text_color(p.warn)
                        .child("没有已连接的目标会话，请在广播列表中选择"),
                )
            })
            .when(!self.suggestions.is_empty(), |d| d.child(popup))
    }
}
