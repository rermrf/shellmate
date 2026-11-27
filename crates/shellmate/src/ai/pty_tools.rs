//! Independent PTY tools; approval, target identity and cancellation before creation.
use super::{
    exec::AppExecutor,
    host::{Approval, Ui},
    pty_tasks::Pool,
    session_catalog::Endpoint,
};
use shellmate_ai::{
    agent::{ExecLimits, LocalResult, Target, ToolCall},
    policy::{Class, PermissionMode},
    session_tools::{Create, Request},
};
use shellmate_core::terminal_interaction::Controller;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::oneshot;
pub struct CreateRequest {
    pub parent: Endpoint,
    pub spec: Create,
    pub pool: Arc<Pool>,
    pub owner: Arc<Controller>,
    pub call_id: String,
    pub source: Option<String>,
    pub shell: Option<shellmate_ssh::PendingShell>,
}
struct CancelApproval {
    tx: async_channel::Sender<Ui>,
    key: String,
}
impl Drop for CancelApproval {
    fn drop(&mut self) {
        let _ = self.tx.try_send(Ui::CancelApproval(self.key.clone()));
    }
}
pub async fn execute(
    executor: &AppExecutor,
    args: Request,
    limits: ExecLimits,
    source: Option<shellmate_ai::SubagentRef>,
    call_id: &str,
) -> LocalResult {
    let control = executor.tasks.control();
    let owner = executor.tasks.interaction_owner(
        source
            .as_ref()
            .map(|s| s.description.clone())
            .unwrap_or_else(|| "Agent".into()),
    );
    let lease = match &args {
        Request::Create(a) => executor
            .router
            .as_ref()
            .and_then(|r| r.catalog.lookup(&a.session_id).ok())
            .map(|e| e.binding.alive),
        Request::Manage(a) => executor
            .router
            .as_ref()
            .and_then(|r| r.catalog.lookup(&a.session_id).ok())
            .map(|e| e.lease),
        _ => None,
    }
    .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(true)));
    let work = run(executor, args, limits, source, call_id, owner.clone());
    tokio::select! {biased;()=control.urgent()=>{owner.release();LocalResult::error("终端操作已取消")},()=super::exec::disconnected(&lease)=>LocalResult::error("终端连接已断开"),r=work=>match r{Ok(r)=>r,Err(e)=>LocalResult::error(e)}}
}
async fn run(
    executor: &AppExecutor,
    args: Request,
    limits: ExecLimits,
    source: Option<shellmate_ai::SubagentRef>,
    call_id: &str,
    owner: Arc<Controller>,
) -> Result<LocalResult, String> {
    let router = executor.router.as_ref().ok_or("终端目录不可用")?;
    let (e, purpose, summary, class) = match &args {
        Request::Create(a) => {
            let e = router.resolve(Some(&a.session_id))?;
            if !e.binding.local && e.binding.connection.is_none() {
                return Err("只支持本机和 SSH 终端".into());
            }
            let cwd = a
                .cwd
                .as_ref()
                .or(e.target.cwd.as_ref())
                .ok_or("目标目录未知，请指定 cwd 绝对路径")?;
            if !cwd.starts_with('/') || cwd.contains('\0') {
                return Err("目标目录必须是绝对路径".into());
            }
            let summary = format!(
                "启动独立终端（{} × {}）\n目录：{cwd}\n{}",
                a.columns, a.rows, a.command
            );
            (
                e,
                a.purpose.clone(),
                summary,
                shellmate_ai::policy::classify(&a.command),
            )
        }
        Request::Manage(a) => {
            let record = executor
                .tasks
                .ptys()
                .get(&a.session_id)
                .ok_or("只能管理本会话创建的独立终端")?;
            let e = router.catalog.lookup(&a.session_id)?;
            if !Arc::ptr_eq(&record.endpoint.lease, &e.lease) {
                return Err("终端身份已变化".into());
            }
            let summary = format!(
                "{}：{}",
                match a.action {
                    shellmate_ai::session_tools::ManageAction::Stop => "停止程序并保留画面",
                    shellmate_ai::session_tools::ManageAction::Close => "停止并关闭独立终端",
                    shellmate_ai::session_tools::ManageAction::Resize => "调整终端尺寸",
                },
                match a.columns.zip(a.rows) {
                    Some((c, r)) => format!("{c} × {r}"),
                    None => e.target.label.clone(),
                }
            );
            (e, a.purpose.clone(), summary, Class::Other)
        }
        _ => return Err("请求无效".into()),
    };
    let policy = router.policy(&e);
    if policy != PermissionMode::Full || class == Class::RedLine {
        executor
            .tasks
            .control()
            .set_phase(shellmate_ai::interaction::Phase::Approval);
        let (reply, rx) = oneshot::channel();
        let _guard = CancelApproval {
            tx: router.ui.clone(),
            key: super::host::approval_key(source.as_ref(), call_id),
        };
        let call = ToolCall {
            service: None,
            cwd: None,
            session_id: Some(e.target.session_id.clone()),
            destination: Some(e.target.clone()),
            id: call_id.into(),
            command: summary.clone(),
            purpose,
            target: Target::Terminal,
            timeout: Duration::from_secs(10),
            run_in_background: false,
            exec_limits: limits,
            class,
        };
        router
            .ui
            .send(Ui::TuiApproval {
                source: source.clone(),
                call,
                reply,
            })
            .await
            .map_err(|_| "审批界面已关闭")?;
        match rx.await {
            Ok(Approval::Approve { command, target })
                if command == summary && target == Target::Terminal => {}
            Ok(Approval::Deny { .. }) => return Err("用户拒绝了终端操作".into()),
            _ => return Err("审批取消或输入已变化".into()),
        }
    }
    executor
        .tasks
        .control()
        .set_phase(shellmate_ai::interaction::Phase::Executing);
    if !owner.active() || executor.tasks.ptys().retired.load(Ordering::Acquire) {
        return Err("任务已停止".into());
    }
    let current = router.catalog.lookup(&e.target.session_id)?;
    if !Arc::ptr_eq(&e.lease, &current.lease) || router.policy(&current) != policy {
        return Err("终端连接或权限已变化，请重新发起".into());
    }
    match args {
        Request::Create(mut spec) => {
            router.catalog.resolve(&e.target.session_id)?;
            spec.cwd = Some(spec.cwd.or(e.target.cwd.clone()).ok_or("目录未知")?);
            let shell = if let Some(connection) = &e.binding.connection {
                let command = format!(
                    "cd -- '{}' && {}",
                    spec.cwd.as_ref().unwrap().replace('\'', "'\\''"),
                    spec.command
                );
                Some(
                    connection
                        .open_pty_command(
                            shellmate_core::PtyDims {
                                cols: spec.columns,
                                rows: spec.rows,
                                ..Default::default()
                            },
                            &command,
                        )
                        .await
                        .map_err(|e| e.to_string())?,
                )
            } else {
                None
            };
            let (reply, rx) = oneshot::channel();
            router
                .catalog
                .bridge
                .send(Ui::CreatePty {
                    request: Box::new(CreateRequest {
                        parent: e,
                        spec,
                        pool: executor.tasks.ptys(),
                        owner,
                        call_id: call_id.into(),
                        source: source.map(|s| s.id),
                        shell,
                    }),
                    reply,
                })
                .await
                .map_err(|_| "终端桥接已关闭")?;
            let target = rx.await.map_err(|_| "创建已取消")??;
            Ok(LocalResult::text(serde_json::json!({"target":target,"created":true,"note":"已创建独立交互终端；程序可能仍在启动，请读屏并等待。可从运行任务查看或接管；停止回复不会停止程序。"}).to_string()))
        }
        Request::Manage(spec) => {
            let (reply, rx) = oneshot::channel();
            router
                .catalog
                .bridge
                .send(Ui::ManagePty {
                    spec,
                    owner,
                    lease: e.lease,
                    reply,
                })
                .await
                .map_err(|_| "终端桥接已关闭")?;
            rx.await.map_err(|_| "操作已取消")??;
            Ok(LocalResult::text("独立终端操作已完成"))
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::terminal_interaction::tests::fixture;
    fn create(id: &str) -> Request {
        Request::Create(Create {
            session_id: id.into(),
            command: "vim example".into(),
            cwd: None,
            purpose: "编辑".into(),
            columns: 100,
            rows: 30,
        })
    }
    #[tokio::test]
    async fn denied_or_changed_approval_does_not_create_process() {
        for change in [false, true] {
            let (_dir, executor, rx, id) = fixture();
            let alive = executor.binding.alive.clone();
            let handle = tokio::spawn(async move {
                let Ui::TuiApproval { call, reply, .. } = rx.recv().await.unwrap() else {
                    panic!("approval expected")
                };
                if change {
                    alive.store(false, Ordering::Release);
                    let _ = reply.send(Approval::Approve {
                        command: call.command,
                        target: Target::Terminal,
                    });
                } else {
                    let _ = reply.send(Approval::Deny { reason: None });
                }
                while let Ok(event) = rx.recv().await {
                    assert!(!matches!(event, Ui::CreatePty { .. }));
                }
            });
            assert!(
                execute(
                    &executor,
                    create(&id),
                    ExecLimits::default(),
                    None,
                    "create"
                )
                .await
                .is_error
            );
            handle.abort();
        }
    }
    #[tokio::test]
    async fn creation_preserves_child_identity_and_chosen_directory() {
        let (_dir, executor, rx, id) = fixture();
        let source = shellmate_ai::SubagentRef {
            id: "agent-1".into(),
            kind: shellmate_ai::SubagentKind::General,
            description: "编辑文件".into(),
        };
        let child_source = source.clone();
        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await.unwrap() {
                    Ui::TuiApproval {
                        source,
                        call,
                        reply,
                    } => {
                        assert_eq!(source.unwrap().id, child_source.id);
                        reply
                            .send(Approval::Approve {
                                command: call.command,
                                target: Target::Terminal,
                            })
                            .ok();
                    }
                    Ui::CreatePty { request, reply } => {
                        assert_eq!(request.source.as_deref(), Some("agent-1"));
                        assert_eq!(request.spec.cwd.as_deref(), Some("/tmp/selected"));
                        assert_eq!(request.call_id, "child-call");
                        let mut target = request.parent.target;
                        target.session_id = "pty-created".into();
                        reply.send(Ok(target)).ok();
                        break;
                    }
                    Ui::CancelApproval(_) => {}
                    _ => panic!("unexpected bridge request"),
                }
            }
        });
        let mut args = create(&id);
        if let Request::Create(a) = &mut args {
            a.cwd = Some("/tmp/selected".into());
        }
        let out = execute(
            &executor,
            args,
            ExecLimits::default(),
            Some(source),
            "child-call",
        )
        .await;
        assert!(!out.is_error, "{}", out.inline);
        assert!(out.inline.contains("pty-created"));
        handle.await.unwrap();
    }
}
