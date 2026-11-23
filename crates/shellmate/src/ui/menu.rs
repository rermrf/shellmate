//! The floating menu card shared by the file list's row menu and the tab bar's tab menu.
use crate::theme::Palette;
use gpui::{BoxShadow, Div, ElementId, Stateful, black, div, point, prelude::*, px};

pub const WIDTH: f32 = 196.;
pub const PADDING: f32 = 6.;
pub const ITEM_HEIGHT: f32 = 32.;
/// A separator line with 4px above and below.
pub const SEPARATOR_HEIGHT: f32 = 9.;

/// The height of a card with this many items and separators.
pub fn card_height(items: usize, separators: usize) -> f32 {
    2. * PADDING + items as f32 * ITEM_HEIGHT + separators as f32 * SEPARATOR_HEIGHT
}

/// A deep drop shadow and a one-pixel ring.
fn shadow(p: &Palette) -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: black().opacity(0.22),
            offset: point(px(0.), px(12.)),
            blur_radius: px(36.),
            spread_radius: px(0.),
            inset: false,
        },
        BoxShadow {
            color: p.line,
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(1.),
            inset: false,
        },
    ]
}

/// The card; the caller adds the items, the position and the dismissal.
pub fn card(id: impl Into<ElementId>, p: &Palette) -> Stateful<Div> {
    div()
        .id(id)
        .occlude()
        .flex()
        .flex_col()
        .w(px(WIDTH))
        .p(px(PADDING))
        .rounded(px(14.))
        .bg(p.card)
        .shadow(shadow(p))
}

/// A row of the card. The caller adds the label and, when `enabled`, the click handler.
pub fn item(id: impl Into<ElementId>, enabled: bool, danger: bool, p: &Palette) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(ITEM_HEIGHT))
        .px(px(10.))
        .rounded(px(9.))
        .text_size(px(13.))
        .text_color(if danger { p.err } else { p.tx })
        .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(p.card2)))
        .when(!enabled, |d| d.opacity(0.45).cursor_not_allowed())
}

pub fn separator(p: &Palette) -> Div {
    div().h(px(1.)).mx(px(4.)).my(px(4.)).bg(p.line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_height_counts_padding_items_and_separators() {
        assert_eq!(card_height(7, 0), 12. + 7. * 32.);
        assert_eq!(card_height(3, 1), 117.);
        assert_eq!(card_height(0, 0), 12.);
    }
}
