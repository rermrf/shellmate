//! Modal dialogs and the small widgets they share.
//!
//! Every dialog is an entity implementing [`DialogBody`]; [`open`] hosts it in a gpui-component
//! `Dialog` (Enter confirms, Esc cancels, no click-away dismissal). Entities, not builder
//! closures, hold the dialog state so that typing and toggling re-render without rebuilding
//! the dialog layer.

pub mod auth;
pub mod clash;
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

    fn can_cancel(&self) -> bool { true }

    /// Esc or the cancel button. Runs before the dialog closes.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>);

    /// Puts keyboard focus on the first control (called once, when the dialog opens).
    fn focus(&self, _window: &mut Window, _cx: &mut Context<Self>) {}

    /// The primary button or Enter was used: confirms and closes if allowed.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm(window, cx) {
            window.close_dialog(cx);
            window.refresh();
        }
    }

    /// The cancel button was used: cancels and closes.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_cancel() { return; }
        self.cancel(window, cx);
        window.close_dialog(cx);
        window.refresh();
    }
}

/// Open a modal using the workspace's full-window backdrop for the first layer.
/// Nested dialogs retain their component backdrop so the underlying dialog is dimmed too.
/// All Shellmate modals are non-dismissable by clicking outside; the dialog host occludes input.
pub fn open_modal(
    window: &mut Window,
    cx: &mut App,
    build: impl Fn(gpui::component::dialog::Dialog, &mut Window, &mut App) -> gpui::component::dialog::Dialog + 'static,
) {
    window.refresh();
    let nested = window.has_active_dialog(cx);
    window.open_dialog(cx, move |dialog, window, cx| {
        build(dialog.overlay(nested), window, cx).on_close(|_, window, _| window.refresh())
    });
}

/// Height of a self-contained modal shell, leaving both measured edge margins.
pub fn shell_height(viewport: f32, preferred: f32, margin: f32) -> f32 {
    preferred.min((viewport - 2. * margin).max(0.))
}

/// A custom shell owns its header, scroll panes and footer. `Dialog::child`
/// adds a whole-body scroll wrapper with an intrinsic-height ancestor; putting
/// a percentage-height shell there lets its form push the footer out of bounds.
/// Use the non-scrolling content slot with a definite height instead.
pub fn fixed_shell<V: Render + 'static>(
    dialog: gpui::component::dialog::Dialog,
    body: Entity<V>,
    height: gpui::Pixels,
) -> gpui::component::dialog::Dialog {
    dialog.h(height).content(move |content, _, _| {
        content
            .h(height)
            .min_h_0()
            .flex_none()
            .overflow_hidden()
            .child(body.clone())
    })
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
    open_modal(window, cx, move |dialog, window, cx| {
        if first_build.replace(false) {
            body.update(cx, |body, cx| body.focus(window, cx));
        }
        let on_ok = body.clone();
        let on_cancel = body.clone();
        // A little above the middle: clear of the tab bar and header, with the scrim over the rest.
        let records = title.as_ref() == "会话记录";
        let settings = title.as_ref() == "设置";
        let host_editor = matches!(title.as_ref(), "新建连接" | "编辑主机");
        let host_key = title.contains("主机密钥已变更") || title.contains("主机指纹");
        let custom_height = px(shell_height(f32::from(window.viewport_size().height), if records { 740. } else { 640. }, 20.));
        let margin_top = if records || settings {
            (window.viewport_size().height - custom_height) / 2.
        } else if host_editor {
            px(30.)
        } else if host_key {
            (window.viewport_size().height - px(402.)) / 2.
        } else {
            (window.viewport_size().height * 0.17).max(px(110.))
        };
        dialog
            .margin_top(margin_top.max(px(20.)))
            .shadow(modal_shadow(None))
            .rounded(px(if records || settings || host_key { 26. } else { 22. }))
            .p(px(if records || settings { 0. } else { 24. }))
            .when(records || settings, |d| d.h(custom_height).bg(cx.palette().win).border_0().gap(px(0.)))
            .when(host_editor, |d| d.h((window.viewport_size().height - px(60.)).min(px(840.))))
            .when(!records && !settings, |d| {
                d.title(
                    div()
                        .text_size(px(if records || settings || host_editor || host_key { 22. } else { 18. }))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.clone()),
                )
            })
            .w(px(width))
            .close_button(host_editor)
            .overlay_closable(false)
            .on_ok(move |_, window, cx| on_ok.update(cx, |body, cx| body.confirm(window, cx)))
            .on_cancel(move |_, window, cx| {
                on_cancel.update(cx, |body, cx| { if !body.can_cancel() { return false; } body.cancel(window, cx); true })
            })
            .map(|d| if records || settings { fixed_shell(d, body.clone(), custom_height) } else { d.child(body.clone()) })
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
    let button = Button::new(id).label(label).disabled(!enabled).h(px(34.)).rounded(px(11.)).text_size(px(13.));
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
        .rounded(px(14.))
        .bg(bg)
        .text_color(fg)
        .text_size(px(12.5))
        .line_height(px(19.))
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

/// The prototype's modal shadow, optionally with the changed-key warning ring.
pub fn modal_shadow(ring: Option<(Hsla, f32)>) -> Vec<gpui::BoxShadow> {
    let mut shadows = vec![gpui::BoxShadow {
        color: gpui::rgba(0x0000004d).into(),
        offset: gpui::point(px(0.), px(30.)),
        blur_radius: px(80.),
        spread_radius: px(0.),
        inset: false,
    }];
    if let Some((color, width)) = ring {
        shadows.push(gpui::BoxShadow {
            color,
            offset: gpui::point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(width),
            inset: false,
        });
    }
    shadows
}

#[cfg(test)]
mod tests {
    use super::shell_height;

    #[test]
    fn custom_dialogs_share_a_bounded_shell_height() {
        for (preferred, margin, large, small) in [
            (740., 20., 740., 660.), // records
            (640., 20., 640., 640.), // settings
            (800., 50., 800., 600.), // file editor
        ] {
            assert_eq!(shell_height(900., preferred, margin), large);
            assert_eq!(shell_height(901., preferred, margin), large);
            assert_eq!(shell_height(700., preferred, margin), small);
        }
        assert_eq!(shell_height(20., 840., 30.), 0.);
    }
}
