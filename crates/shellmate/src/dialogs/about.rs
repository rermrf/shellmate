use gpui::{App, Context, FontWeight, Render, Window, div, img, prelude::*, px};
use crate::theme::ActivePalette;
use super::{DialogBody, Tone};

const VERSION: &str = concat!("版本 ", env!("CARGO_PKG_VERSION"));

pub(crate) fn open(window: &mut Window, cx: &mut App) {
    let body = cx.new(|_| AboutDialog);
    super::open(window, cx, "关于 Shellmate", 350., body);
}

struct AboutDialog;

impl DialogBody for AboutDialog {
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool { true }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}
}

impl Render for AboutDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = cx.palette();
        super::stack().items_center()
            .child(img(if cx.is_dark() { "dock-dark.png" } else { "dock-light.png" }).size(px(76.)))
            .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child("Shellmate"))
            .child(super::hint(VERSION, palette))
            .child(div().text_size(px(14.)).child("macOS 上的 SSH 客户端"))
            .child(super::button("about-close", "关闭", Tone::Primary, true)
                .w(px(88.))
                .on_click(cx.listener(|body, _, window, cx| body.dismiss(window, cx))))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn about_uses_workspace_package_version() {
        assert_eq!(super::VERSION.strip_prefix("版本 "), Some(env!("CARGO_PKG_VERSION")));
    }
}
