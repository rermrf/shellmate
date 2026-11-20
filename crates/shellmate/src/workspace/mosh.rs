//! Mosh terminal lifetime is independent of the ConnGroup used by SSH panels.
use super::*;
use shellmate_ssh::{
    Connection,
    mosh::{self, ServerLease},
};
use std::time::Instant;
#[derive(Default)]
pub(super) struct MoshTab {
    pub independent: bool,
    pub port: Option<u16>,
    pub lease: Option<ServerLease>,
    task: Option<gpui::Task<()>>,
}
impl MoshTab {
    pub fn footer(&self) -> String {
        match self.port {
            Some(port) => format!("Mosh UDP {port} · 预测回显 自适应"),
            None => "Mosh · 预测回显 自适应".into(),
        }
    }
}
impl Workspace {
    pub(super) fn open_mosh(
        &mut self,
        id: usize,
        connection: Connection,
        cx: &mut Context<Self>,
    ) {
        let Some(ssh) = self.ssh_mut(id) else { return };
        let Some(mosh) = ssh.mosh.as_mut() else {
            return;
        };
        if mosh.independent {
            return;
        }
        mosh.independent = true;
        let client = match mosh::find_client() {
            Ok(p) => p,
            Err(e) => {
                self.mosh_failed(id, e, cx);
                return;
            }
        };
        let Some(runtime) = crate::runtime::get(cx) else {
            self.mosh_failed(id, "SSH 运行时未能启动".into(), cx);
            return;
        };
        let join = runtime.spawn(mosh::prepare(connection, client));
        let task = cx.spawn(async move |_, cx| {
            let result = join.await;
            super::Route::Session(id).deliver(cx, |this, window, cx| {
                let prepared = match result {
                    Ok(Ok(p)) => p,
                    Ok(Err(e)) => {
                        this.mosh_failed(id, e, cx);
                        return;
                    }
                    Err(_) => {
                        this.mosh_failed(id, "Mosh 启动任务已中止".into(), cx);
                        return;
                    }
                };
                let Some(session) = this.sessions.iter_mut().find(|s| s.id == id) else {
                    return;
                };
                let SessionKind::Ssh(ssh) = &mut session.kind else {
                    return;
                };
                let Some(mosh) = ssh.mosh.as_mut() else {
                    return;
                };
                mosh.task = None;
                mosh.port = Some(prepared.port);
                mosh.lease = Some(prepared.lease.clone());
                let lease = prepared.lease.clone();
                let started = Instant::now();
                let attached = session.terminal.update(cx, |t, cx| {
                    t.set_auto_reconnect(false);
                    t.attach(
                        move |sink, dims| {
                            let output = sink.clone();
                            match shellmate_term::LocalPty::spawn_program(
                                &prepared.program,
                                dims,
                                move |bytes| output.output(bytes),
                                move |code| {
                                    lease.client_exited(code, started.elapsed());
                                    sink.exited(code);
                                },
                            ) {
                                Ok(pty) => Ok(Box::new(pty)),
                                Err(_) => {
                                    prepared
                                        .lease
                                        .client_exited(None, std::time::Duration::ZERO);
                                    Err(anyhow::anyhow!(
                                        "无法启动 mosh-client，请检查本机程序和权限"
                                    ))
                                }
                            }
                        },
                        cx,
                    )
                });
                ssh.status = if attached {
                    ssh::SshStatus::Connected
                } else {
                    ssh::SshStatus::Failed
                };
                if attached {
                    let host_id = ssh.host.id.clone();
                    if !this.hub.temporary_hosts.borrow().contains_key(&host_id) {
                        let _ = this.hub.store.borrow_mut().touch_connected(&host_id);
                    }
                    this.type_pending(id, window, cx);
                }
                this.sync_sidebar(cx);
                cx.notify();
            });
        });
        if let Some(m) = self.ssh_mut(id).and_then(|s| s.mosh.as_mut()) {
            m.task = Some(task);
        }
    }
    fn mosh_failed(&mut self, id: usize, error: String, cx: &mut Context<Self>) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) {
            if let SessionKind::Ssh(ssh) = &mut session.kind {
                ssh.status = ssh::SshStatus::Failed;
                if let Some(m) = &mut ssh.mosh {
                    m.task = None;
                }
            }
            session.terminal.update(cx, |t, cx| {
                t.set_auto_reconnect(false);
                t.disconnect(error, cx);
            });
        }
        self.sync_sidebar(cx);
        cx.notify();
    }
    pub(super) fn poll_mosh_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lost = self.hub
            .groups.borrow()
            .iter()
            .filter(|g| {
                g.auxiliary.as_ref().is_some_and(Connection::is_closed)
                    && g.machine.phase() == crate::reconnect::Phase::Idle
            })
            .map(|g| g.id)
            .collect::<Vec<_>>();
        for id in lost {
            if let Some(mut g) = self.group_mut(id) {
                g.auxiliary = None;
            }
            self.link_lost(id, cx);
            self.forward_lost(id, cx);
            self.monitor_lost(id, cx);
            self.step_group(id, crate::reconnect::Input::Lost, window, cx);
        }
    }
    /// A panel retry repairs only SSH; it must never reset the UDP terminal.
    pub(super) fn retry_mosh_panel(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(group) = self
            .sessions
            .iter()
            .find(|s| s.id == id)
            .and_then(Session::ssh)
            .filter(|s| s.mosh.is_some())
            .map(|s| s.group)
        else {
            return false;
        };
        if self
            .group(group)
            .is_some_and(|g| g.live_connection().is_some())
        {
            return false;
        }
        self.poll_mosh_connections(window, cx);
        let input = if self
            .group(group)
            .is_some_and(|g| g.machine.phase() == crate::reconnect::Phase::Idle)
        {
            crate::reconnect::Input::Start
        } else {
            crate::reconnect::Input::RetryNow
        };
        self.step_group(group, input, window, cx);
        true
    }

    pub(super) fn offer_mosh_ssh(
        &mut self,
        mut host: shellmate_core::Host,
        mut chain: Vec<shellmate_core::Host>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.entity().downgrade();
        let source = host.id.clone();
        host.protocol = shellmate_core::Protocol::Ssh;
        host.id = HostId::generate();
        host.name = format!("{} · SSH", host.name);
        if let Some(last) = chain.last_mut() {
            *last = host.clone();
        }
        crate::dialogs::confirm::ConfirmDialog::open(
            "无法使用 Mosh",
            mosh::JUMP_REFUSAL,
            "以 SSH 打开",
            false,
            move |w, cx| {
                weak.update(cx, |this, cx| {
                    this.open_ssh_fallback(host.clone(), chain.clone(), source.clone(), w, cx)
                })
                .ok();
                Ok(())
            },
            window,
            cx,
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn footer_reports_udp_and_adaptive_without_fake_latency() {
        let tab = MoshTab {
            port: Some(60001),
            ..Default::default()
        };
        assert_eq!(tab.footer(), "Mosh UDP 60001 · 预测回显 自适应");
    }
}
