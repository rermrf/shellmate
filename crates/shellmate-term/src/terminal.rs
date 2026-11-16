//! Terminal model: owns the alacritty grid and the backend feeding it.
//!
//! The backend is anything implementing [`TerminalBackend`]: a local PTY or an
//! SSH shell channel. Its output is parsed on the backend's own thread or task
//! (under the grid lock) through an [`OutputSink`]; the UI is woken through a
//! coalescing channel so a burst of output costs one repaint.

use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use shellmate_core::io::{OutputHandler, OutputSink, ProcessInfo, PtyDims, TerminalBackend};

use crate::colors::TerminalPalette;
use crate::keys::paste_payload;
use crate::history::Capture;
use crate::osc7::Osc7Scanner;
use crate::pty::{LocalPty, PtyOptions};

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
    /// The size as PTY dimensions (cells and pixels).
    pub fn dims(&self) -> PtyDims {
        PtyDims {
            cols: self.cols,
            rows: self.rows,
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
    Disconnected { generation: u64, reason: String },
    /// The shell reported its working directory (OSC 7).
    Cwd { generation: u64, path: String },
    Command { generation: u64, command: String, time: i64 },
}

/// Forwards alacritty's events (title, bell, replies to queries) to the UI.
#[derive(Clone)]
pub struct Listener(async_channel::Sender<Msg>);

impl EventListener for Listener {
    fn send_event(&self, event: AlacEvent) {
        let _ = self.0.try_send(Msg::Alac(event));
    }
}

/// Events a [`Terminal`] emits to its owner.
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalEvent {
    /// New content was written to the grid.
    Output,
    /// The program set (or reset) the window title.
    TitleChanged,
    /// The program rang the bell.
    Bell,
    /// The program behind the backend ended (local shell exit, remote shell exit).
    Exited(Option<u32>),
    /// The transport broke or could not be established; carries the user-facing reason.
    Disconnected(String),
    /// Enter was pressed on an ended session whose backend is not a local PTY.
    /// The terminal cannot recreate such a backend itself (it needs credentials,
    /// prompts, a connection pool, ...); the owner should reconnect and call
    /// [`Terminal::attach`].
    RestartRequested,
    /// A new backend was attached to a terminal whose previous session had ended.
    Restarted,
    /// The shell reported a new working directory with OSC 7 (`ESC ] 7 ; file://host/path BEL`);
    /// the path is percent-decoded. Only changes are reported. Shells do not do this by default.
    CwdChanged(String),
    /// A command read from the primary screen, after privacy filtering.
    Command { command: String, cwd: Option<String>, time: i64 },
    HistoryRequested,
}

/// Why the attached backend is no longer running.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEnd {
    /// The program exited; `None` when its status is unknown.
    Exited(Option<u32>),
    /// The connection was lost, refused, or never established.
    Disconnected(String),
}

/// Parses backend output into the grid and forwards lifecycle notifications.
/// Bound to the generation of the attach that created it; output from a
/// replaced backend is dropped.
struct GridSink {
    term: Arc<FairMutex<Term<Listener>>>,
    parser: Arc<Mutex<Processor>>,
    capture: Arc<Mutex<Capture>>,
    /// Watches the stream for OSC 7 (alacritty's parser ignores it); sees the bytes before the
    /// parser does and changes nothing about them.
    osc7: Mutex<Osc7Scanner>,
    pending: Arc<AtomicBool>,
    tx: async_channel::Sender<Msg>,
    generation: u64,
    current: Arc<AtomicU64>,
}

impl OutputHandler for GridSink {
    fn output(&self, bytes: &[u8]) {
        if self.current.load(Ordering::Acquire) != self.generation {
            return;
        }
        let reported = self.osc7.lock().feed_with_offsets(bytes);
        {
            let mut term = self.term.lock();
            let mut capture = self.capture.lock();
            let mut parser = self.parser.lock();
            let mut at = 0;
            let emit = |commands: Vec<String>| {
                for command in commands {
                    let _ = self.tx.try_send(Msg::Command { generation: self.generation, command, time: unix_time() });
                }
            };
            for (end, path) in reported {
                emit(capture.advance(&mut *term, &mut parser, &bytes[at..end]));
                at = end;
                let _ = self.tx.try_send(Msg::Cwd { generation: self.generation, path });
            }
            emit(capture.advance(&mut *term, &mut parser, &bytes[at..]));
        }
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.tx.try_send(Msg::Output);
        }
    }

    fn exited(&self, code: Option<u32>) {
        let _ = self.tx.try_send(Msg::Exited { generation: self.generation, code });
    }

    fn disconnected(&self, reason: String) {
        let _ = self.tx.try_send(Msg::Disconnected { generation: self.generation, reason });
    }
}

/// A terminal session: the alacritty grid plus the backend feeding it. Create
/// one with [`Terminal::local`] or [`Terminal::new`] + [`Terminal::attach`].
pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    parser: Arc<Mutex<Processor>>,
    capture: Arc<Mutex<Capture>>,
    backend: Option<Box<dyn TerminalBackend>>,
    /// `Some` for terminals that own a local shell: Enter restarts it in place.
    local: Option<PtyOptions>,
    size: TerminalSize,
    palette: TerminalPalette,
    title: Option<SharedString>,
    /// `Some` once the attached session has ended.
    ended: Option<SessionEnd>,
    /// The working directory the shell last reported with OSC 7.
    cwd: Option<String>,
    /// The owner reconnects by itself when the transport drops: the line that reports the loss
    /// does not tell the user to press Enter (the owner prints its own countdown instead).
    auto_reconnect: bool,
    /// Bumped on every attach so notifications of a replaced backend are ignored.
    generation: Arc<AtomicU64>,
    tx: async_channel::Sender<Msg>,
    output_pending: Arc<AtomicBool>,
    foreground: Option<(Instant, Option<ProcessInfo>)>,
    _io: Task<()>,
}

impl EventEmitter<TerminalEvent> for Terminal {}

impl Terminal {
    /// Starts a local shell. Pressing Enter after it exits restarts it in place.
    pub fn local(options: PtyOptions, palette: TerminalPalette, cx: &mut Context<Self>) -> Self {
        let mut this = Self::build(Some(options), palette, cx);
        this.spawn_local(cx);
        this
    }

    /// Creates a terminal with no backend yet, e.g. for an SSH session that is
    /// still connecting. Show progress with [`notice`](Self::notice) and wire a
    /// backend with [`attach`](Self::attach) once it is ready.
    pub fn new(palette: TerminalPalette, cx: &mut Context<Self>) -> Self {
        Self::build(None, palette, cx)
    }

    fn build(local: Option<PtyOptions>, palette: TerminalPalette, cx: &mut Context<Self>) -> Self {
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

        Self {
            term: Arc::new(FairMutex::new(term)),
            parser: Arc::new(Mutex::new(Processor::new())),
            capture: Arc::new(Mutex::new(Capture::default())),
            backend: None,
            local,
            size,
            palette,
            title: None,
            ended: None,
            cwd: None,
            auto_reconnect: false,
            generation: Arc::new(AtomicU64::new(0)),
            tx,
            output_pending: Arc::new(AtomicBool::new(false)),
            foreground: None,
            _io: io,
        }
    }

    fn spawn_local(&mut self, cx: &mut Context<Self>) {
        let Some(options) = self.local.clone() else { return };
        self.attach(
            move |sink, dims| {
                let (out, exit) = (sink.clone(), sink);
                let pty = LocalPty::spawn(&options, dims, move |bytes| out.output(bytes), move |code| exit.exited(code))
                    .map_err(|err| {
                        log::error!("failed to start shell: {err:#}");
                        anyhow::anyhow!("无法启动 {}：{err:#}", options.command_line())
                    })?;
                Ok(Box::new(pty) as Box<dyn TerminalBackend>)
            },
            cx,
        );
    }

    /// Replaces the backend with one built by `factory`, which receives the
    /// [`OutputSink`] bound to the new session and the current terminal size.
    /// The previous backend, if any, is closed and its late output discarded.
    ///
    /// `factory` must not block: network backends should return immediately and
    /// open their transport in the background, reporting failures through the
    /// sink. If it fails, the error is shown in the terminal, the session counts
    /// as ended (Enter retries) and [`TerminalEvent::Disconnected`] is emitted.
    /// Returns whether a backend is attached. Emits [`TerminalEvent::Restarted`]
    /// when this replaces a session that had ended.
    pub fn attach(
        &mut self,
        factory: impl FnOnce(OutputSink, PtyDims) -> anyhow::Result<Box<dyn TerminalBackend>>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.close_backend();
        *self.capture.lock() = Capture::default();
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let sink = OutputSink::new(GridSink {
            term: self.term.clone(),
            parser: self.parser.clone(),
            capture: self.capture.clone(),
            osc7: Mutex::new(Osc7Scanner::new()),
            pending: self.output_pending.clone(),
            tx: self.tx.clone(),
            generation,
            current: self.generation.clone(),
        });
        let was_ended = self.ended.is_some();
        match factory(sink, self.size.dims()) {
            Ok(backend) => {
                self.backend = Some(backend);
                self.ended = None;
                self.cwd = None;
                self.foreground = None;
                if was_ended {
                    cx.emit(TerminalEvent::Restarted);
                }
                cx.notify();
                true
            }
            Err(err) => {
                self.fail(format!("{err:#}"), cx);
                false
            }
        }
    }

    /// Ends the session because the connection failed or dropped. `reason` is a
    /// self-contained user-facing sentence; it is printed in the terminal and
    /// carried by [`TerminalEvent::Disconnected`]. Enter then asks the owner to
    /// reconnect ([`TerminalEvent::RestartRequested`]).
    pub fn disconnect(&mut self, reason: impl Into<String>, cx: &mut Context<Self>) {
        self.fail(reason.into(), cx);
    }

    fn fail(&mut self, reason: String, cx: &mut Context<Self>) {
        self.end_disconnected(reason, cx);
        self.after_output(cx);
    }

    fn end_disconnected(&mut self, reason: String, cx: &mut Context<Self>) {
        self.close_backend();
        self.ended = Some(SessionEnd::Disconnected(reason.clone()));
        self.print_end(&format!("\x1b[31m[{reason}]\x1b[0m"));
        cx.emit(TerminalEvent::Disconnected(reason));
    }

    /// Whether the owner reconnects by itself when the transport drops (see the field); the owner
    /// sets it while the connection is up and clears it when it stops trying.
    pub fn set_auto_reconnect(&mut self, on: bool) {
        self.auto_reconnect = on;
    }

    /// Prints the line that closes an ended session, with the "press Enter" hint unless an automatic
    /// reconnect is about to follow.
    fn print_end(&mut self, text: &str) {
        let hint = end_hint(self.local.is_some(), self.auto_reconnect);
        let tail = if hint.is_empty() { String::new() } else { format!(" \x1b[2m{hint}\x1b[0m") };
        let column = self.term.lock().grid().cursor.point.column.0;
        let lead = if column == 0 { "" } else { "\r\n" };
        self.inject(&format!("{lead}\r\n{text}{tail}\r\n"));
    }

    fn close_backend(&mut self) {
        if let Some(mut backend) = self.backend.take() {
            backend.close();
        }
    }

    /// Prints a line of text in the terminal as if the remote side had sent it,
    /// e.g. connection progress (`→ 连接 10.0.1.21:22`). ANSI colors may be used;
    /// the line always starts at column 0 and attributes are reset at its end.
    pub fn notice(&mut self, text: &str, cx: &mut Context<Self>) {
        let column = self.term.lock().grid().cursor.point.column.0;
        let mut line = String::with_capacity(text.len() + 8);
        if column != 0 {
            line.push_str("\r\n");
        }
        line.push_str(&text.replace("\r\n", "\n").replace('\n', "\r\n"));
        line.push_str("\x1b[0m\r\n");
        self.inject(&line);
        self.after_output(cx);
    }

    fn after_output(&mut self, cx: &mut Context<Self>) {
        self.foreground = None;
        cx.emit(TerminalEvent::Output);
        cx.notify();
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
                Msg::Exited { generation, code } if generation == self.current_generation() => {
                    self.close_backend();
                    self.ended = Some(SessionEnd::Exited(code));
                    let code_text = code.map_or_else(|| "未知".to_string(), |c| c.to_string());
                    let what = if self.local.is_some() { "进程" } else { "远程会话" };
                    let hint = if self.local.is_some() { "按回车重新启动" } else { "按回车重新连接" };
                    self.inject(&format!("\r\n\x1b[2m[{what}已退出，代码 {code_text}] {hint}\x1b[0m\r\n"));
                    cx.emit(TerminalEvent::Exited(code));
                    output = true;
                }
                Msg::Disconnected { generation, reason } if generation == self.current_generation() => {
                    self.end_disconnected(reason, cx);
                    output = true;
                }
                Msg::Cwd { generation, path } if generation == self.current_generation() => {
                    if self.cwd.as_deref() != Some(path.as_str()) {
                        self.cwd = Some(path.clone());
                        cx.emit(TerminalEvent::CwdChanged(path));
                    }
                }
                Msg::Command { generation, command, time } if generation == self.current_generation() => {
                    cx.emit(TerminalEvent::Command { command, cwd: self.cwd.clone(), time });
                }
                Msg::Exited { .. } | Msg::Disconnected { .. } | Msg::Cwd { .. } | Msg::Command { .. } => {}
            }
        }
        if output {
            self.after_output(cx);
        }
    }

    fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
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
        if let Some(backend) = self.backend.as_mut()
            && let Err(err) = backend.write(bytes)
        {
            log::warn!("backend write failed: {err}");
        }
    }

    /// Sends user input to the process, snapping the view back to the prompt.
    pub fn input(&mut self, bytes: &[u8]) {
        if self.try_input(bytes).is_err() {
            log::warn!("terminal input write failed");
        }
    }

    fn try_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.backend.is_none() {
            return Err(std::io::ErrorKind::NotConnected.into());
        }
        {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
            self.capture.lock().input(&term, bytes);
        }
        self.backend.as_mut().ok_or(std::io::ErrorKind::NotConnected)?.write(bytes)
    }

    pub fn paste(&mut self, text: &str) {
        let mode = self.mode();
        self.input(paste_payload(text, &mode).as_bytes());
    }

    /// Sends a reviewed command as a paste followed by a separate carriage return.
    pub fn send_command(&mut self, command: &str) -> Result<(), &'static str> {
        if self.backend.is_none() || self.ended.is_some() {
            return Err("会话未连接，无法发送");
        }
        crate::send::send(command, &self.mode(), |bytes| self.try_input(bytes))
    }

    /// Called when the user presses Enter on an ended session. A local terminal
    /// respawns its shell in place, keeping the scrollback; any other terminal
    /// emits [`TerminalEvent::RestartRequested`] for its owner to handle.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if self.ended.is_none() {
            return;
        }
        if self.local.is_some() {
            self.spawn_local(cx);
        } else {
            cx.emit(TerminalEvent::RestartRequested);
        }
    }

    pub fn set_size(&mut self, size: TerminalSize) {
        if size == self.size {
            return;
        }
        let grid_changed = (size.cols, size.rows) != (self.size.cols, self.size.rows);
        self.size = size;
        if grid_changed {
            self.term.lock().resize(size);
            let dims = size.dims();
            if let Some(backend) = self.backend.as_mut()
                && let Err(err) = backend.resize(dims.cols, dims.rows, dims.pixel_width, dims.pixel_height)
            {
                log::warn!("backend resize failed: {err}");
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

    /// Foreground process of a local PTY (cached briefly; it is polled while
    /// rendering). Always `None` for network backends.
    pub fn foreground_process(&mut self) -> Option<ProcessInfo> {
        let fresh = |at: &Instant| at.elapsed() < Duration::from_millis(500);
        if let Some((at, info)) = &self.foreground
            && fresh(at)
        {
            return info.clone();
        }
        let info = self.backend.as_ref().and_then(|backend| backend.foreground_process());
        self.foreground = Some((Instant::now(), info.clone()));
        info
    }

    /// The working directory the shell last reported with OSC 7, if it ever did since the
    /// current backend was attached.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
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

    /// Launch options of the local shell; `None` for network terminals.
    pub fn options(&self) -> Option<&PtyOptions> {
        self.local.as_ref()
    }

    /// Whether this terminal owns a local shell (as opposed to a network backend).
    pub fn is_local(&self) -> bool {
        self.local.is_some()
    }

    /// Whether the session has ended (Enter restarts it).
    pub fn is_exited(&self) -> bool {
        self.ended.is_some()
    }

    /// Why the session ended, if it did.
    pub fn session_end(&self) -> Option<&SessionEnd> {
        self.ended.as_ref()
    }

    /// Whether a backend is currently attached and running.
    pub fn is_attached(&self) -> bool {
        self.backend.is_some()
    }
}

fn unix_time() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The hint at the end of the line that closes an ended session: Enter restarts a local shell and
/// reconnects an SSH one, and says nothing when the owner is about to reconnect by itself (its own
/// countdown line follows, with its own hint).
fn end_hint(local: bool, auto_reconnect: bool) -> &'static str {
    match (local, auto_reconnect) {
        (true, _) => "按回车重新启动",
        (false, true) => "",
        (false, false) => "按回车重新连接",
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
    use super::{end_hint, regex_escape};

    #[test]
    fn the_enter_hint_is_left_out_when_a_reconnect_follows() {
        assert_eq!(end_hint(false, false), "按回车重新连接");
        assert_eq!(end_hint(false, true), "", "the countdown line says what happens next");
        assert_eq!(end_hint(true, false), "按回车重新启动");
        assert_eq!(end_hint(true, true), "按回车重新启动", "a local shell never reconnects by itself");
    }

    #[test]
    fn escapes_metacharacters() {
        assert_eq!(regex_escape("a.b*c"), r"a\.b\*c");
        assert_eq!(regex_escape("[error]"), r"\[error\]");
        assert_eq!(regex_escape("中文 ok"), "中文 ok");
    }
}

/// The sink a backend writes to: output parsing, coalescing and generation checks.
/// (`Terminal` itself needs a running gpui app, which cannot be set up headless here.)
#[cfg(test)]
mod sink_tests {
    use alacritty_terminal::index::Line;

    use super::*;

    struct Fixture {
        term: Arc<FairMutex<Term<Listener>>>,
        rx: async_channel::Receiver<Msg>,
        current: Arc<AtomicU64>,
        pending: Arc<AtomicBool>,
        sink: OutputSink,
    }

    fn fixture(generation: u64) -> Fixture {
        let (tx, rx) = async_channel::unbounded();
        let size = TerminalSize::default();
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, Listener(tx.clone()))));
        let current = Arc::new(AtomicU64::new(generation));
        let pending = Arc::new(AtomicBool::new(false));
        let sink = OutputSink::new(GridSink {
            term: term.clone(),
            parser: Arc::new(Mutex::new(Processor::new())),
            capture: Arc::new(Mutex::new(Capture::default())),
            osc7: Mutex::new(Osc7Scanner::new()),
            pending: pending.clone(),
            tx,
            generation,
            current: current.clone(),
        });
        Fixture { term, rx, current, pending, sink }
    }

    fn row(term: &Arc<FairMutex<Term<Listener>>>, line: i32) -> String {
        let term = term.lock();
        let row = &term.grid()[Line(line)];
        (0..40).map(|c| row[Column(c)].c).collect::<String>().trim_end().to_string()
    }

    fn drain(rx: &async_channel::Receiver<Msg>) -> Vec<Msg> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn cwd_and_command_events_preserve_order_in_one_output_chunk() {
        let f = fixture(1);
        f.sink.output(b"\x1b]7;file://host/first\x07\x1b]133;A\x07$ \x1b]133;B\x07cd /next\x1b]133;C\x07\r\n\x1b]7;file://host/next\x07");
        let events: Vec<_> = drain(&f.rx).into_iter().filter_map(|m| match m { Msg::Cwd { path, .. } => Some(path), Msg::Command { command, .. } => Some(command), _ => None }).collect();
        assert_eq!(events, ["/first", "cd /next", "/next"]);
    }

    #[test]
    fn output_is_parsed_into_the_grid() {
        let f = fixture(1);
        f.sink.output(b"hello\r\nshellmate");
        assert_eq!(row(&f.term, 0), "hello");
        assert_eq!(row(&f.term, 1), "shellmate");
    }

    #[test]
    fn a_burst_of_output_wakes_the_ui_once() {
        let f = fixture(1);
        for chunk in ["a", "b", "c"] {
            f.sink.output(chunk.as_bytes());
        }
        let msgs = drain(&f.rx);
        assert_eq!(msgs.iter().filter(|m| matches!(m, Msg::Output)).count(), 1);
        // Once the UI consumed the notification (and reset the flag) the next output wakes it again.
        f.pending.store(false, Ordering::Release);
        f.sink.output(b"d");
        assert!(matches!(drain(&f.rx).as_slice(), [Msg::Output]));
        assert_eq!(row(&f.term, 0), "abcd");
    }

    #[test]
    fn output_of_a_replaced_backend_is_dropped() {
        let f = fixture(1);
        f.sink.output(b"old");
        f.current.store(2, Ordering::Release);
        f.sink.output(b"stale");
        assert_eq!(row(&f.term, 0), "old");
    }

    #[test]
    fn lifecycle_notifications_carry_their_generation() {
        let f = fixture(7);
        f.sink.exited(Some(3));
        f.sink.disconnected("连接已断开：保活超时");
        let msgs = drain(&f.rx);
        assert!(matches!(&msgs[0], Msg::Exited { generation: 7, code: Some(3) }));
        assert!(matches!(&msgs[1], Msg::Disconnected { generation: 7, reason } if reason == "连接已断开：保活超时"));
    }

    #[test]
    fn a_reported_directory_reaches_the_ui_and_the_grid_is_unchanged() {
        let f = fixture(3);
        f.sink.output(b"before\x1b]7;file://h/home/%E7%94%A8%E6%88%B7\x07after");
        let msgs = drain(&f.rx);
        let cwds: Vec<_> = msgs
            .iter()
            .filter_map(|m| match m {
                Msg::Cwd { generation, path } => Some((*generation, path.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(cwds, [(3, "/home/用户")]);
        // The parser still saw everything, exactly as without the scanner.
        assert_eq!(row(&f.term, 0), "beforeafter");
    }

    #[test]
    fn a_report_cut_between_reads_is_still_reported_once() {
        let f = fixture(1);
        f.sink.output(b"x\x1b]7;file://h/va");
        f.sink.output(b"r/log\x1b");
        assert!(!drain(&f.rx).iter().any(|m| matches!(m, Msg::Cwd { .. })), "not complete yet");
        f.sink.output(b"\\y");
        let msgs = drain(&f.rx);
        assert!(matches!(msgs.iter().find(|m| matches!(m, Msg::Cwd { .. })), Some(Msg::Cwd { path, .. }) if path == "/var/log"));
        assert_eq!(row(&f.term, 0), "xy");
    }

    #[test]
    fn output_of_a_replaced_backend_reports_no_directory() {
        let f = fixture(1);
        f.current.store(2, Ordering::Release);
        f.sink.output(b"\x1b]7;file://h/stale\x07");
        assert!(drain(&f.rx).is_empty());
    }
}

#[cfg(all(test, unix))]
#[path = "capture_pty_tests.rs"]
mod capture_pty_tests;
