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
    activity: Option<shellmate_core::io::CloseActivity>,
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

    fn close_activity(&self) -> shellmate_core::io::CloseActivity {
        self.0.lock().unwrap().activity.clone().unwrap_or(shellmate_core::io::CloseActivity::Unknown)
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

async fn titles(cx: &mut AsyncApp, harness: Harness) {
    let terminal = harness.terminal.clone();
    let (_, sink) = attach(&terminal, cx);
    // Actual Claude Code title, split even inside the UTF-8 symbol; OSC 0 + BEL.
    for byte in "\x1b]0;✳ Claude Code\x07".as_bytes() { sink.output(&[*byte]); }
    harness.settle(cx).await;
    terminal.read_with(cx, |t, _| assert_eq!(t.title().map(|s| s.as_ref()), Some("✳ Claude Code")));
    assert_eq!(harness.take_events(), [TerminalEvent::TitleChanged]);
    sink.output("\x1b]0;✳ Claude Code\x07".as_bytes());
    harness.settle(cx).await;
    assert!(harness.take_events().is_empty(), "unchanged title must not trigger more UI work");

    // Standard title save/restore around a nested TUI; OSC 2 + ST, including semicolons.
    sink.output("\x1b[22;0t\x1b]2;⠋ 正在处理;项目 α\x1b\\".as_bytes());
    harness.settle(cx).await;
    terminal.read_with(cx, |t, _| assert_eq!(t.title().map(|s| s.as_ref()), Some("⠋ 正在处理;项目 α")));
    assert_eq!(harness.take_events(), [TerminalEvent::TitleChanged]);
    sink.output(b"\x1b[23;0t");
    harness.settle(cx).await;
    terminal.read_with(cx, |t, _| assert_eq!(t.title().map(|s| s.as_ref()), Some("✳ Claude Code")));
    assert_eq!(harness.take_events(), [TerminalEvent::TitleChanged]);

    sink.output(b"\x1b]2;\x07\x1b[22;0t\x1b]2;temporary\x07\x1b[23;0t");
    harness.settle(cx).await;
    terminal.read_with(cx, |t, _| assert!(t.title().is_none()));
    assert_eq!(harness.take_events(), [const { TerminalEvent::TitleChanged }; 3]);

    // Reconnect keeps scrollback but resets title, saved titles and an incomplete OSC.
    sink.output(b"kept scrollback\x1b]2;old app\x07\x1b[22;0t");
    harness.settle(cx).await;
    harness.take_events();
    sink.output(b"\x1b]2;queued stale title\x07\x1b]2;partial");
    let (_, new_sink) = attach(&terminal, cx);
    assert_eq!(harness.take_events(), [TerminalEvent::TitleChanged]);
    harness.settle(cx).await;
    assert!(harness.take_events().is_empty(), "queued titles belong to the old backend");
    terminal.read_with(cx, |t, _| assert!(t.title().is_none()));
    new_sink.output(b"fresh marker\x07\x1b[23;0t");
    harness.settle(cx).await;
    assert!(harness.shows(cx, "kept scrollback") && harness.shows(cx, "fresh marker"));
    terminal.read_with(cx, |t, _| assert!(t.title().is_none(), "old saved title must not return"));
    assert_eq!(harness.take_events(), [TerminalEvent::Bell], "the incomplete old OSC must not consume new output");
    sink.output(b"\x1b]0;late old sink\x07");
    new_sink.output(b"\x1b[22;0t\x1b]0;new app\x07");
    harness.settle(cx).await;
    terminal.read_with(cx, |t, _| assert_eq!(t.title().map(|s| s.as_ref()), Some("new app")));
    assert_eq!(harness.take_events(), [TerminalEvent::TitleChanged]);

    // Popping the initially unset title produces Alacritty's ResetTitle event.
    new_sink.output(b"\x1b[23;0t");
    harness.settle(cx).await;
    terminal.read_with(cx, |t, _| assert!(t.title().is_none()));
    assert_eq!(harness.take_events(), [TerminalEvent::TitleChanged]);
    new_sink.output(b"\x1b]0;new app\x07");
    harness.settle(cx).await;
    harness.take_events();

    new_sink.disconnected("test disconnect");
    harness.settle(cx).await;
    harness.take_events();
    let (_, restarted) = attach(&terminal, cx);
    assert_eq!(harness.take_events(), [TerminalEvent::TitleChanged, TerminalEvent::Restarted]);
    restarted.output(b"\x1b]0;failure cleanup\x07");
    harness.settle(cx).await;
    harness.take_events();
    terminal.update(cx, |t, cx| { t.attach(|_, _| Err(anyhow::anyhow!("test failure")), cx); });
    terminal.read_with(cx, |t, _| assert!(t.title().is_none()));
    assert_eq!(harness.take_events(), [TerminalEvent::TitleChanged, TerminalEvent::Disconnected("test failure".into())]);
}

async fn close_activity(cx: &mut AsyncApp, harness: Harness) {
    use shellmate_core::io::{CloseActivity, ProcessInfo};
    let terminal = &harness.terminal;
    let (calls, sink) = attach(terminal, cx);
    terminal.read_with(cx, |t, _| assert_eq!(t.close_activity(), CloseActivity::Unknown));
    calls.lock().unwrap().activity = Some(CloseActivity::Idle);
    terminal.update(cx, |t, _| { t.foreground_process(); });
    terminal.read_with(cx, |t, _| assert_eq!(t.close_activity(), CloseActivity::Idle));
    let busy = CloseActivity::Busy(vec![ProcessInfo { pid: 42, name: "vim".into(), cwd: None }]);
    calls.lock().unwrap().activity = Some(busy.clone());
    terminal.read_with(cx, |t, _| assert_eq!(t.close_activity(), busy, "close bypasses the foreground cache"));
    calls.lock().unwrap().activity = Some(CloseActivity::Idle);
    sink.output(b"\x1b[?1049h");
    harness.settle(cx).await;
    terminal.read_with(cx, |t, _| assert_eq!(t.close_activity(), CloseActivity::Unknown));
    sink.output(b"\x1b[?1049l\x1b]133;C\x07");
    harness.settle(cx).await;
    terminal.read_with(cx, |t, _| assert_eq!(t.close_activity(), CloseActivity::Unknown));
    sink.output(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    harness.settle(cx).await;
    terminal.read_with(cx, |t, _| assert_eq!(t.close_activity(), CloseActivity::Idle));
    sink.exited(Some(0));
    harness.settle(cx).await;
    terminal.read_with(cx, |t, _| assert_eq!(t.close_activity(), CloseActivity::Idle));
}

async fn focus_and_notifications(cx: &mut AsyncApp, harness: Harness) {
    let terminal = &harness.terminal;
    let (calls, sink) = attach(terminal, cx);
    // Nothing is sent while the program has not asked for focus reports.
    terminal.update(cx, |t, cx| t.set_focused(true, cx));
    terminal.update(cx, |t, cx| t.set_focused(false, cx));
    assert!(calls.lock().unwrap().writes.is_empty());
    assert!(harness.take_events().is_empty());

    // It asks while the terminal has no focus: it is told at once, and only once per change.
    sink.output(b"\x1b[?1004h");
    harness.settle(cx).await;
    assert_eq!(calls.lock().unwrap().writes, b"\x1b[O");
    assert_eq!(harness.take_events(), [TerminalEvent::FocusReport(false)]);
    terminal.update(cx, |t, cx| t.set_focused(false, cx));
    terminal.update(cx, |t, cx| t.set_focused(true, cx));
    terminal.update(cx, |t, cx| t.set_focused(true, cx));
    assert_eq!(calls.lock().unwrap().writes, b"\x1b[O\x1b[I");
    assert_eq!(harness.take_events(), [TerminalEvent::FocusReport(true)]);

    // A new backend has not asked yet, whatever the old program left on.
    let (calls, sink) = attach(terminal, cx);
    terminal.update(cx, |t, cx| t.set_focused(false, cx));
    assert!(calls.lock().unwrap().writes.is_empty());
    assert!(harness.take_events().is_empty());

    // Requested notifications are events; nothing of them reaches the screen. A bell is still a bell.
    sink.output(b"shown\x1b]9;hello\x07\x1b]777;notify;Title;Body\x1b\\\x07\x1b]9;4;1;50\x07");
    harness.settle(cx).await;
    assert_eq!(
        harness.take_events(),
        [
            TerminalEvent::Notify { title: None, body: "hello".into() },
            TerminalEvent::Notify { title: Some("Title".into()), body: "Body".into() },
            TerminalEvent::Bell,
        ]
    );
    assert!(harness.shows(cx, "shown"));
    assert!(!harness.shows(cx, "hello") && !harness.shows(cx, "Body"));
}


async fn agent_interaction(cx:&mut AsyncApp,harness:Harness) {
    use shellmate_core::terminal_interaction::{Action,Controller,Region};
    let terminal=&harness.terminal;let (calls,sink)=attach(terminal,cx);
    sink.output(b"\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b[HMenu\r\n\x1b[7mFirst\x1b[0m\r\nSecond");harness.settle(cx).await;
    let owner=Arc::new(Controller::new("Test Agent".into()));
    let screen=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());assert!(screen.alternate_screen);assert!(terminal.read_with(cx,|t,_|t.agent_busy()));
    let result=terminal.update(cx,|t,cx|t.interaction_input(&screen.screen_id,&[Action::Key{key:"down".into(),repeat:1}],owner.clone(),cx).unwrap());assert_eq!(result.actions_sent,1);assert!(calls.lock().unwrap().writes.ends_with(b"\x1bOB"));
    let other=Arc::new(Controller::new("Other".into()));assert!(terminal.update(cx,|t,cx|t.interaction_input(&result.screen.screen_id,&[Action::Text{text:"x".into()}],other,cx)).is_err());
    terminal.update(cx,|t,_|t.input(b"user"));let before=calls.lock().unwrap().writes.clone();let snapshot=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());assert!(snapshot.paused);
    assert!(terminal.update(cx,|t,cx|t.interaction_input(&snapshot.screen_id,&[Action::Key{key:"enter".into(),repeat:1}],owner.clone(),cx)).is_err());assert_eq!(calls.lock().unwrap().writes,before);
    terminal.update(cx,|t,cx|t.resume_interaction(cx));let screen=terminal.update(cx,|t,_|t.interaction_snapshot(Some(&Region{row:0,column:0,rows:1,columns:20})).unwrap());
    sink.output(b"\x1b[4;1Hclock changes\x1b[6;1H");harness.settle(cx).await;
    let before=calls.lock().unwrap().writes.clone();
    let failure=terminal.update(cx,|t,cx|t.interaction_input_guarded(&screen.screen_id,&[Action::Paste{text:"hello\nworld".into()}],owner.clone(),&[],cx).unwrap_err());
    assert_eq!(failure.code,shellmate_core::terminal_interaction::ErrorCode::ScreenChanged);assert!(failure.screen.as_ref().unwrap().text.contains("clock changes"));assert_eq!(calls.lock().unwrap().writes,before);
    let guard=shellmate_core::terminal_interaction::Guard{region:Region{row:0,column:0,rows:1,columns:20},text:Some("Menu".into()),absent_text:None,cursor:None};
    terminal.update(cx,|t,cx|t.interaction_input_guarded(&screen.screen_id,&[Action::Paste{text:"hello\nworld".into()}],owner.clone(),&[guard],cx).unwrap());
    let base=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());
    let watch=terminal.update(cx,|t,_|t.interaction_observe(Some(&base.screen_id),None,Some(&Region{row:3,column:0,rows:1,columns:20})).unwrap());
    for _ in 0..40{terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());}
    sink.output(b"\x1b[4;1Hclock 2\x1b[K");harness.settle(cx).await;assert!(watch.updates.try_recv().is_ok());
    let now=terminal.update(cx,|t,_|t.interaction_snapshot(Some(&watch.screen.region)).unwrap());assert_ne!(watch.baseline.unwrap().fingerprint,now.fingerprint);

    let screen=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());terminal.update(cx,|t,_|t.set_size(TerminalSize{cols:81,rows:24,..Default::default()}));assert!(terminal.update(cx,|t,cx|t.interaction_input(&screen.screen_id,&[Action::Text{text:"x".into()}],owner.clone(),cx)).is_err());
    let before=calls.lock().unwrap().writes.clone();terminal.update(cx,|t,cx|t.interaction_release(&owner,cx).unwrap());assert_eq!(calls.lock().unwrap().writes,before);assert!(!calls.lock().unwrap().closed);
    let old=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());attach(terminal,cx);assert!(terminal.update(cx,|t,cx|t.interaction_input(&old.screen_id,&[Action::Text{text:"x".into()}],owner,cx)).is_err());
}
async fn managed_exit(cx:&mut AsyncApp,harness:Harness){
    let terminal=&harness.terminal;let (_,sink)=attach(terminal,cx);
    terminal.update(cx,|t,_|t.set_managed_program());
    let controller=Arc::new(shellmate_core::terminal_interaction::Controller::new("Creator".into()));
    terminal.update(cx,|t,cx|t.claim_interaction(controller,cx).unwrap());
    assert!(terminal.read_with(cx,|t,_|t.interaction_status().is_some()));
    terminal.update(cx,|t,_|t.input(b"human"));assert!(terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap()).paused);
    sink.output(b"last frame");harness.wait_for_text(cx,"last frame").await;
    terminal.update(cx,|t,cx|t.stop_interactive_program(cx));sink.exited(Some(9));harness.settle(cx).await;
    let screen=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());assert!(screen.ended);assert!(screen.text.contains("last frame"));assert!(!screen.text.contains("重新连接"));
    harness.take_events();terminal.update(cx,|t,cx|t.restart(cx));assert!(harness.take_events().is_empty());
}
async fn real_tui(cx:&mut AsyncApp,harness:Harness) {
    use shellmate_core::{Program,Secret,terminal_interaction::{Action,Controller}};
    let dir=tempfile::tempdir().unwrap();let file=dir.path().join("editable.txt");std::fs::write(&file,"original\n").unwrap();
    let program=Program{executable:"/usr/bin/vim".into(),args:vec!["-Nu".into(),"NONE".into(),"-n".into(),"--cmd".into(),"set mouse=a ttymouse=sgr".into(),file.to_string_lossy().into()],cwd:Some(dir.path().into()),env:[("HOME".into(),Secret::new(dir.path().to_str().unwrap())),("TERM".into(),Secret::new("xterm-256color"))].into()};
    let terminal=&harness.terminal;terminal.update(cx,|t,cx|{assert!(t.attach(move|sink,size|{let exit=sink.clone();Ok(Box::new(shellmate_term::LocalPty::spawn_program(&program,size,move|b|sink.output(b),move|code|exit.exited(code))?))},cx));});
    harness.wait_for_text(cx,"original").await;harness.settle(cx).await;
    let owner=Arc::new(Controller::new("Editor test".into()));
    let screen=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());
    assert!(screen.mouse_reporting);
    terminal.update(cx,|t,cx|t.interaction_input(&screen.screen_id,&[Action::Click{row:0,column:0,button:Default::default()}],owner.clone(),cx).unwrap());harness.settle(cx).await;
    let screen=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());
    terminal.update(cx,|t,cx|t.interaction_input(&screen.screen_id,&[Action::Text{text:"i".into()}],owner.clone(),cx).unwrap());harness.settle(cx).await;
    let screen=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());terminal.update(cx,|t,cx|t.interaction_input(&screen.screen_id,&[Action::Text{text:"edited ".into()},Action::Key{key:"escape".into(),repeat:1}],owner.clone(),cx).unwrap());harness.wait_for_text(cx,"edited original").await;harness.settle(cx).await;
    let screen=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());terminal.update(cx,|t,cx|t.interaction_input(&screen.screen_id,&[Action::Text{text:":wq".into()},Action::Key{key:"enter".into(),repeat:1}],owner,cx).unwrap());
    let deadline=Instant::now()+Duration::from_secs(5);while std::fs::read_to_string(&file).unwrap()!="edited original\n" {assert!(Instant::now()<deadline);cx.background_executor().timer(Duration::from_millis(10)).await;}
    harness.settle(cx).await;
    // Pager uses the same generic input path; search and exit leave its file intact.
    let lines=(0..100).map(|n|format!("line {n:03}\n")).collect::<String>();std::fs::write(&file,&lines).unwrap();
    let program=Program{executable:"/usr/bin/less".into(),args:vec!["-N".into(),file.to_string_lossy().into()],cwd:Some(dir.path().into()),env:[("HOME".into(),Secret::new(dir.path().to_str().unwrap())),("TERM".into(),Secret::new("xterm-256color"))].into()};
    terminal.update(cx,|t,cx|{assert!(t.attach(move|sink,size|{let exit=sink.clone();Ok(Box::new(shellmate_term::LocalPty::spawn_program(&program,size,move|b|sink.output(b),move|code|exit.exited(code))?))},cx));});harness.wait_for_text(cx,"line 000").await;harness.settle(cx).await;
    let owner=Arc::new(Controller::new("Pager test".into()));let screen=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());
    terminal.update(cx,|t,cx|t.interaction_input(&screen.screen_id,&[Action::Text{text:"/line 080".into()},Action::Key{key:"enter".into(),repeat:1}],owner.clone(),cx).unwrap());harness.wait_for_text(cx,"line 080").await;harness.settle(cx).await;
    let screen=terminal.update(cx,|t,_|t.interaction_snapshot(None).unwrap());terminal.update(cx,|t,cx|t.interaction_input(&screen.screen_id,&[Action::Text{text:"q".into()}],owner,cx).unwrap());harness.settle(cx).await;assert_eq!(std::fs::read_to_string(&file).unwrap(),lines);
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
        let title = Harness::new(cx);
        let closing = Harness::new(cx);
        let focusing = Harness::new(cx);
        let interacting=Harness::new(cx);let real=Harness::new(cx);let managed=Harness::new(cx);
        cx.spawn(async move |cx| {
            run(cx, lifecycle).await;
            connecting(cx, progress).await;
            titles(cx, title).await;
            close_activity(cx, closing).await;
            focus_and_notifications(cx, focusing).await;
            agent_interaction(cx,interacting).await;
            real_tui(cx,real).await;
            managed_exit(cx,managed).await;
            println!("terminal_session: ok");
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
