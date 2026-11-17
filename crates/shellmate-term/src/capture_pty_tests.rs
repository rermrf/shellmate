//! Real shells through the production GridSink/parser/capture path, without gpui or user files.
use super::*;
use alacritty_terminal::index::Line;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

struct TestHome(PathBuf);
impl TestHome {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "shellmate-capture-{}",
            shellmate_core::HostId::generate()
        ));
        std::fs::create_dir(&path).unwrap();
        for name in ["history", "inputrc"] {
            std::fs::write(path.join(name), "").unwrap();
        }
        std::fs::write(path.join("capture-completion.txt"), "file output\n").unwrap();
        std::fs::write(path.join("capture-pager.txt"), "pager-line\n".repeat(100)).unwrap();
        Self(path)
    }
}
impl Drop for TestHome {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

struct Shell {
    term: Arc<FairMutex<Term<Listener>>>,
    capture: Arc<Mutex<Capture>>,
    messages: async_channel::Receiver<Msg>,
    output: mpsc::Receiver<()>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    reader: Option<JoinHandle<()>>,
    history: Vec<String>,
    _home: TestHome,
}
impl Shell {
    fn new(program: &Path) -> Self {
        let home = TestHome::new();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(program);
        if program.file_name().unwrap() == "bash" {
            command.args(["--norc", "--noprofile", "-i"]);
        } else {
            command.args(["-f", "-i"]);
        }
        // Only this child gets a temporary HOME. No ambient BASH_ENV, ZDOTDIR, INPUTRC,
        // shell functions, or user history/configuration can enter the test.
        command.env_clear();
        command.cwd(&home.0);
        command.env("HOME", &home.0);
        command.env("ZDOTDIR", &home.0);
        command.env("HISTFILE", home.0.join("history"));
        command.env("INPUTRC", home.0.join("inputrc"));
        command.env("HISTSIZE", "1000");
        command.env("SAVEHIST", "0");
        command.env("HISTCONTROL", "");
        command.env("LESSHISTFILE", "-");
        command.env("TERM", "xterm-256color");
        command.env("PATH", "/usr/bin:/bin");
        command.env("LANG", "C");
        command.env("PS1", "SM> ");
        command.env("PS2", "CONT> ");
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, messages) = async_channel::unbounded();
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &TerminalSize {
                cols: 100,
                ..Default::default()
            },
            Listener(tx.clone()),
        )));
        let capture = Arc::new(Mutex::new(Capture::default()));
        let sink = GridSink {
            agent: Arc::new(Mutex::new(crate::agent::AgentState::default())),
            alive: Arc::new(AtomicBool::new(true)),
            raw_grid:Arc::new(Mutex::new(None)),
            term: term.clone(),
            parser: Arc::new(Mutex::new(Processor::new())),
            capture: capture.clone(),
            output_capture: Arc::new(Mutex::new(crate::capture_output::OutputCapture::default())),
            osc7: Mutex::new(Osc7Scanner::new()),
            pending: Arc::new(AtomicBool::new(false)),
            tx,
            generation: 1,
            current: Arc::new(AtomicU64::new(1)),
        };
        let (wake, output) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut bytes = [0u8; 1024];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => {
                        sink.output(&bytes[..n]);
                        let _ = wake.send(());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        let mut this = Self {
            term,
            capture,
            messages,
            output,
            writer,
            master: pair.master,
            child,
            reader: Some(reader),
            history: vec![],
            _home: home,
        };
        this.wait("initial prompt", |s| s.prompt());
        this
    }

    fn drain(&mut self) {
        while let Ok(message) = self.messages.try_recv() {
            match message {
                Msg::Command { command, .. } => self.history.push(command),
                Msg::Alac(AlacEvent::PtyWrite(reply)) => {
                    self.writer.write_all(reply.as_bytes()).unwrap();
                    self.writer.flush().unwrap();
                }
                _ => {}
            }
        }
    }
    fn row(&self) -> String {
        let term = self.term.lock();
        let row = &term.grid()[term.grid().cursor.point.line];
        (0..term.columns())
            .map(|col| row[Column(col)].c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }
    fn prompt(&self) -> bool {
        let term = self.term.lock();
        let cursor = term.grid().cursor.point;
        let row = &term.grid()[cursor.line];
        // A prompt redraw may arrive in several reads. Wait through the final trailing space,
        // not just the trimmed text that can already look like the finished prompt.
        cursor.column.0 == 4
            && (0..term.columns())
                .map(|col| row[Column(col)].c)
                .collect::<String>()
                .trim_end()
                == "SM>"
    }
    fn echo_disabled(&self) -> bool {
        let fd = self.master.as_raw_fd().expect("local PTY exposes termios");
        let mut mode = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: fd belongs to this fixture's live PTY. tcgetattr initializes mode on success.
        assert_eq!(
            unsafe { libc::tcgetattr(fd, mode.as_mut_ptr()) },
            0,
            "read fixture PTY mode"
        );
        unsafe { mode.assume_init() }.c_lflag & libc::ECHO == 0
    }
    fn has_line(&self, expected: &str) -> bool {
        let term = self.term.lock();
        (0..term.screen_lines()).any(|line| {
            (0..term.columns())
                .map(|col| term.grid()[Line(line as i32)][Column(col)].c)
                .collect::<String>()
                .trim_end()
                == expected
        })
    }
    fn alternate(&self) -> bool {
        self.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }
    fn wait(&mut self, reason: &str, check: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            self.drain();
            if check(self) {
                // GridSink publishes capture messages under the grid lock. The predicate may
                // have observed output produced after the first drain; settle those messages too.
                self.drain();
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for {reason}; cursor row: {:?}",
                self.row()
            );
            self.output
                .recv_timeout(remaining.min(Duration::from_millis(50)))
                .ok();
        }
    }
    fn input(&mut self, bytes: &[u8]) {
        // Same lock ordering and call order as Terminal::try_input, before any backend write.
        {
            let term = self.term.lock();
            self.capture.lock().input(&term, bytes);
        }
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }
    fn type_text(&mut self, text: &str) {
        for bytes in text.as_bytes().chunks(2) {
            self.input(bytes);
        }
    }
    fn run(&mut self, command: &str) {
        let count = self.history.len();
        self.type_text(command);
        self.input(b"\r");
        self.wait("executed command and prompt", |s| {
            s.history.len() == count + 1 && s.prompt()
        });
        assert_eq!(self.history.last().unwrap(), command);
    }
}
impl Drop for Shell {
    fn drop(&mut self) {
        // A panic while less/vi owns the terminal must not leak that foreground process group.
        if let Some(fd) = self.master.as_raw_fd() {
            // SAFETY: fd belongs to this test's PTY; only its foreground process group is signalled.
            let group = unsafe { libc::tcgetpgrp(fd) };
            if group > 0 {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn executable(name: &str) -> Option<PathBuf> {
    ["/bin", "/usr/bin"]
        .into_iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
}

fn exercise(name: &str) {
    let Some(program) = executable(name) else {
        eprintln!("skipping {name}: shell is not installed");
        return;
    };
    let mut shell = Shell::new(&program);
    let mut expected = vec!["echo plain".to_string()];
    shell.run("echo plain");
    // Recall exactly the previous command: it executes, but consecutive history is deduplicated.
    shell.input(b"\x1b[A");
    shell.wait("recalled command on screen", |s| {
        s.row() == "SM> echo plain"
    });
    shell.input(b"\r");
    shell.wait("prompt after recall", |s| s.prompt());
    assert_eq!(shell.history, expected);
    shell.run("echo other");
    expected.push("echo other".into());
    shell.input(b"\x1b[A");
    shell.input(b"\x1b[A");
    shell.wait("nonconsecutive recalled command", |s| {
        s.row() == "SM> echo plain"
    });
    shell.input(b"\r");
    expected.push("echo plain".into());
    shell.wait("history after recall", |s| {
        s.history.len() == expected.len() && s.prompt()
    });
    assert_eq!(shell.history, expected);

    shell.type_text("cat capture-comp");
    shell.input(b"\t");
    shell.wait("completed filename on screen", |s| {
        s.row() == "SM> cat capture-completion.txt"
    });
    shell.input(b"\r");
    expected.push("cat capture-completion.txt".into());
    shell.wait("history after completion", |s| {
        s.history.len() == expected.len() && s.prompt()
    });
    assert_eq!(shell.history, expected);

    let read = "printf 'Password: '; read -s value; printf '\\nREAD-DONE\\n'";
    shell.type_text(read);
    shell.input(b"\r");
    expected.push(read.into());
    // printf writes the prompt before read changes termios. Under scheduler load the two
    // operations can be far apart: prompt visibility alone does not mean echo is disabled.
    shell.wait("read command captured and PTY echo disabled", |s| {
        s.row() == "Password:" && s.echo_disabled() && s.history == expected
    });
    shell.type_text("synthetic-unrecorded-password");
    shell.input(b"\r");
    shell.wait("read completed and full prompt redrawn", |s| {
        s.has_line("READ-DONE") && s.prompt()
    });
    assert_eq!(
        shell.history, expected,
        "echo-off input must never enter history"
    );

    let pager = if executable("less").is_some() {
        Some(("less capture-pager.txt", false))
    } else if executable("vim").is_some() {
        Some(("vim -u NONE -i NONE -n capture-pager.txt", true))
    } else {
        None
    };
    if let Some((command, vim)) = pager {
        shell.type_text(command);
        shell.input(b"\r");
        expected.push(command.into());
        shell.wait("pager alternate screen", |s| s.alternate());
        if vim {
            shell.type_text(":set number");
            shell.input(b"\r");
            shell.type_text(":q!");
            shell.input(b"\r");
        } else {
            shell.type_text("/pager-line");
            shell.input(b"\r");
            shell.input(b"q");
        }
        shell.wait("prompt after pager", |s| !s.alternate() && s.prompt());
        assert_eq!(
            shell.history, expected,
            "alternate-screen input must never enter history"
        );
    } else {
        eprintln!("skipping alternate-screen portion: less/vim is not installed");
    }
    // Also ensure the secret did not accidentally remain in the terminal grid.
    let term = shell.term.lock();
    for row in -(term.grid().history_size() as i32)..term.screen_lines() as i32 {
        let text: String = (0..term.columns())
            .map(|c| term.grid()[Line(row)][Column(c)].c)
            .collect();
        assert!(!text.contains("synthetic-unrecorded-password"));
    }
}

#[test]
fn bash_real_pty_history() {
    exercise("bash");
}
#[test]
fn zsh_real_pty_history() {
    exercise("zsh");
}

fn agent_shell_history(name: &str, setting: &str) {
    let Some(program) = executable(name) else {
        eprintln!("skipping {name}: shell is not installed");
        return;
    };
    let mut shell = Shell::new(&program);
    shell.run(setting);
    shell.run("echo user-before");
    shell.type_text("echo discarded-input");
    shell.wait("partially typed command", |s| s.row() == "SM> echo discarded-input");
    let line = crate::agent::command_line("printf 'AI-DONE\\n'", "aB3dE6gH9jK2");
    assert!(line.starts_with(b"\x15 printf"));
    shell.input(&line);
    shell.wait("AI command completed", |s| s.has_line("AI-DONE") && s.prompt());
    // zsh removes an ignored command once the next command is entered.
    shell.run("echo user-after");
    shell.run("fc -ln 1 > shell-history.txt");
    let history = std::fs::read_to_string(shell._home.0.join("shell-history.txt")).unwrap();
    assert!(history.contains("echo user-before"), "{history}");
    assert!(history.contains("echo user-after"), "{history}");
    assert!(!history.contains("AI-DONE"), "{history}");
    assert!(!history.contains("6973"), "{history}");
    assert!(!history.contains("aB3dE6gH9jK2"), "{history}");
    assert!(!history.contains("discarded-input"), "{history}");
}

#[test]
fn bash_agent_wrapper_is_excluded_from_shell_history() {
    for mode in ["HISTCONTROL=ignorespace", "HISTCONTROL=ignoreboth"] {
        agent_shell_history("bash", mode);
    }
}

#[test]
fn zsh_agent_wrapper_is_excluded_from_shell_history() {
    agent_shell_history("zsh", "setopt HIST_IGNORE_SPACE");
}
