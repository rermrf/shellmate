//! Shared viewports and passive edge fades. All painting uses the actual scroll bounds.
use crate::theme::{ActivePalette, Palette};
use gpui::{
    AnyElement, App, Bounds, Div, ElementId, Hsla, IntoElement, Pixels, RenderOnce, ScrollHandle,
    Stateful, StyleRefinement, Window, canvas, div, fill, linear_color_stop, linear_gradient,
    point, prelude::*, px, size,
};

pub const FADE_HEIGHT: f32 = 16.;
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

fn fade_height(viewport_height: f32) -> f32 {
    FADE_HEIGHT.min(viewport_height.max(0.) / 2.)
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
    bottom_padding: Pixels,
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
        bottom_padding: px(BOTTOM_PADDING),
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

    pub fn bottom_padding(mut self, padding: Pixels) -> Self {
        self.bottom_padding = padding;
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
                            .pb(self.bottom_padding)
                            .child(self.content),
                    ),
            )
            .child(scroll_fades(handle, background))
    }
}

/// Paint after the viewport's prepaint has populated bounds, offset and max_offset.
/// This also works with `UniformListScrollHandle.0.borrow().base_handle.clone()`:
/// UniformList calls the same Interactivity::prepaint as Div before painting rows.
/// Canvas inserts no hitbox and registers no input handlers.
pub fn scroll_fades(handle: ScrollHandle, background: Hsla) -> impl IntoElement {
    scroll_fades_with_surface(handle, move || background)
}

/// Resolve transient surfaces during paint (e.g. the file card's drag-over tint).
/// This remains a bare canvas, with no hitbox or input/drag listener of its own.
pub fn scroll_fades_with_surface(
    handle: ScrollHandle,
    surface: impl Fn() -> Hsla + 'static,
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            let background = surface();
            let bounds = handle.bounds();
            let height = f32::from(bounds.size.height);
            let fades = fade_visibility(
                -f32::from(handle.offset().y),
                height + f32::from(handle.max_offset().y),
                height,
            );
            let fade_height = px(fade_height(height));
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
    .inset_0()
}

/// Keep the original viewport's layout, children, focus and event handlers. Only
/// overflowing pages get room for the final card's complete shadow footprint.
/// The baseline padding is retained when content fits, including an exact fit.
#[derive(IntoElement)]
pub struct PageScroll {
    id: ElementId,
    viewport: Stateful<Div>,
    handle: ScrollHandle,
    surface: Hsla,
    baseline: f32,
}

pub fn page_scroll(
    id: impl Into<ElementId>,
    viewport: Stateful<Div>,
    handle: &ScrollHandle,
    surface: Hsla,
    baseline: f32,
) -> PageScroll {
    PageScroll {
        id: id.into(),
        viewport,
        handle: handle.clone(),
        surface,
        baseline,
    }
}

fn page_shadow_padding(p: &Palette) -> f32 {
    // gpui-pre-apple 0.3.12 shaders.metal::shadow_vertex paints out to 3 sigma.
    // The far shadow dominates card_shadow's near shadow (1 + 3 * 2 = 7px).
    (p.shadow_y + 3. * p.shadow_blur).max(7.)
}

fn page_bottom_padding(max_offset: f32, applied: f32, baseline: f32, shadow_room: f32) -> f32 {
    // Remove only the padding introduced by us, not the original page padding.
    if max_offset > applied - baseline {
        baseline.max(shadow_room)
    } else {
        baseline
    }
}

impl RenderOnce for PageScroll {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let baseline = self.baseline;
        let shadow_room = page_shadow_padding(cx.palette());
        let padding = window.use_keyed_state(self.id.clone(), cx, |_, _| baseline);
        let applied = *padding.read(cx);
        let measure = self.handle.clone();
        div()
            .id(self.id)
            .relative()
            .w_full()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.viewport.track_scroll(&self.handle).pb(px(applied)))
            .child(
                canvas(
                    move |_, window, cx| {
                        let next = page_bottom_padding(
                            f32::from(measure.max_offset().y),
                            applied,
                            baseline,
                            shadow_room,
                        );
                        if next != applied {
                            let padding = padding.clone();
                            window.defer(cx, move |window, cx| {
                                padding.update(cx, |value, _| *value = next);
                                window.refresh();
                            });
                        }
                    },
                    |_, _, _, _| (),
                )
                .absolute()
                .inset_0(),
            )
            .child(scroll_fades(self.handle, self.surface))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_height_never_exceeds_half_the_viewport() {
        for (viewport, expected) in [(500., 16.), (32., 16.), (20., 10.), (0., 0.)] {
            assert_eq!(fade_height(viewport), expected);
        }
    }

    #[test]
    fn page_padding_covers_both_themes_shadow_footprints() {
        assert_eq!(page_shadow_padding(&Palette::light()), 46.);
        assert_eq!(page_shadow_padding(&Palette::dark()), 60.);
    }

    #[test]
    fn page_padding_does_not_create_overflow() {
        for room in [46., 60.] {
            for baseline in [0., 2., 8., 10.] {
                // Short and exactly fitting content both have zero max_offset.
                assert_eq!(page_bottom_padding(0., baseline, baseline, room), baseline);
                assert_eq!(page_bottom_padding(0.25, baseline, baseline, room), room);
                assert_eq!(
                    page_bottom_padding(room - baseline + 0.25, room, baseline, room),
                    room
                );
                // Resize/content removal must undo our padding, even at exact fit.
                assert_eq!(
                    page_bottom_padding(room - baseline, room, baseline, room),
                    baseline
                );
                assert_eq!(page_bottom_padding(0., room, baseline, room), baseline);
            }
        }
    }

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
