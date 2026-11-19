//! The workspace owns one optional status item and its foreground action receiver.
mod model;
mod native;

use super::streams::StreamSession;
use super::{Session, SessionKind, Workspace, group::Prompt, lifecycle::MainWindow};
use crate::updater;
use async_channel::Sender;
use gpui::{App, Context};
use model::{Action, Icon, Row, Snapshot, Status};
use native::NativeItem;
use shellmate_store::Preference;
use shellmate_term::SessionEnd;
use std::rc::Rc;

enum MenuRequest {
    Pop,
    #[cfg(debug_assertions)]
    Dump,
}

enum Message {
    Run(Action),
    Menu(MenuRequest),
}

pub(super) struct StatusBar {
    sender: Sender<Message>,
    item: Option<Rc<NativeItem>>,
    last: Option<Icon>,
}

impl StatusBar {
    pub(super) fn new(cx: &mut Context<Workspace>) -> Self {
        let (sender, receiver) = async_channel::unbounded();
        cx.spawn(async move |workspace, cx| {
            while let Ok(message) = receiver.recv().await {
                match message {
                    Message::Run(action) => cx.update(|cx| run(action, cx)),
                    Message::Menu(request) => {
                        let prepared = workspace.read_with(cx, |s, cx| {
                            Some((s.status_bar.item.clone()?, s.status_menu(cx)))
                        });
                        if let Ok(Some((item, rows))) = prepared {
                            // AppKit can spin a nested event loop. Release all GPUI borrows first.
                            item.menu(rows, request);
                        }
                    }
                }
            }
        })
        .detach();
        Self {
            sender,
            item: None,
            last: None,
        }
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_click(&self) {
        if let Some(item) = &self.item {
            item.debug_click();
        }
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_menu(&self) {
        let _ = self.sender.try_send(Message::Menu(MenuRequest::Dump));
    }
}

impl Workspace {
    fn status_of(&self, s: &Session, cx: &App) -> Status {
        match &s.kind {
            SessionKind::Local if s.exited => Status::Ended,
            SessionKind::Local => Status::Local,
            SessionKind::Ssh(ssh) => {
                let group = if ssh.status.wants_channel() {
                    self.group(ssh.group)
                } else {
                    None
                };
                Status::ssh(
                    ssh.status,
                    group.map_or(Prompt::None, |g| g.prompt),
                    group.is_some_and(|g| {
                        g.machine.has_connected()
                            && g.machine.phase() == crate::reconnect::Phase::Failed
                    }),
                )
            }
            SessionKind::Stream(stream) => stream_status(stream, s.terminal.read(cx).session_end()),
        }
    }

    pub(super) fn sync_status_item(&mut self, cx: &App) {
        if !self.preferences.get(Preference::ShowStatusIcon) {
            self.status_bar.item = None;
            self.status_bar.last = None;
            return;
        }
        let icon = Icon::from_sessions(self.sessions.iter().map(|s| self.status_of(s, cx)));
        let bar = &mut self.status_bar;
        if bar.last != Some(icon) {
            let item = bar
                .item
                .get_or_insert_with(|| Rc::new(NativeItem::new(&bar.sender)));
            item.update(bar.last, icon);
            bar.last = Some(icon);
        }
    }

    fn status_menu(&self, cx: &App) -> Vec<Row> {
        let sessions: Vec<_> = self
            .sessions
            .iter()
            .map(|s| Snapshot {
                id: s.id,
                title: &s.title,
                status: self.status_of(s, cx),
            })
            .collect();
        model::menu(
            &sessions,
            cx.global::<updater::Updater>().available_version(),
        )
    }
}

fn stream_status(stream: &StreamSession, end: Option<&SessionEnd>) -> Status {
    match end {
        Some(_) if stream.retry_in_flight() => Status::Reconnecting(None),
        Some(SessionEnd::Exited(Some(0))) => Status::Ended,
        Some(_) => Status::Failed,
        None if stream.connected() => Status::Connected,
        None => match &stream.snapshot {
            Some(state) if state.retrying => Status::Reconnecting(None),
            Some(_) => Status::Closed,
            None => Status::Connecting,
        },
    }
}

fn run(action: Action, cx: &mut App) {
    match action {
        Action::Show => MainWindow::reopen(cx),
        Action::Quit => MainWindow::update(cx, Workspace::request_quit),
        Action::CheckUpdate => MainWindow::with_restored_window(cx, updater::manual),
        Action::OpenUpdate => MainWindow::with_restored_window(cx, updater::open),
        Action::Tab(id) => MainWindow::with_restored_workspace(cx, move |s, w, cx| {
            if let Some(ix) = s.sessions.iter().position(|tab| tab.id == id) {
                s.activate(ix, w, cx);
            }
        }),
        Action::Local => MainWindow::with_restored_workspace(cx, Workspace::open_local),
        Action::Connect => MainWindow::with_restored_workspace(cx, Workspace::new_host),
        Action::Settings => {
            MainWindow::with_restored_workspace(cx, |s, w, cx| s.open_settings(0, w, cx))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_core::{Host, Protocol};

    #[test]
    fn streams_distinguish_closed_retrying_and_terminal_errors() {
        let mut host = Host::new("fixture", "fixture.invalid");
        for protocol in [Protocol::Serial, Protocol::Telnet] {
            host.protocol = protocol;
            let mut stream = StreamSession::new(host.clone());
            let error = SessionEnd::Disconnected("fixture".into());
            assert_eq!(stream_status(&stream, Some(&error)), Status::Failed);
            assert_eq!(
                stream_status(&stream, Some(&SessionEnd::Exited(Some(0)))),
                Status::Ended
            );
            if protocol == Protocol::Serial {
                assert_eq!(stream_status(&stream, None), Status::Closed);
                stream.snapshot.as_mut().unwrap().retrying = true;
                assert_eq!(stream_status(&stream, None), Status::Reconnecting(None));
                stream.snapshot.as_mut().unwrap().open = true;
            } else {
                assert_eq!(stream_status(&stream, None), Status::Connecting);
                stream.connected = true;
            }
            assert_eq!(stream_status(&stream, None), Status::Connected);
        }
    }
}
