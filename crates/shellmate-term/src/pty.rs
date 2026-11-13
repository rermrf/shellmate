//! Local pseudo-terminal backend built on `portable-pty`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::thread;

use anyhow::{Context as _, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

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
        Self { shell: None, login: true, cwd: None }
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
        if self.login { format!("{shell} -l") } else { shell }
    }
}

pub(crate) struct LocalPty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl LocalPty {
    /// Spawns the shell. `on_output` runs on a reader thread for every chunk of
    /// output; `on_exit` runs once with the exit code when the shell exits.
    pub fn spawn(
        options: &PtyOptions,
        size: PtySize,
        mut on_output: impl FnMut(&[u8]) + Send + 'static,
        on_exit: impl FnOnce(Option<u32>) + Send + 'static,
    ) -> Result<Self> {
        let pair = native_pty_system().openpty(size).context("openpty")?;

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

        let mut child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        drop(pair.slave);

        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let writer = pair.master.take_writer().context("take pty writer")?;

        thread::Builder::new()
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
            })
            .context("spawn reader thread")?;

        thread::Builder::new()
            .name("pty-wait".into())
            .spawn(move || {
                let code = child.wait().ok().map(|s| s.exit_code());
                on_exit(code);
            })
            .context("spawn wait thread")?;

        Ok(Self { master: pair.master, writer, killer })
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, size: PtySize) -> Result<()> {
        self.master.resize(size)
    }

    /// The process currently in the foreground of the terminal (the shell
    /// itself when idle, `vim`/`ssh`/... while they run).
    pub fn foreground_process(&self) -> Option<ProcessInfo> {
        let pid = self.master.process_group_leader()?;
        ProcessInfo::of(pid)
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: i32,
    pub name: String,
    pub cwd: Option<PathBuf>,
}

impl ProcessInfo {
    #[cfg(target_os = "macos")]
    fn of(pid: i32) -> Option<Self> {
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
            libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, info.as_mut_ptr().cast(), size)
        };
        let cwd = (got == size).then(|| {
            // SAFETY: proc_pidinfo filled the struct; vip_path is NUL-terminated.
            let info = unsafe { info.assume_init() };
            let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
            PathBuf::from(path.to_string_lossy().into_owned())
        });

        Some(Self { pid, name, cwd })
    }

    #[cfg(not(target_os = "macos"))]
    fn of(_pid: i32) -> Option<Self> {
        None
    }
}
