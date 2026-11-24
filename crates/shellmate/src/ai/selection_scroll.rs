//! One drag-scroll owner for the drawer's external viewports. TextView and
//! literal TextView participants retain logical selection/copy; this only moves their windows.
use super::scroll::State as Viewport;
use gpui::{base::TextSelection, prelude::*, *};
use std::{collections::HashMap, time::Duration};

pub(super) struct DragStarted;
pub(super) struct SelectionScroll {
    conversation: ScrollHandle,
    viewports: HashMap<EntityId, WeakEntity<Viewport>>,
    horizontal: Option<WeakEntity<Viewport>>,
    pointer: Point<Pixels>,
    modifiers: Modifiers,
    pressed: bool,
    moved: bool,
    replaying: bool,
    replay_after_paint: bool,
    generation: u64,
    timer: Option<Task<()>>,
}
impl EventEmitter<DragStarted> for SelectionScroll {}

/// Pixels per 16ms tick: zero inside, symmetric distance ramp, capped at 32px.
fn speed(position: f32, start: f32, end: f32) -> f32 {
    let distance = if position < start {
        position - start
    } else if position > end {
        position - end
    } else {
        return 0.;
    };
    distance.signum() * (2. + distance.abs() * 0.25).min(32.)
}
fn advance(offset: f32, delta: f32, max: f32) -> f32 {
    (offset - delta).clamp(-max.max(0.), 0.)
}
fn clamp_point(position: Point<Pixels>, bounds: Bounds<Pixels>) -> Point<Pixels> {
    point(
        position
            .x
            .clamp(bounds.left() + px(1.), bounds.right() - px(1.)),
        position
            .y
            .clamp(bounds.top() + px(1.), bounds.bottom() - px(1.)),
    )
}
fn usable(bounds: Bounds<Pixels>) -> bool {
    bounds.size.width > px(2.) && bounds.size.height > px(2.)
}

impl SelectionScroll {
    pub fn new(conversation: ScrollHandle) -> Self {
        Self {
            conversation,
            viewports: HashMap::new(),
            horizontal: None,
            pointer: point(px(0.), px(0.)),
            modifiers: Modifiers::default(),
            pressed: false,
            moved: false,
            replaying: false,
            replay_after_paint: false,
            generation: 0,
            timer: None,
        }
    }
    pub fn register(&mut self, viewport: &Entity<Viewport>) {
        self.viewports
            .insert(viewport.entity_id(), viewport.downgrade());
    }
    #[cfg(debug_assertions)]
    pub fn debug_viewport_at(&self, position: Point<Pixels>, cx: &App) -> Option<ScrollHandle> {
        self.viewports
            .values()
            .filter_map(|viewport| {
                let viewport = viewport.upgrade()?;
                let handle = viewport.read(cx).handle.clone();
                let bounds = handle.bounds().intersect(&self.conversation.bounds());
                bounds.contains(&position).then_some((
                    handle,
                    f32::from(bounds.size.width) * f32::from(bounds.size.height),
                ))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(handle, _)| handle)
    }
    pub fn stop(&mut self) {
        self.generation += 1;
        self.pressed = false;
        self.moved = false;
        self.horizontal = None;
        self.timer = None;
        self.replay_after_paint = false;
    }
    fn horizontal(&self, cx: &App) -> Option<ScrollHandle> {
        self.horizontal
            .as_ref()?
            .upgrade()
            .map(|v| v.read(cx).handle.clone())
    }
    fn projected_pointer(&self, cx: &App) -> Point<Pixels> {
        let outer = self.conversation.bounds();
        if !usable(outer) {
            return self.pointer;
        }
        let mut projected = clamp_point(self.pointer, outer);
        if let Some(handle) = self.horizontal(cx) {
            let bounds = handle.bounds().intersect(&outer);
            if usable(bounds) && self.pointer.y >= bounds.top() && self.pointer.y <= bounds.bottom()
            {
                projected.x = projected
                    .x
                    .clamp(bounds.left() + px(1.), bounds.right() - px(1.));
            }
        }
        projected
    }
    fn delta(&self, cx: &App) -> Point<Pixels> {
        let outer = self.conversation.bounds();
        if !usable(outer) {
            return point(px(0.), px(0.));
        }
        let y = speed(
            self.pointer.y.into(),
            outer.top().into(),
            outer.bottom().into(),
        );
        let x = self.horizontal(cx).map_or(0., |handle| {
            let bounds = handle.bounds().intersect(&outer);
            if usable(bounds) && self.pointer.y >= bounds.top() && self.pointer.y <= bounds.bottom()
            {
                speed(
                    self.pointer.x.into(),
                    bounds.left().into(),
                    bounds.right().into(),
                )
            } else {
                0.
            }
        });
        point(px(x), px(y))
    }
    fn moved(&mut self, event: &MouseMoveEvent, w: &mut Window, cx: &mut Context<Self>) {
        if self.replaying {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.stop();
            return;
        }
        if !self.pressed {
            return;
        }
        self.pointer = event.position;
        self.modifiers = event.modifiers;
        if !self.moved {
            self.moved = true;
            cx.emit(DragStarted);
        }
        let delta = self.delta(cx);
        if delta == point(px(0.), px(0.)) {
            self.timer = None;
        } else if self.timer.is_none() {
            self.timer = Some(cx.spawn_in(w, async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                    if !this
                        .update_in(cx, |s, w, cx| s.tick(w, cx))
                        .unwrap_or(false)
                    {
                        break;
                    }
                }
            }));
        }
        // An outside pointer would hit-test a different TextView or the whole
        // custom block. Project it onto the visible text edge instead.
        if self.projected_pointer(cx) != event.position {
            cx.stop_propagation();
            w.prevent_default();
            Self::defer_replay(cx.entity().downgrade(), self.generation, w, cx);
        }
    }
    fn tick(&mut self, w: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.pressed
            || !self.moved
            || !w.is_window_active()
            || !usable(self.conversation.bounds())
        {
            self.stop();
            return false;
        }
        let delta = self.delta(cx);
        let mut changed = false;
        if let Some(handle) = self.horizontal(cx) {
            let old = handle.offset();
            let x = px(advance(
                old.x.into(),
                delta.x.into(),
                handle.max_offset().x.into(),
            ));
            if x != old.x {
                handle.set_offset(point(x, old.y));
                changed = true;
            }
        }
        let old = self.conversation.offset();
        let y = px(advance(
            old.y.into(),
            delta.y.into(),
            self.conversation.max_offset().y.into(),
        ));
        if y != old.y {
            self.conversation.set_offset(point(old.x, y));
            changed = true;
        }
        if !changed {
            self.timer = None;
            return false;
        }
        self.replay_after_paint = true;
        w.refresh();
        true
    }
    fn defer_replay(this: WeakEntity<Self>, generation: u64, w: &mut Window, cx: &mut App) {
        w.defer(cx, move |w, cx| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let event = this.update(cx, |s, cx| {
                if !s.pressed || generation != s.generation {
                    return None;
                }
                s.replaying = true;
                Some(MouseMoveEvent {
                    position: s.projected_pointer(cx),
                    pressed_button: Some(MouseButton::Left),
                    modifiers: s.modifiers,
                })
            });
            if let Some(event) = event {
                w.dispatch_event(event.to_platform_input(), cx);
                this.update(cx, |s, _| s.replaying = false);
            }
        });
    }
}

/// Register before scroll masks, so capture can suppress Base's second scroll
/// writer. Replay is deferred until the entire frame has painted its geometry.
/// This canvas has no hitbox and never changes the list's size or appearance.
pub(super) fn layer(state: Entity<SelectionScroll>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, _, w, cx| {
            let replay = state.update(cx, |s, _| {
                std::mem::take(&mut s.replay_after_paint).then_some(s.generation)
            });
            if let Some(generation) = replay {
                SelectionScroll::defer_replay(state.downgrade(), generation, w, cx);
            }
            let this = state.downgrade();
            w.on_mouse_event(move |e: &MouseDownEvent, phase, w, cx| {
                if !phase.capture() {
                    return;
                }
                let Some(this) = this.upgrade() else {
                    return;
                };
                // Observe only. Native window selection owns focus and begins
                // in bubble; capture must neither consume nor suppress the down.
                let selectable = e.button == MouseButton::Left
                    && this.read(cx).conversation.bounds().contains(&e.position)
                    && TextSelection::is_selectable_at(e.position, w, cx);
                this.update(cx, |s, cx| {
                    s.stop();
                    if !selectable {
                        return;
                    }
                    s.pressed = true;
                    s.pointer = e.position;
                    s.modifiers = e.modifiers;
                    s.viewports.retain(|_, v| v.upgrade().is_some());
                    s.horizontal = s
                        .viewports
                        .values()
                        .filter_map(|v| {
                            let entity = v.upgrade()?;
                            let bounds = entity.read(cx).handle.bounds();
                            bounds.contains(&e.position).then_some((
                                v.clone(),
                                f32::from(bounds.size.width) * f32::from(bounds.size.height),
                            ))
                        })
                        .min_by(|a, b| a.1.total_cmp(&b.1))
                        .map(|(v, _)| v);
                });
            });
            let this = state.downgrade();
            w.on_mouse_event(move |e: &MouseMoveEvent, phase, w, cx| {
                if phase.capture() {
                    this.update(cx, |s, cx| s.moved(e, w, cx)).ok();
                }
            });
            let this = state.downgrade();
            w.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                if phase.capture() {
                    this.update(cx, |s, _| s.stop()).ok();
                }
            });
            let this = state.downgrade();
            w.on_mouse_event(move |_: &ScrollEvent, phase, w, cx| {
                // Base's window TextSelection also synthesizes vertical wheel ticks
                // near the anchor clip. During our drag, only this controller may
                // scroll; suppress that second writer (ordinary wheel is unchanged).
                if phase.capture()
                    && this
                        .upgrade()
                        .is_some_and(|s| s.read(cx).pressed && s.read(cx).moved)
                {
                    cx.stop_propagation();
                    w.prevent_default();
                }
            });
        },
    )
    .absolute()
    .inset_0()
}

#[cfg(test)]
mod tests {
    use super::{SelectionScroll, advance, clamp_point, speed};
    use gpui::{Bounds, ScrollHandle, point, px, size};
    #[test]
    fn speed_is_zero_inside_symmetric_ramped_and_capped() {
        for p in [100., 150., 300.] {
            assert_eq!(speed(p, 100., 300.), 0.);
        }
        assert!(speed(301., 100., 300.) > 0.);
        assert!(speed(340., 100., 300.) > speed(310., 100., 300.));
        assert_eq!(speed(60., 100., 300.), -speed(340., 100., 300.));
        assert_eq!(speed(-10000., 100., 300.), -32.);
        assert_eq!(speed(10000., 100., 300.), 32.);
    }
    #[test]
    fn release_invalidates_pending_replays_and_clears_drag_state() {
        let mut state = SelectionScroll::new(ScrollHandle::new());
        state.pressed = true;
        state.moved = true;
        state.replay_after_paint = true;
        let generation = state.generation;
        state.stop();
        assert!(!state.pressed && !state.moved && !state.replay_after_paint);
        assert!(state.timer.is_none() && state.horizontal.is_none());
        assert_ne!(generation, state.generation);
    }
    #[test]
    fn offsets_stop_at_edges_and_pointer_stays_in_visible_text() {
        assert_eq!(advance(0., 20., 100.), -20.);
        assert_eq!(advance(-90., 20., 100.), -100.);
        assert_eq!(advance(-10., -20., 100.), 0.);
        assert_eq!(advance(0., 20., 0.), 0.);
        let b = Bounds::new(point(px(100.), px(50.)), size(px(200.), px(100.)));
        assert_eq!(
            clamp_point(point(px(500.), px(-20.)), b),
            point(px(299.), px(51.))
        );
    }
}
