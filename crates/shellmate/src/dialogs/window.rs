use super::{DialogBody, Notice, Tone, actions, button, hint, notice, palette, stack};
use gpui::{App, Context, Render, Window, component::checkbox::Checkbox, prelude::*};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    Move,
    Disconnect,
}

impl Choice {
    pub(crate) fn remembered(self, remember: bool) -> Option<shellmate_store::CloseWindowWithSessions> {
        use shellmate_store::CloseWindowWithSessions;
        remember.then_some(match self { Self::Move => CloseWindowWithSessions::Move, Self::Disconnect => CloseWindowWithSessions::Disconnect })
    }
}

type OnChoice = Box<dyn FnMut(Choice, bool, &mut App) -> Result<(), String>>;

pub(crate) struct WindowCloseDialog {
    message: String,
    on_choice: OnChoice,
    remember: bool,
    error: Option<String>,
}

impl WindowCloseDialog {
    pub(crate) fn open(
        sessions: usize,
        connecting: usize,
        impact: Option<String>,
        on_choice: impl FnMut(Choice, bool, &mut App) -> Result<(), String> + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> super::modal::Handle {
        let mut message = if connecting == 0 {
            format!("这个窗口里有 {sessions} 个会话。")
        } else {
            format!("这个窗口里有 {sessions} 个会话，其中 {connecting} 个正在连接。")
        };
        if let Some(impact) = impact {
            message.push_str(&impact);
        }
        let body = cx.new(|_| Self {
            message,
            on_choice: Box::new(on_choice),
            remember: false,
            error: None,
        });
        super::open_tracked(window, cx, "关闭窗口", 460., body)
    }
    fn choose(&mut self, choice: Choice, cx: &mut Context<Self>) -> bool {
        match (self.on_choice)(choice, self.remember, cx) {
            Ok(()) => true,
            Err(error) => { self.error = Some(error); cx.notify(); false }
        }
    }
}

impl DialogBody for WindowCloseDialog {
    fn confirm(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        self.choose(Choice::Move, cx)
    }

    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}

impl Render for WindowCloseDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        stack()
            .child(hint(self.message.clone(), &p))
            .child(Checkbox::new("window-close-remember")
                .label("不再询问")
                .checked(self.remember)
                .on_click(cx.listener(|this, value: &bool, _, cx| { this.remember = *value; cx.notify(); })))
            .children(self.error.clone().map(|error| notice(Notice::Error, error, &p)))
            .child(actions(vec![
                button("window-close-cancel", "取消", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                button("window-close-move", "移到其他窗口", Tone::Primary, true)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                button("window-close-disconnect", "断开并关闭", Tone::Danger, true).on_click(
                    cx.listener(|this, _, window, cx| {
                        if this.choose(Choice::Disconnect, cx) { this.dismiss(window, cx); }
                    }),
                ),
            ]))
    }
}

#[cfg(test)]
#[test]
fn remember_requires_a_confirmed_choice() {
    use shellmate_store::CloseWindowWithSessions;
    assert_eq!(Choice::Move.remembered(true), Some(CloseWindowWithSessions::Move));
    assert_eq!(Choice::Disconnect.remembered(true), Some(CloseWindowWithSessions::Disconnect));
    assert_eq!(Choice::Move.remembered(false), None);
    assert_eq!(Choice::Disconnect.remembered(false), None);
    // Cancel/Esc only calls DialogBody::cancel; it never calls choose or the saving callback.
}
