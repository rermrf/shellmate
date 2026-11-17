//! Executors contain no gpui entities. Terminal I/O is serviced by the drawer's UI bridge.
use super::host::{Executor, Ui};
use shellmate_ai::{
    agent::{Target, ToolCall, ToolOutcome},
    output::shape,
};
use shellmate_ssh::{Connection, ExecOptions, ExecStatus, SshError};
use std::{
    io::{ErrorKind, Read},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
pub const CAP: usize = 256 * 1024;
fn terminal_nonce() -> String {
    const ALPHANUMERIC: &[u8; 62] =
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    // Reuse the existing OS-random UUID v4 source, encoding the low digits in base 62.
    let id = shellmate_core::HostId::generate()
        .to_string()
        .replace('-', "");
    let mut random = u128::from_str_radix(&id, 16).expect("generated UUID is hexadecimal");
    (0..12)
        .map(|_| {
            let c = ALPHANUMERIC[(random % 62) as usize] as char;
            random /= 62;
            c
        })
        .collect()
}
pub fn wrapped(command: &str) -> String {
    format!(
        "export PAGER=cat GIT_PAGER=cat SYSTEMD_PAGER= SYSTEMD_COLORS=0 TERM=dumb LC_ALL=C.UTF-8; {command}"
    )
}
pub async fn ssh(connection: &Connection, call: &ToolCall) -> ToolOutcome {
    let started = Instant::now();
    match connection
        .exec_with(
            &wrapped(&call.command),
            ExecOptions {
                timeout: call.timeout,
                max_output: CAP,
            },
        )
        .await
    {
        Ok(out) => {
            let code = match out.status {
                ExecStatus::Code(n) => i32::try_from(n).ok(),
                ExecStatus::Signal(signal) => signal_code(&signal),
                ExecStatus::Unknown => None,
            };
            ToolOutcome::Completed(shape(
                out.stdout,
                out.stderr,
                code,
                started.elapsed(),
                false,
                out.truncated,
            ))
        }
        Err(SshError::ExecTimeout { .. }) => {
            ToolOutcome::Completed(shape([], [], None, started.elapsed(), true, false))
        }
        Err(SshError::Disconnected(_)) => ToolOutcome::Disconnected,
        Err(_) if connection.is_closed() => ToolOutcome::Disconnected,
        Err(e) => ToolOutcome::Failed(e.to_string()),
    }
}
fn signal_code(signal: &str) -> Option<i32> {
    let name = signal.trim_matches('"');
    let number = match name.strip_prefix("SIG").unwrap_or(name) {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ILL" => 4,
        "ABRT" => 6,
        "KILL" => 9,
        "SEGV" => 11,
        "PIPE" => 13,
        "ALRM" => 14,
        "TERM" => 15,
        _ => return None,
    };
    Some(128 + number)
}
/// The parent is reaped and the entire process group is killed on every exit path.
struct ProcessGroup(Child);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // SAFETY: spawn created this process group; the child remains owned by this guard.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}
fn nonblocking(fd: &impl AsRawFd) -> std::io::Result<()> {
    // SAFETY: the descriptor is borrowed from a live pipe and is not closed here.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
fn drain(pipe: &mut impl Read, kept: &mut Vec<u8>, truncated: &mut bool) -> std::io::Result<()> {
    let mut buf = [0; 8192];
    // Bound each poll even for an endless writer, so cancel/timeout stay responsive.
    for _ in 0..32 {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = CAP.saturating_sub(kept.len());
                *truncated |= n > room;
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock) => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
pub async fn local(
    call: &ToolCall,
    cwd: PathBuf,
    shell: &str,
    home: Option<&std::path::Path>,
) -> ToolOutcome {
    let started = Instant::now();
    let result = async {
        let mut command = Command::new(shell);
        command
            .arg("-c")
            .arg(wrapped(&call.command))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(home) = home {
            command.env("HOME", home).env("ZDOTDIR", home);
        }
        let mut child = ProcessGroup(command.spawn()?);
        let mut stdout = child.0.stdout.take().unwrap();
        let mut stderr = child.0.stderr.take().unwrap();
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let (mut out, mut err, mut truncated) = (Vec::new(), Vec::new(), false);
        let (code, timed_out) = loop {
            drain(&mut stdout, &mut out, &mut truncated)?;
            drain(&mut stderr, &mut err, &mut truncated)?;
            if let Some(status) = child.0.try_wait()? {
                drain(&mut stdout, &mut out, &mut truncated)?;
                drain(&mut stderr, &mut err, &mut truncated)?;
                use std::os::unix::process::ExitStatusExt;
                break (
                    status.code().or_else(|| status.signal().map(|n| 128 + n)),
                    false,
                );
            }
            if started.elapsed() >= call.timeout {
                break (None, true);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        drop(child);
        Ok::<_, std::io::Error>(ToolOutcome::Completed(shape(
            out,
            err,
            code,
            started.elapsed(),
            timed_out,
            truncated,
        )))
    }
    .await;
    result.unwrap_or_else(|e| ToolOutcome::Failed(e.to_string()))
}
#[derive(Clone)]
pub struct Binding {
    pub session: usize,
    pub capabilities: shellmate_ai::agent::Capabilities,
    pub connection: Option<Connection>,
    pub local: bool,
    pub alive: Arc<AtomicBool>,
}
pub async fn disconnected(alive: &AtomicBool) {
    while alive.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
pub struct AppExecutor {
    pub binding: Binding,
    pub ui: async_channel::Sender<Ui>,
}
impl Executor for AppExecutor {
    fn execute(&self, call: ToolCall) -> futures::future::BoxFuture<'_, ToolOutcome> {
        Box::pin(async move {
            if !self.binding.capabilities.offers(call.target) {
                return ToolOutcome::Failed("此会话不支持该执行目标".into());
            }
            tokio::select! {
                biased;
                () = disconnected(&self.binding.alive) => ToolOutcome::Disconnected,
                out = async {
                    match call.target {
                        Target::Terminal => self.terminal(&call).await,
                        Target::Background if self.binding.local => {
                            let (reply, receive) = oneshot::channel();
                            if self.ui.send(Ui::LocalCwd { session: self.binding.session, reply }).await.is_err() { return ToolOutcome::Disconnected; }
                            let directory = match receive.await { Ok(path) => path, Err(_) => return ToolOutcome::Disconnected };
                            if !self.binding.alive.load(Ordering::Acquire) { return ToolOutcome::Disconnected; }
                            let cwd = directory.map(PathBuf::from).or_else(||std::env::var_os("HOME").map(PathBuf::from));
                            let Some(cwd) = cwd else { return ToolOutcome::Failed("无法确定本机工作目录".into()); };
                            let shell = std::env::var("SHELL").ok().filter(|s|!s.is_empty()).unwrap_or_else(||"/bin/zsh".into());
                            local(&call, cwd, &shell, None).await
                        }
                        Target::Background => match &self.binding.connection { Some(c) => ssh(c, &call).await, None => ToolOutcome::Disconnected },
                    }
                } => out,
            }
        })
    }
}
struct TerminalGuard {
    ui: async_channel::Sender<Ui>,
    session: usize,
    nonce: String,
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.ui.try_send(Ui::TerminalFinish {
            session: self.session,
            nonce: self.nonce.clone(),
        });
    }
}
impl AppExecutor {
    async fn terminal(&self, call: &ToolCall) -> ToolOutcome {
        // Literal terminal input must never contain Enter/escape/control injection.
        if call.command.chars().any(char::is_control) {
            return ToolOutcome::Failed("终端命令须为单行且不能含控制字符，请改用后台执行".into());
        }
        let started = Instant::now();
        let nonce = terminal_nonce();
        let _guard = TerminalGuard {
            ui: self.ui.clone(),
            session: self.binding.session,
            nonce: nonce.clone(),
        };
        let (reply, receive) = oneshot::channel();
        if self
            .ui
            .send(Ui::TerminalStart {
                session: self.binding.session,
                command: call.command.clone(),
                nonce,
                lease: self.binding.alive.clone(),
                reply,
            })
            .await
            .is_err()
        {
            return ToolOutcome::Disconnected;
        }
        let handle = match receive.await {
            Ok(Ok(handle)) => handle,
            Ok(Err(out)) => return out,
            Err(_) => return ToolOutcome::Disconnected,
        };
        let mut timed_out = false;
        loop {
            let data = handle.snapshot();
            if data.exit_code.is_some()
                || (timed_out && started.elapsed() >= call.timeout + Duration::from_secs(2))
            {
                let missing = if data.exit_code.is_none() {
                    "未检测到命令结束标记（可能是不支持的 shell）"
                } else {
                    ""
                };
                return ToolOutcome::Completed(shape(
                    shellmate_term::agent::strip_echo(&data.bytes),
                    missing,
                    data.exit_code,
                    started.elapsed(),
                    timed_out,
                    data.truncated,
                ));
            }
            if !timed_out && started.elapsed() >= call.timeout {
                timed_out = true;
                let _ = self
                    .ui
                    .send(Ui::TerminalInterrupt {
                        session: self.binding.session,
                        nonce: handle.nonce.clone(),
                    })
                    .await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
