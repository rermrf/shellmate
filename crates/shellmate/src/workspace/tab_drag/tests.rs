use super::*;

fn handle(id: u64) -> AnyWindowHandle {
    gpui::WindowHandle::<Workspace>::new(id.into()).into()
}
fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
}

#[test]
fn unequal_visible_tabs_keep_their_model_indices_across_overflow() {
    let mut strip = TabStrip {
        bounds: Some(rect(80., 4., 500., 54.)),
        ..Default::default()
    };
    assert_eq!(strip.slot(px(200.)), 0);
    assert_eq!(strip.marker(0), Some(rect(80., 13., 2., 36.)));
    strip.record_tabs(
        &[2, 5, 6],
        vec![
            rect(80., 13., 40., 36.),
            rect(126., 13., 180., 36.),
            rect(312., 13., 60., 36.),
        ],
    );
    for (x, index) in [
        (80., 2),
        (99., 2),
        (100., 5),
        (123., 5),
        (215., 5),
        (216., 6),
        (341., 6),
        (342., 7),
        (560., 7),
    ] {
        assert_eq!(strip.slot(px(x)), index);
    }
    assert_eq!(strip.marker(5), Some(rect(126., 13., 2., 36.)));
    assert_eq!(strip.marker(7), Some(rect(372., 13., 2., 36.)));
    assert_eq!(strip.marker(3), None);
    strip.record_tabs(&[], vec![]);
    assert_eq!(strip.slot(px(560.)), 0);
}

#[test]
fn frontmost_body_masks_a_strip_behind_it_in_screen_coordinates() {
    let front = handle(1);
    let back = handle(2);
    let strip = TabStrip {
        bounds: Some(rect(80., 4., 500., 54.)),
        ..Default::default()
    };
    let screen = point(px(-140.), px(240.));
    let a = (front, rect(-320., 100., 600., 480.), Some(&strip));
    let b = (back, rect(-400., 220., 600., 480.), Some(&strip));
    let hit = |stack: [_; 2]| {
        stack
            .into_iter()
            .find_map(|(h, b, s)| hit_window(h, b, s, screen))
    };
    assert_eq!(hit([a, b]), Some(Over::Body(front)));
    assert_eq!(hit([b, a]), Some(Over::Strip(back, 0)));
    assert_eq!(hit([(front, a.1, None), b]), Some(Over::Obstructed));
    assert_eq!(
        hit_window(back, b.1, Some(&strip), point(px(301.), px(240.))),
        None
    );
}

#[test]
fn reorder_accounts_for_removal_and_both_adjacent_gaps_are_noops() {
    let source = handle(1);
    let drag = ActiveDrag {
        source,
        session_id: 3,
        grab_offset: Point::default(),
        window_grab: Point::default(), frame: Bounds::default(), strip: None, mode: DragMode::Reorder,
        over: Over::Outside,
    };
    for (slot, action) in [
        (0, DropAction::Reorder(0)),
        (1, DropAction::Keep),
        (2, DropAction::Keep),
        (3, DropAction::Reorder(2)),
        (usize::MAX, DropAction::Reorder(3)),
    ] {
        assert_eq!(
            drop_action(
                drag,
                1,
                4,
                Over::Strip(source, slot),
                Point::default(),
                Size::default()
            ),
            action
        );
    }
}

#[test]
fn release_inserts_appends_or_tears_off_at_the_original_grab_offset() {
    let source = handle(1);
    let other = handle(2);
    let grab_offset = point(px(21.), px(12.));
    let screen = point(px(-230.), px(410.));
    let size = size(px(1000.), px(700.));
    let drag = ActiveDrag {
        source,
        session_id: 3,
        grab_offset,
        window_grab: grab_offset, frame: Bounds::default(), strip: None, mode: DragMode::Reorder,
        over: Over::Outside,
    };
    let release = |over, count| drop_action(drag, 0, count, over, screen, size);
    assert_eq!(
        release(Over::Strip(other, 2), 1),
        DropAction::Insert(other, 2)
    );
    assert_eq!(
        release(Over::Body(other), 1),
        DropAction::Insert(other, usize::MAX)
    );
    assert_eq!(release(Over::Body(source), 2), DropAction::Keep);
    assert_eq!(release(Over::Obstructed, 2), DropAction::Keep);
    assert_eq!(release(Over::Outside, 1), DropAction::Keep);
    assert_eq!(
        release(Over::Outside, 2),
        DropAction::Keep
    );
}

#[test]
fn drag_requires_more_than_four_points_from_the_press() {
    let down = point(px(10.), px(20.));
    assert!(within_click_distance(down, point(px(10.), px(24.))));
    assert!(within_click_distance(down, point(px(12.), px(23.))));
    assert!(!within_click_distance(down, point(px(13.), px(23.))));
}

#[test]
fn tearoff_requires_vertical_distance_and_stays_pending_until_release() {
    let strip = Some(rect(200., 100., 400., 54.));
    for p in [point(px(-1000.), px(130.)), point(px(2000.), px(130.)), point(px(300.), px(76.)), point(px(300.), px(178.))] {
        assert_eq!(DragMode::Reorder.moved(strip, p, Over::Outside), DragMode::Reorder);
    }
    for p in [point(px(300.), px(75.9)), point(px(300.), px(178.1))] {
        let mode = DragMode::Reorder.moved(strip, p, Over::Outside);
        assert_eq!(mode, DragMode::TearOffPending);
        assert_eq!(mode.moved(strip, point(px(300.), px(130.)), Over::Strip(handle(1), 0)), mode);
        assert_eq!(mode.restore_origin(), None); // source session has not detached; Esc is lossless.
    }
}

#[test]
fn single_tab_transparency_merge_and_escape_have_one_state_transition() {
    let origin = point(px(-800.), px(60.));
    let source = handle(1);
    let target = handle(2);
    let mode = DragMode::start(1, origin);
    assert_eq!(mode.excluded(source), Some(source));
    assert_eq!(DragMode::start(2, origin), DragMode::Reorder);
    let transparent = mode.moved(None, Point::default(), Over::Strip(target, 2));
    assert_eq!(transparent, DragMode::MoveWindow { origin, transparent: true });
    assert_eq!(transparent.restore_origin(), Some(origin));
    assert_eq!(transparent.moved(None, Point::default(), Over::Outside), mode);
    let drag = ActiveDrag { source, session_id: 9, grab_offset: Point::default(), window_grab: Point::default(), frame: Bounds::default(), strip: None, mode, over: Over::Outside };
    for over in [Over::Outside, Over::Body(target), Over::Obstructed, Over::Strip(source, 0)] {
        assert_eq!(drop_action(drag, 0, 1, over, Point::default(), Size::default()), DropAction::Keep);
    }
    assert_eq!(drop_action(drag, 0, 1, Over::Strip(target, 2), Point::default(), Size::default()), DropAction::Insert(target, 2));
}

#[test]
fn pending_tearoff_can_return_merge_or_create_without_touching_source_until_release() {
    let source = handle(1);
    let target = handle(2);
    let pointer = point(px(2300.), px(-200.));
    let dims = size(px(1200.), px(800.));
    let drag = ActiveDrag { source, session_id: 9, grab_offset: point(px(20.), px(10.)), window_grab: point(px(350.), px(25.)), frame: Bounds::default(), strip: None, mode: DragMode::TearOffPending, over: Over::Outside };
    for over in [Over::Outside, Over::Body(source), Over::Body(target), Over::Obstructed] {
        assert_eq!(drop_action(drag, 1, 3, over, pointer, dims), DropAction::New(rect(1950., -225., 1200., 800.)));
    }
    assert_eq!(drop_action(drag, 1, 3, Over::Strip(source, 0), pointer, dims), DropAction::Reorder(0));
    assert_eq!(drop_action(drag, 1, 3, Over::Strip(source, 2), pointer, dims), DropAction::Keep);
    assert_eq!(drop_action(drag, 1, 3, Over::Strip(target, 2), pointer, dims), DropAction::Insert(target, 2));
}
