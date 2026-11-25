use super::*;

impl OutputPane {
    pub(super) fn navigate(
        &mut self,
        vertical: Option<bool>,
        horizontal: Option<bool>,
        pages: f32,
        cx: &mut Context<Self>,
    ) {
        let handle = self.viewport.read(cx).handle.clone();
        let max = handle.max_offset();
        let mut offset = handle.offset();
        if let Some(bottom) = vertical {
            offset.y = if bottom { -max.y } else { px(0.) };
            self.follow = bottom;
            if bottom {
                self.selection = None;
                if self.source.running {
                    self.load(cx);
                }
            }
        }
        if let Some(right) = horizontal {
            offset.x = if right { -max.x } else { px(0.) };
        }
        if pages != 0. {
            offset.y = (offset.y + px(pages * f32::from(handle.bounds().size.height)))
                .clamp(-max.y, px(0.));
            self.follow = false;
        }
        handle.set_offset(offset);
        cx.emit(Reading);
        cx.notify();
    }
    pub(super) fn select_at(&mut self, point: Point<Pixels>, cx: &mut Context<Self>) {
        let g = self.geometry.borrow();
        let Some(doc) = &self.doc else {
            return;
        };
        if g.viewport.size.width <= px(1.) || g.viewport.size.height <= px(1.) {
            return;
        }
        let y = point
            .y
            .clamp(g.viewport.top(), g.viewport.bottom() - px(1.));
        let x = point
            .x
            .clamp(g.viewport.left(), g.viewport.right() - px(1.));
        let row =
            ((f32::from(y - g.origin.y) / 18.).max(0.) as usize).min(doc.rows.saturating_sub(1));
        let actual = doc.visible_row(row, self.expanded);
        if let Some((_, carets)) = g.hits.iter().find(|(row, _)| Some(*row) == actual) {
            let at = carets
                .partition_point(|(position, _)| *position <= x)
                .saturating_sub(1);
            let Some((_, byte)) = carets.get(at) else {
                return;
            };
            let byte = *byte;
            if let Some((_, end)) = &mut self.selection {
                *end = byte;
            } else {
                self.selection = Some((byte, byte));
            }
        }
        drop(g);
        cx.notify();
    }
    pub(super) fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(w, cx);
        self.dragging = true;
        self.follow = false;
        self.pointer = event.position;
        if !event.modifiers.shift {
            self.selection = None;
        }
        self.select_at(event.position, cx);
        cx.emit(Reading);
        cx.stop_propagation();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let keep = this
                    .update(cx, |s, cx| {
                        if !s.dragging {
                            return false;
                        }
                        let g = s.geometry.borrow();
                        let p = s.pointer;
                        let handle = s.viewport.read(cx).handle.clone();
                        let speed = |x: Pixels, a: Pixels, b: Pixels| {
                            if x < a {
                                f32::from(a - x).min(32.)
                            } else if x > b {
                                -f32::from(x - b).min(32.)
                            } else {
                                0.
                            }
                        };
                        let delta = point(
                            px(speed(p.x, g.viewport.left(), g.viewport.right())),
                            px(speed(p.y, g.viewport.top(), g.viewport.bottom())),
                        );
                        drop(g);
                        if delta.x != px(0.) || delta.y != px(0.) {
                            let max = handle.max_offset();
                            let old = handle.offset();
                            handle.set_offset(point(
                                (old.x + delta.x).clamp(-max.x, px(0.)),
                                (old.y + delta.y).clamp(-max.y, px(0.)),
                            ));
                            s.select_at(p, cx);
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }
    pub(super) fn copy_selection(
        &mut self,
        _: &CopySelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((a, b)) = self.selection.filter(|(a, b)| a != b) {
            self.copy(Some(a.min(b)..a.max(b)), cx);
        } else {
            cx.propagate();
        }
    }
    pub(super) fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(doc) = &self.doc {
            self.selection = Some((0, doc.bytes));
            self.follow = false;
            cx.notify();
        }
    }
}
