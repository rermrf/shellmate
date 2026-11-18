//! The question about names that already exist at the destination of an upload.

use std::rc::Rc;

use gpui::component::WindowExt as _;
use gpui::prelude::*;
use gpui::{App, Context, IntoElement, Render, Window, div, px};
use shellmate_sftp::transfer::{Choice, Clash, ItemId};

use super::{DialogBody, Tone, actions, button, palette, stack};
use crate::theme::MONO_FONT;

/// What happened to the question: the choice, or `None` when the dialog was closed with Esc (the
/// item then keeps waiting; its row in the queue has a 处理 button to ask again).
pub type Answer = Rc<dyn Fn(ItemId, Option<Choice>, &mut Window, &mut App)>;

/// "N 个文件已存在": skip, keep both or overwrite, for all of them.
pub struct ClashDialog {
    id: ItemId,
    clash: Clash,
    answer: Answer,
    answered: bool,
    local: bool,
}

/// The names to list: up to five, and ` 等` when there are more.
pub fn names_line(clash: &Clash) -> String {
    let names = clash.names.iter().take(5).cloned().collect::<Vec<_>>().join("、");
    if clash.count > clash.names.len().min(5) { format!("{names} 等") } else { names }
}

impl ClashDialog {
    /// Opens the dialog for item `id`.
    pub fn open(id: ItemId, clash: Clash, answer: Answer, window: &mut Window, cx: &mut App) {
        let title = clash.title();
        let body = cx.new(|_| Self { id, clash, answer, answered: false, local: false });
        super::open(window, cx, title, 480., body);
    }

    pub fn local(clash: Clash, answer: Answer, window: &mut Window, cx: &mut App) {
        let title = clash.title();
        let body = cx.new(|_| Self { id: ItemId(0), clash, answer, answered: false, local: true });
        super::open(window, cx, title, 480., body);
    }

    fn choose(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        self.answered = true;
        (self.answer)(self.id, Some(choice), window, cx);
        window.close_dialog(cx);
    }
}

impl DialogBody for ClashDialog {
    /// Enter does nothing: every answer changes what happens to the user's files, so none is the default.
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        false
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.answered {
            (self.answer)(self.id, None, window, cx);
        }
    }
}

impl Render for ClashDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        stack()
            .child(div().text_size(px(13.)).line_height(px(21.)).text_color(p.tx).child(names_line(&self.clash)))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_baseline()
                    .text_size(px(13.))
                    .line_height(px(21.))
                    .text_color(p.tx2)
                    .child("目标目录\u{a0}")
                    .child(div().font_family(MONO_FONT).text_color(p.tx).child(self.clash.dir.clone()))
                    .child("\u{a0}中已有同名文件，选择处理方式（应用到全部）。"),
            )
            .child(actions(vec![
                button("clash-skip", "跳过", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.choose(Choice::Skip, window, cx))),
                button("clash-both", "保留两者", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.choose(Choice::KeepBoth, window, cx))),
                button("clash-overwrite", if self.local { "替换" } else { "覆盖" }, Tone::Danger, true)
                    .on_click(cx.listener(|this, _, window, cx| this.choose(Choice::Overwrite, window, cx))),
            ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clash(count: usize, names: &[&str]) -> Clash {
        Clash { dir: "/srv/app".into(), count, names: names.iter().map(|n| n.to_string()).collect() }
    }

    #[test]
    fn the_names_are_listed_and_more_are_hinted_at() {
        assert_eq!(names_line(&clash(1, &["a.txt"])), "a.txt");
        assert_eq!(names_line(&clash(2, &["a.txt", "b.txt"])), "a.txt、b.txt");
        assert_eq!(
            names_line(&clash(5, &["1", "2", "3", "4", "5"])),
            "1、2、3、4、5",
            "five names, five files: nothing left out"
        );
        assert_eq!(names_line(&clash(8, &["1", "2", "3", "4", "5"])), "1、2、3、4、5 等", "three more");
        assert_eq!(clash(3, &[]).title(), "3 个文件已存在");
    }
}
