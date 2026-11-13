//! A yes / no confirmation whose "yes" can still fail (the reason is shown inside the dialog).

use gpui::prelude::*;
use gpui::{App, Context, IntoElement, Render, SharedString, Window};

use super::{DialogBody, Notice, Tone, actions, button, hint, notice, palette, stack};

type Action = Box<dyn Fn(&mut Window, &mut App) -> Result<(), String>>;

/// Asks the user to confirm something; the confirm action reports an error to show in place.
pub struct ConfirmDialog {
    text: SharedString,
    confirm_label: SharedString,
    danger: bool,
    on_confirm: Action,
    error: Option<SharedString>,
}

impl ConfirmDialog {
    /// Opens the dialog. `on_confirm` runs on the primary button or Enter; `Ok` closes the
    /// dialog, `Err(message)` keeps it open with the message.
    pub fn open(
        title: impl Into<SharedString>,
        text: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
        danger: bool,
        on_confirm: impl Fn(&mut Window, &mut App) -> Result<(), String> + 'static,
        window: &mut Window,
        cx: &mut App,
    ) {
        let title: SharedString = title.into();
        let body = cx.new(|_| Self {
            text: text.into(),
            confirm_label: confirm_label.into(),
            danger,
            on_confirm: Box::new(on_confirm),
            error: None,
        });
        super::open(window, cx, title, 440., body);
    }
}

impl DialogBody for ConfirmDialog {
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match (self.on_confirm)(window, cx) {
            Ok(()) => true,
            Err(message) => {
                self.error = Some(message.into());
                cx.notify();
                false
            }
        }
    }

    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}

impl Render for ConfirmDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let tone = if self.danger { Tone::Danger } else { Tone::Primary };
        stack()
            .child(hint(self.text.clone(), &p))
            .children(self.error.clone().map(|message| notice(Notice::Error, message, &p)))
            .child(actions(vec![
                button("confirm-cancel", "取消", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                button("confirm-ok", self.confirm_label.clone(), tone, true)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            ]))
    }
}
