use super::{DialogBody, Notice, Tone, actions, button, hint, notice, palette, stack};
use crate::workspace::Workspace;
use gpui::{App, Context, WeakEntity, Window, component::checkbox::Checkbox, prelude::*};
use shellmate_store::Preference;

pub(crate) struct QuitDialog {
    message: String,
    workspace: WeakEntity<Workspace>,
    remember: bool,
    error: Option<String>,
}

impl QuitDialog {
    pub(crate) fn open(
        message: String,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> WeakEntity<Self> {
        let body = cx.new(|_| Self {
            message,
            workspace,
            remember: false,
            error: None,
        });
        let weak = body.downgrade();
        super::open(window, cx, "退出 Shellmate？", 460., body);
        weak
    }
}

impl DialogBody for QuitDialog {
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.remember
            && let Ok(Err(error)) = self.workspace.update(cx, |s, cx| {
                s.set_preference(Preference::ConfirmQuit, false, window, cx)
            })
        {
            self.error = Some(error);
            cx.notify();
            return false;
        }
        cx.quit();
        true
    }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}

impl Render for QuitDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        stack()
            .child(hint(self.message.clone(), &p))
            .child(
                Checkbox::new("quit-remember")
                    .label("不再询问")
                    .checked(self.remember)
                    .on_click(cx.listener(|s, value: &bool, _, cx| {
                        s.remember = *value;
                        cx.notify();
                    })),
            )
            .children(self.error.clone().map(|e| notice(Notice::Error, e, &p)))
            .child(actions(vec![
                button("quit-cancel", "取消", Tone::Plain, true)
                    .on_click(cx.listener(|s, _, w, cx| s.dismiss(w, cx))),
                button("quit-confirm", "退出", Tone::Danger, true)
                    .on_click(cx.listener(|s, _, w, cx| s.submit(w, cx))),
            ]))
    }
}
