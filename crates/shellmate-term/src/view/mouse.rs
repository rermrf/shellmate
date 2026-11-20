//! Xterm pointer reports, viewport coordinates, and wheel routing.
use super::Geometry;
use alacritty_terminal::{
    index::{Column, Point as GridPoint, Side},
    term::TermMode,
};
use gpui::{Modifiers, Pixels, Point};

impl Geometry {
    pub(super) fn viewport_cell(
        &self,
        position: Point<Pixels>,
        rows: usize,
    ) -> (GridPoint<usize>, Side) {
        let x = f32::from(position.x - self.origin.x).max(0.) / f32::from(self.size.cell_width);
        let y = f32::from(position.y - self.origin.y).max(0.) / f32::from(self.size.line_height);
        let column = (x as usize).min(usize::from(self.size.cols) - 1);
        let row = (y as usize).min(rows - 1);
        let side = if x.fract() > 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        (GridPoint::new(row, Column(column)), side)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Action {
    History,
    Arrow(&'static [u8]),
    Report(Vec<u8>),
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ButtonEvent {
    LeftDown,
    LeftUp,
    LeftDrag,
    WheelUp,
    WheelDown,
}

pub(super) fn report(
    mode: TermMode,
    event: ButtonEvent,
    modifiers: Modifiers,
    cell: GridPoint<usize>,
) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    if event == ButtonEvent::LeftDrag
        && !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
    {
        return None;
    }
    let sgr = mode.contains(TermMode::SGR_MOUSE);
    let button = match event {
        ButtonEvent::LeftUp if !sgr => 3,
        ButtonEvent::LeftDown | ButtonEvent::LeftUp => 0,
        ButtonEvent::LeftDrag => 32,
        ButtonEvent::WheelUp => 64,
        ButtonEvent::WheelDown => 65,
    } + 4 * u8::from(modifiers.shift)
        + 8 * u8::from(modifiers.alt)
        + 16 * u8::from(modifiers.control);
    let (x, y) = (cell.column.0 + 1, cell.line + 1);
    if sgr {
        let suffix = if event == ButtonEvent::LeftUp {
            'm'
        } else {
            'M'
        };
        return Some(format!("\x1b[<{button};{x};{y}{suffix}").into_bytes());
    }
    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    let limit = if utf8 { 2015 } else { 223 };
    if x > limit || y > limit {
        return None;
    }
    let mut bytes = vec![0x1b, b'[', b'M', button + 32];
    for coord in [x, y] {
        let value = coord + 32;
        if utf8 && value >= 128 {
            bytes.push(0xc0 | (value >> 6) as u8);
            bytes.push(0x80 | (value & 0x3f) as u8);
        } else {
            bytes.push(value as u8);
        }
    }
    Some(bytes)
}

pub(super) fn wheel_action(
    mode: TermMode,
    modifiers: Modifiers,
    lines: i32,
    geometry: Geometry,
    position: Point<Pixels>,
) -> Action {
    if modifiers.shift {
        return Action::History;
    }
    if mode.intersects(TermMode::MOUSE_MODE) {
        let (cell, _) = geometry.viewport_cell(position, usize::from(geometry.size.rows));
        let event = if lines > 0 {
            ButtonEvent::WheelUp
        } else {
            ButtonEvent::WheelDown
        };
        return report(mode, event, modifiers, cell).map_or(Action::Ignore, Action::Report);
    }
    if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        return Action::Arrow(match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        });
    }
    Action::History
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalSize;
    use gpui::{point, px};

    fn geometry() -> Geometry {
        Geometry {
            origin: point(px(30.), px(40.)),
            size: TerminalSize {
                cols: 2020,
                ..Default::default()
            },
        }
    }

    #[test]
    fn button_reports_pair_press_and_release_and_respect_drag_tracking() {
        let cell = GridPoint::new(5, Column(9));
        let bytes = |mode, event| report(mode, event, Modifiers::default(), cell);
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            bytes(sgr, ButtonEvent::LeftDown).as_deref(),
            Some(&b"\x1b[<0;10;6M"[..])
        );
        assert_eq!(
            bytes(sgr, ButtonEvent::LeftUp).as_deref(),
            Some(&b"\x1b[<0;10;6m"[..])
        );
        assert_eq!(bytes(sgr, ButtonEvent::LeftDrag), None);
        assert_eq!(
            bytes(sgr | TermMode::MOUSE_DRAG, ButtonEvent::LeftDrag).as_deref(),
            Some(&b"\x1b[<32;10;6M"[..])
        );
        assert_eq!(
            bytes(TermMode::MOUSE_REPORT_CLICK, ButtonEvent::LeftDown).as_deref(),
            Some(&b"\x1b[M *&"[..])
        );
        assert_eq!(
            bytes(TermMode::MOUSE_REPORT_CLICK, ButtonEvent::LeftUp).as_deref(),
            Some(&b"\x1b[M#*&"[..])
        );
        assert_eq!(bytes(TermMode::SGR_MOUSE, ButtonEvent::LeftDown), None);
    }

    #[test]
    fn every_tracking_mode_gets_sgr_wheel_presses_on_either_screen() {
        let g = geometry();
        let pos = point(px(48.), px(63.));
        for tracking in [
            TermMode::MOUSE_REPORT_CLICK,
            TermMode::MOUSE_DRAG,
            TermMode::MOUSE_MOTION,
        ] {
            for screen in [
                TermMode::empty(),
                TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL,
            ] {
                let mode = screen | tracking | TermMode::SGR_MOUSE;
                assert_eq!(
                    wheel_action(mode, Modifiers::default(), 1, g, pos),
                    Action::Report(b"\x1b[<64;3;2M".to_vec())
                );
                let mods = Modifiers {
                    alt: true,
                    control: true,
                    ..Default::default()
                };
                assert_eq!(
                    wheel_action(mode, mods, -1, g, pos),
                    Action::Report(b"\x1b[<89;3;2M".to_vec())
                );
                let shift = Modifiers {
                    shift: true,
                    ..Default::default()
                };
                assert_eq!(wheel_action(mode, shift, 1, g, pos), Action::History);
            }
        }
    }

    #[test]
    fn classic_and_utf8_reports_respect_the_coordinate_range() {
        let g = geometry();
        let report = |extra, column: usize| {
            wheel_action(
                TermMode::MOUSE_REPORT_CLICK | extra,
                Modifiers::default(),
                -1,
                g,
                point(g.origin.x + px(8. * (column - 1) as f32), g.origin.y),
            )
        };
        assert_eq!(
            report(TermMode::empty(), 223),
            Action::Report(b"\x1b[Ma\xff!".to_vec())
        );
        assert_eq!(report(TermMode::empty(), 224), Action::Ignore);
        assert_eq!(
            report(TermMode::UTF8_MOUSE, 95),
            Action::Report(b"\x1b[Ma\x7f!".to_vec())
        );
        assert_eq!(
            report(TermMode::UTF8_MOUSE, 96),
            Action::Report(b"\x1b[Ma\xc2\x80!".to_vec())
        );
        assert_eq!(
            report(TermMode::UTF8_MOUSE, 2015),
            Action::Report(b"\x1b[Ma\xdf\xbf!".to_vec())
        );
        assert_eq!(report(TermMode::UTF8_MOUSE, 2016), Action::Ignore);
    }

    #[test]
    fn an_encoding_alone_does_not_enable_mouse_tracking() {
        let fallback = |mode, lines| {
            wheel_action(
                mode | TermMode::SGR_MOUSE,
                Modifiers::default(),
                lines,
                geometry(),
                Point::default(),
            )
        };
        let alt = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(fallback(TermMode::ALTERNATE_SCROLL, 1), Action::History);
        assert_eq!(fallback(TermMode::ALT_SCREEN, -1), Action::History);
        assert_eq!(fallback(alt, 1), Action::Arrow(b"\x1b[A"));
        assert_eq!(
            fallback(alt | TermMode::APP_CURSOR, -1),
            Action::Arrow(b"\x1bOB")
        );
    }
}
