//! Session lifecycle of `Terminal` (attach, exit, disconnect, restart) with a scripted backend.
//!
//! gpui's headless test support cannot be built with the registry mirror in use, so this
//! runs the model inside a real, windowless gpui application (`harness = false`). That
//! needs a macOS login session, hence it is opt-in:
//!
//! ```text
//! SHELLMATE_GUI_TESTS=1 cargo test -p shellmate-term --test terminal_session
//! ```

use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{AppContext as _, AsyncApp, Entity};
use shellmate_term::{OutputSink, SessionEnd, Terminal, TerminalBackend, TerminalEvent, TerminalPalette, TerminalSize};

#[derive(Default)]
struct Calls {
    writes: Vec<u8>,
    resizes: Vec<(u16, u16)>,
    closed: bool,
}

struct Fake(Arc<Mutex<Calls>>);

impl TerminalBackend for Fake {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.lock().unwrap().writes.extend_from_slice(bytes);
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16, _: u16, _: u16) -> io::Result<()> {
        self.0.lock().unwrap().resizes.push((cols, rows));
        Ok(())
    }

    fn close(&mut self) {
        self.0.lock().unwrap().closed = true;
    }
}

/// Attaches a fake backend; returns what it records and the sink bound to it.
fn attach(terminal: &Entity<Terminal>, cx: &mut AsyncApp) -> (Arc<Mutex<Calls>>, OutputSink) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let slot = Rc::new(RefCell::new(None));
    let (recorded, taken) = (calls.clone(), slot.clone());
    let attached = terminal.update(cx, |terminal, cx| {
        terminal.attach(
            move |sink, _| {
                *taken.borrow_mut() = Some(sink);
                Ok(Box::new(Fake(recorded)))
            },
            cx,
        )
    });
    assert!(attached);
    let sink = slot.borrow_mut().take().unwrap();
    (calls, sink)
}

struct Harness {
    terminal: Entity<Terminal>,
    events: Rc<RefCell<Vec<TerminalEvent>>>,
}

impl Harness {
    fn new(cx: &mut gpui::App) -> Self {
        let terminal = cx.new(|cx| Terminal::new(TerminalPalette::dark(), cx));
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        cx.subscribe(&terminal, move |_, event: &TerminalEvent, _| {
            if !matches!(event, TerminalEvent::Output) {
                sink.borrow_mut().push(event.clone());
            }
        })
        .detach();
        Self { terminal, events }
    }

    /// Events other than `Output` since the last call.
    fn take_events(&self) -> Vec<TerminalEvent> {
        std::mem::take(&mut *self.events.borrow_mut())
    }

    fn shows(&self, cx: &mut AsyncApp, text: &str) -> bool {
        self.terminal.read_with(cx, |terminal, _| !terminal.search(text).is_empty())
    }

    /// Waits (the terminal consumes backend notifications on the UI executor) until `text` is on screen.
    async fn wait_for_text(&self, cx: &mut AsyncApp, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.shows(cx, text) {
            assert!(Instant::now() < deadline, "timed out waiting for {text:?} on screen");
            cx.background_executor().timer(Duration::from_millis(5)).await;
        }
    }

    /// Lets queued backend notifications that must NOT have an effect be processed.
    async fn settle(&self, cx: &mut AsyncApp) {
        cx.background_executor().timer(Duration::from_millis(100)).await;
    }
}

async fn run(cx: &mut AsyncApp, harness: Harness) {
    let terminal = harness.terminal.clone();

    // Output reaches the grid; input and resizes reach the backend.
    let (calls, sink) = attach(&terminal, cx);
    sink.output("你好 shellmate".as_bytes());
    harness.wait_for_text(cx, "你好 shellmate").await;
    terminal.update(cx, |t, _| t.input(b"ls\r"));
    assert_eq!(calls.lock().unwrap().writes, b"ls\r");
    terminal.update(cx, |t, _| t.set_size(TerminalSize { cols: 100, rows: 30, ..TerminalSize::default() }));
    assert_eq!(calls.lock().unwrap().resizes, [(100, 30)]);
    assert!(harness.take_events().is_empty());

    // Remote exit: reported, shown, backend released. Enter asks the owner to reconnect.
    sink.exited(Some(3));
    harness.wait_for_text(cx, "按回车重新连接").await;
    assert_eq!(harness.take_events(), [TerminalEvent::Exited(Some(3))]);
    assert!(harness.shows(cx, "远程会话已退出，代码 3"));
    assert!(calls.lock().unwrap().closed, "the ended backend is closed and dropped");
    terminal.read_with(cx, |t, _| {
        assert!(t.is_exited() && !t.is_attached() && !t.is_local());
        assert_eq!(t.session_end(), Some(&SessionEnd::Exited(Some(3))));
    });
    terminal.update(cx, |t, cx| t.restart(cx));
    assert_eq!(harness.take_events(), [TerminalEvent::RestartRequested]);

    // The owner reconnects: a new backend is attached and the scrollback is kept.
    let (second_calls, second_sink) = attach(&terminal, cx);
    assert_eq!(harness.take_events(), [TerminalEvent::Restarted]);
    assert!(harness.shows(cx, "远程会话已退出"));
    terminal.read_with(cx, |t, _| assert!(t.is_attached() && !t.is_exited()));

    // A dropped connection is shown and reported; Enter again asks the owner.
    second_sink.disconnected("连接已断开：保活超时");
    harness.wait_for_text(cx, "连接已断开：保活超时").await;
    assert_eq!(harness.take_events(), [TerminalEvent::Disconnected("连接已断开：保活超时".into())]);
    assert!(second_calls.lock().unwrap().closed);
    terminal.read_with(cx, |t, _| {
        assert_eq!(t.session_end(), Some(&SessionEnd::Disconnected("连接已断开：保活超时".into())));
    });
    terminal.update(cx, |t, cx| t.restart(cx));
    assert_eq!(harness.take_events(), [TerminalEvent::RestartRequested]);

    // A replaced backend can no longer touch the terminal.
    let (_, old_sink) = attach(&terminal, cx);
    let (old_calls, new_sink) = attach(&terminal, cx);
    harness.take_events();
    old_sink.output(b"STALE");
    old_sink.exited(Some(1));
    old_sink.disconnected("stale");
    new_sink.output(b"fresh output");
    harness.wait_for_text(cx, "fresh output").await;
    harness.settle(cx).await;
    assert!(!harness.shows(cx, "STALE"));
    assert!(harness.take_events().is_empty(), "late notifications of the old session are ignored");
    terminal.read_with(cx, |t, _| assert!(t.is_attached() && !t.is_exited()));
    assert!(!old_calls.lock().unwrap().closed);

    // A factory that fails ends the session with the error shown.
    let attached = terminal.update(cx, |t, cx| t.attach(|_, _| Err(anyhow::anyhow!("无法打开通道")), cx));
    assert!(!attached);
    assert_eq!(harness.take_events(), [TerminalEvent::Disconnected("无法打开通道".into())]);
    assert!(harness.shows(cx, "无法打开通道") && terminal.read_with(cx, |t, _| t.is_exited()));
}

async fn connecting(cx: &mut AsyncApp, harness: Harness) {
    // The owner shows progress and reports a failure while no backend exists yet.
    harness.terminal.update(cx, |t, cx| {
        t.notice("→ 连接 10.0.1.21:22", cx);
        t.notice("\x1b[32m✓ 主机指纹已校验\x1b[0m", cx);
        t.disconnect("认证失败：服务器拒绝了私钥", cx);
    });
    assert_eq!(harness.take_events(), [TerminalEvent::Disconnected("认证失败：服务器拒绝了私钥".into())]);
    for text in ["→ 连接 10.0.1.21:22", "✓ 主机指纹已校验", "认证失败：服务器拒绝了私钥", "按回车重新连接"]
    {
        assert!(harness.shows(cx, text), "{text} should be on screen");
    }
    harness.terminal.update(cx, |t, cx| t.restart(cx));
    assert_eq!(harness.take_events(), [TerminalEvent::RestartRequested]);

    // Notices never glue onto a half-written line (e.g. a shell prompt).
    let (_, sink) = attach(&harness.terminal, cx);
    sink.output(b"user@host:~$ ");
    harness.wait_for_text(cx, "user@host:~$").await;
    harness.terminal.update(cx, |t, cx| t.notice("✓ 已自动重连", cx));
    assert!(harness.shows(cx, "✓ 已自动重连"));
    assert!(!harness.shows(cx, "user@host:~$ ✓"), "the notice starts on its own line");
}

fn main() {
    if std::env::var_os("SHELLMATE_GUI_TESTS").is_none() {
        println!("terminal_session: skipped (set SHELLMATE_GUI_TESTS=1 to run; needs a macOS login session)");
        return;
    }
    // A failed assertion inside the app would otherwise leave the event loop running.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(60));
        eprintln!("terminal_session: timed out");
        std::process::exit(1);
    });
    std::panic::set_hook(Box::new(|info| {
        eprintln!("terminal_session: FAILED: {info}");
        std::process::exit(1);
    }));
    gpui::application().run(|cx| {
        let lifecycle = Harness::new(cx);
        let progress = Harness::new(cx);
        cx.spawn(async move |cx| {
            run(cx, lifecycle).await;
            connecting(cx, progress).await;
            println!("terminal_session: ok");
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
