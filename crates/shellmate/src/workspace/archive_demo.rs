//! Scratch-only archive screenshot fixtures and explicitly requested real-server hooks.
use super::*;
use shellmate_store::{Preference as P, Recording};
use shellmate_term::{
    archive,
    cast::{Header, Writer},
};
use std::time::Duration;
impl Workspace {
    pub(crate) fn demo_archives(&mut self, scenario: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.demo_productivity("commands", window, cx);
        self.commands_open = false;
        let scratch = std::env::temp_dir().join(format!(
            "shellmate-archive-demo-{}",
            shellmate_core::HostId::generate()
        ));
        self.data_root = scratch.join("data");
        self.log_root = scratch.join("logs");
        if let Some(section) = scenario.strip_prefix("settings-") {
            let section = match section {
                "cred" => 0,
                "sync" => 1,
                "term" => 2,
                "local" => 3,
                _ => 4,
            };
            self.open_settings(section, window, cx);
            return;
        }
        if scenario == "recording" {
            if let Some(id) = self.focused_session().map(|s| s.id) {
                let _ = self.start_recording(id, cx);
                if let Some(r) = self.running_recordings.get_mut(&id) {
                    r.demo_elapsed = Some(12.);
                }
            }
            return;
        }
        let now = archive::now();
        for (i, (title, host)) in [
            ("检查 nginx 状态", "prod-web-01"),
            ("统计库表大小", "prod-db-01"),
            ("部署日志", "staging-api"),
        ]
        .into_iter()
        .enumerate()
        {
            let (path, file) =
                archive::recording_file(&self.data_root, host, now - i as i64 * 86400).unwrap();
            let mut header = Header::new(100, 28);
            header.title = Some(title.into());
            header.timestamp = Some(now);
            header.env.insert("TERM".into(), "xterm-256color".into());
            let mut writer = Writer::new(std::io::BufWriter::new(file), &header).unwrap();
            for (at, text) in [
                (0., "Last login: Mon Nov 16 09:41:03 2026\r\n"),
                (1., "\x1b[32mdeploy@prod-web-01\x1b[0m:~$ uptime\r\n"),
                (
                    2.,
                    " 09:41:13 up 12 days, 2 users, load average: 0.12, 0.18, 0.23\r\n",
                ),
                (3., "$ systemctl status nginx --no-pager\r\n"),
                (
                    4.,
                    "\x1b[32m● nginx.service - A high performance web server\r\n   Active: active (running)\x1b[0m\r\n",
                ),
                (8., "$ printf '中文 ✓\\n'\r\n中文 ✓\r\n"),
                (10., "$ "),
            ] {
                writer.output(at, text.as_bytes()).unwrap();
            }
            let writer = writer.finish(10.).unwrap();
            let size = writer.get_ref().metadata().unwrap().len();
            let row = Recording {
                id: 0,
                title: title.into(),
                host: host.into(),
                label: host.into(),
                path: path.to_string_lossy().into_owned(),
                started_at: now - i as i64 * 86400,
                duration: 10.,
                size,
                cols: 100,
                rows: 28,
            };
            self.store.borrow_mut().add_recording(&row).unwrap();
        }
        let mut log = shellmate_term::session_log::LogWriter::new(self.log_root.clone(), "prod-web-01");
        log.lines(
            &[
                "nginx: configuration file syntax is ok".into(),
                "systemctl reload nginx".into(),
                "nginx.service: Reloaded successfully".into(),
                "health check: 200 OK".into(),
                "进度 99%".into(),
            ],
            now,
        )
        .unwrap();
        log.flush().unwrap();
        self.open_records(None, scenario == "logs", window, cx);
        if let Some(view) = self.records_window.as_ref().and_then(|v| v.upgrade()) {
            view.update(cx, |r, cx| {
                if scenario == "logs" {
                    r.demo_query("nginx", window, cx);
                } else {
                    r.demo_position(false, cx);
                }
                if scenario == "records-delete" {
                    r.delete_selected(window, cx);
                }
            });
        }
    }
    pub(crate) fn demo_archive_live(&mut self, args: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((mode, name)) = args.split_once(':') else {
            return;
        };
        let Some(host) = self
            .book
            .hosts
            .iter()
            .find(|h| h.name == name)
            .map(|h| h.id.clone())
        else {
            log::info!("p6live archive missing-host");
            return;
        };
        // Live review always passes scratch overrides; refuse to fall back to user directories.
        if std::env::var_os("SHELLMATE_DATA_DIR").is_none() || std::env::var_os("SHELLMATE_LOG_DIR").is_none()
        {
            log::info!("p6live archive requires scratch directories");
            return;
        }
        if mode == "log"
            && let Err(e) = self.set_preference(P::SessionLog, true, window, cx)
        {
            window.push_notification(Notification::error(e), cx);
            return;
        }
        self.connect_host(&host, window, cx);
        let mode = mode.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let mut id = None;
            for _ in 0..1200 {
                id = this
                    .update_in(cx, |w, _, cx| {
                        w.sessions
                            .iter()
                            .find(|s| {
                                s.ssh().is_some_and(|s| s.host.id == host)
                                    && w.command_available(Some(s.id), cx)
                            })
                            .map(|s| s.id)
                    })
                    .ok()
                    .flatten();
                if id.is_some() {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(50)).await;
            }
            let Some(id) = id else {
                log::info!("p6live archive connect-timeout");
                return;
            };
            cx.background_executor().timer(Duration::from_millis(600)).await;
            if mode == "record" {
                this.update_in(cx, |w, window, cx| {
                    log::info!("p6live record-start");
                    if let Err(e) = w.start_recording(id, cx) {
                        window.push_notification(Notification::error(e), cx);
                    }
                    w.archive_hook_send(id, "ls -la /etc | head -5", window, cx);
                })
                .ok();
                cx.background_executor().timer(Duration::from_millis(700)).await;
                this.update_in(cx, |w, window, cx| {
                    log::info!("p6live record-top");
                    w.archive_hook_send(id, "top", window, cx);
                })
                .ok();
                cx.background_executor().timer(Duration::from_secs(3)).await;
                this.update_in(cx, |w, _, cx| {
                    log::info!("p6live record-top-quit");
                    if let Some(s) = w.sessions.iter().find(|s| s.id == id) {
                        s.terminal.update(cx, |t, _| t.input(b"q"));
                    }
                })
                .ok();
                cx.background_executor().timer(Duration::from_millis(600)).await;
                this.update_in(cx, |w, window, cx| {
                    log::info!("p6live record-unicode");
                    w.archive_hook_send(id, "printf '中文 ✓\\n'", window, cx);
                })
                .ok();
                cx.background_executor().timer(Duration::from_millis(700)).await;
                this.update_in(cx, |w, window, cx| {
                    log::info!("p6live record-stop");
                    match w.stop_recording(id, cx) {
                        Ok(row) => {
                            w.open_records(row, false, window, cx);
                            if let Some(view) = w.records_window.as_ref().and_then(|v| v.upgrade()) {
                                view.update(cx, |r, cx| r.demo_position(true, cx));
                            }
                        }
                        Err(e) => window.push_notification(Notification::error(e), cx),
                    }
                })
                .ok();
            } else {
                for (step, command, delay) in [
                    (
                        "log-progress",
                        "for i in 1 2 3; do printf '\\r进度 %d%%' $((i*33)); sleep 0.3; done; echo",
                        1600,
                    ),
                    ("log-scroll", "seq 1 80", 600),
                    ("log-clear", "clear", 600),
                    ("log-after-clear", "echo after-clear", 600),
                    ("log-less", "less /etc/hosts", 2000),
                ] {
                    this.update_in(cx, |w, window, cx| {
                        log::info!("p6live {step}");
                        w.archive_hook_send(id, command, window, cx);
                    })
                    .ok();
                    cx.background_executor().timer(Duration::from_millis(delay)).await;
                }
                this.update_in(cx, |w, _, cx| {
                    log::info!("p6live log-less-quit");
                    if let Some(s) = w.sessions.iter().find(|s| s.id == id) {
                        s.terminal.update(cx, |t, _| t.input(b"q"));
                    }
                })
                .ok();
                cx.background_executor().timer(Duration::from_millis(600)).await;
                this.update_in(cx, |w, window, cx| {
                    log::info!("p6live log-close");
                    if let Some(ix) = w.sessions.iter().position(|s| s.id == id) {
                        w.close_now(ix, window, cx);
                    }
                    w.open_records(None, true, window, cx);
                })
                .ok();
            }
        })
        .detach();
    }
    fn archive_hook_send(&mut self, id: usize, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let request = self.send_request(vec![id], false, false);
        if let Err(e) = self.send_reviewed(&request, text, window, cx) {
            window.push_notification(Notification::error(e), cx);
        }
    }
}
