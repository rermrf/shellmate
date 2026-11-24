//! Debug-build-only input replay through GPUI's production event dispatcher.
use crate::workspace::Workspace;
use gpui::{
    AnyWindowHandle, App, AsyncApp, Entity, InputEvent, Keystroke, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, OperatingSystem, Pixels, PlatformInput, Point,
    ScrollHandle, WeakEntity, Window, point, px,
};
use std::time::Duration;

const READY_DELAY: Duration = Duration::from_millis(1500);
const MOVE_DELAY: Duration = Duration::from_millis(16);
const ACTIVATION_DELAY: Duration = Duration::from_millis(300);
const HOVER_DELAY: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Drag {
    start: Point<Pixels>,
    end: Point<Pixels>,
    hold: Duration,
}
fn coordinate(text: &str) -> Option<Point<Pixels>> {
    let (x, y) = text.split_once(',')?;
    let (x, y) = (x.parse::<f32>().ok()?, y.parse::<f32>().ok()?);
    (x.is_finite() && y.is_finite()).then(|| point(px(x), px(y)))
}
fn parse_drag(text: &str) -> Option<Drag> {
    let mut parts = text.split_whitespace();
    let drag = Drag {
        start: coordinate(parts.next()?)?,
        end: coordinate(parts.next()?)?,
        hold: Duration::from_millis(parts.next()?.parse().ok()?),
    };
    parts.next().is_none().then_some(drag)
}
/// The existing tab-drag hook retains its colon-separated command syntax.
pub(crate) fn coordinate_drag(spec: &str) -> bool {
    !spec.contains(':')
}

struct Key {
    label: String,
    stroke: Keystroke,
}
fn parse_keys(text: &str, os: OperatingSystem) -> Result<Vec<Key>, String> {
    text.split_whitespace()
        .map(|label| {
            // Keystroke::parse takes &'static str in this version. This dynamic
            // counterpart uses the same parser, without leaking environment strings.
            Keystroke::parse_with_os(label, os)
                .map(|stroke| Key {
                    label: label.into(),
                    stroke,
                })
                .map_err(|e| format!("invalid debug key {label:?}: {e}"))
        })
        .collect()
}
fn delay(value: Option<&str>) -> Option<Duration> {
    value.map_or(Some(Duration::from_millis(400)), |s| {
        s.parse::<u64>().ok().map(Duration::from_millis)
    })
}
fn drag_moves(drag: Drag) -> impl Iterator<Item = Point<Pixels>> {
    (1..=8).map(move |step| drag.start + (drag.end - drag.start) * (step as f32 / 8.))
}
fn down(position: Point<Pixels>) -> PlatformInput {
    MouseDownEvent {
        button: MouseButton::Left,
        position,
        click_count: 1,
        first_mouse: false,
        modifiers: Modifiers::default(),
    }
    .to_platform_input()
}
fn moved(position: Point<Pixels>) -> PlatformInput {
    MouseMoveEvent {
        position,
        pressed_button: Some(MouseButton::Left),
        modifiers: Modifiers::default(),
    }
    .to_platform_input()
}
fn hover(position: Point<Pixels>) -> PlatformInput {
    MouseMoveEvent {
        position,
        pressed_button: None,
        modifiers: Modifiers::default(),
    }
    .to_platform_input()
}
fn up(position: Point<Pixels>) -> PlatformInput {
    MouseUpEvent {
        button: MouseButton::Left,
        position,
        click_count: 1,
        modifiers: Modifiers::default(),
    }
    .to_platform_input()
}

pub(crate) fn install(handle: AnyWindowHandle, workspace: &Entity<Workspace>, cx: &mut App) {
    let key_spec = std::env::var("SHELLMATE_DEBUG_KEYS").ok();
    let drag_spec = std::env::var("SHELLMATE_DEBUG_DRAG")
        .ok()
        .filter(|s| coordinate_drag(s));
    if key_spec.is_none() && drag_spec.is_none() {
        return;
    }
    let keys = match parse_keys(key_spec.as_deref().unwrap_or(""), cx.platform_info().os) {
        Ok(keys) => keys,
        Err(e) => {
            log::warn!("{e}");
            return;
        }
    };
    let drag = match drag_spec.as_deref() {
        Some(spec) => match parse_drag(spec) {
            Some(drag) => Some(drag),
            None => {
                log::warn!("invalid SHELLMATE_DEBUG_DRAG: expected x1,y1 x2,y2 hold_ms");
                return;
            }
        },
        None => None,
    };
    let Some(delay) = delay(
        std::env::var("SHELLMATE_DEBUG_KEYS_DELAY_MS")
            .ok()
            .as_deref(),
    ) else {
        log::warn!("invalid SHELLMATE_DEBUG_KEYS_DELAY_MS: expected milliseconds");
        return;
    };
    let workspace = workspace.downgrade();
    // Start after scene creation and two layout frames, then allow 1.5s for the
    // window, focus and prototype's post-layout adjustments to settle.
    handle
        .update(cx, move |_, w, _| {
            w.on_next_frame(move |w, _| {
                w.on_next_frame(move |_, cx| run(handle, workspace, keys, drag, delay, cx));
                w.refresh();
            });
            w.refresh();
        })
        .ok();
}
fn run(
    handle: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
    keys: Vec<Key>,
    drag: Option<Drag>,
    delay: Duration,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(READY_DELAY).await;
        // Key-only replay starts at the composer. A drag acquires focus through
        // the real mouse-down selection path, with no composer focus override.
        if handle
            .update(cx, |_, w, cx| {
                if drag.is_some() {
                    cx.activate(true);
                    w.activate_window();
                } else {
                    workspace
                        .update(cx, |s, cx| s.debug_focus_ai_composer(w, cx))
                        .ok();
                }
                w.refresh();
            })
            .is_err()
        {
            return;
        }
        if let Some(drag) = drag {
            cx.background_executor().timer(ACTIVATION_DELAY).await;
            if handle
                .update(cx, |_, w, cx| {
                    w.dispatch_event(hover(drag.start), cx);
                    w.refresh();
                })
                .is_err()
            {
                return;
            }
            // Keyed hitboxes/hover state must be painted at the start point
            // before a click. A sleep alone does not guarantee rendered frames.
            cx.background_executor().timer(HOVER_DELAY).await;
            if !after_frames(handle, 2, cx).await {
                return;
            }
            let viewport = match handle.update(cx, |_, w, cx| {
                let viewport = workspace
                    .update(cx, |s, cx| s.debug_ai_viewport_at(drag.start, cx))
                    .ok()
                    .flatten();
                log::info!(
                    "debug drag start x={} y={} to_x={} to_y={} hold_ms={}",
                    f32::from(drag.start.x),
                    f32::from(drag.start.y),
                    f32::from(drag.end.x),
                    f32::from(drag.end.y),
                    drag.hold.as_millis()
                );
                w.dispatch_event(down(drag.start), cx);
                viewport
            }) {
                Ok(viewport) => viewport,
                Err(_) => return,
            };
            for position in drag_moves(drag) {
                cx.background_executor().timer(MOVE_DELAY).await;
                if handle
                    .update(cx, |_, w, cx| {
                        w.dispatch_event(moved(position), cx);
                    })
                    .is_err()
                {
                    return;
                }
            }
            // No extra moves during the hold: this exercises the real selection
            // auto-scroll timer and its post-paint endpoint extension.
            cx.background_executor().timer(drag.hold).await;
            if !after_frames(handle, 1, cx).await {
                return;
            }
            if handle
                .update(cx, |_, w, cx| {
                    w.dispatch_event(up(drag.end), cx);
                    log_drag_end(viewport.as_ref());
                })
                .is_err()
            {
                return;
            }
            // Keep the focus acquired by selection. Refresh native copy
            // projections before logging and before the subsequent Cmd-C.
            if !after_frames(handle, 1, cx).await {
                return;
            }
            if handle
                .update(cx, |_, w, cx| {
                    let selected = gpui::base::TextSelection::selected_text(w, cx);
                    log::info!("debug drag selection={} chars", selected.chars().count());
                })
                .is_err()
            {
                return;
            }
        } else if !after_frames(handle, 1, cx).await {
            return;
        }
        for (index, key) in keys.into_iter().enumerate() {
            if index > 0 {
                cx.background_executor().timer(delay).await;
            }
            if handle
                .update(cx, |_, w, cx| {
                    w.dispatch_keystroke(key.stroke, cx);
                    log::info!("debug key {} dispatched", key.label);
                })
                .is_err()
            {
                return;
            }
        }
    })
    .detach();
}

/// on_next_frame callbacks run before drawing. A trailing callback/defer barrier
/// therefore acknowledges `frames` fully painted frames, not just timer ticks.
async fn after_frames(handle: AnyWindowHandle, frames: usize, cx: &mut AsyncApp) -> bool {
    let (done, wait) = async_channel::bounded(1);
    if handle
        .update(cx, |_, w, _| schedule_frames(w, frames, done))
        .is_err()
    {
        return false;
    }
    wait.recv().await.is_ok()
}
fn schedule_frames(window: &mut Window, remaining: usize, done: async_channel::Sender<()>) {
    window.on_next_frame(move |window, cx| {
        if remaining == 0 {
            window.defer(cx, move |_, _| {
                let _ = done.try_send(());
            });
        } else {
            schedule_frames(window, remaining - 1, done);
        }
    });
    window.refresh();
}
fn log_drag_end(viewport: Option<&ScrollHandle>) {
    if let Some(viewport) = viewport {
        log::info!(
            "debug drag done offset_x={}",
            f32::from(viewport.offset().x)
        );
    } else {
        log::info!("debug drag done offset_x=unavailable (no AI viewport at start)");
    }
}

/// Called only by debug code-block rendering; logs once per keyed block after
/// layout has settled, without adding hitboxes or changing its dimensions.
pub(crate) fn log_code_bounds(
    block: gpui::Div,
    id: gpui::SharedString,
    w: &mut Window,
    cx: &mut App,
) -> gpui::Div {
    struct State {
        bounds: Option<gpui::Bounds<Pixels>>,
        scheduled: bool,
    }
    let state = w.use_keyed_state(
        gpui::SharedString::from(format!("{id}:debug-bounds")),
        cx,
        |_, _| State {
            bounds: None,
            scheduled: false,
        },
    );
    if !state.read(cx).scheduled {
        state.update(cx, |s, _| s.scheduled = true);
        let weak = state.downgrade();
        w.spawn(cx, async move |cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            weak.update(cx, |s, _| {
                if let Some(b) = s.bounds {
                    log::info!(
                        "ai-code-block bounds x={} y={} w={} h={}",
                        f32::from(b.origin.x),
                        f32::from(b.origin.y),
                        f32::from(b.size.width),
                        f32::from(b.size.height)
                    );
                }
            })
            .ok();
        })
        .detach();
    }
    block.on_children_prepainted(move |bounds, _, cx| {
        state.update(cx, |s, _| s.bounds = bounds.first().copied());
    })
}

#[cfg(test)]
mod tests {
    use super::{
        coordinate_drag, delay, down, drag_moves, hover, moved, parse_drag, parse_keys, up,
    };
    use gpui::{Keystroke, MouseButton, OperatingSystem, PlatformInput, point, px};
    use std::time::Duration;
    #[test]
    fn key_specs_use_gpui_parser_and_spacing_defaults() {
        let keys = parse_keys(
            "down down up tab enter escape shift-enter cmd-c",
            OperatingSystem::Mac,
        )
        .unwrap();
        assert_eq!(keys.len(), 8);
        assert_eq!(keys[0].stroke, Keystroke::parse("down").unwrap());
        assert_eq!(keys[6].stroke, Keystroke::parse("shift-enter").unwrap());
        assert_eq!(keys[7].stroke, Keystroke::parse("cmd-c").unwrap());
        assert!(parse_keys("down-up", OperatingSystem::Mac).is_err());
        assert_eq!(delay(None), Some(Duration::from_millis(400)));
        assert_eq!(delay(Some("25")), Some(Duration::from_millis(25)));
        assert!(delay(Some("-1")).is_none());
    }
    #[test]
    fn coordinate_drag_and_real_event_shapes_preserve_held_button() {
        let drag = parse_drag("1590,742 1990,742 2500").unwrap();
        assert_eq!(drag.start, point(px(1590.), px(742.)));
        assert_eq!(drag.hold, Duration::from_millis(2500));
        assert_eq!(drag_moves(drag).last(), Some(drag.end));
        assert_eq!(drag_moves(drag).count(), 8);
        assert!(
            matches!(hover(drag.start),PlatformInput::MouseMove(e) if e.pressed_button.is_none() && e.position==drag.start)
        );
        assert!(
            matches!(down(drag.start),PlatformInput::MouseDown(e) if e.button==MouseButton::Left && e.click_count==1 && !e.first_mouse)
        );
        assert!(
            matches!(moved(drag.end),PlatformInput::MouseMove(e) if e.pressed_button==Some(MouseButton::Left) && e.position==drag.end)
        );
        assert!(
            matches!(up(drag.end),PlatformInput::MouseUp(e) if e.button==MouseButton::Left && e.position==drag.end)
        );
        for invalid in [
            "1,2 3,4",
            "1,2 3,4 -1",
            "NaN,2 3,4 1",
            "1,2 inf,4 1",
            "1,2 3,4 1 extra",
        ] {
            assert!(parse_drag(invalid).is_none());
        }
        assert!(parse_drag("-1.5,2 3,4 0").is_some());
        assert!(!coordinate_drag("2:self:1"));
        assert!(coordinate_drag("1,2 3,4 100"));
    }
}
