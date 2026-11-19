use super::{Clock, Generation, ease_out, mix};
use gpui::{AnyElement, App, AvailableSpace, Div, ElementId, Window, div, prelude::*, px, size};
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn interpolate(self, end: Self, progress: f32) -> Self {
        Self::new(
            mix(self.x, end.x, progress),
            mix(self.y, end.y, progress),
            mix(self.width, end.width, progress),
            mix(self.height, end.height, progress),
        )
    }

    fn follow(self, initial: Self, current: Self) -> Self {
        Self::new(
            self.x + (current.x - initial.x),
            self.y + (current.y - initial.y),
            (self.width + (current.width - initial.width)).max(0.),
            (self.height + (current.height - initial.height)).max(0.),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Layout<const N: usize> {
    pub rects: [Rect; N],
    pub height: f32,
}

impl<const N: usize> Layout<N> {
    pub fn interpolate(self, end: Self, progress: f32) -> Self {
        Self {
            rects: std::array::from_fn(|index| {
                self.rects[index].interpolate(end.rects[index], progress)
            }),
            height: mix(self.height, end.height, progress),
        }
    }

    fn follow(self, initial: Self, current: Self) -> Self {
        Self {
            rects: std::array::from_fn(|index| {
                self.rects[index].follow(initial.rects[index], current.rects[index])
            }),
            height: (self.height + (current.height - initial.height)).max(0.),
        }
    }
}

#[derive(Default)]
struct Memory<const N: usize> {
    generation: Generation<bool>,
    displayed: Rc<Cell<Option<Layout<N>>>>,
    start: Option<Layout<N>>,
    initial_target: Rc<Cell<Option<Layout<N>>>>,
}

pub(crate) struct Blocks<const N: usize> {
    displayed: Rc<Cell<Option<Layout<N>>>>,
    flight: Option<(Layout<N>, f32)>,
    initial_target: Rc<Cell<Option<Layout<N>>>>,
    frozen: bool,
    remember: bool,
}

impl<const N: usize> Blocks<N> {
    pub fn region(
        id: impl Into<ElementId>,
        stacked: bool,
        clock: Clock,
        frozen: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let memory = window.use_keyed_state(id, cx, |_, _| Memory::<N>::default());
        let region = memory.update(cx, |memory, cx| {
            let (changed, progress) = memory.generation.visible_sample(stacked, clock, cx);
            if changed {
                memory.start = memory.displayed.get();
                memory.initial_target.set(None);
            }
            Self {
                displayed: memory.displayed.clone(),
                flight: memory.start.zip(progress),
                initial_target: memory.initial_target.clone(),
                frozen,
                remember: clock.animate,
            }
        });
        if window.is_visible() && region.flight.is_some() {
            window.request_animation_frame();
        }
        region
    }

    pub fn frame(&self, mut target: impl FnMut(bool) -> Layout<N>) -> Option<Layout<N>> {
        if self.frozen {
            return Some(target(true).interpolate(target(false), ease_out(0.5)));
        }
        self.flight.map(|(start, progress)| {
            let current = target(false);
            let initial = self.initial_target.get().unwrap_or(current);
            self.initial_target.set(Some(initial));
            start
                .interpolate(initial, progress)
                .follow(initial, current)
        })
    }

    pub fn native(self, group: Div) -> AnyElement {
        if !self.remember {
            return group.into_any_element();
        }
        group
            .relative()
            .child(div().absolute().inset_0())
            .on_children_prepainted(move |children, _, _| {
                let Some(parent) = children.get(N) else {
                    return;
                };
                let rects = std::array::from_fn(|index| {
                    let child = children[index];
                    Rect::new(
                        (child.origin.x - parent.origin.x).into(),
                        (child.origin.y - parent.origin.y).into(),
                        child.size.width.into(),
                        child.size.height.into(),
                    )
                });
                self.displayed.set(Some(Layout {
                    rects,
                    height: parent.size.height.into(),
                }));
            })
            .into_any_element()
    }

    pub fn absolute(self, group: Div, frame: Layout<N>, children: [AnyElement; N]) -> AnyElement {
        self.displayed.set(Some(frame));
        group
            .relative()
            .w_full()
            .flex_none()
            .h(px(frame.height))
            .children(frame.rects.into_iter().zip(children).map(|(rect, child)| {
                div()
                    .absolute()
                    .left(px(rect.x))
                    .top(px(rect.y))
                    .w(px(rect.width))
                    .h(px(rect.height))
                    .min_w_0()
                    .flex()
                    .child(child)
            }))
            .into_any_element()
    }
}

pub(crate) fn measure(
    mut element: AnyElement,
    width: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> (f32, f32) {
    let width = width.map_or(AvailableSpace::MaxContent, |width| {
        AvailableSpace::Definite(px(width))
    });
    let measured = element.layout_as_root(size(width, AvailableSpace::MaxContent), window, cx);
    (measured.width.into(), measured.height.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_all_rect_edges_and_container_height() {
        let start = Layout {
            rects: [Rect::new(100., 0., 40., 30.)],
            height: 30.,
        };
        let end = Layout {
            rects: [Rect::new(0., 40., 100., 60.)],
            height: 100.,
        };
        assert_eq!(
            start.interpolate(end, 0.5),
            Layout {
                rects: [Rect::new(50., 20., 70., 45.)],
                height: 65.
            }
        );
        assert_eq!(start.interpolate(end, 1.), end);
    }

    #[test]
    fn live_resize_applies_the_full_target_delta_during_flip() {
        let start = Layout {
            rects: [Rect::new(200., 0., 120., 40.)],
            height: 40.,
        };
        let initial = Layout {
            rects: [Rect::new(0., 80., 500., 80.)],
            height: 160.,
        };
        let resized = Layout {
            rects: [Rect::new(0., 80., 480., 80.)],
            height: 160.,
        };
        let mid = start.interpolate(initial, ease_out(0.5));
        let moved = mid.follow(initial, resized);
        assert_eq!(moved.rects[0].width, mid.rects[0].width - 20.);
        assert_eq!(moved.rects[0].y, mid.rects[0].y);
    }
}
