//! Stable, one-based window/tab numbers for exercising the actual drop handler.
use super::*;
use std::time::Duration;

#[derive(Debug, PartialEq)]
enum Target {
    Here(usize),
    Window(usize, usize),
    Outside(Point<Pixels>),
}

fn ordinal(s: &str) -> Option<usize> {
    s.parse::<usize>().ok()?.checked_sub(1)
}

fn parse(spec: &str) -> Option<(usize, Target)> {
    let (session, target) = spec.split_once(':')?;
    let (kind, args) = target.split_once(':')?;
    let target = match kind {
        "self" => Target::Here(ordinal(args)?),
        "window" => {
            let (window, slot) = args.split_once(':')?;
            Target::Window(ordinal(window)? + 1, ordinal(slot)?)
        }
        "outside" => {
            let (x, y) = args.split_once(',')?;
            let x: f32 = x.parse().ok()?;
            let y: f32 = y.parse().ok()?;
            if !x.is_finite() || !y.is_finite() {
                return None;
            }
            Target::Outside(point(px(x), px(y)))
        }
        _ => return None,
    };
    Some((ordinal(session)?, target))
}

pub(in crate::workspace) fn install(cx: &mut App) {
    let Ok(spec) = std::env::var("SHELLMATE_DEBUG_DRAG") else {
        return;
    };
    let Some((index, target)) = parse(&spec) else {
        log::warn!("invalid SHELLMATE_DEBUG_DRAG: {spec}");
        return;
    };
    let Some(source) = Hub::windows(cx)
        .iter()
        .min_by_key(|entry| entry.number)
        .cloned()
    else {
        return;
    };
    let timer = cx.background_executor().timer(Duration::from_secs(2));
    cx.spawn(async move |cx| {
        timer.await;
        cx.update(move |cx| {
            let found = source
                .update(cx, |ws, window, cx| {
                    let session_id = ws.sessions.get(index)?.id;
                    let mut screen = Point::default();
                    let over = match target {
                        Target::Here(slot) => Over::Strip(source.handle, slot),
                        Target::Window(n, slot) => Over::Strip(
                            Hub::windows(cx).iter().find(|e| e.number == n)?.handle,
                            slot,
                        ),
                        Target::Outside(p) => {
                            screen = p;
                            Over::Outside
                        }
                    };
                    let drag = ActiveDrag {
                        source: source.handle,
                        session_id,
                        grab_offset: Point::default(),
                        over,
                    };
                    finish(drag, over, screen, window.bounds().size, cx);
                    Some(())
                })
                .flatten()
                .is_some();
            if !found {
                log::warn!("SHELLMATE_DEBUG_DRAG source tab or target window unavailable: {spec}");
            }
            cx.defer(|cx| {
                for entry in Hub::windows(cx) {
                    if let Some(ws) = entry.workspace.upgrade() {
                        let ids: Vec<_> = ws.read(cx).sessions.iter().map(|s| s.id).collect();
                        log::info!(
                            "SHELLMATE_DEBUG_DRAG window {} sessions {ids:?}",
                            entry.number
                        );
                    }
                }
            });
        });
    })
    .detach();
}

#[cfg(test)]
#[test]
fn commands_are_one_based_and_coordinates_can_be_negative() {
    assert_eq!(parse("2:self:1"), Some((1, Target::Here(0))));
    assert_eq!(parse("1:window:3:2"), Some((0, Target::Window(3, 1))));
    assert_eq!(
        parse("3:outside:-200,40.5"),
        Some((2, Target::Outside(point(px(-200.), px(40.5)))))
    );
    for invalid in [
        "0:self:1",
        "2:self:0",
        "1:window:0:2",
        "1:outside:NaN,2",
        "1:self:1:2",
    ] {
        assert!(parse(invalid).is_none());
    }
}
