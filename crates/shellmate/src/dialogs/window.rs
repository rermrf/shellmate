use super::{DialogBody, Tone, actions, button, hint, palette, stack};
use gpui::{App, Context, Render, Window, prelude::*};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    Move,
    Disconnect,
}

type OnChoice = Box<dyn FnOnce(Choice, &mut App)>;

pub(crate) struct WindowCloseDialog {
    message: String,
    on_choice: Option<OnChoice>,
}

impl WindowCloseDialog {
    pub(crate) fn open(
        sessions: usize,
        connecting: usize,
        impact: Option<String>,
        on_choice: impl FnOnce(Choice, &mut App) + 'static,
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
            on_choice: Some(Box::new(on_choice)),
        });
        super::open_tracked(window, cx, "关闭窗口", 460., body)
    }
}

impl DialogBody for WindowCloseDialog {
    fn confirm(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(answer) = self.on_choice.take() {
            answer(Choice::Move, cx);
        }
        true
    }

    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}

impl Render for WindowCloseDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        stack()
            .child(hint(self.message.clone(), &palette(cx)))
            .child(actions(vec![
                button("window-close-cancel", "取消", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                button("window-close-move", "移到其他窗口", Tone::Primary, true)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                button("window-close-disconnect", "断开并关闭", Tone::Danger, true).on_click(
                    cx.listener(|this, _, window, cx| {
                        if let Some(answer) = this.on_choice.take() {
                            answer(Choice::Disconnect, cx);
                        }
                        this.dismiss(window, cx);
                    }),
                ),
            ]))
    }
}
