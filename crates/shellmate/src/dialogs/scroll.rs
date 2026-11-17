//! Dialog viewports with passive edge fades and space below the final card.
use crate::theme::ActivePalette;
use gpui::{
    AnyElement, App, Bounds, Div, ElementId, Hsla, IntoElement, RenderOnce, ScrollHandle,
    StyleRefinement, Window, canvas, div, fill, linear_color_stop, linear_gradient, point,
    prelude::*, px, size,
};

const FADE_HEIGHT: f32 = 16.;
const BOTTOM_PADDING: f32 = 24.;

#[derive(Debug, PartialEq, Eq)]
struct FadeVisibility {
    top: bool,
    bottom: bool,
}

/// `offset` is the positive distance scrolled down (the inverse of GPUI's offset).
fn fade_visibility(offset: f32, content_height: f32, viewport_height: f32) -> FadeVisibility {
    let max = (content_height - viewport_height).max(0.);
    let position = offset.clamp(0., max);
    FadeVisibility {
        top: viewport_height > 0. && position > 0.,
        bottom: viewport_height > 0. && position < max,
    }
}

/// A flexing viewport by default; use `.flex_none().max_h(...)` for bounded lists.
/// Styles apply to the outer frame, so margins stay outside the clipped viewport.
#[derive(IntoElement)]
pub struct ScrollBody {
    id: ElementId,
    frame: Div,
    content: AnyElement,
    handle: Option<ScrollHandle>,
    background: Option<Hsla>,
}

pub fn scroll_body(id: impl Into<ElementId>, content: impl IntoElement) -> ScrollBody {
    ScrollBody {
        id: id.into(),
        frame: div()
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col(),
        content: content.into_any_element(),
        handle: None,
        background: None,
    }
}

impl Styled for ScrollBody {
    fn style(&mut self) -> &mut StyleRefinement {
        self.frame.style()
    }
}

impl ScrollBody {
    /// Share an existing handle for focus scrolling or a deterministic fixture.
    pub fn track_scroll(mut self, handle: &ScrollHandle) -> Self {
        self.handle = Some(handle.clone());
        self
    }

    /// Embedded lists can sit on a card instead of the dialog's window surface.
    pub fn surface(mut self, background: Hsla) -> Self {
        self.background = Some(background);
        self
    }
}

impl RenderOnce for ScrollBody {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let handle = self.handle.unwrap_or_else(|| {
            window
                .use_keyed_state(self.id.clone(), cx, |_, _| ScrollHandle::new())
                .read(cx)
                .clone()
        });
        let background = self.background.unwrap_or(cx.palette().win);
        self.frame
            .id(self.id)
            .child(
                div()
                    .id("viewport")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_y_scroll()
                    .track_scroll(&handle)
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex_none()
                            .min_w_0()
                            .pb(px(BOTTOM_PADDING))
                            .child(self.content),
                    ),
            )
            .child(
                // Paint after the viewport has laid out and clamped its offset.
                // No hitbox or mouse listener: wheel/click events reach the cards.
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        let bounds = handle.bounds();
                        let height = f32::from(bounds.size.height);
                        let fades = fade_visibility(
                            -f32::from(handle.offset().y),
                            height + f32::from(handle.max_offset().y),
                            height,
                        );
                        let fade_height = px(FADE_HEIGHT.min(height / 2.));
                        for (visible, top) in [(fades.top, true), (fades.bottom, false)] {
                            if visible {
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(
                                            bounds.left(),
                                            if top {
                                                bounds.top()
                                            } else {
                                                bounds.bottom() - fade_height
                                            },
                                        ),
                                        size(bounds.size.width, fade_height),
                                    ),
                                    linear_gradient(
                                        if top { 180. } else { 0. },
                                        linear_color_stop(background, 0.),
                                        linear_color_stop(background.opacity(0.), 1.),
                                    ),
                                ));
                            }
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fades_follow_scroll_position_and_overflow() {
        for (offset, content, viewport, top, bottom) in [
            (0., 800., 500., false, true),
            (120., 800., 500., true, true),
            (300., 800., 500., true, false),
            (0., 300., 500., false, false),
            (0., 500., 500., false, false),
            (0., 0., 500., false, false),
            (120., 800., 0., false, false),
            // Clamp elastic overscroll, including stale offsets after a resize
            // or a page/list becoming shorter.
            (-20., 800., 500., false, true),
            (320., 800., 500., true, false),
            (120., 300., 500., false, false),
            (120., 800., 900., false, false),
            // Preserve real subpixel overflow at either edge.
            (0.25, 800., 500., true, true),
            (299.75, 800., 500., true, true),
        ] {
            assert_eq!(
                fade_visibility(offset, content, viewport),
                FadeVisibility { top, bottom },
                "offset={offset}, content={content}, viewport={viewport}",
            );
        }
    }
}
