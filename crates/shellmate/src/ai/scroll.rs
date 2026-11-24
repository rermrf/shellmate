//! Shared horizontal viewport for Markdown and agent blocks.
use crate::theme::ActivePalette;
use gpui::{
    App, Axis, Bounds, Div, ElementId, HitboxBehavior, Hsla, IntoElement, Pixels, RenderOnce,
    ScrollEvent, ScrollHandle, Window, canvas,
    component::scroll::{ScrollableMask, Scrollbar, ScrollbarMode},
    div, fill, linear_color_stop, linear_gradient, point,
    prelude::*,
    px, size, transparent_black,
};

const SCROLLBAR_GUTTER: f32 = 8.;

#[derive(Debug, PartialEq)]
struct OverflowHint {
    scrollbar: bool,
    left: bool,
    right: bool,
}

fn overflow_hint(viewport: f32, content: f32, offset: f32) -> OverflowHint {
    let max = (content - viewport).max(0.);
    let position = (-offset).clamp(0., max);
    OverflowHint {
        scrollbar: max > 0.5,
        left: max > 0.5 && position > 0.5,
        right: max > 0.5 && position < max - 0.5,
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) handle: ScrollHandle,
    hovered: bool,
}

#[derive(IntoElement)]
pub(super) struct ScrollArea {
    id: ElementId,
    content: Div,
    background: Hsla,
    max_height: Option<Pixels>,
    vertical: bool,
    selection: Option<gpui::Entity<super::selection_scroll::SelectionScroll>>,
}

impl ScrollArea {
    pub fn new(id: impl Into<ElementId>, content: Div, background: Hsla) -> Self {
        Self {
            id: id.into(),
            content,
            background,
            max_height: None,
            vertical: false,
            selection: None,
        }
    }

    pub fn selection(
        mut self,
        selection: gpui::Entity<super::selection_scroll::SelectionScroll>,
    ) -> Self {
        self.selection = Some(selection);
        self
    }

    pub fn height_limit(mut self, height: f32, scroll: bool) -> Self {
        self.max_height = Some(px(height));
        self.vertical = scroll;
        self
    }
}

impl RenderOnce for ScrollArea {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id.clone(), cx, |_, _| State::default());
        if let Some(selection) = &self.selection {
            selection.update(cx, |s, _| s.register(&state));
        }
        let handle = state.read(cx).handle.clone();
        let hovered = state.read(cx).hovered;
        let shift_handle = handle.clone();
        let paint_handle = handle.clone();
        let bg = self.background;
        let thumb = cx.palette().tx2;
        div()
            .id(self.id)
            .relative()
            .w_full()
            .min_w_0()
            .flex_none()
            // The gutter is outside the clipped viewport. Padding inside a
            // height-capped viewport exposes the next line underneath the thumb.
            .pb(px(SCROLLBAR_GUTTER))
            .on_hover(move |hovered, _, cx| {
                state.update(cx, |s, cx| {
                    s.hovered = *hovered;
                    cx.notify();
                })
            })
            // Capture Shift+wheel before either the horizontal mask or the
            // message list. macOS may already have translated it to delta.x.
            .child(
                canvas(
                    move |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |_, hitbox, window, _| {
                        let handle = shift_handle.clone();
                        window.on_mouse_event(move |e: &ScrollEvent, phase, window, cx| {
                            if !phase.capture()
                                || !hitbox.id.should_handle_scroll(window)
                                || !e.modifiers.shift
                            {
                                return;
                            }
                            let delta = e.delta.pixel_delta(px(18.));
                            let max = handle.max_offset().x;
                            if max <= px(0.) {
                                return;
                            }
                            let mut offset = handle.offset();
                            offset.x = (offset.x
                                + if delta.x != px(0.) { delta.x } else { delta.y })
                            .clamp(-max, px(0.));
                            handle.set_offset(offset);
                            window.refresh();
                            cx.stop_propagation();
                        });
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(
                div()
                    .id("viewport")
                    .w_full()
                    .min_w_0()
                    .overflow_hidden()
                    .track_scroll(&handle)
                    .flex()
                    .flex_col()
                    .items_start()
                    .when_some(self.max_height, |d, h| d.max_h(h))
                    .child(self.content.flex_none()),
            )
            .child(ScrollableMask::new(Axis::Horizontal, &handle).id("horizontal-mask"))
            .when(self.vertical, |d| {
                d.child(ScrollableMask::new(Axis::Vertical, &handle).id("vertical-mask"))
            })
            // Read the current handle in paint, after layout, so both hints
            // track wheel/thumb motion and window resizing without stale state.
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        let bounds = paint_handle.bounds();
                        let max = f32::from(paint_handle.max_offset().x);
                        let width = f32::from(bounds.size.width);
                        let hint =
                            overflow_hint(width, width + max, f32::from(paint_handle.offset().x));
                        for (visible, left) in [(hint.left, true), (hint.right, false)] {
                            if visible {
                                let origin = point(
                                    if left {
                                        bounds.left()
                                    } else {
                                        bounds.right() - px(24.)
                                    },
                                    bounds.top(),
                                );
                                window.paint_quad(fill(
                                    Bounds::new(origin, size(px(24.), bounds.size.height)),
                                    linear_gradient(
                                        if left { 90. } else { 270. },
                                        linear_color_stop(bg, 0.),
                                        linear_color_stop(bg.opacity(0.), 1.),
                                    ),
                                ));
                            }
                        }
                        if hint.scrollbar && width > 0. {
                            let length = (width * width / (width + max)).max(24.).min(width);
                            let x = (-f32::from(paint_handle.offset().x) / max).clamp(0., 1.)
                                * (width - length);
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(bounds.left() + px(x + 2.), bounds.bottom() + px(2.)),
                                    size(px((length - 4.).max(0.)), px(4.)),
                                ),
                                thumb.opacity(0.22),
                            ));
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::horizontal(&handle)
                        .id("horizontal-scrollbar")
                        .viewport_from_layout()
                        .mode(if hovered {
                            ScrollbarMode::Always
                        } else {
                            ScrollbarMode::Hover
                        })
                        .styles(|s| {
                            s.track(|s| s.bg(transparent_black()).width(px(8.)))
                                .thumb(|s| {
                                    s.bg(thumb.opacity(0.6))
                                        .width(px(4.))
                                        .inset(px(2.))
                                        .min_length(px(24.))
                                        .radius(px(2.))
                                })
                                .thumb_hover(|s| {
                                    s.bg(thumb.opacity(0.85))
                                        .width(px(6.))
                                        .inset(px(2.))
                                        .min_length(px(24.))
                                        .radius(px(3.))
                                })
                                .thumb_active(|s| {
                                    s.bg(thumb)
                                        .width(px(6.))
                                        .inset(px(2.))
                                        .min_length(px(24.))
                                        .radius(px(3.))
                                })
                        }),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overflow_hints_follow_offset_and_content_width() {
        assert_eq!(
            overflow_hint(300., 600., 0.),
            OverflowHint {
                scrollbar: true,
                left: false,
                right: true
            }
        );
        assert_eq!(
            overflow_hint(300., 600., -120.),
            OverflowHint {
                scrollbar: true,
                left: true,
                right: true
            }
        );
        assert_eq!(
            overflow_hint(300., 600., -300.),
            OverflowHint {
                scrollbar: true,
                left: true,
                right: false
            }
        );
        assert_eq!(
            overflow_hint(600., 300., -300.),
            OverflowHint {
                scrollbar: false,
                left: false,
                right: false
            }
        );
        assert_eq!(
            overflow_hint(300., 300., 0.),
            OverflowHint {
                scrollbar: false,
                left: false,
                right: false
            }
        );
    }
}
