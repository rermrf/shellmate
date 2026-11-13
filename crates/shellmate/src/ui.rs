//! Small presentational helpers shared by the sidebar and workspace.

use gpui::component::Icon;
use gpui::component::tooltip::Tooltip;
use gpui::prelude::*;
use gpui::{
    App, BoxShadow, ElementId, FontWeight, Hsla, MouseButton, SharedString, Stateful,
    Window, div, point, px,
};

use crate::theme::{ActivePalette, Palette};

/// Raised card shadow: hairline ring + two soft drops (prototype `--tsh`).
pub fn card_shadow(p: &Palette) -> Vec<BoxShadow> {
    vec![
        BoxShadow { color: p.line, offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(1.), inset: false },
        BoxShadow { color: p.shadow.opacity(0.7), offset: point(px(0.), px(1.)), blur_radius: px(2.), spread_radius: px(0.), inset: false },
        BoxShadow { color: p.shadow, offset: point(px(0.), px(4.)), blur_radius: px(14.), spread_radius: px(0.), inset: false },
    ]
}

/// Subtle shadow for the selected item inside segmented controls and lists.
pub fn item_shadow(p: &Palette) -> Vec<BoxShadow> {
    vec![
        BoxShadow { color: p.shadow.opacity(0.9), offset: point(px(0.), px(1.)), blur_radius: px(2.), spread_radius: px(0.), inset: false },
        BoxShadow { color: p.shadow.opacity(0.6), offset: point(px(0.), px(2.)), blur_radius: px(8.), spread_radius: px(0.), inset: false },
    ]
}

pub fn dot(color: Hsla, size: f32) -> impl IntoElement {
    div().size(px(size)).flex_none().rounded_full().bg(color)
}

/// Keyboard hint like `⌘K`.
pub fn kbd(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    let p = cx.palette();
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(5.))
        .bg(p.card2)
        .text_size(px(10.5))
        .text_color(p.tx3)
        .child(text.into())
}

pub struct SegmentItem {
    pub label: SharedString,
    pub active: bool,
    pub enabled: bool,
    pub tooltip: Option<SharedString>,
    pub badge: Option<SharedString>,
}

impl SegmentItem {
    pub fn new(label: impl Into<SharedString>, active: bool) -> Self {
        Self { label: label.into(), active, enabled: true, tooltip: None, badge: None }
    }

    pub fn disabled(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.enabled = false;
        self.tooltip = Some(tooltip.into());
        self
    }
}

/// Segmented control. `on_pick` receives the index of the clicked item.
pub fn segmented(
    id: impl Into<ElementId>,
    items: Vec<SegmentItem>,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<gpui::Div> {
    let p = cx.palette().clone();
    let on_pick = std::rc::Rc::new(on_pick);
    div()
        .id(id.into())
        .flex()
        .items_center()
        .gap(px(2.))
        .p(px(3.))
        .rounded(px(11.))
        .bg(if cx.is_dark() { p.card2 } else { p.card3.opacity(0.7) })
        .children(items.into_iter().enumerate().map(|(i, item)| {
            let on_pick = on_pick.clone();
            let mut seg = div()
                .id(i)
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .h(px(28.))
                .px(px(14.))
                .rounded(px(8.))
                .text_size(px(13.))
                .font_weight(if item.active { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
                .text_color(if item.active { p.tx } else { p.tx2 })
                .when(item.active, |d| d.bg(p.card).shadow(item_shadow(&p)))
                .child(item.label.clone())
                .children(item.badge.clone().map(|b| {
                    div().px(px(5.)).rounded(px(5.)).bg(p.card3).text_size(px(10.5)).text_color(p.tx2).child(b)
                }));
            if item.enabled {
                seg = seg
                    .cursor_pointer()
                    .when(!item.active, |d| d.hover(|d| d.text_color(p.tx)))
                    .on_click(move |_, window, cx| on_pick(i, window, cx));
            } else {
                seg = seg.opacity(0.4).cursor_not_allowed();
            }
            if let Some(tip) = item.tooltip {
                seg = seg.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx));
            }
            seg
        }))
}

/// Text + icon button used in the top toolbar (广播 / AI / 快捷命令).
pub fn toolbar_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    label: Option<&'static str>,
    tooltip: impl Into<SharedString>,
    enabled: bool,
    cx: &App,
) -> Stateful<gpui::Div> {
    let p = cx.palette();
    let tooltip: SharedString = tooltip.into();
    div()
        .id(id.into())
        .flex()
        .items_center()
        .gap(px(5.))
        .h(px(30.))
        .px(px(9.))
        .rounded(px(8.))
        .text_size(px(13.))
        .text_color(p.tx2)
        .child(icon.into().size(px(14.)).text_color(p.tx2))
        .children(label)
        .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(p.card3.opacity(0.6)).text_color(p.tx)))
        .when(!enabled, |d| d.opacity(0.45))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}

/// Makes an element drag the window (macOS: double-click zooms), like a titlebar.
pub fn window_drag_area<E: InteractiveElement>(element: E) -> E {
    element.on_mouse_down(MouseButton::Left, |event, window, _| {
        if event.click_count == 2 {
            window.titlebar_double_click();
        } else {
            window.start_window_move();
        }
    })
}
