//! Local pseudo-terminal backend built on `portable-pty`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::thread;

use anyhow::{Context as _, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use shellmate_core::io::{ProcessInfo, PtyDims, TerminalBackend};

/// How to launch the local shell.
#[derive(Clone, Debug)]
pub struct PtyOptions {
    /// Shell program; defaults to `$SHELL`, then `/bin/zsh`.
    pub shell: Option<String>,
    /// Start as a login shell (`-l`) so `~/.zprofile` and PATH match Terminal.app.
    pub login: bool,
    pub cwd: Option<PathBuf>,
}

impl Default for PtyOptions {
    fn default() -> Self {
        Self {
            shell: None,
            login: true,
            cwd: None,
        }
    }
}

impl PtyOptions {
    pub fn shell_path(&self) -> String {
        self.shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "/bin/zsh".to_string())
    }

    /// Display form, e.g. `/bin/zsh -l`.
    pub fn command_line(&self) -> String {
        let shell = self.shell_path();
        if self.login {
            format!("{shell} -l")
        } else {
            shell
        }
    }
}

pub struct LocalPty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl LocalPty {
    /// Spawns the shell. `on_output` runs on a reader thread for every chunk of
    /// output; `on_exit` runs once with the exit code when the shell exits.
    pub fn spawn(
        options: &PtyOptions,
        dims: PtyDims,
        on_output: impl FnMut(&[u8]) + Send + 'static,
        on_exit: impl FnOnce(Option<u32>) + Send + 'static,
    ) -> Result<Self> {
        let mut cmd = CommandBuilder::new(options.shell_path());
        if options.login {
            cmd.arg("-l");
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if let Some(cwd) = options.cwd.clone().or(home) {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "Shellmate");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        // Apps launched from Finder have no locale; without one zsh mangles CJK input.
        if std::env::var("LANG").map(|l| l.is_empty()).unwrap_or(true) {
            cmd.env("LANG", "en_US.UTF-8");
        }

        Self::spawn_builder(cmd, dims, on_output, on_exit)
    }

    /// Launch an external transport client using the same PTY path as shells.
    /// Secret environment values are never included in error messages/argv.
    pub fn spawn_program(
        program: &shellmate_core::Program,
        dims: PtyDims,
        on_output: impl FnMut(&[u8]) + Send + 'static,
        on_exit: impl FnOnce(Option<u32>) + Send + 'static,
    ) -> Result<Self> {
        let mut cmd = CommandBuilder::new(&program.executable);
        cmd.args(&program.args);
        for (key, value) in &program.env {
            cmd.env(key, value.expose());
        }
        if let Some(cwd) = &program.cwd {
            cmd.cwd(cwd);
        }
        Self::spawn_builder(cmd, dims, on_output, on_exit)
    }

    fn spawn_builder(
        cmd: CommandBuilder,
        dims: PtyDims,
        mut on_output: impl FnMut(&[u8]) + Send + 'static,
        on_exit: impl FnOnce(Option<u32>) + Send + 'static,
    ) -> Result<Self> {
        let dims = nonzero_dims(dims);
        let pair = native_pty_system()
            .openpty(pty_size(dims))
            .context("无法分配 PTY")?;
        // Some PTY implementations defer TIOCSWINSZ. Do it explicitly BEFORE
        // exec: mosh-client exits with Error: vector if either dimension is 0.
        pair.master
            .resize(pty_size(dims))
            .context("无法设置 PTY 大小")?;
        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let writer = pair.master.take_writer().context("take pty writer")?;
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|_| anyhow::anyhow!("无法启动终端进程，请检查程序路径和权限"))?;
        drop(pair.slave);
        let mut killer = child.clone_killer();
        let (read_done, drained) = std::sync::mpsc::channel();
        if let Err(e) = thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => on_output(&buf[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let _ = read_done.send(());
            })
        {
            let _ = killer.kill();
            let _ = child.wait();
            return Err(e.into());
        }
        if let Err(e) = thread::Builder::new()
            .name("pty-wait".into())
            .spawn(move || {
                let code = child.wait().ok().map(|s| s.exit_code());
                // Drain the final command/error output before announcing exit. A
                // descendant holding a slave open must not hang the wait forever.
                let _ = drained.recv_timeout(std::time::Duration::from_millis(500));
                on_exit(code);
            })
        {
            let _ = killer.kill();
            return Err(e.into());
        }
        Ok(Self {
            master: pair.master,
            writer,
            killer,
        })
    }
}

/// Retain the current pane geometry; use standard terminal dimensions only
/// before the first layout (never pass a 0×0 winsize to an external client).
pub fn nonzero_dims(mut dims: PtyDims) -> PtyDims {
    if dims.cols == 0 {
        dims.cols = 80;
    }
    if dims.rows == 0 {
        dims.rows = 24;
    }
    dims
}

fn pty_size(dims: PtyDims) -> PtySize {
    PtySize {
        rows: dims.rows,
        cols: dims.cols,
        pixel_width: dims.pixel_width,
        pixel_height: dims.pixel_height,
    }
}

impl TerminalBackend for LocalPty {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> std::io::Result<()> {
        self.master
            .resize(pty_size(PtyDims {
                cols,
                rows,
                pixel_width,
                pixel_height,
            }))
            .map_err(std::io::Error::other)
    }

    fn close(&mut self) {
        let _ = self.killer.kill();
    }

    /// The process currently in the foreground of the terminal (the shell
    /// itself when idle, `vim`/`ssh`/... while they run).
    fn foreground_process(&self) -> Option<ProcessInfo> {
        let pid = self.master.process_group_leader()?;
        process_info(pid)
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

#[cfg(target_os = "macos")]
fn process_info(pid: i32) -> Option<ProcessInfo> {
    use std::ffi::CStr;
    use std::mem::{MaybeUninit, size_of};

    let mut name_buf = [0u8; 256];
    // SAFETY: the buffer is valid for `len` bytes and proc_name NUL-terminates.
    let len = unsafe { libc::proc_name(pid, name_buf.as_mut_ptr().cast(), name_buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    let name = String::from_utf8_lossy(&name_buf[..len as usize]).into_owned();

    let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = size_of::<libc::proc_vnodepathinfo>() as i32;
    // SAFETY: `info` is a zeroed proc_vnodepathinfo of exactly `size` bytes.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    let cwd = (got == size).then(|| {
        // SAFETY: proc_pidinfo filled the struct; vip_path is NUL-terminated.
        let info = unsafe { info.assume_init() };
        let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
        PathBuf::from(path.to_string_lossy().into_owned())
    });

    Some(ProcessInfo { pid, name, cwd })
}

#[cfg(not(target_os = "macos"))]
fn process_info(_pid: i32) -> Option<ProcessInfo> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_core::{Program, Secret};
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };
    #[test]
    fn external_client_observes_size_at_exec_and_key_only_in_env() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("client");
        std::fs::write(&script,b"#!/bin/sh\nstty size\nprintf 'arg:%s\\n' \"$@\"\n[ -n \"$MOSH_KEY\" ] && printf 'env:MOSH_KEY\\n'\nprintf 'term:%s locale:%s\\n' \"$TERM\" \"$LC_ALL\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        for (cols, rows, expected) in [(100, 30, "30 100"), (132, 40, "40 132"), (0, 0, "24 80")] {
            let program = Program {
                executable: script.clone(),
                args: vec!["2001:db8::1".into(), "60001".into()],
                env: [
                    (
                        "MOSH_KEY".into(),
                        Secret::new("fixture-secret-never-output"),
                    ),
                    ("TERM".into(), Secret::new("xterm-256color")),
                    ("LC_ALL".into(), Secret::new("en_US.UTF-8")),
                ]
                .into(),
                cwd: Some(dir.path().into()),
            };
            let output = Arc::new(Mutex::new(Vec::new()));
            let copy = output.clone();
            let (tx, rx) = mpsc::channel();
            let _pty = LocalPty::spawn_program(
                &program,
                PtyDims {
                    cols,
                    rows,
                    ..Default::default()
                },
                move |b| copy.lock().unwrap().extend_from_slice(b),
                move |code| {
                    let _ = tx.send(code);
                },
            )
            .unwrap();
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Some(0));
            let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
            assert_eq!(output.lines().next().unwrap().trim(), expected);
            assert!(output.contains("env:MOSH_KEY"));
            assert!(output.contains("arg:2001:db8::1"));
            assert!(output.contains("arg:60001"));
            assert!(!output.contains("fixture-secret-never-output"));
        }
    }
}
