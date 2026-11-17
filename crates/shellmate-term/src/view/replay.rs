//! A fixed-cell replay viewport. Its scroll position never resizes the emulator.
use super::*;
use gpui::component::scroll::{Scrollbar, ScrollbarMode};
use gpui::{
    HitboxBehavior, ScrollHandle, canvas, fill, linear_color_stop, linear_gradient, point, size,
    transparent_black,
};
use std::{cell::RefCell, rc::Rc};

const PADDING_X: f32 = 18.;
const PADDING_Y: f32 = 16.;
const GUTTER: f32 = 8.;

#[derive(Debug, PartialEq)]
struct OverflowHint {
    horizontal: bool,
    vertical: bool,
    right: bool,
}

impl OverflowHint {
    fn new(
        viewport: gpui::Size<Pixels>,
        content: gpui::Size<Pixels>,
        offset: gpui::Point<Pixels>,
    ) -> Self {
        let max_x = (content.width - viewport.width).max(px(0.));
        Self {
            horizontal: max_x > px(0.5),
            vertical: content.height - viewport.height > px(0.5),
            right: max_x > px(0.5) && -offset.x < max_x - px(0.5),
        }
    }
}

#[derive(Default)]
pub(super) struct ReplayViewport {
    pub handle: ScrollHandle,
    pub follow: Rc<RefCell<crate::replay::FollowCursor>>,
    hovered: bool,
}

impl TerminalView {
    pub(super) fn scroll_replay(&mut self, up: bool, edge: bool, cx: &mut Context<Self>) -> bool {
        if !self.terminal.read(cx).is_replay() {
            return false;
        }
        self.replay_viewport.follow.borrow_mut().manual_scroll();
        let handle = &self.replay_viewport.handle;
        let mut offset = handle.offset();
        let max = handle.max_offset().y;
        offset.y = if edge {
            if up { px(0.) } else { -max }
        } else {
            (offset.y + handle.bounds().size.height * if up { 1. } else { -1. }).clamp(-max, px(0.))
        };
        handle.set_offset(offset);
        cx.notify();
        true
    }

    pub fn restart_replay_follow(&mut self, cx: &mut Context<Self>) {
        self.replay_viewport.follow.borrow_mut().restart();
        cx.notify();
    }

    pub(super) fn render_replay(
        &self,
        element: TerminalElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let handle = self.replay_viewport.handle.clone();
        let wheel_handle = handle.clone();
        let paint_handle = handle.clone();
        let follow = self.replay_viewport.follow.clone();
        let wheel_follow = follow.clone();
        let terminal = self.terminal.clone();
        let thumb = to_hsla(self.style.palette.dim);
        let background = to_hsla(self.style.palette.background);
        let hovered = self.replay_viewport.hovered;
        let show_times = self.replay_show_times;
        let time_font = gpui::font(self.replay_time_font.clone());
        let time_color = self.replay_time_color;
        let time_handle = handle.clone();
        let time_terminal = terminal.clone();

        // Reserve the left gutter outside horizontal scrolling.
        div()
            .size_full()
            .relative()
            .pl(px(PADDING_X
                + if show_times {
                    crate::replay::TIME_GUTTER
                } else {
                    0.
                }))
            .pr(px(PADDING_X - GUTTER))
            .pt(px(PADDING_Y))
            .pb(px(PADDING_Y - GUTTER))
            .child(
                div()
                    .id("replay-scroll-area")
                    .size_full()
                    .relative()
                    // Gutters sit outside the clipped text viewport, inside the player.
                    .pr(px(GUTTER))
                    .pb(px(GUTTER))
                    .on_hover(cx.listener(|s, hovered, _, cx| {
                        s.replay_viewport.hovered = *hovered;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .id("replay-viewport")
                            .size_full()
                            .overflow_hidden()
                            .track_scroll(&handle)
                            .child(element),
                    )
                    .child(
                        canvas(
                            |bounds, window, _| {
                                window.insert_hitbox(bounds, HitboxBehavior::Normal)
                            },
                            move |_, hitbox, window, cx| {
                                // Scroll events are captured before the live terminal's
                                // wheel handler. Shift supports both macOS-translated x
                                // deltas and mice that still report y.
                                let handle = wheel_handle.clone();
                                let scroll_follow = wheel_follow.clone();
                                window.on_mouse_event(move |e: &ScrollEvent, phase, window, cx| {
                                    if !phase.capture() || !hitbox.id.should_handle_scroll(window) {
                                        return;
                                    }
                                    scroll_follow.borrow_mut().manual_scroll();
                                    let delta = crate::replay::wheel_delta(
                                        e.delta.pixel_delta(px(crate::replay::LINE_HEIGHT)),
                                        e.modifiers.shift,
                                    );
                                    let max = handle.max_offset();
                                    let offset = handle.offset();
                                    handle.set_offset(point(
                                        (offset.x + delta.x).clamp(-max.x, px(0.)),
                                        (offset.y + delta.y).clamp(-max.y, px(0.)),
                                    ));
                                    window.refresh();
                                    cx.stop_propagation();
                                });
                                let handle = paint_handle.clone();
                                let drag_follow = follow.clone();
                                window.on_mouse_event(move |e: &MouseDownEvent, phase, _, _| {
                                    let b = handle.bounds();
                                    let max = handle.max_offset();
                                    let with_gutters = Bounds::new(
                                        b.origin,
                                        b.size + size(px(GUTTER), px(GUTTER)),
                                    );
                                    if phase.capture()
                                        && e.button == MouseButton::Left
                                        && with_gutters.contains(&e.position)
                                        && ((max.x > px(0.) && e.position.y >= b.bottom())
                                            || (max.y > px(0.) && e.position.x >= b.right()))
                                    {
                                        drag_follow.borrow_mut().manual_scroll();
                                    }
                                });

                                // Layout has now populated the viewport bounds, even on
                                // the first frame of an end-state fixture or after resize.
                                let b = paint_handle.bounds();
                                if b.size.width <= px(0.) || b.size.height <= px(0.) {
                                    return;
                                }
                                let terminal = terminal.read(cx);
                                let grid = terminal.size();
                                let term = terminal.term().lock();
                                let history = term.history_size();
                                let content = crate::replay::scrollback_content_size(grid, history);
                                let cursor = term.grid().cursor.point;
                                let offset = paint_handle.offset();
                                let next = follow.borrow().position(
                                    [-f32::from(offset.x), -f32::from(offset.y)],
                                    [b.size.width.into(), b.size.height.into()],
                                    [content.width.into(), content.height.into()],
                                    [
                                        f32::from(grid.cell_width) * cursor.column.0 as f32,
                                        crate::replay::row_y(cursor.line.0, history, 0.),
                                    ],
                                    [grid.cell_width.into(), grid.line_height.into()],
                                );
                                let next = point(px(-next[0]), px(-next[1]));
                                if next != offset {
                                    paint_handle.set_offset(next);
                                    window.refresh();
                                }

                                // Like the AI drawer, paint faint resting thumbs even
                                // when the interactive scrollbar's hover animation ends.
                                // Read geometry after layout and follow/scroll updates.
                                let offset = paint_handle.offset();
                                let max = paint_handle.max_offset();
                                let hint =
                                    OverflowHint::new(b.size, b.size + size(max.x, max.y), offset);
                                if hint.right {
                                    let width = px(24.).min(b.size.width);
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            point(b.right() - width, b.top()),
                                            size(width, b.size.height),
                                        ),
                                        linear_gradient(
                                            270.,
                                            linear_color_stop(background, 0.),
                                            linear_color_stop(background.opacity(0.), 1.),
                                        ),
                                    ));
                                }
                                for (visible, vertical) in
                                    [(hint.horizontal, false), (hint.vertical, true)]
                                {
                                    if !visible {
                                        continue;
                                    }
                                    let (extent, max, offset) = if vertical {
                                        (b.size.height, max.y, offset.y)
                                    } else {
                                        (b.size.width, max.x, offset.x)
                                    };
                                    let extent = f32::from(extent);
                                    let max = f32::from(max);
                                    let length =
                                        (extent * extent / (extent + max)).max(24.).min(extent);
                                    let start = (-f32::from(offset) / max).clamp(0., 1.)
                                        * (extent - length);
                                    let bounds = if vertical {
                                        Bounds::new(
                                            point(b.right() + px(2.), b.top() + px(start + 2.)),
                                            size(px(4.), px((length - 4.).max(0.))),
                                        )
                                    } else {
                                        Bounds::new(
                                            point(b.left() + px(start + 2.), b.bottom() + px(2.)),
                                            size(px((length - 4.).max(0.)), px(4.)),
                                        )
                                    };
                                    window.paint_quad(
                                        fill(bounds, thumb.opacity(0.22)).corner_radii(px(2.)),
                                    );
                                }
                            },
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .children([false, true].map(|vertical| {
                        // Explicitly anchor each overlay at the top-left. Its scroll
                        // axis matches the text viewport exactly; only the cross
                        // axis includes a gutter, so thumb dragging reaches the end.
                        div()
                            .absolute()
                            .inset_0()
                            .when(vertical, |d| d.bottom(px(GUTTER)))
                            .when(!vertical, |d| d.right(px(GUTTER)))
                            .child(
                                (if vertical {
                                    Scrollbar::vertical(&handle)
                                } else {
                                    Scrollbar::horizontal(&handle)
                                })
                                .id(if vertical {
                                    "replay-scrollbar-y"
                                } else {
                                    "replay-scrollbar-x"
                                })
                                .viewport_from_layout()
                                .mode(if hovered {
                                    ScrollbarMode::Always
                                } else {
                                    ScrollbarMode::Hover
                                })
                                .styles(|s| {
                                    s.track(|s| s.bg(transparent_black()).width(px(GUTTER)))
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
                                                .inset(px(1.))
                                                .min_length(px(24.))
                                                .radius(px(3.))
                                        })
                                        .thumb_active(|s| {
                                            s.bg(thumb)
                                                .width(px(6.))
                                                .inset(px(1.))
                                                .min_length(px(24.))
                                                .radius(px(3.))
                                        })
                                }),
                            )
                    })),
            )
            .when(show_times, |d| {
                d.child(
                    canvas(
                        move |_, _, _| (time_handle.bounds(), time_handle.offset()),
                        move |bounds, (viewport, offset), window, cx| {
                            let terminal = time_terminal.read(cx);
                            let term = terminal.term().lock();
                            let history = term.history_size();
                            let clip = Bounds::new(
                                point(bounds.left(), viewport.top()),
                                size(px(crate::replay::TIME_WIDTH), viewport.size.height),
                            );
                            let first =
                                ((-f32::from(offset.y) / crate::replay::LINE_HEIGHT).floor()
                                    as i32
                                    - history as i32)
                                    .max(-(history as i32));
                            let last = ((f32::from(viewport.size.height - offset.y)
                                / crate::replay::LINE_HEIGHT)
                                .ceil() as i32
                                - history as i32)
                                .min(term.screen_lines() as i32);
                            let labels: Vec<_> = (first..last)
                                .filter_map(|line| {
                                    terminal
                                        .replay_times
                                        .label(
                                            &term,
                                            alacritty_terminal::index::Line(line),
                                            terminal.replay_timestamp,
                                        )
                                        .map(|text| (line, text))
                                })
                                .collect();
                            drop(term);
                            window.with_content_mask(
                                Some(gpui::ContentMask { bounds: clip }),
                                |window| {
                                    for (line, text) in labels {
                                        let run = gpui::TextRun {
                                            len: text.len(),
                                            font: time_font.clone(),
                                            color: time_color,
                                            background_color: None,
                                            underline: None,
                                            strikethrough: None,
                                        };
                                        let shaped = window.text_system().shape_line(
                                            text.into(),
                                            px(crate::replay::TIME_FONT_SIZE),
                                            &[run],
                                            None,
                                        );
                                        let y = viewport.top()
                                            + px(crate::replay::row_y(
                                                line,
                                                history,
                                                offset.y.into(),
                                            ));
                                        let origin = point(clip.right() - shaped.width, y);
                                        shaped
                                            .paint(
                                                origin,
                                                px(crate::replay::LINE_HEIGHT),
                                                gpui::TextAlign::Left,
                                                None,
                                                window,
                                                cx,
                                            )
                                            .ok();
                                    }
                                },
                            );
                        },
                    )
                    .absolute()
                    .left(px(PADDING_X))
                    .top(px(PADDING_Y))
                    .w(px(crate::replay::TIME_WIDTH))
                    .bottom(px(PADDING_Y)),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_grid_overflows_both_player_axes_and_fade_tracks_right_edge() {
        // The 1140 x 740 recording dialog: list, padding, header and controls
        // leave an 844 x 552 player. Gutters reuse its terminal padding.
        let viewport = size(px(844. - 2. * PADDING_X), px(552. - 2. * PADDING_Y));
        let grid = crate::replay::grid_size(
            crate::TerminalSize {
                cols: 142,
                rows: 36,
                ..Default::default()
            },
            px(7.8),
        );
        assert_eq!(crate::replay::FONT_SIZE, 13.);
        let content = crate::replay::content_size(grid);
        let followed = crate::replay::FollowCursor::default().position(
            [0.; 2],
            [viewport.width.into(), viewport.height.into()],
            [content.width.into(), content.height.into()],
            [14. * 7.8, 34. * crate::replay::LINE_HEIGHT],
            [7.8, crate::replay::LINE_HEIGHT],
        );
        assert_eq!(followed, [0., 180.]);
        let start = OverflowHint::new(viewport, content, point(px(-followed[0]), px(-followed[1])));
        assert_eq!(
            start,
            OverflowHint {
                horizontal: true,
                vertical: true,
                right: true
            }
        );
        let end = OverflowHint::new(
            viewport,
            content,
            point(
                viewport.width - content.width,
                viewport.height - content.height,
            ),
        );
        assert_eq!(
            end,
            OverflowHint {
                horizontal: true,
                vertical: true,
                right: false
            }
        );
        assert_eq!(
            OverflowHint::new(content, content, point(px(0.), px(0.))),
            OverflowHint {
                horizontal: false,
                vertical: false,
                right: false
            }
        );
    }
}
