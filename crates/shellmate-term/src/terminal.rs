//! Terminal model: owns the alacritty grid and the process feeding it.
//!
//! Output is parsed on the PTY reader thread (under the grid lock); the UI is
//! woken through a coalescing channel so a burst of output costs one repaint.

use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{NamedColor, Processor};
use gpui::{ClipboardItem, Context, EventEmitter, Pixels, SharedString, Task, px};
use parking_lot::Mutex;
use portable_pty::PtySize;

use crate::colors::TerminalPalette;
use crate::keys::paste_payload;
use crate::pty::{LocalPty, ProcessInfo, PtyOptions};

/// A search hit, in grid coordinates.
pub type SearchMatch = RangeInclusive<Point>;

const MAX_SEARCH_MATCHES: usize = 2000;

/// Grid size plus the cell metrics it was computed from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: Pixels,
    pub line_height: Pixels,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { cols: 80, rows: 24, cell_width: px(8.), line_height: px(18.) }
    }
}

impl TerminalSize {
    fn pty_size(&self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: (f32::from(self.cell_width) * self.cols as f32) as u16,
            pixel_height: (f32::from(self.line_height) * self.rows as f32) as u16,
        }
    }

    fn window_size(&self) -> WindowSize {
        WindowSize {
            num_lines: self.rows,
            num_cols: self.cols,
            cell_width: f32::from(self.cell_width) as u16,
            cell_height: f32::from(self.line_height) as u16,
        }
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

enum Msg {
    /// New output was parsed into the grid.
    Output,
    Alac(AlacEvent),
    Exited { generation: u64, code: Option<u32> },
}

/// Forwards alacritty's events (title, bell, replies to queries) to the UI.
#[derive(Clone)]
pub struct Listener(async_channel::Sender<Msg>);

impl EventListener for Listener {
    fn send_event(&self, event: AlacEvent) {
        let _ = self.0.try_send(Msg::Alac(event));
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TerminalEvent {
    Output,
    TitleChanged,
    Bell,
    Exited(Option<u32>),
    Restarted,
}

pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    parser: Arc<Mutex<Processor>>,
    pty: Option<LocalPty>,
    options: PtyOptions,
    size: TerminalSize,
    palette: TerminalPalette,
    title: Option<SharedString>,
    /// `Some(code)` once the shell has exited.
    exit: Option<Option<u32>>,
    /// Bumped on every (re)spawn so exits of an old process are ignored.
    generation: u64,
    tx: async_channel::Sender<Msg>,
    output_pending: Arc<AtomicBool>,
    foreground: Option<(Instant, Option<ProcessInfo>)>,
    _io: Task<()>,
}

impl EventEmitter<TerminalEvent> for Terminal {}

impl Terminal {
    /// Starts a local shell.
    pub fn local(options: PtyOptions, palette: TerminalPalette, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded::<Msg>();
        let size = TerminalSize::default();
        let term = Term::new(Config::default(), &size, Listener(tx.clone()));

        let io = cx.spawn(async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                let mut batch = vec![first];
                while let Ok(msg) = rx.try_recv() {
                    batch.push(msg);
                }
                if this.update(cx, |this, cx| this.process(batch, cx)).is_err() {
                    break;
                }
            }
        });

        let mut this = Self {
            term: Arc::new(FairMutex::new(term)),
            parser: Arc::new(Mutex::new(Processor::new())),
            pty: None,
            options,
            size,
            palette,
            title: None,
            exit: None,
            generation: 0,
            tx,
            output_pending: Arc::new(AtomicBool::new(false)),
            foreground: None,
            _io: io,
        };
        this.spawn_shell();
        this
    }

    fn spawn_shell(&mut self) {
        self.generation += 1;
        let generation = self.generation;
        let (term, parser, pending) = (self.term.clone(), self.parser.clone(), self.output_pending.clone());
        let (tx_out, tx_exit) = (self.tx.clone(), self.tx.clone());

        let spawned = LocalPty::spawn(
            &self.options,
            self.size.pty_size(),
            move |bytes| {
                {
                    let mut term = term.lock();
                    parser.lock().advance(&mut *term, bytes);
                }
                if !pending.swap(true, Ordering::AcqRel) {
                    let _ = tx_out.try_send(Msg::Output);
                }
            },
            move |code| {
                let _ = tx_exit.try_send(Msg::Exited { generation, code });
            },
        );

        match spawned {
            Ok(pty) => {
                self.pty = Some(pty);
                self.exit = None;
            }
            Err(err) => {
                log::error!("failed to start shell: {err:#}");
                self.inject(&format!("\x1b[31m无法启动 {}：{err:#}\x1b[0m\r\n", self.options.command_line()));
                self.exit = Some(None);
            }
        }
    }

    fn process(&mut self, batch: Vec<Msg>, cx: &mut Context<Self>) {
        let mut output = false;
        for msg in batch {
            match msg {
                Msg::Output => {
                    self.output_pending.store(false, Ordering::Release);
                    output = true;
                }
                Msg::Alac(event) => self.handle_alac_event(event, cx),
                Msg::Exited { generation, code } if generation == self.generation => {
                    self.pty = None;
                    self.exit = Some(code);
                    let code_text = code.map_or_else(|| "未知".to_string(), |c| c.to_string());
                    self.inject(&format!(
                        "\r\n\x1b[2m[进程已退出，代码 {code_text}] 按回车重新启动\x1b[0m\r\n"
                    ));
                    cx.emit(TerminalEvent::Exited(code));
                    output = true;
                }
                Msg::Exited { .. } => {}
            }
        }
        if output {
            self.foreground = None;
            cx.emit(TerminalEvent::Output);
            cx.notify();
        }
    }

    fn handle_alac_event(&mut self, event: AlacEvent, cx: &mut Context<Self>) {
        match event {
            AlacEvent::PtyWrite(text) => self.write_raw(text.as_bytes()),
            AlacEvent::Title(title) => {
                self.title = Some(title.into());
                cx.emit(TerminalEvent::TitleChanged);
            }
            AlacEvent::ResetTitle => {
                self.title = None;
                cx.emit(TerminalEvent::TitleChanged);
            }
            AlacEvent::Bell => cx.emit(TerminalEvent::Bell),
            AlacEvent::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            AlacEvent::ColorRequest(index, format) => {
                let overrides = *self.term.lock().colors();
                let rgb = overrides[index].unwrap_or_else(|| match index {
                    0..=255 => self.palette.indexed(index as u8),
                    i if i == NamedColor::Foreground as usize => self.palette.foreground,
                    i if i == NamedColor::Background as usize => self.palette.background,
                    _ => self.palette.cursor,
                });
                self.write_raw(format(rgb).as_bytes());
            }
            AlacEvent::TextAreaSizeRequest(format) => {
                self.write_raw(format(self.size.window_size()).as_bytes())
            }
            _ => {}
        }
    }

    /// Feeds bytes to the grid as if the process had printed them.
    fn inject(&self, text: &str) {
        let mut term = self.term.lock();
        self.parser.lock().advance(&mut *term, text.as_bytes());
    }

    fn write_raw(&mut self, bytes: &[u8]) {
        if let Some(pty) = self.pty.as_mut()
            && let Err(err) = pty.write(bytes)
        {
            log::warn!("pty write failed: {err:#}");
        }
    }

    /// Sends user input to the process, snapping the view back to the prompt.
    pub fn input(&mut self, bytes: &[u8]) {
        if self.pty.is_none() {
            return;
        }
        {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        }
        self.write_raw(bytes);
    }

    pub fn paste(&mut self, text: &str) {
        let mode = self.mode();
        self.input(paste_payload(text, &mode).as_bytes());
    }

    /// Respawns the shell after it exited, keeping the scrollback.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if self.exit.is_none() {
            return;
        }
        self.spawn_shell();
        cx.emit(TerminalEvent::Restarted);
        cx.notify();
    }

    pub fn set_size(&mut self, size: TerminalSize) {
        if size == self.size {
            return;
        }
        let grid_changed = (size.cols, size.rows) != (self.size.cols, self.size.rows);
        self.size = size;
        if grid_changed {
            self.term.lock().resize(size);
            if let Some(pty) = &self.pty
                && let Err(err) = pty.resize(size.pty_size())
            {
                log::warn!("pty resize failed: {err:#}");
            }
        }
    }

    pub fn set_palette(&mut self, palette: TerminalPalette) {
        self.palette = palette;
    }

    pub fn scroll(&mut self, lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_page(&mut self, up: bool) {
        self.term.lock().scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
    }

    pub fn scroll_to_edge(&mut self, top: bool) {
        self.term.lock().scroll_display(if top { Scroll::Top } else { Scroll::Bottom });
    }

    pub fn scroll_to_point(&mut self, point: Point) {
        self.term.lock().scroll_to_point(point);
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string().filter(|s| !s.is_empty())
    }

    /// Clears the screen and scrollback, like ⌘K in Terminal.app.
    pub fn clear(&mut self) {
        // ED 3 (scrollback) + clear screen + home, then a form feed so the
        // shell redraws its prompt.
        self.inject("\x1b[3J\x1b[2J\x1b[H");
        self.input(b"\x0c");
    }

    /// Finds `query` (plain text, smart case) in the whole scrollback.
    pub fn search(&self, query: &str) -> Vec<SearchMatch> {
        if query.is_empty() {
            return Vec::new();
        }
        let Ok(mut regex) = RegexSearch::new(&regex_escape(query)) else {
            return Vec::new();
        };
        let term = self.term.lock();
        let start = Point::new(term.topmost_line(), Column(0));
        let end = Point::new(term.bottommost_line(), term.last_column());
        RegexIter::new(start, end, Direction::Right, &term, &mut regex)
            .take(MAX_SEARCH_MATCHES)
            .collect()
    }

    /// Foreground process of the PTY (cached briefly; it is polled while rendering).
    pub fn foreground_process(&mut self) -> Option<ProcessInfo> {
        let fresh = |at: &Instant| at.elapsed() < Duration::from_millis(500);
        if let Some((at, info)) = &self.foreground
            && fresh(at)
        {
            return info.clone();
        }
        let info = self.pty.as_ref().and_then(LocalPty::foreground_process);
        self.foreground = Some((Instant::now(), info.clone()));
        info
    }

    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    pub fn term(&self) -> &Arc<FairMutex<Term<Listener>>> {
        &self.term
    }

    pub fn size(&self) -> TerminalSize {
        self.size
    }

    pub fn title(&self) -> Option<&SharedString> {
        self.title.as_ref()
    }

    pub fn options(&self) -> &PtyOptions {
        &self.options
    }

    pub fn is_exited(&self) -> bool {
        self.exit.is_some()
    }
}

/// Escapes regex metacharacters so user input is searched literally.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::regex_escape;

    #[test]
    fn escapes_metacharacters() {
        assert_eq!(regex_escape("a.b*c"), r"a\.b\*c");
        assert_eq!(regex_escape("[error]"), r"\[error\]");
        assert_eq!(regex_escape("中文 ok"), "中文 ok");
    }
}
