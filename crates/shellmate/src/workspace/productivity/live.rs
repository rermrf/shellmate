//! Explicit debug-only real-server review hooks. Never log command text.
use super::*;
use crate::{dialogs::DialogBody, productivity::multi::Pane};
use std::time::Duration;

impl Workspace {
    pub(crate) fn demo_productivity_live(&mut self, args: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((mode, rest)) = args.split_once(':') else {
            log::info!("p6live invalid hook");
            return;
        };
        let (hosts, command) = rest.split_once(':').unwrap_or((rest, ""));
        let names: Vec<_> = hosts.split(',').collect();
        if names.len() != 2 || !matches!(mode, "broadcast" | "split") {
            log::info!("p6live invalid hook");
            return;
        }
        let Some(ids) = names
            .iter()
            .map(|name| {
                self.book
                    .hosts
                    .iter()
                    .find(|h| h.name == *name)
                    .map(|h| h.id.clone())
            })
            .collect::<Option<Vec<_>>>()
        else {
            log::info!("p6live missing saved host");
            return;
        };
        if ids[0] == ids[1] {
            log::info!("p6live requires two distinct hosts");
            return;
        }
        self.live_review = true;
        for id in &ids {
            self.connect_host(id, window, cx);
        }
        let workspace = cx.entity();
        let mode = mode.to_string();
        let (command, drop_second) = command
            .strip_suffix(":drop-second")
            .map_or((command, false), |c| (c, true));
        let command = if command == "drop-second" {
            "hostname; echo bc-$((6*7))"
        } else {
            command
        }
        .to_string();
        let drop_second = drop_second || rest.ends_with(":drop-second");
        cx.spawn_in(window, async move |_, cx| {
            let mut tabs = None;
            for _ in 0..1200 {
                tabs = workspace
                    .update_in(cx, |w, _, cx| {
                        ids.iter()
                            .map(|host| {
                                w.sessions
                                    .iter()
                                    .find(|s| s.ssh().is_some_and(|s| &s.host.id == host))
                                    .filter(|s| w.command_available(Some(s.id), cx))
                                    .map(|s| s.id)
                            })
                            .collect::<Option<Vec<_>>>()
                    })
                    .ok()
                    .flatten();
                if tabs.is_some() {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(50)).await;
            }
            let Some(tabs) = tabs else {
                log::info!("p6live connection wait timed out");
                return;
            };
            log::info!("p6live both-connected");
            // Let shell startup and its mode-2004 request settle before interacting.
            cx.background_executor().timer(Duration::from_millis(500)).await;
            if mode == "broadcast" {
                workspace
                    .update_in(cx, |w, window, cx| {
                        w.broadcast.checked = tabs.iter().copied().collect();
                        w.set_broadcast(true, window, cx);
                        log::info!("p6live broadcast-enabled");
                    })
                    .ok();
                if drop_second {
                    let connection = workspace
                        .update_in(cx, |w, _, _| {
                            let s = w.sessions.iter().find(|s| s.id == tabs[1])?;
                            log::info!("p6live drop-second → {}", s.title);
                            w.group(s.ssh()?.group)?.live_connection()
                        })
                        .ok()
                        .flatten();
                    if let Some(connection) = connection {
                        let task = workspace
                            .update_in(cx, |_, _, cx| {
                                crate::runtime::get(cx).map(|rt| {
                                    rt.spawn(async move {
                                        connection.disconnect().await;
                                        connection.closed().await;
                                    })
                                })
                            })
                            .ok()
                            .flatten();
                        if let Some(task) = task {
                            let _ = task.await;
                        }
                    }
                    for _ in 0..100 {
                        if workspace
                            .update_in(cx, |w, _, cx| !w.command_available(Some(tabs[1]), cx))
                            .unwrap_or(true)
                        {
                            break;
                        }
                        cx.background_executor().timer(Duration::from_millis(20)).await;
                    }
                }
                workspace
                    .update_in(cx, |w, window, cx| {
                        if drop_second && w.command_available(Some(tabs[1]), cx) {
                            log::info!("p6live disconnect wait failed");
                            return;
                        }
                        log::info!("p6live broadcast-submit");
                        w.submit_broadcast(&command, window, cx);
                        if let Some(body) = w.review_confirmation.take().and_then(|w| w.upgrade()) {
                            window.defer(cx, move |window, cx| {
                                log::info!("p6live confirm-defined-command");
                                body.update(cx, |b, cx| b.submit(window, cx));
                                log::info!("p6live broadcast-complete");
                            });
                        } else {
                            log::info!("p6live broadcast-complete");
                        }
                        if let Some(ix) = w.sessions.iter().position(|s| s.id == tabs[0]) {
                            w.activate(ix, window, cx);
                            log::info!("p6live show-first");
                        }
                    })
                    .ok();
            } else {
                workspace
                    .update_in(cx, |w, window, cx| {
                        if let Some(ix) = w.sessions.iter().position(|s| s.id == tabs[0]) {
                            w.activate(ix, window, cx);
                        }
                        w.show_panel(Panel::Terminal, window, cx);
                        w.split.close();
                        w.recent_sessions.retain(|id| *id != tabs[1]);
                        w.recent_sessions.insert(0, tabs[1]);
                        w.toggle_split(window, cx);
                        log::info!("p6live split-left-right");
                    })
                    .ok();
                for step in 0..2 {
                    // Wait for a layout pass and transport window-change before the stty command.
                    cx.background_executor().timer(Duration::from_millis(600)).await;
                    for pane in [Pane::Main, Pane::Other] {
                        workspace
                            .update_in(cx, |w, window, cx| {
                                log::info!(
                                    "p6live {}",
                                    match (step, pane) {
                                        (0, Pane::Main) => "focus-left",
                                        (0, Pane::Other) => "focus-right",
                                        (_, Pane::Main) => "focus-top",
                                        (_, Pane::Other) => "focus-bottom",
                                    }
                                );
                                w.focus_pane(pane, window, cx);
                                let id = w.focused_session().map(|s| s.id);
                                let request = w.send_request(id.into_iter().collect(), false, false);
                                log::info!("p6live split-stty-size");
                                let _ = w.send_reviewed(&request, "stty size", window, cx);
                            })
                            .ok();
                        cx.background_executor().timer(Duration::from_millis(250)).await;
                    }
                    if step == 0 {
                        workspace
                            .update_in(cx, |w, window, cx| {
                                w.flip_split(window, cx);
                                log::info!("p6live split-top-bottom");
                            })
                            .ok();
                    }
                }
                log::info!("p6live split-complete");
            }
        })
        .detach();
    }
}
