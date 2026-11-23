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
use alacritty_terminal::vte::ansi::{Handler as _, NamedColor, NamedPrivateMode, PrivateMode, Processor};
use gpui::{ClipboardItem, Context, EventEmitter, Pixels, SharedString, Task, px};
use parking_lot::Mutex;
use shellmate_core::io::{OutputHandler, OutputSink, ProcessInfo, PtyDims, TerminalBackend};

use crate::colors::TerminalPalette;
use crate::keys::paste_payload;
use crate::focus::FocusReporter;
use crate::history::Capture;
use crate::notify::NotifyScanner;
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
    /// Grid dimensions from the actual terminal content rectangle, excluding pane chrome/padding.
    pub fn from_geometry(width: f32, height: f32, cell_width: Pixels, line_height: Pixels) -> Self {
        let cols = (width.max(0.) / f32::from(cell_width).max(1.)).floor().clamp(2., u16::MAX as f32) as u16;
        let rows = (height.max(0.) / f32::from(line_height).max(1.)).floor().clamp(1., u16::MAX as f32) as u16;
        Self { cols, rows, cell_width, line_height }
    }

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
    Alac { generation: u64, event: AlacEvent },
    Exited { generation: u64, code: Option<u32> },
    Disconnected { generation: u64, reason: String },
    /// The shell reported its working directory (OSC 7).
    Cwd { generation: u64, path: String },
    Command { generation: u64, command: String, time: i64 },
    /// The program asked for a desktop notification (OSC 9 or OSC 777).
    Notify { generation: u64, title: Option<String>, body: String },
}

/// Forwards alacritty's events (title, bell, replies to queries) to the UI.
#[derive(Clone)]
pub struct Listener(async_channel::Sender<Msg>, Arc<AtomicU64>);

impl EventListener for Listener {
    fn send_event(&self, event: AlacEvent) {
        let generation = self.1.load(Ordering::Acquire);
        let _ = self.0.try_send(Msg::Alac { generation, event });
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
    /// The user focused this terminal view.
    Focused,
    /// The painted content rectangle changed the PTY dimensions.
    SizeChanged,
    /// The program asked for a desktop notification with `ESC ] 9 ; text BEL` or
    /// `ESC ] 777 ; notify ; title ; body BEL`. The text has no control characters and is at most
    /// 1 KiB. Nothing of it reaches the screen.
    Notify { title: Option<String>, body: String },
    /// The program was told the terminal gained (`true`) or lost the focus, as it asked with
    /// `?1004h`. Only changes are reported to the program, and so to this event.
    FocusReport(bool),
}

/// Why the attached backend is no longer running.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEnd {
    /// The program exited; `None` when its status is unknown.
    Exited(Option<u32>),
    /// The connection was lost, refused, or never established.
    Disconnected(String),
}

/// Serial display filters must not modify the stream used by history or archives.
struct Silent;
impl EventListener for Silent {fn send_event(&self,_:AlacEvent){}}
struct RawGrid {term:Term<Silent>,parser:Processor}
impl RawGrid {fn new(size:TerminalSize)->Self {Self{term:Term::new(Config::default(),&size,Silent),parser:Processor::new()}}}

/// Parses backend output into the grid and forwards lifecycle notifications.
/// Bound to the generation of the attach that created it; output from a
/// replaced backend is dropped.
struct GridSink {
    term: Arc<FairMutex<Term<Listener>>>,
    parser: Arc<Mutex<Processor>>,
    capture: Arc<Mutex<Capture>>,
    raw_grid: Arc<Mutex<Option<RawGrid>>>,
    output_capture: Arc<Mutex<crate::capture_output::OutputCapture>>,
    agent: Arc<Mutex<crate::agent::AgentState>>,
    alive: Arc<Mutex<Arc<AtomicBool>>>,
    /// Watches the stream for OSC 7 (alacritty's parser ignores it); sees the bytes before the
    /// parser does and changes nothing about them.
    osc7: Mutex<Osc7Scanner>,
    /// Watches the stream for notification requests (OSC 9, OSC 777), which the parser ignores too.
    notify: Mutex<NotifyScanner>,
    pending: Arc<AtomicBool>,
    tx: async_channel::Sender<Msg>,
    generation: u64,
    current: Arc<AtomicU64>,
}

impl GridSink {
    /// Passes the notification requests found in the bytes the parser just took to the UI.
    fn send_notifications(&self, bytes: &[u8]) {
        for notification in self.notify.lock().feed(bytes) {
            let _ = self.tx.try_send(Msg::Notify {
                generation: self.generation,
                title: notification.title,
                body: notification.body,
            });
        }
    }
}

impl OutputHandler for GridSink {
    fn output(&self, bytes: &[u8]) {
        if self.current.load(Ordering::Acquire) != self.generation {
            return;
        }
        let filtered = self.agent.lock().output(bytes);
        let bytes = filtered.as_slice();
        let reported = self.osc7.lock().feed_with_offsets(bytes);
        self.send_notifications(bytes);
        {
            let mut term = self.term.lock();
            // Attach advances the generation under this lock. An old reader may
            // have passed the first check before waiting for it.
            if self.current.load(Ordering::Acquire) != self.generation { return; }
            self.output_capture.lock().output(bytes);
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
            self.output_capture.lock().sync_logger(&*term);
        }
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.tx.try_send(Msg::Output);
        }
    }

    fn transformed_output(&self,raw:&[u8],display:&[u8]) {
        if self.current.load(Ordering::Acquire)!=self.generation {return;}
        if !raw.is_empty() || !display.is_empty() { self.agent.lock().last_output = Some(Instant::now()); }
        let mut source=self.raw_grid.lock();
        if self.current.load(Ordering::Acquire)!=self.generation {return;}
        let source=source.get_or_insert_with(||{
            let term=self.term.lock();let size=TerminalSize{cols:term.columns() as u16,rows:term.screen_lines() as u16,..Default::default()};RawGrid::new(size)
        });
        self.output_capture.lock().output(raw);
        for command in self.capture.lock().advance(&mut source.term,&mut source.parser,raw) {
            let _=self.tx.try_send(Msg::Command{generation:self.generation,command,time:unix_time()});
        }
        self.output_capture.lock().sync_logger(&source.term);
        let mut term=self.term.lock();self.parser.lock().advance(&mut *term,display);
        drop(term);
        // The screen shows `display`, so that is what can ask for a notification (as it rings the bell).
        self.send_notifications(display);
        if !self.pending.swap(true,Ordering::AcqRel){let _=self.tx.try_send(Msg::Output);}
    }

    fn exited(&self, code: Option<u32>) {
        self.alive.lock().store(false, Ordering::Release);
        let _ = self.tx.try_send(Msg::Exited { generation: self.generation, code });
    }

    fn disconnected(&self, reason: String) {
        self.alive.lock().store(false, Ordering::Release);
        let _ = self.tx.try_send(Msg::Disconnected { generation: self.generation, reason });
    }
}

/// A terminal session: the alacritty grid plus the backend feeding it. Create
/// one with [`Terminal::local`] or [`Terminal::new`] + [`Terminal::attach`].
pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    parser: Arc<Mutex<Processor>>,
    capture: Arc<Mutex<Capture>>,
    raw_grid: Arc<Mutex<Option<RawGrid>>>,
    output_capture: Arc<Mutex<crate::capture_output::OutputCapture>>,
    agent: Arc<Mutex<crate::agent::AgentState>>,
    alive: Arc<Mutex<Arc<AtomicBool>>>,
    backend: Option<Box<dyn TerminalBackend>>,
    /// `Some` for terminals that own a local shell: Enter restarts it in place.
    local: Option<PtyOptions>,
    size: TerminalSize,
    resize_throttle: crate::resize::ResizeThrottle,
    resize_clock: Instant,
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
    pub(crate) display_paused: bool,
    bracketed_paste: bool,
    replay: bool,
    pub(crate) replay_times: crate::replay::timestamps::LineTimes,
    pub(crate) replay_timestamp: Option<i64>,
    pub(crate) prompt_highlighter: crate::prompt::PromptHighlighter,
    prompt_idle: Option<Task<()>>,
    /// Focus reporting (DECSET 1004); the owner says whether the terminal has the focus.
    focus: FocusReporter,
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
        let generation = Arc::new(AtomicU64::new(0));
        let term = Term::new(Config::default(), &size, Listener(tx.clone(), generation.clone()));

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
            raw_grid:Arc::new(Mutex::new(None)),
            output_capture: Arc::new(Mutex::new(crate::capture_output::OutputCapture::default())),
            agent: Arc::new(Mutex::new(crate::agent::AgentState::default())),
            alive: Arc::new(Mutex::new(Arc::new(AtomicBool::new(false)))),
            backend: None,
            local,
            size,
            resize_throttle: Default::default(),
            resize_clock: Instant::now(),
            palette,
            title: None,
            ended: None,
            cwd: None,
            auto_reconnect: false,
            generation,
            tx,
            output_pending: Arc::new(AtomicBool::new(false)),
            foreground: None,
            display_paused: false,
            bracketed_paste: true,
            replay: false,
            replay_times: Default::default(),
            replay_timestamp: None,
            prompt_highlighter: Default::default(),
            prompt_idle: None,
            focus: FocusReporter::default(),
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
        self.prompt_idle = None;
        self.prompt_highlighter.reset();
        let generation = {
            // Match the serial-output lock order, preserving screen and scrollback.
            let mut raw = self.raw_grid.lock();
            let mut term = self.term.lock();
            // alacritty 0.26 caps its private title stack at 4096 and exposes only
            // pop_title. Drain it before changing generation so these events,
            // and any already queued old titles, are discarded together.
            for _ in 0..4096 { term.pop_title(); }
            term.set_title(None);
            // The new program has not asked for focus reports, whatever the old one left on.
            term.unset_private_mode(PrivateMode::Named(NamedPrivateMode::ReportFocusInOut));
            *self.parser.lock() = Processor::new();
            *self.capture.lock() = Capture::default();
            *raw = None;
            self.generation.fetch_add(1, Ordering::AcqRel) + 1
        };
        self.cwd = None;
        self.foreground = None;
        self.focus.reset();
        self.set_title(None, cx);
        self.alive = Arc::new(Mutex::new(Arc::new(AtomicBool::new(true))));
        *self.agent.lock() = crate::agent::AgentState::default();
        let sink = OutputSink::new(GridSink {
            term: self.term.clone(),
            parser: self.parser.clone(),
            capture: self.capture.clone(),
            raw_grid:self.raw_grid.clone(),
            output_capture: self.output_capture.clone(),
            agent: self.agent.clone(),
            alive: self.alive.clone(),
            osc7: Mutex::new(Osc7Scanner::new()),
            notify: Mutex::new(NotifyScanner::new()),
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
        self.alive.lock().store(false, Ordering::Release);
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
        // The output may have turned focus reporting on.
        self.send_focus_report(cx);
        self.schedule_prompt_learning(cx);
        cx.emit(TerminalEvent::Output);
        if !self.display_paused { cx.notify(); }
    }

    fn schedule_prompt_learning(&mut self, cx: &mut Context<Self>) {
        if self.replay || self.ended.is_some() { return; }
        // Replacing the task debounces output bursts. Recheck the shared AI
        // output clock under the grid lock: more bytes may precede their UI wake.
        self.prompt_idle = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(crate::prompt::QUIET).await;
                let retry = this.update(cx, |this, cx| {
                    let term = this.term.lock();
                    let last_output = this.agent.lock().last_output;
                    let now = Instant::now();
                    if last_output.is_some_and(|at| now.saturating_duration_since(at) < crate::prompt::QUIET) {
                        return true;
                    }
                    if this.prompt_highlighter.learn(&*term, last_output, now) && !this.display_paused { cx.notify(); }
                    false
                });
                if !matches!(retry, Ok(true)) { break; }
            }
        }));
    }

    fn process(&mut self, batch: Vec<Msg>, cx: &mut Context<Self>) {
        let mut output = false;
        for msg in batch {
            match msg {
                Msg::Output => {
                    self.output_pending.store(false, Ordering::Release);
                    output = true;
                }
                Msg::Alac { generation, event } if generation == self.current_generation() => self.handle_alac_event(event, cx),
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
                Msg::Notify { generation, title, body } if generation == self.current_generation() => {
                    if !self.replay {
                        cx.emit(TerminalEvent::Notify { title, body });
                    }
                }
                Msg::Alac { .. } | Msg::Exited { .. } | Msg::Disconnected { .. } | Msg::Cwd { .. } | Msg::Command { .. } | Msg::Notify { .. } => {}
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
        if self.replay{return;}
        match event {
            AlacEvent::PtyWrite(text) => self.write_raw(text.as_bytes()),
            AlacEvent::Title(title) => self.set_title(Some(title.into()), cx),
            AlacEvent::ResetTitle => self.set_title(None, cx),
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

    fn set_title(&mut self, title: Option<SharedString>, cx: &mut Context<Self>) {
        let title = title.filter(|title| !title.trim().is_empty());
        if self.title != title {
            self.title = title;
            cx.emit(TerminalEvent::TitleChanged);
            cx.notify();
        }
    }

    /// Feeds bytes to the grid as if the process had printed them.
    fn inject(&self, text: &str) {
        self.output_capture.lock().log_output(text.as_bytes());
        let mut term = self.term.lock();
        self.parser.lock().advance(&mut *term, text.as_bytes());
    }

    /// Send terminal protocol bytes without treating them as typed command input.
    pub(crate) fn write_raw(&mut self, bytes: &[u8]) {
        if let Some(backend) = self.backend.as_mut()
            && let Err(err) = backend.write(bytes)
        {
            log::warn!("backend write failed: {err}");
        }
    }

    /// A generation-specific connection lease: reconnect never revives an old run.
    /// Retire outstanding AI work when its window loses the tab. The output sink
    /// still signals disconnect on the new lease for this same backend.
    pub fn rebind_agent(&mut self) {
        let nonce = self.agent.lock().active.as_ref().map(|(id,_)| id.clone());
        if let Some(nonce) = nonce { self.agent_finish(&nonce, true); }
        let mut lease = self.alive.lock();
        let connected = lease.swap(false, Ordering::AcqRel);
        *lease = Arc::new(AtomicBool::new(connected));
    }
    pub fn agent_liveness(&self) -> Arc<AtomicBool> { self.alive.lock().clone() }
    pub fn agent_connected(&self) -> bool { self.alive.lock().load(Ordering::Acquire) && self.backend.is_some() && !self.replay }
    pub fn agent_busy(&self) -> bool { self.agent.lock().busy(self.mode().contains(TermMode::ALT_SCREEN)) || self.capture.lock().running() }
    pub fn agent_start(&mut self, command: &str, nonce: &str) -> Result<crate::agent::CaptureHandle, &'static str> {
        if !self.agent_connected() { return Err("disconnected"); }
        if self.capture.lock().running() { return Err("busy"); }
        let (scanner, handle) = crate::agent::MarkerScanner::new(nonce);
        {
            let mut agent = self.agent.lock();
            if agent.busy(self.mode().contains(TermMode::ALT_SCREEN)) { return Err("busy"); }
            agent.active = Some((nonce.to_owned(), scanner));
        }
        if self.try_input(&crate::agent::command_line(command, nonce)).is_err() {
            self.agent_finish(nonce, false);
            return Err("disconnected");
        }
        Ok(handle)
    }
    pub fn agent_interrupt(&mut self, nonce: &str) {
        let running = self.agent.lock().active.as_ref().is_some_and(|(id, scanner)| id == nonce && scanner.running());
        if running { self.input(b"\x03"); }
    }
    pub fn agent_finish(&mut self, nonce: &str, interrupt: bool) {
        if interrupt { self.agent_interrupt(nonce); }
        let remaining = {
            let mut agent = self.agent.lock();
            if agent.active.as_ref().is_none_or(|(id, _)| id != nonce) { return; }
            agent.active.take().unwrap().1.flush()
        };
        // Match the output parser's lock order: grid before parser.
        let mut term = self.term.lock();
        self.parser.lock().advance(&mut *term, &remaining);
    }

    /// Says whether this terminal has the focus: it is the focused element of the active window of
    /// the foreground application. A program that turned focus reporting on (`?1004h`) is told when
    /// that changes; see [`TerminalEvent::FocusReport`].
    pub fn set_focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        // The owner tells this on every frame: only a change is worth a look at the mode.
        if self.focus.focused() == focused {
            return;
        }
        self.focus.set_focused(focused);
        self.send_focus_report(cx);
    }

    fn send_focus_report(&mut self, cx: &mut Context<Self>) {
        let reporting = self.backend.is_some()
            && self.ended.is_none()
            && !self.replay
            && self.mode().contains(TermMode::FOCUS_IN_OUT);
        if let Some(report) = self.focus.poll(reporting) {
            self.write_raw(report.bytes());
            cx.emit(TerminalEvent::FocusReport(report.focused()));
        }
    }

    /// Sends user input to the process, snapping the view back to the prompt.
    pub fn input(&mut self, bytes: &[u8]) {
        if self.replay{return;}
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
        }
        {
            let source=self.raw_grid.lock();
            if let Some(source)=source.as_ref(){self.capture.lock().input(&source.term,bytes);}
            else {self.capture.lock().input(&*self.term.lock(),bytes);}
        }
        self.backend.as_mut().ok_or(std::io::ErrorKind::NotConnected)?.write(bytes)
    }

    pub fn paste(&mut self, text: &str) {
        let mode = self.paste_mode();
        self.input(paste_payload(text, &mode).as_bytes());
    }

    /// Checks connection and paste mode before a multi-target send starts.
    pub fn validate_command(&self, command: &str) -> Result<(), &'static str> {
        if self.backend.is_none() || self.ended.is_some() { return Err("会话未连接，无法发送"); }
        crate::send::validate(command, &self.paste_mode())
    }

    /// Sends a reviewed command as a paste followed by a separate carriage return.
    pub fn send_command(&mut self, command: &str) -> Result<(), &'static str> {
        self.validate_command(command)?;
        crate::send::send(command, &self.paste_mode(), |bytes| self.try_input(bytes))
    }

    pub fn set_bracketed_paste(&mut self,enabled:bool){self.bracketed_paste=enabled;}
    pub fn set_prompt_highlighting(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.prompt_highlighter.enabled = enabled;
        cx.notify();
    }
    fn paste_mode(&self)->TermMode{let source=self.raw_grid.lock();let mode=source.as_ref().map_or_else(||self.mode(),|s|*s.term.mode());crate::send::effective_mode(mode,self.bracketed_paste)}
    pub fn start_local(&mut self,options:PtyOptions,cx:&mut Context<Self>){self.local=Some(options);self.spawn_local(cx);}
    pub fn start_recording(&mut self,path:std::path::PathBuf,file:std::fs::File,header:&crate::cast::Header)->std::io::Result<()>{
        // Lock the grid before the tap, matching GridSink's output ordering.
        let source = self.raw_grid.lock();
        if let Some(source) = source.as_ref() {
            let seed = crate::archive::cursor_line_seed(&source.term);
            self.output_capture.lock().start(path, file, header, seed.as_deref())
        } else {
            let term = self.term.lock();
            let seed = crate::archive::cursor_line_seed(&*term);
            self.output_capture.lock().start(path, file, header, seed.as_deref())
        }
    }
    pub fn stop_recording(&mut self)->std::io::Result<Option<crate::capture_output::RecordingResult>>{self.output_capture.lock().stop()}
    pub fn recording_seconds(&self)->Option<f64>{self.output_capture.lock().seconds()}
    pub fn recording_duration(&self)->Option<f64>{self.output_capture.lock().playback_duration()}
    pub fn flush_archives(&mut self)->Option<String>{let mut c=self.output_capture.lock();c.flush();c.error()}
    pub fn set_logging(&mut self,root:Option<std::path::PathBuf>,host:&str){
        self.output_capture.lock().stop_logging();
        if let Some(root)=root{
            let source=self.raw_grid.lock();
            if let Some(source)=source.as_ref() {
                let writer=crate::session_log::LogWriter::new(root,host);
                if source.term.mode().contains(TermMode::ALT_SCREEN){self.output_capture.lock().defer_logging(writer);}
                else {let mut tracker=crate::session_log::LineTracker::new(self.size.cols,self.size.rows);tracker.seed(&source.term);self.output_capture.lock().logging(tracker,writer);}
                return;
            }
            let term=self.term.lock();
            let writer=crate::session_log::LogWriter::new(root,host);
            if term.mode().contains(TermMode::ALT_SCREEN){
                // The primary grid is private to alacritty. Seed once it becomes visible again;
                // swapping the live grid to inspect it would destroy an application's screen.
                self.output_capture.lock().defer_logging(writer);
            }else{
                let mut tracker=crate::session_log::LineTracker::new(self.size.cols,self.size.rows);
                tracker.seed(&*term);self.output_capture.lock().logging(tracker,writer);
            }
        }
    }
    pub fn finalize_log(&mut self){self.output_capture.lock().stop_logging();}
    pub fn is_replay(&self)->bool{self.replay}
    pub fn replay_reset(&mut self,cols:u16,rows:u16){self.replay=true;self.replay_timestamp=None;self.replay_times=Default::default();self.prompt_idle=None;self.prompt_highlighter.reset();self.size.cols=cols;self.size.rows=rows;let term=Term::new(Config::default(),&self.size,Listener(self.tx.clone(), self.generation.clone()));*self.term.lock()=term;*self.parser.lock()=Processor::new();}
    pub fn replay_event(&mut self, event: &crate::cast::Event, timestamp: Option<i64>, cx: &mut Context<Self>) {
        self.replay_timestamp = timestamp;
        match &event.data {
            crate::cast::Data::Output(text) => {
                let mut term = self.term.lock();
                self.replay_times.advance(&mut *term, &mut self.parser.lock(), text.as_bytes(), event.recorded_at);
                if event.recorded_at == 0. { self.prompt_highlighter.learn_replay_seed(&*term); }
            }
            crate::cast::Data::Resize(cols, rows) => {
                let size = TerminalSize { cols: *cols, rows: *rows, ..self.size };
                self.replay_times.resize(&mut *self.term.lock(), size);
                self.size = size;
            }
        }
        cx.notify();
    }
    /// Run once after a batch/seek, including prompts that moved into scrollback.
    pub fn learn_replay_prompts(&mut self) { self.prompt_highlighter.learn_replay(&*self.term.lock()); }
    pub fn screen_text(&self)->String{crate::replay::screen_text(&*self.term.lock())}
    /// Rendered cells only, including scrollback above the current viewport.
    pub fn recent_displayed_text(&self,lines:usize)->String{crate::replay::recent_text(&*self.term.lock(),lines)}

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

    /// Call before/after a continuous layout drag. Release flushes from the next
    /// measured bounds, including a mouse-up that arrives before the last paint.
    pub fn set_interactive_resize(&mut self, active: bool) {
        self.resize_throttle.set_dragging(active);
    }

    pub(crate) fn resize_pending(&self) -> bool { self.resize_throttle.pending() }

    pub fn set_size(&mut self, size: TerminalSize) {
        let grid_changed = (size.cols, size.rows) != (self.size.cols, self.size.rows);
        self.size = size;
        if grid_changed {
            self.output_capture.lock().resize(size.cols,size.rows);
            if let Some(source)=self.raw_grid.lock().as_mut(){source.term.resize(size);}
            self.term.lock().resize(size);
        }
        if let Some(dims) = self.resize_throttle.update(size.dims(), self.resize_clock.elapsed())
            && let Some(backend) = self.backend.as_mut()
            && let Err(err) = backend.resize(dims.cols, dims.rows, dims.pixel_width, dims.pixel_height)
        {
            log::warn!("backend resize failed: {err}");
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
        if self.replay{return;}
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

    /// Destructive actions must not reuse the title's 500 ms process cache.
    pub fn close_activity(&self) -> shellmate_core::io::CloseActivity {
        use shellmate_core::io::CloseActivity;
        let Some(backend) = &self.backend else { return CloseActivity::Idle };
        let activity = backend.close_activity();
        if activity == CloseActivity::Idle
            && (self.mode().contains(TermMode::ALT_SCREEN)
                || self.capture.lock().running()
                || self.agent.lock().active.as_ref().is_some_and(|(_, scanner)| scanner.running()))
        {
            return CloseActivity::Unknown;
        }
        activity
    }

    /// The working directory the shell last reported with OSC 7, if it ever did since the
    /// current backend was attached.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Whether this attached backend has produced bytes, excluding local connection notices.
    pub fn has_backend_output(&self) -> bool {
        self.agent.lock().last_output.is_some()
    }

    pub fn child_pid(&self) -> Option<i32> {
        self.backend.as_ref()?.child_pid()?.try_into().ok()
    }

    pub fn last_output(&self) -> Option<Instant> { self.agent.lock().last_output }

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
    fn geometry_drives_full_split_flipped_and_resized_pty_dimensions() {
        let grid = |w, h| super::TerminalSize::from_geometry(w, h, gpui::px(8.), gpui::px(20.)).dims();
        // Width/height here are each terminal element's measured content area.
        assert_eq!((grid(960.,600.).cols,grid(960.,600.).rows),(120,30));
        assert_eq!((grid(480.,600.).cols,grid(480.,600.).rows),(60,30));
        assert_eq!((grid(960.,300.).cols,grid(960.,300.).rows),(120,15));
        assert_eq!((grid(400.,240.).cols,grid(400.,240.).rows),(50,12));
        assert_eq!((grid(0.,-1.).cols,grid(0.,-1.).rows),(2,1));
        assert_eq!((grid(479.9,299.9).cols,grid(479.9,299.9).rows),(59,14));
    }

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
        capture:Arc<Mutex<Capture>>,
        raw_grid:Arc<Mutex<Option<RawGrid>>>,
        output_capture:Arc<Mutex<crate::capture_output::OutputCapture>>,
        agent: Arc<Mutex<crate::agent::AgentState>>,
    }

    fn fixture(generation: u64) -> Fixture {
        let (tx, rx) = async_channel::unbounded();
        let size = TerminalSize::default();
        let current = Arc::new(AtomicU64::new(generation));
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, Listener(tx.clone(), current.clone()))));
        let pending = Arc::new(AtomicBool::new(false));
        let capture=Arc::new(Mutex::new(Capture::default()));
        let raw_grid=Arc::new(Mutex::new(None));
        let output_capture=Arc::new(Mutex::new(crate::capture_output::OutputCapture::default()));
        let agent = Arc::new(Mutex::new(crate::agent::AgentState::default()));
        let sink = OutputSink::new(GridSink {
            term: term.clone(),
            parser: Arc::new(Mutex::new(Processor::new())),
            capture:capture.clone(),
            raw_grid:raw_grid.clone(),
            output_capture:output_capture.clone(),
            agent: agent.clone(),
            alive: Arc::new(Mutex::new(Arc::new(AtomicBool::new(true)))),
            osc7: Mutex::new(Osc7Scanner::new()),
            notify: Mutex::new(NotifyScanner::new()),
            pending: pending.clone(),
            tx,
            generation,
            current: current.clone(),
        });
        Fixture { term, rx, current, pending, sink,capture,raw_grid,output_capture,agent }
    }

    #[test]
    fn prompt_learning_uses_shared_output_clock_for_normal_and_serial_streams() {
        for transformed in [false, true] {
            let f = fixture(1);
            let bytes = b"root@host:~# ";
            if transformed { f.sink.transformed_output(bytes, bytes); }
            else { f.sink.output(bytes); }
            let last = f.agent.lock().last_output.expect("both output paths update the clock");
            let term = f.term.lock();
            let mut highlighter = crate::prompt::PromptHighlighter::default();
            assert!(!highlighter.learn(&*term, Some(last), last + Duration::from_millis(99)));
            assert!(highlighter.learn(&*term, Some(last), last + crate::prompt::QUIET));
            assert!(highlighter.row(&*term, Line(0)).is_some());
        }
    }

    #[test]
    fn agent_real_pty_alt_screen_is_busy_and_marker_never_renders() {
        use shellmate_core::{Program, Secret};
        let f = fixture(1);
        let root = tempfile::tempdir().unwrap();
        let program = Program { executable: "/bin/sh".into(), args: vec!["-i".into()],
            env: [("HOME".into(),Secret::new(root.path().to_str().unwrap())), ("ENV".into(),Secret::new("/dev/null")), ("PS1".into(),Secret::new("agent-test$ "))].into(), cwd: Some(root.path().into()) };
        let sink=f.sink.clone();
        let mut pty=LocalPty::spawn_program(&program,TerminalSize::default().dims(),move|b|sink.output(b), |_|{}).unwrap();
        let started=Instant::now();
        while f.agent.lock().last_output.is_none() {assert!(started.elapsed()<Duration::from_secs(3));std::thread::sleep(Duration::from_millis(10));}
        pty.write(b"printf '\\033[?1049h'\r").unwrap();
        while !f.term.lock().mode().contains(TermMode::ALT_SCREEN) {assert!(started.elapsed()<Duration::from_secs(3));std::thread::sleep(Duration::from_millis(10));}
        std::thread::sleep(Duration::from_millis(350));
        assert!(f.agent.lock().busy(f.term.lock().mode().contains(TermMode::ALT_SCREEN)));
        pty.write(b"printf '\\033[?1049l'\r").unwrap();
        while f.term.lock().mode().contains(TermMode::ALT_SCREEN) {assert!(started.elapsed()<Duration::from_secs(3));std::thread::sleep(Duration::from_millis(10));}
        let (scanner,handle)=crate::agent::MarkerScanner::new("aB3dE6gH9jK2");
        f.agent.lock().active=Some(("aB3dE6gH9jK2".into(),scanner));
        pty.write(&crate::agent::command_line("printf 'captured\\n'", "aB3dE6gH9jK2")).unwrap();
        while handle.snapshot().exit_code.is_none() {assert!(started.elapsed()<Duration::from_secs(3));std::thread::sleep(Duration::from_millis(10));}
        assert_eq!(handle.snapshot().exit_code,Some(0));
        assert!(String::from_utf8_lossy(&handle.snapshot().bytes).contains("captured"));
        pty.close();
    }

    #[test]
    fn serial_display_transforms_preserve_raw_recording_and_command_history() {
        let f=fixture(1);
        let root=std::env::temp_dir().join(format!("shellmate-serial-capture-{}",shellmate_core::HostId::generate()));
        let(path,file)=crate::archive::recording_file(&root,"fixture",1_700_000_000).unwrap();
        f.output_capture.lock().start(path.clone(),file,&crate::cast::Header::new(80,24),None).unwrap();
        let log_root=root.join("logs");
        f.output_capture.lock().logging(crate::session_log::LineTracker::new(80,24),crate::session_log::LogWriter::new(log_root.clone(),"fixture"));
        f.sink.transformed_output(b"device> ",b"HEX display\r\n");
        {let raw=f.raw_grid.lock();f.capture.lock().input(&raw.as_ref().unwrap().term,b"echo hello");}
        f.sink.transformed_output(b"echo hello",b"65 63 68 6F\r\n");
        {let raw=f.raw_grid.lock();f.capture.lock().input(&raw.as_ref().unwrap().term,b"\r");}
        f.sink.transformed_output(b"\r\nhello\r\n",b"ASCII display\r\n");
        let commands=drain(&f.rx).into_iter().filter_map(|m|if let Msg::Command{command,..}=m{Some(command)}else{None}).collect::<Vec<_>>();
        assert_eq!(commands,["echo hello"]);assert_eq!(row(&f.term,0),"HEX display");
        // Flushing an incomplete HEX row has display bytes but no new raw bytes.
        f.sink.transformed_output(&[],b"last partial HEX row\r\n");
        f.output_capture.lock().stop().unwrap();
        let cast=crate::cast::Cast::open(&path).unwrap();
        let recorded=cast.events.iter().filter_map(|e|if let crate::cast::Data::Output(s)=&e.data{Some(s.as_str())}else{None}).collect::<String>();
        assert_eq!(recorded,"device> echo hello\r\nhello\r\n");
        f.output_capture.lock().stop_logging();
        let path=std::fs::read_dir(log_root.join("fixture")).unwrap().next().unwrap().unwrap().path();
        let logged=std::fs::read_to_string(path).unwrap();
        assert!(logged.contains("device> echo hello"));assert!(logged.contains("hello"));assert!(!logged.contains("HEX"));
        std::fs::remove_dir_all(root).unwrap();
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

    fn notifications(rx: &async_channel::Receiver<Msg>) -> Vec<(u64, Option<String>, String)> {
        drain(rx)
            .into_iter()
            .filter_map(|m| match m {
                Msg::Notify { generation, title, body } => Some((generation, title, body)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_requested_notification_reaches_the_ui_and_the_grid_is_unchanged() {
        let f = fixture(4);
        f.sink.output(b"a\x1b]9;hello\x07b\x1b]777;notify;Title;Body\x1b\\c\x1b]9;4;1;50\x07");
        assert_eq!(
            notifications(&f.rx),
            [(4, None, "hello".to_string()), (4, Some("Title".to_string()), "Body".to_string())],
            "the progress report is not one"
        );
        // The parser still saw everything, exactly as without the scanner.
        assert_eq!(row(&f.term, 0), "abc");
    }

    #[test]
    fn a_notification_cut_between_reads_is_still_reported_once() {
        let f = fixture(1);
        f.sink.output(b"x\x1b]9;in two");
        f.sink.output(b" parts\x1b");
        assert!(notifications(&f.rx).is_empty(), "not complete yet");
        f.sink.output(b"\\y");
        assert_eq!(notifications(&f.rx), [(1, None, "in two parts".to_string())]);
        assert_eq!(row(&f.term, 0), "xy");
    }

    #[test]
    fn output_of_a_replaced_backend_reports_no_notification() {
        let f = fixture(1);
        f.current.store(2, Ordering::Release);
        f.sink.output(b"\x1b]9;stale\x07");
        assert!(notifications(&f.rx).is_empty());
    }

    #[test]
    fn only_what_the_screen_shows_can_ask_for_a_notification() {
        // A serial display (hex, timestamps, ...) replaces the device's bytes with its own.
        let f = fixture(1);
        f.sink.transformed_output(b"\x1b]9;from the device\x07", b"1B 5D 39\r\n");
        assert!(notifications(&f.rx).is_empty());
        f.sink.transformed_output(b"\x1b]9;plain\x07", b"\x1b]9;plain\x07");
        assert_eq!(notifications(&f.rx), [(1, None, "plain".to_string())]);
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

impl Drop for Terminal { fn drop(&mut self) { self.alive.lock().store(false, Ordering::Release); } }
