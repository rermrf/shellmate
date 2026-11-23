use super::{DialogBody, Notice, Tone, actions, button, hint, notice, palette, stack};
use crate::workspace::Workspace;
use crate::workspace::close_guard::CloseImpact;
use gpui::{App, Context, WeakEntity, Window, component::checkbox::Checkbox, prelude::*};
use shellmate_store::Preference;

/// Said when quitting, and when an update restarts the app, while the next launch restores the session.
pub(crate) const RESTORE_NOTE: &str = "重启后会恢复窗口和标签，正在运行的程序会结束";

pub(crate) struct QuitDialog {
    message: String,
    review: Option<CloseImpact>,
    /// The next launch opens the windows and tabs again.
    restores: bool,
    workspace: WeakEntity<Workspace>,
    remember: bool,
    error: Option<String>,
}

impl QuitDialog {
    pub(crate) fn open(
        message: String,
        review: Option<CloseImpact>,
        restores: bool,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> WeakEntity<Self> {
        let body = cx.new(|_| Self {
            message,
            review,
            restores,
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
        if let Some(reviewed) = &self.review {
            let Ok(current) = self.workspace.update(cx, |s, cx| s.quit_impact(cx)) else {
                self.error = Some("窗口已关闭，请重新发起退出。".into());
                cx.notify();
                return false;
            };
            if !current.covered_by(reviewed) {
                self.message = current.message();
                self.review = Some(current);
                self.error = Some("运行任务或连接已变化，请确认更新后的影响。".into());
                cx.notify();
                return false;
            }
        }
        if self.remember
            && let Ok(Err(error)) = self.workspace.update(cx, |s, cx| {
                s.set_preference(Preference::ConfirmQuit, false, window, cx)
            })
        {
            self.error = Some(error);
            cx.notify();
            return false;
        }
        crate::workspace::Hub::quit(cx);
        true
    }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}

impl Render for QuitDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        stack()
            .child(hint(self.message.clone(), &p))
            .children(self.restores.then(|| hint(RESTORE_NOTE, &p)))
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

#[cfg(test)]
mod tests {
    #[test]
    fn the_restore_note_says_what_the_next_launch_does() {
        assert_eq!(super::RESTORE_NOTE, "重启后会恢复窗口和标签，正在运行的程序会结束");
    }
}
