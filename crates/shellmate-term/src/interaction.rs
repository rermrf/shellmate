//! Cell-grid observations and input ownership; no terminal process is killed on release.
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line},
    term::{Term, TermMode, cell::Flags},
    vte::ansi::Color,
};
use shellmate_core::terminal_interaction::{Action, Controller, Cursor, Region, Row, Screen, Span};
use shellmate_core::terminal_interaction::{ErrorCode, Guard, InteractionError, contains};
use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};
#[derive(Clone)]
pub(crate) struct Observation {
    pub id: String,
    pub epoch: u64,
    pub region: Region,
    pub frame: Arc<Frame>,
    pub at: Instant,
}
/// A saved physical grid. A crop never reduces the input's default full-screen guard.
#[derive(Clone)]
pub(crate) struct Frame {
    rows: u16,
    columns: u16,
    mode: TermMode,
    cursor: Cursor,
    cells: Vec<alacritty_terminal::term::cell::Cell>,
}
impl Frame {
    pub fn capture<T>(term: &Term<T>) -> Result<Self, String> {
        let rows = term.screen_lines().min(u16::MAX as usize) as u16;
        let columns = term.columns().min(u16::MAX as usize) as u16;
        if usize::from(rows) * usize::from(columns) > 160_000 {
            return Err("终端过大，请先缩小尺寸（完整基准最多 160000 单元格）".into());
        }
        let point = term.grid().cursor.point;
        Ok(Self {
            rows,
            columns,
            mode: *term.mode(),
            cursor: Cursor {
                row: point.line.0.max(0) as u16,
                column: point.column.0 as u16,
                visible: term.mode().contains(TermMode::SHOW_CURSOR),
            },
            cells: (0..rows)
                .flat_map(|r| {
                    (0..columns).map(move |c| {
                        term.grid()[Line(i32::from(r))][Column(usize::from(c))].clone()
                    })
                })
                .collect(),
        })
    }
    pub fn screen(&self, region: Option<&Region>) -> Result<Screen, String> {
        render(self, region)
    }
    fn modes_match(&self, other: &Self) -> bool {
        self.rows == other.rows
            && self.columns == other.columns
            && (self.mode & !TermMode::SHOW_CURSOR) == (other.mode & !TermMode::SHOW_CURSOR)
    }
    fn changed_rows(&self, other: &Self) -> Vec<u16> {
        if !self.modes_match(other) {
            return (0..other.rows).take(100).collect();
        }
        (0..self.rows)
            .filter(|r| {
                let start = usize::from(*r) * usize::from(self.columns);
                self.cells[start..start + usize::from(self.columns)]
                    != other.cells[start..start + usize::from(self.columns)]
            })
            .take(100)
            .collect()
    }
}
pub struct ObservationWatch {
    pub screen: Screen,
    pub baseline: Option<Screen>,
    pub updates: async_channel::Receiver<()>,
}
#[derive(Default)]
pub(crate) struct State {
    pub owner: Option<Arc<Controller>>,
    pub paused: bool,
    pub epoch: u64,
    pub observations: VecDeque<Observation>,
    watchers: Vec<async_channel::Sender<()>>,
}
impl State {
    pub fn owner(&self) -> Option<&Arc<Controller>> {
        self.owner.as_ref().filter(|o| o.active())
    }
    pub fn signal(&mut self) {
        self.watchers.retain(|tx| !tx.is_closed());
        for tx in &self.watchers {
            let _ = tx.try_send(());
        }
    }
    pub fn watch(&mut self) -> async_channel::Receiver<()> {
        let (tx, rx) = async_channel::bounded(1);
        self.watchers.retain(|t| !t.is_closed());
        self.watchers.push(tx);
        rx
    }
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.observations.clear();
        self.signal();
    }
    pub fn takeover(&mut self) {
        if self.owner().is_some() {
            self.paused = true;
        }
        self.invalidate();
    }
    pub fn resume(&mut self) {
        self.paused = false;
        self.invalidate();
    }
    pub fn record(&mut self, screen: &mut Screen, frame: Arc<Frame>) {
        screen.screen_id = shellmate_core::HostId::generate().to_string();
        self.observations.push_back(Observation {
            id: screen.screen_id.clone(),
            epoch: self.epoch,
            region: screen.region.clone(),
            frame,
            at: Instant::now(),
        });
        while self.observations.len() > 32 {
            self.observations.pop_front();
        }
    }
    pub fn observation(&self, id: &str) -> Result<&Observation, InteractionError> {
        self.observations
            .iter()
            .find(|o| {
                o.id == id && o.epoch == self.epoch && o.at.elapsed() <= Duration::from_secs(120)
            })
            .ok_or_else(|| {
                InteractionError::new(ErrorCode::InvalidReference, "画面引用已失效，请重新读取")
            })
    }
    pub fn legacy(
        &self,
        fingerprint: &str,
        region: Option<&Region>,
    ) -> Result<&Observation, InteractionError> {
        self.observations
            .iter()
            .rev()
            .find(|o| {
                o.epoch == self.epoch
                    && o.at.elapsed() <= Duration::from_secs(120)
                    && region.is_none_or(|r| *r == o.region)
                    && o.frame
                        .screen(Some(&o.region))
                        .is_ok_and(|s| s.fingerprint == fingerprint)
            })
            .ok_or_else(|| {
                InteractionError::new(
                    ErrorCode::InvalidReference,
                    "指纹没有同区域的有效基准，请使用 screen_id",
                )
            })
    }
    pub fn validate(
        &self,
        id: &str,
        owner: &Controller,
        current: &Frame,
        guards: &[Guard],
    ) -> Result<(), InteractionError> {
        if !owner.active() {
            return Err(InteractionError::new(
                ErrorCode::Stopped,
                "任务已停止，输入未发送",
            ));
        }
        if self.owner().is_some_and(|o| o.id == owner.id) && self.paused {
            return Err(InteractionError::new(
                ErrorCode::UserTakeover,
                "用户已接管，请等待用户允许继续",
            ));
        }
        if self.owner().is_some_and(|o| o.id != owner.id) {
            return Err(InteractionError::new(
                ErrorCode::ControllerBusy,
                "另一个 Agent 正在操作此终端",
            ));
        }
        let o = self.observation(id)?;
        let changed = || {
            let mut e = InteractionError::new(
                ErrorCode::ScreenChanged,
                "画面或前置条件已变化，本次未发送。根据当前画面调整操作；动态程序可显式指定与动作相关的 guards，不能仅用标题或空白规避校验。",
            );
            e.changed_rows = o.frame.changed_rows(current);
            e
        };
        if !o.frame.modes_match(current) {
            return Err(changed());
        }
        if guards.is_empty() {
            if o.frame.cells != current.cells || o.frame.cursor != current.cursor {
                return Err(changed());
            }
        } else {
            if guards.len() > 4 {
                return Err(InteractionError::new(
                    ErrorCode::InvalidArguments,
                    "最多指定 4 个前置条件",
                ));
            }
            for guard in guards {
                guard
                    .validate()
                    .map_err(|e| InteractionError::new(ErrorCode::InvalidArguments, e))?;
                let r = &guard.region;
                if !contains(&o.region, r.row, r.column)
                    || u32::from(r.row) + u32::from(r.rows)
                        > u32::from(o.region.row) + u32::from(o.region.rows)
                    || u32::from(r.column) + u32::from(r.columns)
                        > u32::from(o.region.column) + u32::from(o.region.columns)
                {
                    return Err(InteractionError::new(
                        ErrorCode::InvalidArguments,
                        "前置条件必须位于已读取的区域内",
                    ));
                }
                let before = o
                    .frame
                    .screen(Some(r))
                    .map_err(|e| InteractionError::new(ErrorCode::InvalidArguments, e))?;
                let now = current
                    .screen(Some(r))
                    .map_err(|e| InteractionError::new(ErrorCode::InvalidArguments, e))?;
                if !guard.matches(&before)
                    || !guard.matches(&now)
                    || (guard.exact() && before.fingerprint != now.fingerprint)
                {
                    return Err(changed());
                }
            }
        }
        Ok(())
    }
}
fn color(c: Color) -> String {
    match c {
        Color::Named(c) => format!("{c:?}"),
        Color::Indexed(i) => format!("index:{i}"),
        Color::Spec(c) => format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b),
    }
}
#[cfg(test)]
pub(crate) fn snapshot<T>(term: &Term<T>, region: Option<&Region>) -> Result<Screen, String> {
    Frame::capture(term)?.screen(region)
}
fn render(frame: &Frame, region: Option<&Region>) -> Result<Screen, String> {
    let rows = frame.rows;
    let columns = frame.columns;
    let region = region.cloned().unwrap_or(Region {
        row: 0,
        column: 0,
        rows,
        columns,
    });
    if region.rows == 0
        || region.columns == 0
        || u32::from(region.row) + u32::from(region.rows) > u32::from(rows)
        || u32::from(region.column) + u32::from(region.columns) > u32::from(columns)
    {
        return Err("region 超出当前屏幕，行列从 0 开始".into());
    }
    if u64::from(region.rows) * u64::from(region.columns) > 40_000 {
        return Err("画面过大，请指定较小的 region（最多 40000 个单元格）".into());
    }
    let mut lines = vec![];
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let mode = frame.mode;
    (
        rows,
        columns,
        (mode & !TermMode::SHOW_CURSOR).bits(),
        region.row,
        region.column,
        region.rows,
        region.columns,
    )
        .hash(&mut hash);
    for r in region.row..region.row + region.rows {
        let mut text = String::new();
        let mut styles: Vec<Span> = vec![];
        for c in region.column..region.column + region.columns {
            let cell = &frame.cells[usize::from(r) * usize::from(columns) + usize::from(c)];
            cell.c.hash(&mut hash);
            cell.flags.bits().hash(&mut hash);
            format!("{:?}{:?}{:?}", cell.fg, cell.bg, cell.zerowidth()).hash(&mut hash);
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let hidden = cell.flags.contains(Flags::HIDDEN);
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            if hidden {
                text.push_str(&" ".repeat(width));
            } else {
                text.push(cell.c);
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra);
                }
            }
            let span = Span {
                column: c,
                columns: width as u16,
                foreground: color(cell.fg),
                background: color(cell.bg),
                inverse: cell.flags.contains(Flags::INVERSE),
                bold: cell.flags.contains(Flags::BOLD),
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            };
            if let Some(old) = styles.last_mut()
                && old.column + old.columns == span.column
                && old.foreground == span.foreground
                && old.background == span.background
                && old.inverse == span.inverse
                && old.bold == span.bold
                && old.underline == span.underline
            {
                old.columns += span.columns;
            } else {
                styles.push(span);
            }
        }
        lines.push(Row {
            row: r,
            text: text.trim_end().into(),
            styles,
        });
    }
    let cursor = frame.cursor.clone();
    // Cursor can blink/move during continuous refresh; include it only inside the observed region.
    if cursor.row >= region.row
        && cursor.row < region.row + region.rows
        && cursor.column >= region.column
        && cursor.column < region.column + region.columns
    {
        (cursor.row, cursor.column, cursor.visible).hash(&mut hash);
    }
    Ok(Screen {
        ended: false,
        screen_id: String::new(),
        fingerprint: format!("{:016x}", hash.finish()),
        rows,
        columns,
        cursor,
        alternate_screen: mode.contains(TermMode::ALT_SCREEN),
        application_cursor: mode.contains(TermMode::APP_CURSOR),
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        mouse_reporting: mode.intersects(TermMode::MOUSE_MODE),
        text: lines
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        region,
        lines,
        truncated: false,
        controller: None,
        paused: false,
    })
}
pub(crate) fn encode(action: &Action, mode: &TermMode) -> Result<Vec<u8>, String> {
    match action {
        Action::Click {
            row,
            column,
            button,
        } => {
            use crate::view::mouse::ButtonEvent::*;
            let (down, up) = match button {
                shellmate_core::terminal_interaction::MouseButton::Left => (LeftDown, LeftUp),
                shellmate_core::terminal_interaction::MouseButton::Middle => (MiddleDown, MiddleUp),
                shellmate_core::terminal_interaction::MouseButton::Right => (RightDown, RightUp),
            };
            let mut bytes = pointer(mode, down, *row, *column)?;
            bytes.extend(pointer(mode, up, *row, *column)?);
            Ok(bytes)
        }
        Action::Scroll { row, column, lines } => {
            use crate::view::mouse::ButtonEvent::*;
            Ok(pointer(
                mode,
                if *lines > 0 { WheelDown } else { WheelUp },
                *row,
                *column,
            )?
            .repeat(usize::from(lines.unsigned_abs())))
        }
        Action::Drag {
            row,
            column,
            to_row,
            to_column,
        } => {
            use crate::view::mouse::ButtonEvent::*;
            let steps = row
                .abs_diff(*to_row)
                .max(column.abs_diff(*to_column))
                .max(1);
            let mut bytes = pointer(mode, LeftDown, *row, *column)?;
            for n in 1..=steps {
                let at = |a: u16, b: u16| {
                    (i32::from(a) + (i32::from(b) - i32::from(a)) * i32::from(n) / i32::from(steps))
                        as u16
                };
                bytes.extend(pointer(
                    mode,
                    LeftDrag,
                    at(*row, *to_row),
                    at(*column, *to_column),
                )?);
            }
            bytes.extend(pointer(mode, LeftUp, *to_row, *to_column)?);
            Ok(bytes)
        }
        Action::Text { text } => Ok(text.as_bytes().to_vec()),
        Action::Paste { text } => {
            if text.contains('\n') && !mode.contains(TermMode::BRACKETED_PASTE) {
                return Err(
                    "程序未启用括号粘贴，请拆分文字与 enter，避免多行被当作命令执行".into(),
                );
            }
            Ok(crate::keys::paste_payload(text, mode).into_bytes())
        }
        Action::Key { key, repeat } => {
            if shellmate_core::terminal_interaction::printable_key(key) {
                return Ok(key.as_bytes().repeat(usize::from(*repeat)));
            }
            if key == "space" {
                return Ok(vec![b' '; usize::from(*repeat)]);
            }
            let mut parts = key.split('-').collect::<Vec<_>>();
            let key = parts.pop().ok_or("按键无效")?;
            let stroke = gpui::Keystroke {
                key: key.into(),
                key_char: None,
                modifiers: gpui::Modifiers {
                    control: parts.contains(&"ctrl"),
                    alt: parts.contains(&"alt"),
                    shift: parts.contains(&"shift"),
                    ..Default::default()
                },
            };
            if let Some(s) = crate::keys::to_esc_str(&stroke, mode) {
                return Ok(s.as_bytes().repeat(usize::from(*repeat)));
            }
            if !stroke.modifiers.control && key.len() == 1 {
                let character = if stroke.modifiers.shift {
                    key.to_uppercase()
                } else {
                    key.into()
                };
                return Ok(format!(
                    "{}{character}",
                    if stroke.modifiers.alt { "\x1b" } else { "" }
                )
                .as_bytes()
                .repeat(usize::from(*repeat)));
            }
            Err("该按键属于应用快捷键，不能发送给终端".into())
        }
    }
}
fn pointer(
    mode: &TermMode,
    event: crate::view::mouse::ButtonEvent,
    row: u16,
    column: u16,
) -> Result<Vec<u8>, String> {
    crate::view::mouse::report(
        *mode,
        event,
        gpui::Modifiers::default(),
        alacritty_terminal::index::Point::new(usize::from(row), Column(usize::from(column))),
    )
    .ok_or_else(|| "程序未启用所需鼠标模式，或坐标超出协议范围；请使用键盘操作".into())
}
pub(crate) fn release_pointer(action: &Action, mode: &TermMode) -> Option<Vec<u8>> {
    use crate::view::mouse::ButtonEvent::*;
    use shellmate_core::terminal_interaction::MouseButton;
    let (event, row, column) = match action {
        Action::Click {
            row,
            column,
            button,
        } => (
            match button {
                MouseButton::Left => LeftUp,
                MouseButton::Middle => MiddleUp,
                MouseButton::Right => RightUp,
            },
            *row,
            *column,
        ),
        Action::Drag {
            to_row, to_column, ..
        } => (LeftUp, *to_row, *to_column),
        _ => return None,
    };
    pointer(mode, event, row, column).ok()
}
pub(crate) fn validate_pointer(actions: &[Action], region: &Region) -> Result<(), String> {
    let contains = |r: u16, c: u16| {
        r >= region.row
            && u32::from(r) < u32::from(region.row) + u32::from(region.rows)
            && c >= region.column
            && u32::from(c) < u32::from(region.column) + u32::from(region.columns)
    };
    for a in actions {
        let valid = match a {
            Action::Click { row, column, .. } | Action::Scroll { row, column, .. } => {
                contains(*row, *column)
            }
            Action::Drag {
                row,
                column,
                to_row,
                to_column,
            } => contains(*row, *column) && contains(*to_row, *to_column),
            _ => true,
        };
        if !valid {
            return Err("鼠标动作必须位于刚读取的画面区域内".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::{
        event::{Event, EventListener},
        term::Config,
        vte::ansi::Processor,
    };
    #[derive(Clone)]
    struct Listener;
    impl EventListener for Listener {
        fn send_event(&self, _: Event) {}
    }
    fn grid() -> (Term<Listener>, Processor) {
        (
            Term::new(
                Config::default(),
                &crate::TerminalSize {
                    cols: 20,
                    rows: 6,
                    ..Default::default()
                },
                Listener,
            ),
            Processor::new(),
        )
    }
    #[test]
    fn snapshots_preserve_rows_styles_wide_characters_and_live_screen_when_scrolled() {
        let (mut term, mut p) = grid();
        p.advance(
            &mut term,
            "old\r\nrows\r\n\x1b[?1049h\x1b[H\x1b[7m选择一\x1b[0m\r\n选择二\x1b[4;3H".as_bytes(),
        );
        let screen = snapshot(&term, None).unwrap();
        assert!(screen.alternate_screen);
        assert_eq!(screen.lines[0].text, "选择一");
        assert!(screen.lines[0].styles[0].inverse);
        assert_eq!(screen.lines[0].styles[0].columns, 6);
        assert_eq!((screen.cursor.row, screen.cursor.column), (3, 2));
        assert!(!screen.text.contains("old"));
        p.advance(&mut term, b"\x1b[?1049l\x1b[H\x1b[2J");
        for _ in 0..12 {
            p.advance(&mut term, b"older\r\n");
        }
        p.advance(&mut term, b"current");
        term.scroll_display(alacritty_terminal::grid::Scroll::Top);
        assert!(snapshot(&term, None).unwrap().text.contains("current"));
    }
    #[test]
    fn cropped_reads_keep_full_guard_and_explicit_conditions_tolerate_animation() {
        let (mut term, mut p) = grid();
        p.advance(&mut term, b"Music[2;1HSearch > Song[4;1HClock 1[6;1H");
        let title = Region {
            row: 0,
            column: 0,
            rows: 1,
            columns: 20,
        };
        let mut state = State::default();
        let owner = Arc::new(Controller::new("Agent".into()));
        let base = Arc::new(Frame::capture(&term).unwrap());
        let mut screen = base.screen(Some(&title)).unwrap();
        state.record(&mut screen, base.clone());
        p.advance(&mut term, b"[4;1HClock 2[6;1H");
        let current = Frame::capture(&term).unwrap();
        assert_eq!(
            state
                .validate(&screen.screen_id, &owner, &current, &[])
                .unwrap_err()
                .code,
            ErrorCode::ScreenChanged
        );
        let mut full = base.screen(None).unwrap();
        state.record(&mut full, base);
        let guard = Guard {
            region: Region {
                row: 1,
                column: 0,
                rows: 1,
                columns: 20,
            },
            text: Some("Search > Song".into()),
            absent_text: None,
            cursor: None,
        };
        assert!(
            state
                .validate(
                    &full.screen_id,
                    &owner,
                    &current,
                    std::slice::from_ref(&guard)
                )
                .is_ok()
        );
        p.advance(&mut term, b"\x1b[?25l");
        let hidden_cursor = Frame::capture(&term).unwrap();
        assert!(
            state
                .validate(
                    &full.screen_id,
                    &owner,
                    &hidden_cursor,
                    std::slice::from_ref(&guard)
                )
                .is_ok()
        );
        p.advance(&mut term, b"[2;1HDelete file?[K[6;1H");
        let changed = Frame::capture(&term).unwrap();
        assert_eq!(
            state
                .validate(
                    &full.screen_id,
                    &owner,
                    &changed,
                    std::slice::from_ref(&guard)
                )
                .unwrap_err()
                .code,
            ErrorCode::ScreenChanged
        );
        state.owner = Some(owner.clone());
        state.takeover();
        let frame = Arc::new(Frame::capture(&term).unwrap());
        let mut fresh = frame.screen(None).unwrap();
        state.record(&mut fresh, frame.clone());
        assert_eq!(
            state
                .validate(&fresh.screen_id, &owner, &frame, &[])
                .unwrap_err()
                .code,
            ErrorCode::UserTakeover
        );
        state.resume();
        state.record(&mut fresh, frame.clone());
        assert!(
            state
                .validate(&fresh.screen_id, &owner, &frame, &[])
                .is_ok()
        );
        let other = Controller::new("Other".into());
        assert_eq!(
            state
                .validate(&fresh.screen_id, &other, &frame, &[])
                .unwrap_err()
                .code,
            ErrorCode::ControllerBusy
        );
        let mut resized = (*frame).clone();
        resized.columns += 1;
        assert_eq!(
            state
                .validate(&fresh.screen_id, &owner, &resized, &[])
                .unwrap_err()
                .code,
            ErrorCode::ScreenChanged
        );
        state.observations.back_mut().unwrap().at = Instant::now() - Duration::from_secs(121);
        assert_eq!(
            state
                .validate(&fresh.screen_id, &owner, &frame, &[])
                .unwrap_err()
                .code,
            ErrorCode::InvalidReference
        );
    }
    #[test]
    fn cropped_fingerprints_are_scoped_and_baseline_survives_cache_eviction() {
        let (term, _) = grid();
        let frame = Arc::new(Frame::capture(&term).unwrap());
        let mut full = frame.screen(None).unwrap();
        let mut state = State::default();
        state.record(&mut full, frame.clone());
        let region = Region {
            row: 0,
            column: 0,
            rows: 1,
            columns: 20,
        };
        assert!(state.legacy(&full.fingerprint, Some(&region)).is_err());
        let pinned = state.observation(&full.screen_id).unwrap().clone();
        let expected = pinned.frame.screen(Some(&region)).unwrap();
        for _ in 0..40 {
            let mut s = frame.screen(None).unwrap();
            state.record(&mut s, frame.clone());
        }
        assert!(state.observation(&full.screen_id).is_err());
        assert_eq!(expected, pinned.frame.screen(Some(&region)).unwrap());
        assert_ne!(expected.fingerprint, full.fingerprint);
        assert_eq!(
            encode(
                &Action::Key {
                    key: "6".into(),
                    repeat: 1
                },
                &TermMode::empty()
            )
            .unwrap(),
            b"6"
        );
        assert_eq!(
            encode(
                &Action::Key {
                    key: "?".into(),
                    repeat: 1
                },
                &TermMode::empty()
            )
            .unwrap(),
            b"?"
        );
    }
    #[test]
    fn terminal_keys_and_paste_follow_application_modes() {
        let down = Action::Key {
            key: "down".into(),
            repeat: 2,
        };
        assert_eq!(encode(&down, &TermMode::empty()).unwrap(), b"\x1b[B\x1b[B");
        assert_eq!(
            encode(&down, &TermMode::APP_CURSOR).unwrap(),
            b"\x1bOB\x1bOB"
        );
        assert_eq!(
            encode(
                &Action::Key {
                    key: "ctrl-c".into(),
                    repeat: 1
                },
                &TermMode::empty()
            )
            .unwrap(),
            b"\x03"
        );
        assert_eq!(
            encode(
                &Action::Text { text: ":wq".into() },
                &TermMode::BRACKETED_PASTE
            )
            .unwrap(),
            b":wq"
        );
        let paste = Action::Paste {
            text: "hello\nworld".into(),
        };
        assert!(encode(&paste, &TermMode::empty()).is_err());
        assert_eq!(
            encode(&paste, &TermMode::BRACKETED_PASTE).unwrap(),
            b"\x1b[200~hello\nworld\x1b[201~"
        );
    }
}

#[cfg(test)]
mod mouse_tests {
    use super::*;
    use shellmate_core::terminal_interaction::MouseButton;
    #[test]
    fn mouse_gestures_are_balanced_mode_aware_and_bounded_by_observed_region() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let click = Action::Click {
            row: 2,
            column: 3,
            button: MouseButton::Right,
        };
        assert_eq!(encode(&click, &mode).unwrap(), b"\x1b[<2;4;3M\x1b[<2;4;3m");
        assert!(encode(&click, &TermMode::SGR_MOUSE).is_err());
        assert_eq!(
            encode(
                &Action::Scroll {
                    row: 2,
                    column: 3,
                    lines: -2
                },
                &mode
            )
            .unwrap(),
            b"\x1b[<64;4;3M\x1b[<64;4;3M"
        );
        let drag = Action::Drag {
            row: 2,
            column: 3,
            to_row: 2,
            to_column: 5,
        };
        assert!(encode(&drag, &mode).is_err());
        assert_eq!(
            encode(&drag, &(mode | TermMode::MOUSE_DRAG)).unwrap(),
            b"\x1b[<0;4;3M\x1b[<32;5;3M\x1b[<32;6;3M\x1b[<0;6;3m"
        );
        let region = Region {
            row: 2,
            column: 3,
            rows: 1,
            columns: 3,
        };
        assert!(validate_pointer(&[click.clone(), drag], &region).is_ok());
        assert!(
            validate_pointer(
                &[click],
                &Region {
                    column: 4,
                    ..region
                }
            )
            .is_err()
        );
    }
}
