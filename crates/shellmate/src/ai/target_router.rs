use super::{
    exec::AppExecutor,
    host::{Executor, Host, Ui},
    session_catalog::{Catalog, Endpoint},
    tasks::Tasks,
};
use shellmate_ai::{
    PermissionMode,
    agent::{LocalResult, ToolCall},
    session_tools::{Destination, Request},
};
use shellmate_store::SharedStore;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
};
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Scope {
    pub default: Option<String>,
    #[serde(default)]
    pub rebindings: BTreeMap<String, String>,
    /// Previously referenced terminals and per-run directory snapshots, not an allowlist.
    pub targets: BTreeMap<String, Destination>,
}
#[derive(Clone)]
pub struct Router {
    pub catalog: Catalog,
    pub scope: Arc<Mutex<Scope>>,
    pub db: SharedStore,
    pub root: PathBuf,
    pub ui: async_channel::Sender<Ui>,
}
impl Router {
    pub fn new(
        catalog: Catalog,
        db: SharedStore,
        root: PathBuf,
        ui: async_channel::Sender<Ui>,
    ) -> Self {
        Self {
            catalog,
            scope: Default::default(),
            db,
            root,
            ui,
        }
    }
    pub fn add(&self, id: &str, default: bool) -> Result<(), String> {
        let e = self.catalog.resolve(id)?;
        let mut scope = self.scope.lock().unwrap();
        scope.targets.entry(id.into()).or_insert(e.target);
        if default {
            scope.default = Some(id.into());
        }
        Ok(())
    }
    /// Explicit UI association only. Ordinary tool resolution never follows old references.
    pub fn rebind(&self, old: &str, new: &str) -> Result<(), String> {
        let endpoint = self.catalog.resolve(new)?;
        let mut scope = self.scope.lock().unwrap();
        if !scope.targets.contains_key(old) {
            return Err("此对话没有记录该终端".into());
        }
        if self.catalog.resolve(old).is_ok() {
            return Err("原终端仍可用，请直接选择目标".into());
        }
        scope.targets.remove(old);
        scope.targets.insert(new.into(), endpoint.target);
        if scope.default.as_deref() == Some(old) {
            scope.default = Some(new.into());
        }
        for target in scope.rebindings.values_mut() {
            if target == old {
                *target = new.into();
            }
        }
        scope.rebindings.insert(old.into(), new.into());
        Ok(())
    }
    pub fn resumed_reference(&self, old: &str) -> Result<String, String> {
        if self.resolve(Some(old)).is_ok() {
            return Ok(old.into());
        }
        let new = self
            .scope
            .lock()
            .unwrap()
            .rebindings
            .get(old)
            .cloned()
            .ok_or("原终端已失效，请在目标列表重新关联")?;
        self.resolve(Some(&new))?;
        Ok(new)
    }
    pub fn remove(&self, id: &str) {
        let mut s = self.scope.lock().unwrap();
        s.targets.remove(id);
        if s.default.as_deref() == Some(id) {
            s.default = None;
        }
    }
    pub fn refresh_run(&self) {
        let mut scope = self.scope.lock().unwrap();
        for (id, target) in &mut scope.targets {
            if let Ok(e) = self.catalog.resolve(id) {
                *target = e.target;
            }
        }
    }
    pub fn resolve(&self, id: Option<&str>) -> Result<Endpoint, String> {
        let mut scope = self.scope.lock().unwrap();
        let id = id
            .or(scope.default.as_deref())
            .ok_or("没有默认终端，请用 list_sessions 查找并明确提供 session_id")?
            .to_owned();
        // The model chooses a live resource. Never remap an expired reference or
        // infer authorization from substrings in user text.
        let mut endpoint = self.catalog.resolve(&id)?;
        let target = scope.targets.entry(id).or_insert_with(|| endpoint.target.clone());
        endpoint.binding.capabilities.cwd = target.cwd.clone();
        endpoint.target = target.clone();
        Ok(endpoint)
    }
    pub fn context(&self) -> shellmate_ai::ContextSnapshot {
        let scope = self.scope.lock().unwrap();
        let target = scope.default.as_ref().and_then(|id| scope.targets.get(id));
        shellmate_ai::ContextSnapshot {
            label: target
                .map(|t| t.label.clone())
                .unwrap_or_else(|| "多终端任务".into()),
            protocol: target.map(|t| t.protocol.clone()).unwrap_or_default(),
            text: format!("默认终端与已引用终端（不是可用资源限制）：{}。应用内其他终端可通过 list_sessions 发现并直接使用。", serde_json::to_string(&*scope).unwrap()),
        }
    }
    pub fn policy(&self, e: &Endpoint) -> PermissionMode {
        self.db
            .borrow()
            .ai_permission(if e.binding.local {
                "local"
            } else {
                e.target.host_id.as_deref().unwrap_or("temporary")
            })
            .unwrap_or_default()
    }
    pub fn host(&self, call: &mut ToolCall, tasks: Tasks) -> Result<Host, String> {
        let mut e = self.resolve(call.session_id.as_deref())?;
        if let Some(cwd) = &call.cwd {
            if call.target != shellmate_ai::agent::Target::Background
                || !cwd.starts_with('/')
                || cwd.contains('\0')
            {
                return Err("显式目录仅支持独立通道命令的绝对路径".into());
            }
            e.target.cwd = Some(cwd.clone());
            e.binding.capabilities.cwd = Some(cwd.clone());
        }
        call.session_id = Some(e.target.session_id.clone());
        call.destination = Some(e.target.clone());
        let mode = self.policy(&e);
        let memory = shellmate_core::memory::Memory::new(
            &self.root,
            if e.binding.local {
                Some("local")
            } else {
                e.target.host_id.as_deref()
            },
        );
        Ok(Host {
            compaction: Default::default(),
            mode: Arc::new(Mutex::new(mode)),
            ui: self.ui.clone(),
            executor: Box::new(Resolved {
                router: self.clone(),
                mode,
                inner: AppExecutor {
                    lease: Some(e.lease.clone()),
                    slots: Some(e.slots.clone()),
                    subagents: None,
                    memory: Some((memory, e.target.label)),
                    binding: e.binding.clone(),
                    tasks,
                    ui: self.catalog.bridge.clone(),
                    router: None,
                },
            }),
            alive: e.lease,
            resolved: Default::default(),
        })
    }
    pub fn subset(&self, ids: &[String], default: Option<&str>) -> Result<Self, String> {
        let inherited = self.scope.lock().unwrap().clone();
        let mut scope = if ids.is_empty() {
            // Context may mention a disconnected terminal. That does not prevent
            // discovering or using other resources; resolve validates each actual call.
            inherited.clone()
        } else {
            Scope::default()
        };
        for id in ids {
            let e = self.resolve(Some(id))?;
            scope.targets.insert(id.clone(), e.target);
        }
        let chosen = default.map(str::to_owned).or_else(|| if ids.len() == 1 {
            ids.first().cloned()
        } else {
            inherited.default.clone()
        });
        if let Some(id) = default {
            scope.targets.insert(id.into(), self.resolve(Some(id))?.target);
        } else if let Some(id) = &chosen
            && let Some(target) = inherited.targets.get(id) {
            scope.targets.entry(id.clone()).or_insert_with(|| target.clone());
        } else if let Some(id) = &chosen {
            scope.targets.insert(id.clone(), self.resolve(Some(id))?.target);
        }
        scope.default = chosen;
        let mut child = self.clone();
        child.scope = Arc::new(Mutex::new(scope));
        Ok(child)
    }
    pub async fn local(&self, request: Request, tasks: &Tasks) -> LocalResult {
        match request {
            Request::Compare(args) => super::directory_compare::compare(self, args, tasks).await,
            Request::List(args) => {
                let scope = self.scope.lock().unwrap().clone();
                let entries = self.catalog.list();
                let matching: Vec<_> = entries.iter().filter(|e|args.query.is_empty()||e.target.label.contains(&args.query)||e.target.protocol.contains(&args.query)).collect();
                let rows:Vec<_>=matching.iter().enumerate().skip(args.offset).take(50).map(|(index,e)|serde_json::json!({"number":index+1,"session_id":e.target.session_id,"name":e.target.label,"protocol":e.target.protocol,"window":e.window,"connected":e.connected,"cwd":e.target.cwd,"terminal_busy":e.binding.capabilities.terminal_busy,"terminal":e.binding.capabilities.terminal,"background":e.binding.capabilities.background,"default":scope.default.as_ref()==Some(&e.target.session_id)})).collect();
                LocalResult::text(serde_json::json!({"sessions":rows,"sampled_at":super::now(),"offset":args.offset,"next_offset":(args.offset+rows.len()<matching.len()).then_some(args.offset+rows.len()),"note":"number 只对应本次列表。用户说第几个时按对话中对应列表映射 session_id，不能用最新列表位置替换。default 是对话默认终端，不是当前选中标签。terminal_busy=false 仅表示未检测到占用，不保证 shell 空闲。全部已连接终端均可按任务使用，无需加入。"}).to_string())
            }
            Request::Read(args) => {
                let e = match self.resolve(Some(&args.session_id)) {
                    Ok(e) => e,
                    Err(e) => return LocalResult::error(e),
                };
                match self.catalog.read(&args.session_id,args.lines).await{Ok(c)=>LocalResult::text(serde_json::json!({"target":e.target,"sampled_at":super::now(),"truncated":c.text.starts_with("[较早输出已省略]"),"text":c.text,"max_lines":args.lines,"max_bytes":65536}).to_string()),Err(e)=>LocalResult::error(e)}
            }
        }
    }
    pub fn scoped_memory(&self, id: &str, command: shellmate_ai::memory::Command) -> LocalResult {
        let e = if id == "global" {
            None
        } else {
            match self.resolve(Some(id)) {
                Ok(e) => Some(e),
                Err(e) => return LocalResult::error(e),
            }
        };
        let key = e.as_ref().and_then(|e| {
            if e.binding.local {
                Some("local")
            } else {
                e.target.host_id.as_deref()
            }
        });
        command.execute(&shellmate_core::memory::Memory::new(&self.root, key))
    }
    pub fn validate(&self, call: &ToolCall) -> Result<(), String> {
        let e = self.resolve(call.session_id.as_deref())?;
        if !e.binding.alive.load(Ordering::Acquire) {
            return Err("目标连接已变化".into());
        }
        Ok(())
    }
}
struct Resolved {
    router: Router,
    mode: PermissionMode,
    inner: AppExecutor,
}
impl Executor for Resolved {
    fn control(&self) -> Option<shellmate_ai::interaction::Control> {
        self.inner.control()
    }
    fn output_id(&self, c: &ToolCall) -> Option<String> {
        self.inner.output_id(c)
    }
    fn execute(&self, call: ToolCall) -> futures::future::BoxFuture<'_, shellmate_ai::ToolOutcome> {
        Box::pin(async move {
            if let Err(e) = self.router.validate(&call) {
                return shellmate_ai::ToolOutcome::Failed(e);
            }
            let endpoint = match self.router.resolve(call.session_id.as_deref()) {
                Ok(e) => e,
                Err(e) => return shellmate_ai::ToolOutcome::Failed(e),
            };
            if self.router.policy(&endpoint) != self.mode {
                return shellmate_ai::ToolOutcome::Failed(
                    "目标权限设置已变化，请重新请求本步骤".into(),
                );
            }
            self.inner.execute(call).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{exec::Binding, host::Approval};
    use shellmate_ai::{
        ToolHost,
        agent::{Target, ToolOutcome},
        policy::Class,
    };
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    fn fixture() -> (tempfile::TempDir, Router, async_channel::Receiver<Ui>) {
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = async_channel::unbounded();
        let catalog = Catalog::new(tx.clone());
        catalog.update(
            (1..=2)
                .map(|id| {
                    (
                        Binding {
                            session: id,
                            local: true,
                            connection: None,
                            alive: Arc::new(AtomicBool::new(true)),
                            capabilities: shellmate_ai::Capabilities {
                                background: true,
                                cwd: Some(root.path().to_string_lossy().into()),
                                ..Default::default()
                            },
                        },
                        format!("terminal-{id}"),
                        "本机终端".into(),
                        None,
                        "window".into(),
                    )
                })
                .collect(),
        );
        let db = SharedStore::new(shellmate_store::HostStore::open_in_memory().unwrap());
        let router = Router::new(catalog, db, root.path().into(), tx);
        (root, router, rx)
    }
    #[test]
    fn available_resources_need_no_enrollment_and_default_does_not_follow_them() {
        let (_root, r, _) = fixture();
        let one = r.catalog.session(1).unwrap().target.session_id;
        let two = r.catalog.session(2).unwrap().target.session_id;
        assert!(r.resolve(None).is_err());
        r.add(&one, true).unwrap();
        assert_eq!(r.resolve(Some(&two)).unwrap().target.session_id, two);
        let scope = r.scope.lock().unwrap().clone();
        assert_eq!(scope.targets.len(), 2);
        let default = scope.default.unwrap();
        assert_eq!(r.resolve(None).unwrap().target.session_id, default);
        let mut changed = r
            .catalog
            .list()
            .into_iter()
            .map(|e| {
                (
                    e.binding,
                    e.target.label,
                    e.target.protocol,
                    e.target.host_id,
                    e.window,
                )
            })
            .collect::<Vec<_>>();
        changed[0].0.alive = Arc::new(AtomicBool::new(true));
        r.catalog.update(changed);
        assert!(r.resolve(Some(&default)).is_err());
    }
    #[tokio::test]
    async fn approval_cannot_execute_after_terminal_is_closed() {
        let (root, r, rx) = fixture();
        let id = r.catalog.session(1).unwrap().target.session_id;
        r.add(&id, true).unwrap();
        let tasks = Tasks::new(root.path().into(), "test".into());
        let mut call = ToolCall {
            id: "c".into(),
            session_id: Some(id.clone()),
            cwd: None,
            destination: None,
            command: "touch should-not-exist".into(),
            purpose: "check".into(),
            target: Target::Background,
            timeout: Duration::from_secs(2),
            run_in_background: false,
            exec_limits: Default::default(),
            class: Class::Other,
        };
        let host = r.host(&mut call, tasks).unwrap();
        let mut run = Box::pin(host.run(call));
        let reply = tokio::select! {_=&mut run=>panic!("approval required"),message=rx.recv()=>match message.unwrap(){Ui::Approval{reply,..}=>reply,_=>panic!("approval")}};
        r.catalog.update(vec![]);
        reply
            .send(Approval::Approve {
                command: "touch should-not-exist".into(),
                target: Target::Background,
            })
            .unwrap_or_else(|_| panic!("reply"));
        loop {
            tokio::select! {out=&mut run=>{assert!(matches!(out,ToolOutcome::Failed(_)));break;},message=rx.recv()=>if let Ui::Running{ready,..}=message.unwrap(){ready.send(()).ok();}}
        }
        assert!(!root.path().join("should-not-exist").exists());
    }
    #[test]
    fn child_inherits_resources_with_an_independent_default() {
        let (_root, r, _) = fixture();
        let one = r.catalog.session(1).unwrap().target.session_id;
        let two = r.catalog.session(2).unwrap().target.session_id;
        r.add(&one, true).unwrap();
        let remote_child = r.subset(std::slice::from_ref(&two), None).unwrap();
        assert_eq!(remote_child.resolve(None).unwrap().target.session_id, two);
        let child = r.subset(std::slice::from_ref(&one), Some(&one)).unwrap();
        assert_eq!(child.resolve(None).unwrap().target.session_id, one);
        assert_eq!(child.resolve(Some(&two)).unwrap().target.session_id, two);
        assert_eq!(r.resolve(None).unwrap().target.session_id, one);
    }
    #[tokio::test]
    async fn mixed_permissions_and_ssh_directory_are_resolved_per_call() {
        use crate::support::{Env, ServerOptions, TestServer};
        use shellmate_ssh::{Connection, Route, Secret, SshHost};
        let server = TestServer::start(ServerOptions {
            exec: Some(Arc::new(|command, channel| {
                tokio::spawn(async move {
                    channel.stdout(command.into_bytes()).await;
                    channel.exit(0).await;
                });
            })),
            ..Default::default()
        })
        .await;
        let env = Env::new();
        let connection = Connection::connect(
            Route::direct(
                SshHost::new(
                    "127.0.0.1",
                    server.port,
                    "alice",
                    shellmate_core::AuthMethod::Password,
                )
                .with_password(Secret::new("secret")),
            ),
            env.trusting(),
        )
        .await
        .unwrap();
        let (root, r, rx) = fixture();
        let mut entries = r
            .catalog
            .list()
            .into_iter()
            .map(|e| {
                (
                    e.binding,
                    e.target.label,
                    e.target.protocol,
                    e.target.host_id,
                    e.window,
                )
            })
            .collect::<Vec<_>>();
        entries[1].0.local = false;
        entries[1].0.connection = Some(connection);
        entries[1].0.capabilities.cwd = Some("/srv/with ' quote".into());
        entries[1].2 = "SSH".into();
        entries[1].3 = Some("remote".into());
        r.catalog.update(entries);
        let local = r.catalog.session(1).unwrap().target.session_id;
        let remote = r.catalog.session(2).unwrap().target.session_id;
        r.add(&local, true).unwrap();
        r.add(&remote, false).unwrap();
        r.db.borrow_mut()
            .set_ai_permission("local", PermissionMode::Full)
            .unwrap();
        assert_eq!(
            r.policy(&r.resolve(Some(&local)).unwrap()),
            PermissionMode::Full
        );
        assert_eq!(
            r.policy(&r.resolve(Some(&remote)).unwrap()),
            PermissionMode::Ask
        );
        let tasks = Tasks::new(root.path().into(), "ssh-route".into());
        let mut call = ToolCall {
            id: "remote-command".into(),
            session_id: Some(remote.clone()),
            cwd: None,
            destination: None,
            command: "pwd".into(),
            purpose: "directory".into(),
            target: Target::Background,
            timeout: Duration::from_secs(3),
            run_in_background: false,
            exec_limits: Default::default(),
            class: Class::ReadOnly,
        };
        let host = r.host(&mut call, tasks.clone()).unwrap();
        assert_eq!(
            call.destination.as_ref().unwrap().cwd.as_deref(),
            Some("/srv/with ' quote")
        );
        let mut future = Box::pin(host.run(call));
        let reply = tokio::select! {_=&mut future=>panic!("remote must ask"),message=rx.recv()=>match message.unwrap(){Ui::Approval{reply,..}=>reply,_=>panic!("approval")}};
        reply
            .send(Approval::Approve {
                command: "pwd".into(),
                target: Target::Background,
            })
            .ok();
        let outcome = loop {
            tokio::select! {out=&mut future=>break out,message=rx.recv()=>if let Ui::Running{ready,..}=message.unwrap(){ready.send(()).ok();}}
        };
        let ToolOutcome::Completed(output) = outcome else {
            panic!("remote execution failed")
        };
        let result = tasks
            .local_tool(
                shellmate_ai::agent::LocalTool::ReadOutput(shellmate_ai::agent::ReadOutput {
                    id: output.output_id.unwrap(),
                    offset_line: 0,
                    limit_lines: None,
                    grep: None,
                }),
                Default::default(),
            )
            .await;
        assert!(result.inline.contains("cd '/srv/with '"));
        assert!(result.inline.contains("quote' && pwd"));
        assert_eq!(r.resolve(None).unwrap().target.session_id, local);
    }

    #[test]
    fn stale_context_does_not_block_a_new_child_or_silently_replace_its_default() {
        let (_root, r, _) = fixture();
        let one = r.catalog.session(1).unwrap().target.session_id;
        let two = r.catalog.session(2).unwrap().target.session_id;
        r.add(&one, true).unwrap();
        r.catalog.session(1).unwrap().binding.alive.store(false, Ordering::Release);
        let child = r.subset(&[], None).unwrap();
        assert!(child.resolve(None).is_err());
        assert_eq!(child.resolve(Some(&two)).unwrap().target.session_id, two);
        assert_eq!(child.scope.lock().unwrap().default.as_deref(), Some(one.as_str()));
        assert!(r.subset(&[], Some(&one)).is_err(), "explicit stale selection must fail");
    }

    #[test]
    fn explicit_reassociation_allows_resume_but_old_calls_remain_invalid() {
        let (_root, r, _) = fixture();
        let old = r.catalog.session(1).unwrap().target.session_id;
        r.add(&old, true).unwrap();
        let mut entries = r
            .catalog
            .list()
            .into_iter()
            .map(|e| {
                (
                    e.binding,
                    e.target.label,
                    e.target.protocol,
                    e.target.host_id,
                    e.window,
                )
            })
            .collect::<Vec<_>>();
        entries[0].0.alive = Arc::new(AtomicBool::new(true));
        r.catalog.update(entries);
        let new = r.catalog.session(1).unwrap().target.session_id;
        assert!(r.resumed_reference(&old).is_err());
        r.rebind(&old, &new).unwrap();
        assert_eq!(r.resumed_reference(&old).unwrap(), new);
        assert!(r.resolve(Some(&old)).is_err());
        assert_eq!(r.resolve(None).unwrap().target.session_id, new);
    }
}
