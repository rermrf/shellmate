//! Executors contain no gpui entities. Terminal I/O is serviced by the drawer's UI bridge.
use super::host::{Executor, Host, Ui};
use shellmate_ai::{
    agent::{Target, ToolCall, ToolOutcome},
    output::shape,
};
use shellmate_core::ai_output::Spool;
use shellmate_ssh::{Connection, ExecStatus, SshError};
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
pub async fn ssh(connection: &Connection, call: &ToolCall, spool: &Spool) -> ToolOutcome {
    let started = Instant::now();
    let result = async {
        let mut stream = connection.exec_spooled(&wrapped(&call.command)).await?;
        while let Some(chunk) = stream.next_chunk().await? {
            if !spool
                .write(&chunk.bytes, chunk.stderr)
                .map_err(|e| SshError::Channel(e.to_string()))?
            {
                return Ok(ToolOutcome::Failed("输出超过 5 GB，命令已停止".into()));
            }
        }
        let code = match stream.status() {
            Some(ExecStatus::Code(n)) => i32::try_from(n).ok(),
            Some(ExecStatus::Signal(signal)) => signal_code(&signal),
            _ => None,
        };
        Ok(ToolOutcome::Completed(shape(
            [],
            [],
            code,
            started.elapsed(),
            false,
            false,
        )))
    }
    .await;
    match result {
        Ok(out) => out,
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
fn drain(pipe: &mut impl Read, spool: &Spool, stderr: bool) -> std::io::Result<bool> {
    let mut buf = [0; 8192];
    // One chunk per pipe per poll, preserving observed interleaving and cancellation.
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if !spool.write(&buf[..n], stderr)? {
                    return Err(std::io::Error::other("输出超过 5 GB，命令已停止"));
                }
                return Ok(false);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}
pub async fn local(
    call: &ToolCall,
    cwd: PathBuf,
    shell: &str,
    home: Option<&std::path::Path>,
    spool: &Spool,
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
        let mut status = None;
        let mut eof = (false, false);
        loop {
            if !eof.0 {
                eof.0 = drain(&mut stdout, spool, false)?;
            }
            if !eof.1 {
                eof.1 = drain(&mut stderr, spool, true)?;
            }
            if status.is_none() {
                status = child.0.try_wait()?;
            }
            if status.is_some() && eof.0 && eof.1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        use std::os::unix::process::ExitStatusExt;
        let code = status.and_then(|s| s.code().or_else(|| s.signal().map(|n| 128 + n)));
        drop(child);
        Ok::<_, std::io::Error>(ToolOutcome::Completed(shape(
            [],
            [],
            code,
            started.elapsed(),
            false,
            false,
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
/// Capture the bound session's directory, not the app process or focused tab.
/// Local shells need not emit OSC 7; even a reported path can be stale after cd.
pub fn session_cwd(local: bool, shell_pid: Option<i32>, reported: Option<&str>) -> Option<String> {
    if local {
        #[cfg(target_os = "macos")]
        {
            shell_pid
                .filter(|pid| *pid > 0)
                .and_then(shellmate_term::process_cwd)
                .and_then(|path| path.into_os_string().into_string().ok())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = shell_pid;
            None
        }
    } else {
        reported
            .filter(|path| std::path::Path::new(path).is_absolute())
            .map(str::to_owned)
    }
}
impl Binding {
    /// The model and all delegated commands use this same per-turn snapshot.
    fn local_directory(&self) -> Result<PathBuf, &'static str> {
        self.capabilities
            .cwd
            .as_deref()
            .filter(|path| std::path::Path::new(path).is_absolute())
            .map(PathBuf::from)
            .ok_or("无法确定当前终端的工作目录，命令未执行；请确认该终端仍然打开后重试")
    }
}
pub async fn disconnected(alive: &AtomicBool) {
    while alive.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
#[derive(Clone)]
pub struct AppExecutor {
    pub lease: Option<Arc<AtomicBool>>,
    pub slots: Option<Arc<tokio::sync::Semaphore>>,
    pub router: Option<super::target_router::Router>,
    pub subagents: Option<Arc<super::subagents::Service>>,
    pub memory: Option<(shellmate_core::memory::Memory, String)>,
    pub binding: Binding,
    pub tasks: super::tasks::Tasks,
    pub ui: async_channel::Sender<Ui>,
}
impl Executor for AppExecutor {
    fn model_slots(&self) -> Option<Arc<tokio::sync::Semaphore>> { self.router.as_ref().map(|r|r.catalog.models.clone()) }
    fn sessions_enabled(&self) -> bool {
        self.router.is_some()
    }
    fn routed_host(&self, call: &mut ToolCall) -> Result<Option<Host>, String> {
        self.router
            .as_ref()
            .map(|r| r.host(call, self.tasks.clone()))
            .transpose()
    }
    fn web_enabled(&self) -> bool {
        self.tasks.web_enabled()
    }
    fn take_external_steps(&self) -> Vec<shellmate_ai::Step> {
        self.tasks.take_external_steps()
    }
    fn send_agent_message(
        &self,
        id: &str,
        text: &str,
        immediate: bool,
        resume: bool,
        call_id: &str,
    ) -> futures::future::BoxFuture<'_, shellmate_ai::agent::LocalResult> {
        match &self.subagents {
            Some(service) => service.message(id, text, immediate, resume, call_id),
            None => Box::pin(async {
                shellmate_ai::agent::LocalResult::error("当前会话不支持追加子任务消息")
            }),
        }
    }
    fn control(&self) -> Option<shellmate_ai::interaction::Control> {
        Some(self.tasks.control())
    }
    fn subagents(&self) -> bool {
        self.subagents.as_ref().is_some_and(|s| s.supported())
    }
    fn subagent(
        &self,
        request: shellmate_ai::SubagentRequest,
        call_id: &str,
    ) -> futures::future::BoxFuture<'_, shellmate_ai::agent::LocalResult> {
        match &self.subagents {
            Some(service) => service.start(request, call_id),
            None => Box::pin(async {
                shellmate_ai::agent::LocalResult::error("当前会话不支持子任务")
            }),
        }
    }
    fn cancelled(&self) -> bool {
        self.subagents
            .as_ref()
            .is_some_and(|s| s.cancel.is_cancelled())
    }
    fn subagent_results(&self) -> Vec<(String, shellmate_ai::LoggedResult)> {
        self.tasks.agent_updates()
    }
    fn finish_subagent(&self) {
        self.tasks.stop_all();
    }

    fn memory_snapshot(&self, auto_memory: bool) -> shellmate_ai::memory::Snapshot {
        let mut snapshot = self.memory.as_ref().map_or_else(
            || shellmate_ai::memory::Snapshot {
                auto_memory,
                ..Default::default()
            },
            |(memory, label)| shellmate_ai::memory::Snapshot::load(memory, label, auto_memory),
        );
        if let Some(router) = &self.router {
            let endpoint=router.resolve(None).ok();
            let memory=shellmate_core::memory::Memory::new(&router.root,endpoint.as_ref().and_then(|e|if e.binding.local{Some("local")}else{e.target.host_id.as_deref()}));
            snapshot=shellmate_ai::memory::Snapshot::load(&memory,endpoint.as_ref().map_or("无默认终端",|e|e.target.label.as_str()),auto_memory);
            snapshot.instructions.push_str(&format!("\n本轮终端范围：{}。操作时明确 session_id；没有默认终端时不得回退到当前标签。主机记忆按 session_id 分开读取和更新。",serde_json::to_string(&*router.scope.lock().unwrap()).unwrap()));
        }
        if let Some(service) = &self.subagents {
            *service.snapshot.lock().unwrap() = snapshot.clone();
        }
        snapshot
    }
    fn output_id(&self, call: &ToolCall) -> Option<String> {
        self.tasks.output_id_for(&call.id)
    }
    fn local_tool(
        &self,
        tool: shellmate_ai::agent::LocalTool,
        limits: shellmate_ai::agent::ExecLimits,
    ) -> futures::future::BoxFuture<'_, shellmate_ai::agent::LocalResult> {
        match tool {
            shellmate_ai::agent::LocalTool::Sessions(request) => {
                return Box::pin(async move {
                    match &self.router {
                        Some(r) => {
                            if matches!(&request,shellmate_ai::session_tools::Request::Compare(_)) {
                                let result=r.local(request,&self.tasks).await;
                                if let Some(id)=&result.output_id{self.tasks.remember_output(id.clone(),"目录比较".into());}
                                return result;
                            }
                            tokio::select! {biased;()=async{self.tasks.control().urgent().await}=>shellmate_ai::agent::LocalResult::error("终端读取已取消"),result=r.local(request,&self.tasks)=>{if let Some(id)=&result.output_id{self.tasks.remember_output(id.clone(),"目录比较".into());}result}}
                        }
                        None => shellmate_ai::agent::LocalResult::error("终端目录不可用"),
                    }
                });
            }
            shellmate_ai::agent::LocalTool::ScopedMemory {
                session_id,
                command,
            } => {
                return Box::pin(async move {
                    self.router
                        .as_ref()
                        .map(|r| r.scoped_memory(&session_id, command))
                        .unwrap_or_else(|| {
                            shellmate_ai::agent::LocalResult::error("目标记忆不可用")
                        })
                });
            }
            _ => {}
        }
        if let shellmate_ai::agent::LocalTool::Memory(command) = tool {
            if let Some(router) = &self.router {
                return Box::pin(async move {
                    let id = router.scope.lock().unwrap().default.clone();
                    match id {
                        Some(id) => router.scoped_memory(&id, command),
                        None => command
                            .execute(&shellmate_core::memory::Memory::new(&router.root, None)),
                    }
                });
            }
            return Box::pin(async move {
                self.memory.as_ref().map_or_else(
                    || shellmate_ai::agent::LocalResult {
                        subagent: None,
                        inline: "记忆存储不可用".into(),
                        output_id: None,
                        is_error: true,
                    },
                    |(memory, _)| command.execute(memory),
                )
            });
        }
        Box::pin(self.tasks.local_tool(tool, limits))
    }
    fn take_notices(&self) -> Vec<String> {
        self.tasks.take_notices()
    }
    fn task_state(&self) -> (Vec<String>, Vec<String>) {
        self.tasks.state()
    }
    fn execute(&self, call: ToolCall) -> futures::future::BoxFuture<'_, ToolOutcome> {
        Box::pin(async move {
            if call.cwd.is_some() && call.target!=Target::Background {return ToolOutcome::Failed("此步骤指定了独立通道工作目录，请保留该通道或重新提出命令".into());}
            if !self.binding.capabilities.offers(call.target) {
                return ToolOutcome::Failed("此会话不支持该执行目标".into());
            }
            if !self.binding.alive.load(Ordering::Acquire) {
                return ToolOutcome::Disconnected;
            }
            // Refuse before reserving a task or running a process. Never replace
            // an unknown terminal directory with HOME or the application cwd.
            let cwd = if call.target == Target::Background && self.binding.local {
                match self.binding.local_directory() {
                    Ok(path) => Some(path),
                    Err(error) => return ToolOutcome::Failed(error.into()),
                }
            } else {
                None
            };
            // Workers own the binding and bridge, but never a strong registry reference.
            let binding = self.binding.clone();
            let lease = self.lease.clone().unwrap_or_else(||binding.alive.clone());
            let slots = self.slots.clone();
            let ui = self.ui.clone();
            let worker_call = call.clone();
            let mut worker_call = worker_call;
            if worker_call.target == Target::Background
                && !binding.local
                && worker_call.destination.is_some()
            {
                let Some(cwd) = binding
                    .capabilities
                    .cwd
                    .as_deref()
                    .filter(|p| p.starts_with('/'))
                else {
                    return ToolOutcome::Failed(
                        "远端工作目录未知，请在 run_command 的 cwd 中指定绝对目录，或在目标终端中执行".into(),
                    );
                };
                worker_call.command = format!(
                    "cd '{}' && {}",
                    cwd.replace('\'', "'\\''"),
                    worker_call.command
                );
            }
            let (ready_tx, ready_rx) = if call.target == Target::Terminal && call.run_in_background
            {
                let (tx, rx) = oneshot::channel();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
            self.tasks.run(call, ready_rx, move |spool| Box::pin(async move {
                tokio::select! {
                    biased;
                    () = disconnected(&binding.alive) => ToolOutcome::Disconnected,
                    () = disconnected(&lease) => ToolOutcome::Disconnected,
                    out = async {
                        let _permit = if let Some(slots)=slots { slots.acquire_owned().await.ok() } else { None };
                        match worker_call.target {
                            Target::Terminal => terminal(&binding, &ui, &worker_call, spool, ready_tx).await,
                            Target::Background if binding.local => {
                                let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
                                local(&worker_call, cwd.expect("local directory validated before scheduling"), &shell, None, &spool).await
                            }
                            Target::Background => match &binding.connection { Some(c) => ssh(c, &worker_call, &spool).await, None => ToolOutcome::Disconnected },
                        }
                    } => out,
                }
            })).await
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
async fn terminal(
    binding: &Binding,
    ui: &async_channel::Sender<Ui>,
    call: &ToolCall,
    spool: Spool,
    ready: Option<oneshot::Sender<()>>,
) -> ToolOutcome {
    // Literal terminal input must never contain Enter/escape/control injection.
    if call.command.chars().any(char::is_control) {
        return ToolOutcome::Failed("终端命令须为单行且不能含控制字符，需要使用独立通道执行".into());
    }
    let started = Instant::now();
    let nonce = terminal_nonce();
    let _guard = TerminalGuard {
        ui: ui.clone(),
        session: binding.session,
        nonce: nonce.clone(),
    };
    let (reply, receive) = oneshot::channel();
    if ui
        .send(Ui::TerminalStart {
            session: binding.session,
            command: call.command.clone(),
            nonce,
            spool,
            lease: binding.alive.clone(),
            reply,
        })
        .await
        .is_err()
    {
        return ToolOutcome::Disconnected;
    }
    let handle = match receive.await {
        Ok(Ok(handle)) => handle,
        Ok(Err(out)) => return *out,
        Err(_) => return ToolOutcome::Disconnected,
    };
    if let Some(ready) = ready {
        let _ = ready.send(());
    }
    loop {
        let data = handle.snapshot();
        if let Some(error) = data.error {
            return ToolOutcome::Failed(error);
        }
        if data.exit_code.is_some() {
            return ToolOutcome::Completed(shape(
                [],
                [],
                data.exit_code,
                started.elapsed(),
                false,
                false,
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
