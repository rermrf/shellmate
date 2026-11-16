//! Small presentational helpers shared by the sidebar and workspace.

use gpui::component::tooltip::Tooltip;
use gpui::prelude::*;
use gpui::{
    App, BoxShadow, ElementId, FontWeight, Hsla, MouseButton, SharedString, Stateful, Window, div, point, px,
};

use crate::theme::{ActivePalette, Palette};

/// The prototype's exact two-layer `--shadow` (light and dark have different offsets).
pub fn card_shadow(p: &Palette) -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: p.shadow_near,
            offset: point(px(0.), px(1.)),
            blur_radius: px(2.),
            spread_radius: px(0.),
            inset: false,
        },
        BoxShadow {
            color: p.shadow_far,
            offset: point(px(0.), px(p.shadow_y)),
            blur_radius: px(p.shadow_blur),
            spread_radius: px(0.),
            inset: false,
        },
    ]
}
pub fn item_shadow(p: &Palette) -> Vec<BoxShadow> {
    card_shadow(p)
}
/// Terminal/player cards use a 1px hairline in addition to the shared surface shadow.
pub fn terminal_shadow(p: &Palette) -> Vec<BoxShadow> {
    let mut shadow = card_shadow(p);
    shadow.insert(
        0,
        BoxShadow {
            color: p.line,
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(1.),
            inset: false,
        },
    );
    shadow
}

/// A border-only ring that does not paint under a translucent surface.
/// A zero-blur box shadow fills its interior in gpui, so it cannot serve as this outline.
/// Keep the stroke inside the surface bounds so a surrounding scroll viewport cannot clip it.
pub fn surface_ring(radius: f32, width: f32, color: Hsla) -> impl IntoElement {
    gpui::canvas(|_, _, _| (), move |bounds, _, window, _| {
        window.paint_quad(gpui::quad(bounds, px(radius), gpui::transparent_black(), px(width), color, gpui::BorderStyle::Solid));
    }).absolute().inset_0()
}

/// One-line variable server text; the tooltip preserves the full value at every width.
pub fn ellipsis(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<gpui::Div> {
    let text=text.into();
    let tip=text.clone();
    div().id(id).min_w_0().truncate().child(text)
        .tooltip(move |window,cx|Tooltip::new(tip.clone()).max_w((window.viewport_size().width-px(48.)).min(px(680.))).whitespace_normal().build(window,cx))
}

/// Observe the allocated viewport without allowing intrinsic child widths to set breakpoints.
/// Defer entity updates until after layout, and notify only when its pixel width changes.
pub fn observe_width<V: 'static>(entity: gpui::WeakEntity<V>, previous: f32, set: fn(&mut V, f32)) -> impl IntoElement {
    gpui::canvas(move |bounds, window, cx| {
        let width=f32::from(bounds.size.width).floor();
        if width>0. && (width-previous).abs()>=1. {
            let entity=entity.clone();
            window.defer(cx,move |_,cx| {entity.update(cx,|view,cx|{set(view,width);cx.notify();}).ok();});
        }
    },|_,_,_,_|()).absolute().inset_0()
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
        .rounded(px(6.))
        .bg(p.card2)
        .text_size(px(11.))
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
        Self {
            label: label.into(),
            active,
            enabled: true,
            tooltip: None,
            badge: None,
        }
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
    let id = id.into();
    let compact = matches!(&id, ElementId::Name(name) if matches!(name.as_ref(),"process-sort"|"service-filter"|"fwd-kind"));
    let panels = id == ElementId::Name("panels".into());
    let editor = matches!(&id, ElementId::Name(name) if matches!(name.as_ref(),"editor-protocol"|"editor-auth"));
    let host_filter = id == ElementId::Name("host-filter".into());
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(2.))
        .p(px(3.))
        .rounded(px(11.))
        .bg(p.card3)
        .children(items.into_iter().enumerate().map(|(i, item)| {
            let on_pick = on_pick.clone();
            let mut seg = div()
                .id(i)
                .flex()
                .flex_auto()
                .when(editor || host_filter, |d|d.flex_1().min_w_0())
                .items_center()
                .justify_center()
                .gap(px(6.))
                .h(px(if editor {32.} else if panels { 30. } else { 28. }))
                .px(px(if compact {12.}else{14.}))
                .rounded(px(if panels { 9. } else { 8. }))
                .text_size(px(if host_filter || compact { 12.5 } else { 13. }))
                .line_height(px(if host_filter || compact { 15. } else { 16. }))
                .font_weight(if item.active {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::MEDIUM
                })
                .text_color(if item.active { p.tx } else { p.tx2 })
                .when(item.active, |d| d.bg(p.card).shadow(item_shadow(&p)))
                .child(div().when(host_filter, |d|d.relative().top(px(-1.))).child(item.label.clone()))
                .children(item.badge.clone().map(|b| {
                    div()
                        .px(px(5.))
                        .rounded(px(5.))
                        .bg(p.card3)
                        .text_size(px(10.5))
                        .text_color(p.tx2)
                        .child(b)
                }));
            if item.enabled {
                seg = seg
                    .cursor_pointer()
                    .when(!item.active, |d| d.hover(|d| d.text_color(p.tx)))
                    .on_click(move |_, window, cx| on_pick(i, window, cx));
            } else {
                seg = seg.when(!editor, |d|d.opacity(0.4)).cursor_not_allowed();
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
    icon: crate::icons::AppIcon,
    label: Option<&'static str>,
    tooltip: impl Into<SharedString>,
    enabled: bool,
    cx: &App,
) -> Stateful<gpui::Div> {
    let p = cx.palette();
    let tooltip: SharedString = tooltip.into();
    div()
        .id(id.into())
        .flex().flex_none()
        .items_center()
        .gap(px(6.))
        .h(px(32.))
        .when(label.is_some(),|d|d.px(px(12.))).when(label.is_none(),|d|d.w(px(32.)).justify_center())
        .rounded(px(10.))
        .text_size(px(12.5))
        .line_height(px(15.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.tx2)
        .child(icon.icon())
        .children(label)
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|d| d.bg(p.card3.opacity(0.6)).text_color(p.tx))
        })
        .when(!enabled, |d| d.cursor_not_allowed())
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

/// Prototype switch geometry on gpui's accessible switch behavior (42×24, 20px thumb).
/// The component library's medium/large presets are 36×20 and 44×24 respectively.
#[derive(gpui::IntoElement)]
pub struct Toggle {
    id: ElementId,
    checked: bool,
    disabled: bool,
    color: Option<Hsla>,
    change: Option<std::rc::Rc<ToggleChange>>,
}
type ToggleChange = dyn Fn(&bool, &mut Window, &mut App);
impl Toggle {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            checked: false,
            disabled: false,
            color: None,
            change: None,
        }
    }
    pub fn checked(mut self, value: bool) -> Self {
        self.checked = value;
        self
    }
    pub fn disabled(mut self, value: bool) -> Self {
        self.disabled = value;
        self
    }
    pub fn color(mut self, value: Hsla) -> Self {
        self.color = Some(value);
        self
    }
    pub fn on_click(mut self, change: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.change = Some(std::rc::Rc::new(change));
        self
    }
}
impl gpui::RenderOnce for Toggle {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette().clone();
        let focus = window
            .use_keyed_state(self.id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let x = if self.checked { 18. } else { 0. };
        let x = if crate::prototype_fixture() {
            px(x)
        } else {
            gpui::base::spring(
                (self.id.clone(), "thumb"),
                px(x),
                gpui::base::Spring::new(std::time::Duration::from_millis(180)),
                window,
                cx,
            )
        };
        gpui::base::Switch::new(self.id)
            .checked(self.checked)
            .disabled(self.disabled)
            .track_focus(&focus)
            .tab_stop(true)
            .when_some(self.change, |s, change| {
                s.on_change(move |value, _, window, cx| change(&value, window, cx))
            })
            .child(
                gpui::base::SwitchTrack::new("track")
                    .relative()
                    .w(px(42.))
                    .h(px(24.))
                    .p(px(2.))
                    .rounded_full()
                    .bg(if self.checked {
                        self.color.unwrap_or(p.ok)
                    } else {
                        p.card3
                    })
                    .when(self.disabled, |s| s.opacity(0.5))
                    .child(
                        gpui::base::SwitchThumb::new(self.checked)
                            .size(px(20.))
                            .rounded_full()
                            .left(x)
                            .bg(gpui::white())
                            .shadow(vec![BoxShadow {
                                color: gpui::rgba(0x00000033).into(),
                                offset: point(px(0.), px(1.)),
                                blur_radius: px(3.),
                                spread_radius: px(0.),
                                inset: false,
                            }]),
                    ),
            )
    }
}
