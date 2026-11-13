//! Modal dialogs and the small widgets they share.
//!
//! Every dialog is an entity implementing [`DialogBody`]; [`open`] hosts it in a gpui-component
//! `Dialog` (Enter confirms, Esc cancels, no click-away dismissal). Entities, not builder
//! closures, hold the dialog state so that typing and toggling re-render without rebuilding
//! the dialog layer.

pub mod auth;
pub mod confirm;
pub mod host_editor;
pub mod hostkey;
pub mod import;

use std::cell::Cell;

use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::{Disableable as _, WindowExt as _};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FontWeight, Hsla, IntoElement, Render, SharedString, Window, div, px};

use crate::theme::{ActivePalette, Palette};

/// A dialog's content and behaviour.
pub trait DialogBody: Render + Sized + 'static {
    /// Enter or the primary button. Returns whether the dialog may close.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool;

    /// Esc or the cancel button. Runs before the dialog closes.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>);

    /// Puts keyboard focus on the first control (called once, when the dialog opens).
    fn focus(&self, _window: &mut Window, _cx: &mut Context<Self>) {}

    /// The primary button or Enter was used: confirms and closes if allowed.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm(window, cx) {
            window.close_dialog(cx);
        }
    }

    /// The cancel button was used: cancels and closes.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel(window, cx);
        window.close_dialog(cx);
    }
}

/// Opens `body` as a modal dialog `width` pixels wide.
pub fn open<V: DialogBody>(
    window: &mut Window,
    cx: &mut App,
    title: impl Into<SharedString>,
    width: f32,
    body: Entity<V>,
) {
    let title: SharedString = title.into();
    let first_build = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_build.replace(false) {
            body.update(cx, |body, cx| body.focus(window, cx));
        }
        let on_ok = body.clone();
        let on_cancel = body.clone();
        dialog
            .title(title.clone())
            .w(px(width))
            .close_button(false)
            .overlay_closable(false)
            .on_ok(move |_, window, cx| on_ok.update(cx, |body, cx| body.confirm(window, cx)))
            .on_cancel(move |_, window, cx| {
                on_cancel.update(cx, |body, cx| body.cancel(window, cx));
                true
            })
            .child(body.clone())
    });
}

/// Visual weight of a dialog button.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Primary,
    Danger,
    Plain,
}

/// A dialog button.
pub fn button(id: &'static str, label: impl Into<SharedString>, tone: Tone, enabled: bool) -> Button {
    let button = Button::new(id).label(label).disabled(!enabled);
    match tone {
        Tone::Primary => button.primary(),
        Tone::Danger => button.danger(),
        Tone::Plain => button,
    }
}

/// The row of buttons at the bottom of a dialog, right aligned.
pub fn actions(buttons: Vec<Button>) -> impl IntoElement {
    div().flex().items_center().justify_end().gap(px(8.)).pt(px(6.)).children(buttons)
}

/// A caption above a control.
pub fn field(label: impl Into<SharedString>, p: &Palette, control: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(div().text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(p.tx2).child(label.into()))
        .child(control)
}

/// Kind of a message box.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Info,
    Warn,
    Error,
}

/// A tinted message box.
pub fn notice(kind: Notice, text: impl Into<SharedString>, p: &Palette) -> impl IntoElement {
    let (bg, fg): (Hsla, Hsla) = match kind {
        Notice::Info => (p.accs, p.acc),
        Notice::Warn => (p.warns, p.warn),
        Notice::Error => (p.errs, p.err),
    };
    div()
        .px(px(12.))
        .py(px(9.))
        .rounded(px(10.))
        .bg(bg)
        .text_color(fg)
        .text_size(px(12.5))
        .line_height(px(19.))
        .child(text.into())
}

/// Monospaced value in a soft box (fingerprints, paths).
pub fn mono(text: impl Into<SharedString>, p: &Palette, color: Hsla) -> impl IntoElement {
    div()
        .px(px(12.))
        .py(px(9.))
        .rounded(px(10.))
        .bg(p.card2)
        .font_family(crate::theme::MONO_FONT)
        .text_size(px(12.))
        .line_height(px(18.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(text.into())
}

/// Small muted text.
pub fn hint(text: impl Into<SharedString>, p: &Palette) -> impl IntoElement {
    div().text_size(px(12.5)).line_height(px(19.)).text_color(p.tx2).child(text.into())
}

/// A dialog's vertical stack.
pub fn stack() -> gpui::Div {
    div().flex().flex_col().gap(px(14.)).w_full().pb(px(2.))
}

/// The palette for dialog bodies.
pub fn palette(cx: &App) -> Palette {
    cx.palette().clone()
}
