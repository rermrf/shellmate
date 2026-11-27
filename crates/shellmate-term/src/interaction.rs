//! Cell-grid observations and input ownership; no terminal process is killed on release.
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line},
    term::{Term, TermMode, cell::Flags},
    vte::ansi::Color,
};
use shellmate_core::terminal_interaction::{Action, Controller, Cursor, Region, Row, Screen, Span};
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
    pub alternate: bool,
    pub rows: u16,
    pub columns: u16,
    pub fingerprint: String,
    pub region: Region,
    pub at: Instant,
}
#[derive(Default)]
pub(crate) struct State {
    pub owner: Option<Arc<Controller>>,
    pub paused: bool,
    pub epoch: u64,
    pub observations: VecDeque<Observation>,
}
impl State {
    pub fn owner(&self) -> Option<&Arc<Controller>> {
        self.owner.as_ref().filter(|o| o.active())
    }
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.observations.clear();
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
    pub fn record(&mut self, screen: &mut Screen) {
        screen.screen_id = shellmate_core::HostId::generate().to_string();
        self.observations.push_back(Observation {
            id: screen.screen_id.clone(),
            epoch: self.epoch,
            alternate: screen.alternate_screen,
            rows: screen.rows,
            columns: screen.columns,
            fingerprint: screen.fingerprint.clone(),
            region: screen.region.clone(),
            at: Instant::now(),
        });
        while self.observations.len() > 32 {
            self.observations.pop_front();
        }
    }
    pub fn validate(&self, id: &str, owner: &Controller, screen: &Screen) -> Result<(), String> {
        if !owner.active() {
            return Err("任务已停止，终端输入未发送".into());
        }
        if self.owner().is_some_and(|o| o.id == owner.id) && self.paused {
            return Err("用户已接管，请等待用户在终端上允许继续".into());
        }
        if self.owner().is_some_and(|o| o.id != owner.id) {
            return Err("另一个 Agent 正在操作此终端，请等待它交还控制".into());
        }
        let o = self
            .observations
            .iter()
            .find(|o| o.id == id)
            .ok_or("画面引用已失效，请重新读取当前画面")?;
        if o.at.elapsed() > Duration::from_secs(120)
            || o.epoch != self.epoch
            || o.alternate != screen.alternate_screen
            || o.rows != screen.rows
            || o.columns != screen.columns
        {
            return Err("终端状态或尺寸已变化，请重新读取画面".into());
        }
        if o.fingerprint != screen.fingerprint {
            return Err(
                "观察区域已变化，本次未发送输入；重新读屏，持续刷新时可选不含动态内容的 region"
                    .into(),
            );
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
pub(crate) fn snapshot<T>(term: &Term<T>, region: Option<&Region>) -> Result<Screen, String> {
    let rows = term.screen_lines().min(u16::MAX as usize) as u16;
    let columns = term.columns().min(u16::MAX as usize) as u16;
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
    let mode = *term.mode();
    (rows, columns, mode.bits()).hash(&mut hash);
    for r in region.row..region.row + region.rows {
        let mut text = String::new();
        let mut styles: Vec<Span> = vec![];
        for c in region.column..region.column + region.columns {
            let cell = &term.grid()[Line(i32::from(r))][Column(usize::from(c))];
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
    let point = term.grid().cursor.point;
    let cursor = Cursor {
        row: point.line.0.max(0) as u16,
        column: point.column.0 as u16,
        visible: mode.contains(TermMode::SHOW_CURSOR),
    };
    // Cursor can blink/move during continuous refresh; include it only inside the observed region.
    if cursor.row >= region.row
        && cursor.row < region.row + region.rows
        && cursor.column >= region.column
        && cursor.column < region.column + region.columns
    {
        (cursor.row, cursor.column, cursor.visible).hash(&mut hash);
    }
    Ok(Screen {
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
    fn observed_region_ignores_changing_metrics_but_rejects_stale_input_takeover_and_resize() {
        let (mut term, mut p) = grid();
        p.advance(&mut term, b"\x1b[HMenu\x1b[3;1HClock 1\x1b[6;1H");
        let region = Region {
            row: 0,
            column: 0,
            rows: 1,
            columns: 20,
        };
        let mut screen = snapshot(&term, Some(&region)).unwrap();
        let mut state = State::default();
        state.record(&mut screen);
        let owner = Arc::new(Controller::new("Agent".into()));
        p.advance(&mut term, b"\x1b[3;1HClock 2\x1b[6;1H");
        assert!(
            state
                .validate(
                    &screen.screen_id,
                    &owner,
                    &snapshot(&term, Some(&region)).unwrap()
                )
                .is_ok()
        );
        state.owner = Some(owner.clone());
        let other = Controller::new("Child".into());
        assert!(state.validate(&screen.screen_id, &other, &screen).is_err());
        state.takeover();
        assert!(state.validate(&screen.screen_id, &owner, &screen).is_err());
        let mut fresh = snapshot(&term, Some(&region)).unwrap();
        state.record(&mut fresh);
        assert!(state.validate(&fresh.screen_id, &owner, &fresh).is_err());
        state.resume();
        state.record(&mut fresh);
        assert!(state.validate(&fresh.screen_id, &owner, &fresh).is_ok());
        state.observations.back_mut().unwrap().at = Instant::now() - Duration::from_secs(121);
        assert!(state.validate(&fresh.screen_id, &owner, &fresh).is_err());
        state.record(&mut fresh);
        let mut resized = fresh.clone();
        resized.columns += 1;
        assert!(state.validate(&fresh.screen_id, &owner, &resized).is_err());
        p.advance(&mut term, b"\x1b[HExit\x1b[6;1H");
        assert!(
            state
                .validate(
                    &fresh.screen_id,
                    &owner,
                    &snapshot(&term, Some(&region)).unwrap()
                )
                .is_err()
        );
        owner.release();
        assert!(state.validate(&fresh.screen_id, &owner, &fresh).is_err());
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
