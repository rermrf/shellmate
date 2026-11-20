//! One foreground gesture across all workspaces. Geometry stays local to each painted strip.
use super::*;
use gpui::{
    AnyWindowHandle, Bounds, DispatchPhase, DragMoveEvent, MouseDownEvent, MouseMoveEvent, Pixels,
    Point, Size, WindowId, point, size,
};
use std::cell::Cell;

#[cfg(debug_assertions)]
pub(super) mod debug;
#[cfg(target_os = "macos")]
mod ghost;
#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct TabStrip {
    pub(super) bounds: Option<Bounds<Pixels>>,
    tabs: Vec<(usize, Bounds<Pixels>)>,
}

impl TabStrip {
    pub(super) fn record_tabs(&mut self, indices: &[usize], bounds: Vec<Bounds<Pixels>>) {
        self.tabs.clear();
        self.tabs.extend(indices.iter().copied().zip(bounds));
    }

    fn slot(&self, x: Pixels) -> usize {
        for &(index, rect) in &self.tabs {
            if x < rect.center().x {
                return index;
            }
        }
        self.tabs.last().map_or(0, |(index, _)| index + 1)
    }

    fn marker(&self, index: usize) -> Option<Bounds<Pixels>> {
        let strip = self.bounds?;
        let (x, y, height) = if let Some((_, tab)) = self.tabs.iter().find(|(i, _)| *i == index) {
            (tab.left(), tab.top(), tab.size.height)
        } else if let Some((last, tab)) = self.tabs.last() {
            if index != last + 1 {
                return None;
            }
            (tab.right(), tab.top(), tab.size.height)
        } else {
            (strip.left(), strip.center().y - px(18.), px(36.))
        };
        Some(Bounds::new(point(x, y), size(px(2.), height)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Over {
    Strip(AnyWindowHandle, usize),
    Body(AnyWindowHandle),
    Outside,
    Obstructed,
}

impl Over {
    fn marker(self) -> Option<(AnyWindowHandle, usize)> {
        match self {
            Self::Strip(window, index) => Some((window, index)),
            _ => None,
        }
    }
}

fn hit_window(
    handle: AnyWindowHandle,
    bounds: Bounds<Pixels>,
    strip: Option<&TabStrip>,
    screen: Point<Pixels>,
) -> Option<Over> {
    if !bounds.contains(&screen) {
        return None;
    }
    let local = screen - bounds.origin;
    Some(match strip {
        None => Over::Obstructed,
        Some(strip) if strip.bounds.is_some_and(|b| b.contains(&local)) => {
            Over::Strip(handle, strip.slot(local.x))
        }
        Some(_) => Over::Body(handle),
    })
}

#[cfg(target_os = "macos")]
fn primary_screen_top() -> Option<f64> {
    let screens = objc2_app_kit::NSScreen::screens(objc2::MainThreadMarker::new().expect("UI thread"));
    let primary = screens.firstObject()?.frame();
    Some(primary.origin.y + primary.size.height)
}

// gpui-pre-macos 0.3.12 reports Window::bounds relative to its NSScreen.
// Normalize to the primary display's top-left when comparing different displays.
fn screen_bounds(window: &Window) -> Bounds<Pixels> {
    let bounds = window.bounds();
    #[cfg(target_os = "macos")]
    if let Some(screen) = lifecycle::native_window(window).and_then(|w| w.screen()) {
        let frame = screen.frame();
        if let Some(top) = primary_screen_top() {
            let offset = point(
                px(frame.origin.x as f32),
                px((top - frame.origin.y - frame.size.height) as f32),
            );
            return Bounds::new(bounds.origin + offset, bounds.size);
        }
    }
    bounds
}

fn over_at(screen: Point<Pixels>, window: &Window, cx: &mut App) -> Over {
    // On macOS GPUI's window_stack() wraps NSApp.orderedWindows in front-to-back order.
    let stack = cx
        .window_stack()
        .unwrap_or_else(|| Hub::windows(cx).iter().map(|entry| entry.handle).collect());
    stack
        .into_iter()
        .find_map(|handle| {
            #[cfg(target_os = "macos")]
            if handle.downcast::<ghost::FloatingTab>().is_some() {
                return None;
            }
            let inspect = |w: &Window, cx: &App| {
                if !w.is_visible() {
                    return None;
                }
                let workspace = Hub::entry(handle, cx).and_then(|entry| entry.workspace.upgrade());
                let strip = workspace.as_ref().map(|entity| &entity.read(cx).tab_strip);
                hit_window(handle, screen_bounds(w), strip, screen)
            };
            if handle == window.window_handle() {
                inspect(window, cx)
            } else {
                handle.update(cx, |_, w, cx| inspect(w, cx)).ok().flatten()
            }
        })
        .unwrap_or(Over::Outside)
}

pub(super) struct DraggedTab {
    source: AnyWindowHandle,
    session_id: usize,
    // GPUI installs the payload at render time. Fill the offset when it starts the gesture.
    grab_offset: Cell<Point<Pixels>>,
}

impl DraggedTab {
    pub(super) fn new(source: AnyWindowHandle, session_id: usize) -> Self {
        Self {
            source,
            session_id,
            grab_offset: Cell::default(),
        }
    }

    pub(super) fn begin(
        &self,
        offset: Point<Pixels>,
        width: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<TabPreview> {
        let current = window.mouse_position();
        let drag = &mut cx.global_mut::<Hub>().tab_drag;
        let down = drag.press.take().map_or(current, |(_, pos)| pos);
        self.grab_offset.set(down - (current - offset));
        drag.consume_release = Some(self.source);
        drag.active = Some(ActiveDrag {
            source: self.source,
            session_id: self.session_id,
            grab_offset: self.grab_offset.get(),
            over: Over::Outside,
        });
        update_over(current, window, cx);
        cx.new(|_| TabPreview {
            source: self.source,
            id: self.session_id,
            width,
            displacement: offset - self.grab_offset.get(),
        })
    }
}

pub(super) struct TabPreview {
    source: AnyWindowHandle,
    id: usize,
    width: Pixels,
    displacement: Point<Pixels>,
}

impl Render for TabPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let over = cx.global::<Hub>().tab_drag.active.map(|drag| drag.over);
        let visible = matches!(over, Some(Over::Strip(handle, _)) if handle == self.source && handle == window.window_handle());
        let mut preview = div().font_family(theme::UI_FONT);
        if visible
            && let Some(owner) = Hub::entry(self.source, cx).and_then(|e| e.workspace.upgrade())
        {
            let tab = owner.update(cx, |ws, cx| {
                let ix = ws.sessions.iter().position(|tab| tab.id == self.id)?;
                Some(
                    ws.tab_face(ix, self.width, true, cx)
                        .relative()
                        .left(self.displacement.x)
                        .top(self.displacement.y),
                )
            });
            // GPUI paints the drag view after the workspace. Keep the 2 pt line
            // above the tab when the pointer and the insertion boundary overlap.
            preview = preview.children(tab).child(
                gpui::canvas(
                    |_, _, _| (),
                    |_, _, w, cx| {
                        paint_marker(w, cx);
                    },
                )
                .absolute()
                .size_0(),
            );
        }
        preview
    }
}

#[derive(Clone, Copy)]
struct ActiveDrag {
    source: AnyWindowHandle,
    session_id: usize,
    grab_offset: Point<Pixels>,
    over: Over,
}

#[derive(Default)]
pub(super) struct TabDrag {
    press: Option<(AnyWindowHandle, Point<Pixels>)>,
    active: Option<ActiveDrag>,
    #[cfg(target_os = "macos")]
    ghost: Option<gpui::WindowHandle<ghost::FloatingTab>>,
    // A cancelled drag still owns the eventual release (also over close/retry buttons).
    consume_release: Option<AnyWindowHandle>,
}

pub(super) fn press(event: &MouseDownEvent, window: &mut Window, cx: &mut App) {
    let drag = &mut cx.global_mut::<Hub>().tab_drag;
    drag.press = Some((window.window_handle(), event.position));
    drag.consume_release = None;
}

fn notify_marker(over: Over, cx: &mut App) {
    if let Some((handle, _)) = over.marker()
        && let Some(entry) = Hub::entry(handle, cx)
    {
        cx.notify(entry.workspace.entity_id());
    }
}

fn update_over(position: Point<Pixels>, window: &Window, cx: &mut App) {
    let screen = screen_bounds(window).origin + position;
    let over = over_at(screen, window, cx);
    if let Some(drag) = &mut cx.global_mut::<Hub>().tab_drag.active {
        let previous = std::mem::replace(&mut drag.over, over);
        #[cfg(target_os = "macos")]
        let active = *drag;
        if previous.marker() != over.marker() {
            notify_marker(previous, cx);
            notify_marker(over, cx);
        }
        #[cfg(target_os = "macos")]
        if over == Over::Outside {
            ghost::follow(active, screen, cx);
        } else {
            ghost::close(cx);
        }
    }
}

pub(super) fn moved(event: &DragMoveEvent<DraggedTab>, window: &mut Window, cx: &mut App) {
    if event.drag(cx).source == window.window_handle() {
        update_over(event.event.position, window, cx);
    }
}

fn clear(cx: &mut App) -> Option<ActiveDrag> {
    let drag = cx.global_mut::<Hub>().tab_drag.active.take()?;
    #[cfg(target_os = "macos")]
    ghost::close(cx);
    notify_marker(drag.over, cx);
    Some(drag)
}

pub(super) fn escape(window: &mut Window, cx: &mut App) {
    if clear(cx).is_some() {
        cx.stop_active_drag(window);
        cx.stop_propagation();
    }
}

pub(super) fn window_closed(id: WindowId, cx: &mut App) {
    let gesture = &mut cx.global_mut::<Hub>().tab_drag;
    if gesture.press.is_some_and(|(h, _)| h.window_id() == id) {
        gesture.press = None;
    }
    if gesture.consume_release.is_some_and(|h| h.window_id() == id) {
        gesture.consume_release = None;
    }
    if gesture
        .active
        .is_some_and(|drag| drag.source.window_id() == id)
    {
        clear(cx);
    }
}

fn within_click_distance(down: Point<Pixels>, current: Point<Pixels>) -> bool {
    (current - down).magnitude() <= 4.
}

fn paint_marker(window: &mut Window, cx: &App) {
    let handle = window.window_handle();
    let gesture = &cx.global::<Hub>().tab_drag;
    if let Some((target, index)) = gesture.active.and_then(|drag| drag.over.marker())
        && target == handle
        && let Some(owner) = Hub::entry(handle, cx).and_then(|entry| entry.workspace.upgrade())
        && let Some(marker) = owner.read(cx).tab_strip.marker(index)
    {
        window.paint_quad(gpui::fill(marker, cx.palette().acc));
    }
}

pub(super) fn paint_and_listen(window: &mut Window, cx: &mut App) {
    let handle = window.window_handle();
    paint_marker(window, cx);
    window.on_mouse_event::<MouseMoveEvent>(move |event, phase, _, cx| {
        // GPUI normally starts on_drag at 2 pt. Keep its mouse-down/click pending until >4 pt.
        if phase == DispatchPhase::Capture
            && event.pressed_button == Some(MouseButton::Left)
            && let Some((source, down)) = cx.global::<Hub>().tab_drag.press
            && source == handle
            && within_click_distance(down, event.position)
        {
            cx.stop_propagation();
        }
    });
    window.on_mouse_event::<MouseUpEvent>(move |event, phase, window, cx| {
        if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
            return;
        }
        let gesture = &mut cx.global_mut::<Hub>().tab_drag;
        if gesture.press.is_some_and(|(h, _)| h == handle) {
            gesture.press = None;
        }
        if gesture.consume_release != Some(handle) {
            return;
        }
        gesture.consume_release = None;
        if let Some(drag) = clear(cx) {
            let frame = screen_bounds(window);
            let screen = frame.origin + event.position;
            let over = over_at(screen, window, cx);
            finish(drag, over, screen, frame.size, cx);
        }
        cx.stop_propagation();
    });
}

#[derive(Debug, PartialEq)]
enum DropAction {
    Keep,
    Reorder(usize),
    Insert(AnyWindowHandle, usize),
    New(Bounds<Pixels>),
}

fn drop_action(
    drag: ActiveDrag,
    from: usize,
    count: usize,
    over: Over,
    screen: Point<Pixels>,
    size: Size<Pixels>,
) -> DropAction {
    match over {
        Over::Strip(target, slot) if target == drag.source => {
            let slot = slot.min(count);
            let index = slot - usize::from(from < slot);
            if index == from {
                DropAction::Keep
            } else {
                DropAction::Reorder(index)
            }
        }
        Over::Strip(target, slot) => DropAction::Insert(target, slot),
        Over::Body(target) if target != drag.source => DropAction::Insert(target, usize::MAX),
        Over::Outside if count > 1 => DropAction::New(Bounds::new(screen - drag.grab_offset, size)),
        _ => DropAction::Keep,
    }
}

fn finish(drag: ActiveDrag, over: Over, screen: Point<Pixels>, size: Size<Pixels>, cx: &mut App) {
    // End the source mouse dispatch before updating either native window.
    cx.defer(move |cx| {
        let Some(source) = Hub::entry(drag.source, cx) else {
            return;
        };
        let Some(owner) = source.workspace.upgrade() else {
            return;
        };
        let ws = owner.read(cx);
        let Some(from) = ws.sessions.iter().position(|s| s.id == drag.session_id) else {
            return;
        };
        let action = drop_action(drag, from, ws.sessions.len(), over, screen, size);
        log::debug!("tab drop session {}: {action:?}", drag.session_id);
        let destination = match action {
            DropAction::Keep => return,
            DropAction::Reorder(index) => {
                source.update(cx, |ws, w, cx| {
                    let active = ws.main_id();
                    if let Some(tab) = ws.detach_session(drag.session_id, ws.handle, w, cx) {
                        ws.attach_session(tab, index, w, cx);
                        ws.active = ws.sessions.iter().position(|s| Some(s.id) == active);
                        ws.sync_productivity(cx);
                        cx.notify();
                    }
                });
                return;
            }
            DropAction::Insert(handle, index) => transfer::Destination::Existing(handle, index),
            // The bounds are global. GPUI uses the primary display as their reference.
            DropAction::New(bounds) => transfer::Destination::New((bounds, None)),
        };
        transfer::move_sessions(source.handle, destination, vec![drag.session_id], cx);
    });
}
