//! The 端口转发 panel of one tab: the card on top, the form for a new rule, the rule cards. What it
//! says comes from [`super::model`]; what the user does with it goes to the workspace as
//! [`ForwardPanelEvent`]s (which saves rules, starts and stops them).

use gpui::component::input::{Input, InputEvent, InputState};
use crate::ui::Toggle as Switch;
use gpui::prelude::*;
use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, SharedString, Subscription, Window, div,
    px,
};
use shellmate_forward::{ForwardKind, ForwardRule, RuleId};

use super::model::{
    EMPTY_KINDS, EMPTY_TITLE, ForwardView, KINDS, RetryAction, RuleCard, RuleForm, Tone, kind_name, segment_label,
};
use crate::theme::{ActivePalette, CARD_RADIUS, MONO_FONT, Palette};
use crate::ui::{SegmentItem, card_shadow, dot, segmented};

/// What the panel asks of the workspace.
pub enum ForwardPanelEvent {
    /// A rule the form made: save it and start it.
    Add(ForwardRule),
    /// The toggle of a rule: switch it on or off (and save which).
    Toggle(RuleId, bool),
    /// A retry button: 换用 M 重试 (listen on this port instead, save that and start the rule), or
    /// 重试 / 立即重试 (ask the server for the remote port again).
    Retry(RuleId, RetryAction),
    /// The × of a rule: ask, and delete it.
    Delete(RuleId),
}

impl EventEmitter<ForwardPanelEvent> for ForwardPanel {}

/// The new-rule form.
struct FormFields {
    kind: ForwardKind,
    name: Entity<InputState>,
    listen: Entity<InputState>,
    target: Entity<InputState>,
    /// 添加并启动 was pressed with something wrong: every problem is shown, not only those of fields that were typed in.
    submitted: bool,
    _subscriptions: Vec<Subscription>,
}

/// The panel.
pub struct ForwardPanel {
    view: ForwardView,
    form: Option<FormFields>,
    focus: FocusHandle,
}

impl Focusable for ForwardPanel {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ForwardPanel {
    pub fn new(view: ForwardView, cx: &mut Context<Self>) -> Self {
        Self { view, form: None, focus: cx.focus_handle() }
    }

    /// What the panel shows now.
    pub fn set_view(&mut self, view: ForwardView, cx: &mut Context<Self>) {
        if self.view != view {
            self.view = view;
            cx.notify();
        }
    }

    /// Opens or closes the new-rule form (the button on the top card).
    pub fn toggle_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.form.is_some() {
            self.form = None;
        } else {
            self.open_form(RuleForm::default(), window, cx);
        }
        cx.notify();
    }

    /// Opens the form with `values` typed in (the review hooks use this too).
    pub fn open_form(&mut self, values: RuleForm, window: &mut Window, cx: &mut Context<Self>) {
        let make = |text: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).default_value(text.to_string()))
        };
        let (name, listen, target) =
            (make(&values.name, window, cx), make(&values.listen, window, cx), make(&values.target, window, cx));
        let mut subscriptions = Vec::new();
        for input in [&name, &listen, &target] {
            subscriptions.push(cx.subscribe_in(input, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::PressEnter { .. } => this.submit(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                }
            }));
        }
        let kind = values.kind;
        let first = name.clone();
        self.form = Some(FormFields { kind, name, listen, target, submitted: false, _subscriptions: subscriptions });
        self.set_placeholders(window, cx);
        first.update(cx, |input, cx| window.focus(&input.focus_handle(cx), cx));
        cx.notify();
    }

    /// Debug review hook: 添加并启动 pressed with the form as it is.
    #[cfg(debug_assertions)]
    pub fn demo_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
    }

    /// What is typed in the form.
    fn values(&self, cx: &gpui::App) -> Option<RuleForm> {
        let form = self.form.as_ref()?;
        Some(RuleForm {
            kind: form.kind,
            name: form.name.read(cx).value().to_string(),
            listen: form.listen.read(cx).value().to_string(),
            target: form.target.read(cx).value().to_string(),
        })
    }

    fn set_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(values) = self.values(cx) else { return };
        let Some(form) = &self.form else { return };
        form.name.update(cx, |input, cx| input.set_placeholder("规则名称", window, cx));
        form.listen.update(cx, |input, cx| input.set_placeholder(values.listen_placeholder(), window, cx));
        form.target.update(cx, |input, cx| input.set_placeholder(values.target_placeholder(), window, cx));
    }

    fn pick_kind(&mut self, kind: ForwardKind, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.kind = kind;
        }
        self.set_placeholders(window, cx);
        cx.notify();
    }

    /// 添加并启动: hands the rule to the workspace, or shows what is wrong with the form.
    fn submit(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(values) = self.values(cx) else { return };
        let check = values.check(&self.view.rules, RuleId::generate());
        match check.rule {
            Some(rule) => {
                self.form = None;
                cx.emit(ForwardPanelEvent::Add(rule));
            }
            None => {
                if let Some(form) = &mut self.form {
                    form.submitted = true;
                }
            }
        }
        cx.notify();
    }

    // ---- drawing -------------------------------------------------------------------------------

    fn render_top(&self, p: &Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let summary = &self.view.summary;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.)).min_h(px(72.))
            .px(px(18.))
            .py(px(16.))
            .bg(p.card)
            .rounded(CARD_RADIUS)
            .shadow(card_shadow(p))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_size(px(15.)).line_height(px(21.)).font_weight(FontWeight::BOLD).child(summary.title.clone()))
                    .child(div().text_size(px(12.5)).line_height(px(17.)).text_color(p.tx2).child(summary.subtitle.clone()))
                    .children(
                        summary
                            .notes
                            .iter()
                            .map(|note| div().mt(px(4.)).text_size(px(12.)).text_color(p.warn).child(note.clone())),
                    ),
            )
            .child(
                div()
                    .id("fwd-new")
                    .h(px(36.))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .rounded(px(12.))
                    .bg(p.acc)
                    .text_color(gpui::white())
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .hover(|d| d.opacity(0.9))
                    .child("新建规则")
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_form(window, cx))),
            )
            .into_any_element()
    }

    fn field_box(&self, p: &Palette, mono: bool, input: &Entity<InputState>) -> gpui::Div {
        div()
            .flex_1()
            .min_w_0()
            .h(px(36.))
            .px(px(12.))
            .flex()
            .items_center()
            .rounded(px(11.))
            .bg(p.card2)
            .text_size(px(13.))
            .when(mono, |d| d.font_family(MONO_FONT))
            .child(div().w_full().child(Input::new(input).appearance(false).bordered(false)))
    }

    fn render_form(&self, form: &FormFields, p: &Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let values = self.values(cx).unwrap_or_default();
        let check = values.check(&self.view.rules, RuleId::from_raw("form"));
        let show =
            |text: &str, error: &Option<String>| error.clone().filter(|_| form.submitted || !text.trim().is_empty());
        let listen_error = show(&values.listen, &check.listen_error);
        let target_error =
            if values.kind == ForwardKind::Dynamic { None } else { show(&values.target, &check.target_error) };
        let segments =
            KINDS.iter().map(|kind| SegmentItem::new(segment_label(*kind), *kind == form.kind)).collect::<Vec<_>>();
        let on_pick = {
            let panel = cx.entity();
            move |ix: usize, window: &mut Window, cx: &mut gpui::App| {
                panel.update(cx, |this, cx| this.pick_kind(KINDS[ix], window, cx));
            }
        };
        let problem = |id: &'static str, text: String| div().id(id).text_size(px(12.)).text_color(p.err).child(text);
        let target_slot = if values.kind == ForwardKind::Dynamic {
            div()
                .flex_1()
                .min_w_0()
                .h(px(36.))
                .px(px(12.))
                .flex()
                .items_center()
                .rounded(px(11.))
                .bg(p.card2)
                .text_size(px(13.))
                .text_color(p.tx2)
                .child(RuleForm::DYNAMIC_TARGET)
        } else {
            self.field_box(p, true, &form.target)
        };

        div()
            .flex()
            .flex_none()
            .flex_col()
            .gap(px(12.))
            .px(px(18.))
            .py(px(16.))
            .bg(p.card)
            .rounded(CARD_RADIUS)
            .shadow(card_shadow(p))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(segmented("fwd-kind", segments, on_pick, cx).flex_none())
                    .child(self.field_box(p, false, &form.name)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(self.field_box(p, true, &form.listen))
                    .child(div().text_color(p.tx2).child("→"))
                    .child(target_slot)
                    .child(
                        div()
                            .id("fwd-add")
                            .h(px(36.))
                            .px(px(16.))
                            .flex()
                            .flex_none()
                            .items_center()
                            .rounded(px(11.))
                            .bg(p.acc)
                            .text_color(gpui::white())
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|d| d.opacity(0.9))
                            .child("添加并启动")
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
            .children(listen_error.map(|text| problem("fwd-listen-error", text)))
            .children(target_error.map(|text| problem("fwd-target-error", text)))
            .children(
                check
                    .warnings
                    .iter()
                    .map(|line| div().text_size(px(12.)).text_color(p.warn).child(format!("⚠ {line}"))),
            )
            .into_any_element()
    }

    fn tone_color(p: &Palette, tone: Tone) -> Hsla {
        match tone {
            Tone::Ok => p.ok,
            Tone::Waiting => p.warn,
            Tone::Off => p.tx3,
            Tone::Failed => p.err,
        }
    }

    fn render_card(&self, ix: usize, card: &RuleCard, p: &Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (bg, fg) = match card.kind {
            ForwardKind::Local => (p.accs, p.acc),
            ForwardKind::Remote => (p.oks, p.ok),
            ForwardKind::Dynamic => (p.warns, p.warn),
        };
        let status_color = if card.tone == Tone::Off { p.tx2 } else { Self::tone_color(p, card.tone) };
        let (id, enabled) = (card.id.clone(), card.enabled);
        let toggle_id = id.clone();
        let delete_id = id.clone();
        div()
            .id(("fwd-rule", ix)).min_h(px(76.))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(16.))
            .px(px(18.))
            .py(px(16.))
            .bg(p.card)
            .rounded(CARD_RADIUS)
            .shadow(card_shadow(p))
            .child(
                div()
                    .size(px(44.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(14.))
                    .bg(bg)
                    .text_color(fg)
                    .font_family(MONO_FONT)
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(14.)).line_height(px(19.))
                    .child(div().relative().top(px(1.5)).child(card.flag)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(8.))
                            .child(div().text_size(px(14.)).line_height(px(20.)).font_weight(FontWeight::BOLD).child(card.name.clone()))
                            .child(div().text_size(px(12.)).line_height(px(15.)).text_color(p.tx2).child(kind_name(card.kind))),
                    )
                    .child(
                        div()
                            .truncate()
                            .font_family(MONO_FONT)
                            .text_size(px(12.5)).line_height(px(17.)).relative().top(px(2.))
                            .text_color(p.tx2)
                            .child(card.route.clone()),
                    )
                    .children(
                        card.warnings
                            .iter()
                            .map(|line| div().text_size(px(11.5)).text_color(p.warn).child(format!("⚠ {line}"))),
                    ),
            )
            .child(
                div()
                    .w(px(260.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(4.)).relative().top(px(2.5))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_start()
                            .gap(px(6.))
                            .text_size(px(12.5)).line_height(px(17.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(status_color)
                            .child(div().mt(px(5.)).child(dot(Self::tone_color(p, card.tone), 7.)))
                            .child(div().flex_1().min_w_0().child(card.status.clone())),
                    )
                    .children(card.traffic.clone().map(|traffic| {
                        div().text_size(px(12.)).text_color(p.tx2).font_family(MONO_FONT).child(traffic)
                    }))
                    .children(card.retry.clone().map(|retry| {
                        let retry_id = id.clone();
                        let (bg, fg) = if card.tone == Tone::Failed { (p.errs, p.err) } else { (p.warns, p.warn) };
                        div()
                            .id(("fwd-retry", ix))
                            .h(px(24.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .rounded(px(8.))
                            .bg(bg)
                            .text_color(fg)
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|d| d.opacity(0.8))
                            .child(SharedString::from(retry.label))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(ForwardPanelEvent::Retry(retry_id.clone(), retry.action));
                            }))
                    })),
            )
            .child(Switch::new(("fwd-toggle", ix)).checked(enabled).color(p.ok).on_click(cx.listener(
                move |_, checked: &bool, _, cx| {
                    cx.emit(ForwardPanelEvent::Toggle(toggle_id.clone(), *checked));
                },
            )))
            .child(
                div()
                    .id(("fwd-delete", ix))
                    .size(px(28.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(9.))
                    .text_color(p.tx3)
                    .text_size(px(15.))
                    .cursor_pointer()
                    .hover(|d| d.bg(p.card2))
                    .child("×")
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(ForwardPanelEvent::Delete(delete_id.clone())))),
            )
            .into_any_element()
    }

    fn render_empty(&self, p: &Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .flex()
            .flex_none()
            .flex_col()
            .gap(px(14.))
            .px(px(24.))
            .py(px(22.))
            .bg(p.card)
            .rounded(CARD_RADIUS)
            .shadow(card_shadow(p))
            .child(div().text_size(px(15.)).font_weight(FontWeight::BOLD).child(EMPTY_TITLE))
            .children(EMPTY_KINDS.iter().map(|(name, what)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(*name))
                    .child(div().text_size(px(12.5)).line_height(px(19.)).text_color(p.tx2).child(*what))
            }))
            .child(
                div().flex().child(
                    div()
                        .id("fwd-empty-new")
                        .h(px(36.))
                        .px(px(16.))
                        .flex()
                        .items_center()
                        .rounded(px(12.))
                        .bg(p.acc)
                        .text_color(gpui::white())
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .hover(|d| d.opacity(0.9))
                        .child("新建规则")
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.form.is_none() {
                                this.toggle_form(window, cx);
                            }
                        })),
                ),
            )
            .into_any_element()
    }
}

impl Render for ForwardPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let mut cards = Vec::new();
        for (ix, card) in self.view.cards.iter().enumerate() {
            cards.push(self.render_card(ix, card, &p, cx));
        }
        let form = self.form.as_ref().map(|form| self.render_form(form, &p, cx));
        let empty = self.view.cards.is_empty() && self.form.is_none();
        div()
            .id("forward-panel")
            .track_focus(&self.focus)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .overflow_y_scroll()
            .child(self.render_top(&p, cx))
            .children(form)
            .children(cards)
            .children(empty.then(|| self.render_empty(&p, cx)))
    }
}
