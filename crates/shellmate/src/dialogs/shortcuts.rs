//! 帮助 › 键盘快捷键: every shortcut of the app, grouped. The content comes from
//! `menus::shortcuts`, which reads the keys from the real keymap.
use gpui::{App, Context, FontWeight, Render, Window, div, prelude::*, px};

use super::{DialogBody, Tone, actions, button, palette, scroll_body, stack};
use crate::menus::shortcuts::{self, Section};
use crate::theme::Palette;

const WIDTH: f32 = 760.;
/// The dialog's own height beside the list: title, button row, padding.
const CHROME: f32 = 24. * 2. + 34. + 14. + 6. + 34. + 14.;

pub(crate) fn open(window: &mut Window, cx: &mut App) {
    let body = cx.new(|_| ShortcutSheet);
    let width = WIDTH.min(f32::from(window.viewport_size().width) - 40.);
    super::open(window, cx, "键盘快捷键", width, body);
}

struct ShortcutSheet;

impl DialogBody for ShortcutSheet {
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool { true }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}

/// How many sections go to the left column: the split that leaves the shorter columns, in reading order.
fn left_count(sections: &[Section]) -> usize {
    let height = |sections: &[Section]| sections.iter().map(|s| s.rows.len() as f32 + 1.5).sum::<f32>();
    (1..sections.len())
        .min_by(|&a, &b| {
            let tallest = |at: usize| height(&sections[..at]).max(height(&sections[at..]));
            tallest(a).total_cmp(&tallest(b))
        })
        .unwrap_or(sections.len())
}

/// The height of the scrolling list for a window `viewport` points high. The dialog sits at 17 %
/// of the window (at least 110) and keeps a margin below.
fn list_height(viewport: f32) -> f32 {
    let top = (viewport * 0.17).max(110.);
    (viewport - top - CHROME - 20.).clamp(160., 600.)
}

fn group(section: &Section, keymap: &gpui::Keymap, p: &Palette) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .px(px(4.))
                .text_size(px(12.5))
                .line_height(px(18.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.tx2)
                .child(section.title),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .px(px(14.))
                .rounded(px(14.))
                .bg(p.card2)
                .children(section.rows.iter().enumerate().map(|(ix, row)| {
                    div()
                        .min_h(px(34.))
                        .py(px(6.))
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .when(ix > 0, |d| d.border_t_1().border_color(p.line))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(13.))
                                .line_height(px(18.))
                                .text_color(p.tx)
                                .child(row.label),
                        )
                        .child(
                            div().flex_none().flex().items_center().gap(px(6.)).children(
                                shortcuts::chips(&row.keys, keymap).into_iter().map(|key| {
                                    div()
                                        .h(px(22.))
                                        .px(px(7.))
                                        .flex()
                                        .items_center()
                                        .rounded(px(7.))
                                        .bg(p.card)
                                        .border_1()
                                        .border_color(p.line)
                                        .text_size(px(12.))
                                        .line_height(px(16.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(p.tx2)
                                        .whitespace_nowrap()
                                        .child(key)
                                }),
                            ),
                        )
                })),
        )
}

impl Render for ShortcutSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let sections = shortcuts::sections();
        let (left, right) = sections.split_at(left_count(&sections));
        let column = |sections: &[Section]| {
            div().flex_1().min_w_0().flex().flex_col().gap(px(18.))
                .children(sections.iter().map(|section| group(section, &keymap, &p)))
        };
        let list = div().flex().items_start().gap(px(20.)).child(column(left)).child(column(right));
        stack()
            .child(
                scroll_body("shortcuts-list", list)
                    .flex_none()
                    .h(px(list_height(f32::from(window.viewport_size().height))))
                    .surface(p.card),
            )
            .child(actions(vec![
                button("shortcuts-close", "关闭", Tone::Primary, true)
                    .on_click(cx.listener(|body, _, window, cx| body.dismiss(window, cx))),
            ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menus::shortcuts::Section;

    fn section(rows: usize) -> Section {
        Section {
            title: "",
            rows: (0..rows).map(|_| shortcuts::Row { label: "", keys: shortcuts::Keys::Manual { shown: &[], keys: &[] } }).collect(),
        }
    }

    #[test]
    fn the_columns_split_in_reading_order_with_the_shorter_tall_side() {
        // The real sheet: 会话与标签, 窗口, 显示 on the left; 编辑与终端, 工具 on the right.
        assert_eq!(left_count(&shortcuts::sections()), 3);
        assert_eq!(left_count(&[section(5), section(5)]), 1);
        assert_eq!(left_count(&[section(30), section(2), section(2)]), 1);
        assert_eq!(left_count(&[section(2), section(2), section(30)]), 2);
        assert_eq!(left_count(&[section(3)]), 1, "a single group stays on the left");
    }

    #[test]
    fn the_list_never_outgrows_the_window() {
        // 620 is the smallest window height the app allows.
        for viewport in [620., 700., 900., 1200., 2000.] {
            let top = (viewport * 0.17_f32).max(110.);
            let height = list_height(viewport);
            assert!((160.0..=600.0).contains(&height), "{viewport}: {height}");
            assert!(top + CHROME + height <= viewport, "{viewport}: the dialog reaches below the window");
        }
    }
}
