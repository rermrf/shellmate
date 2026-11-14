//! The connection that SSH tabs share.
//!
//! Tabs opened with ⌘⇧T ride on the connection of the tab they were copied from, each on its own
//! shell channel. Such tabs form a [`ConnGroup`]: it owns everything that belongs to the
//! connection rather than to one tab: the reconnect loop ([`Reconnect`]), the running connection
//! attempt with its dialogs, the countdown timer and the secrets that authenticated. One loop per
//! connection means a lost connection with five tabs reconnects (and asks, if it must) once.
//!
//! The group does not hold the connection open: the shell channels in the tabs do, so the
//! connection closes with its last tab. It only keeps a [`WeakConnection`] to hand the live
//! connection to new tabs.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::{Entity, Task};
use shellmate_core::{Host, HostId};
use shellmate_sftp::link::SftpLink;
use shellmate_sftp::transfer::ItemId;
use shellmate_ssh::{Connection, WeakConnection};
use tokio::task::AbortHandle;

use crate::connect::{AuthMemory, Endpoint};
use crate::files::transfers::TransferList;
use crate::reconnect::{Phase, Reconnect};

/// What makes two connections interchangeable: the saved host and the route to it as resolved
/// (jump hosts included). A host edited after a tab was opened has a different key, so copying
/// that tab connects afresh instead of reusing a connection made with the old settings.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PoolKey {
    host: HostId,
    route: Vec<Endpoint>,
}

impl PoolKey {
    /// The key of the resolved `chain` (jump hosts first, the target last).
    pub fn of(chain: &[Host], default_user: &str) -> Self {
        Self {
            host: chain.last().map(|host| host.id.clone()).unwrap_or_else(HostId::generate),
            route: chain.iter().map(|host| Endpoint::of(host, default_user)).collect(),
        }
    }
}

/// Aborts a task on the SSH runtime when dropped: closing a tab that is still connecting must
/// stop the connection from going on in the background.
pub struct AbortOnDrop(Option<AbortHandle>);

impl AbortOnDrop {
    pub fn new(handle: AbortHandle) -> Self {
        Self(Some(handle))
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// A connection attempt in flight: the SSH task and the UI tasks following it. Dropping it
/// cancels all of them.
pub struct Attempt {
    _abort: AbortOnDrop,
    _tasks: Vec<Task<()>>,
}

impl Attempt {
    pub fn new(abort: AbortHandle, tasks: Vec<Task<()>>) -> Self {
        Self { _abort: AbortOnDrop::new(abort), _tasks: tasks }
    }
}

/// Which questions about names that exist are on screen or were put aside.
#[derive(Default)]
pub struct ClashState {
    /// The item whose dialog is open.
    pub open: Option<ItemId>,
    /// Items whose dialog was closed with Esc: they keep waiting (their queue row has 处理) and are
    /// not asked about again by themselves.
    pub dismissed: HashSet<ItemId>,
}

/// One connection and the tabs on it.
pub struct ConnGroup {
    pub id: usize,
    pub key: PoolKey,
    /// The reconnect loop.
    pub machine: Reconnect,
    /// The live connection, if any (see the module docs).
    pub connection: Option<WeakConnection>,
    /// How many shell channels were opened on the current connection (the next one is `+ 1`).
    pub channels: u32,
    /// What authenticated on this connection, for the next attempt.
    pub memory: Arc<AuthMemory>,
    /// Why the last attempt failed, for the progress line before the next one.
    pub last_failure: Option<String>,
    /// The last attempt ended because the user declined a question (not because anything broke).
    pub declined: bool,
    /// The running attempt was started by the user (Enter, a click), not by the countdown.
    pub manual: bool,
    /// A dialog of the running attempt may be open.
    pub prompting: bool,
    /// The running attempt.
    pub attempt: Option<Attempt>,
    /// The countdown before the next attempt.
    pub timer: Option<Task<()>>,
    /// The SFTP session and transfer queue of the connection, shared by its tabs (`None` only if the
    /// SSH runtime could not be started). Dropping the group shuts it down, which lets the
    /// connection close with its last tab.
    pub link: Option<SftpLink>,
    /// The UI tasks that follow the link; they end with the group.
    pub link_tasks: Vec<Task<()>>,
    /// What the connection's transfer queue holds, as every one of its panels shows it.
    pub transfers: Option<Entity<TransferList>>,
    /// The dialog about names that exist.
    pub clash: ClashState,
}

impl ConnGroup {
    pub fn new(id: usize, key: PoolKey, machine: Reconnect, memory: Arc<AuthMemory>) -> Self {
        Self {
            id,
            key,
            machine,
            connection: None,
            channels: 0,
            memory,
            last_failure: None,
            declined: false,
            manual: false,
            prompting: false,
            attempt: None,
            timer: None,
            link: None,
            link_tasks: Vec::new(),
            transfers: None,
            clash: ClashState::default(),
        }
    }

    /// The connection if it is up, for a new tab to open a channel on. Not while the group is
    /// reconnecting: a connection from before the loss is not to be trusted.
    pub fn live_connection(&self) -> Option<Connection> {
        if self.machine.phase() != Phase::Idle {
            return None;
        }
        self.connection.as_ref()?.upgrade()
    }

    /// Whether a dialog of this group may be open.
    pub fn has_open_prompt(&self) -> bool {
        (self.prompting && self.attempt.is_some()) || self.clash.open.is_some()
    }

    /// Stops whatever is running: the attempt (and with it its questions) and the countdown.
    pub fn stop(&mut self) {
        self.attempt = None;
        self.timer = None;
        self.prompting = false;
        self.manual = false;
    }

    /// Claims the next channel number on the current connection.
    pub fn next_channel(&mut self) -> u32 {
        self.channels += 1;
        self.channels
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn host(name: &str) -> Host {
        Host::new(name, format!("{name}.example.com"))
    }

    #[test]
    fn the_key_is_the_host_and_its_resolved_route() {
        let (bastion, db) = (host("bastion"), host("db"));
        let direct = PoolKey::of(std::slice::from_ref(&db), "me");
        assert_eq!(direct, PoolKey::of(std::slice::from_ref(&db), "me"), "the same host and route");
        assert_ne!(direct, PoolKey::of(&[bastion.clone(), db.clone()], "me"), "a jump host changes the route");
        assert_ne!(direct, PoolKey::of(std::slice::from_ref(&host("web")), "me"), "another host");

        let mut moved = db.clone();
        moved.addr = "db2.example.com".into();
        assert_ne!(direct, PoolKey::of(std::slice::from_ref(&moved), "me"), "the same host edited to another address");
        let mut other_port = db.clone();
        other_port.port = 2222;
        assert_ne!(direct, PoolKey::of(&[other_port], "me"));
    }

    #[test]
    fn the_login_name_counts_and_the_default_user_is_resolved() {
        let mut db = host("db");
        let default = PoolKey::of(std::slice::from_ref(&db), "me");
        db.user = Some("me".into());
        assert_eq!(
            default,
            PoolKey::of(std::slice::from_ref(&db), "me"),
            "no user and the local user are the same login"
        );
        db.user = Some("root".into());
        assert_ne!(default, PoolKey::of(std::slice::from_ref(&db), "me"));
    }

    #[tokio::test]
    async fn dropping_a_guard_aborts_the_task() {
        let task = tokio::spawn(async { std::future::pending::<()>().await });
        let guard = AbortOnDrop::new(task.abort_handle());
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!task.is_finished(), "still waiting, e.g. for the user to answer a dialog");
        drop(guard);
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn dropping_a_group_with_a_running_attempt_aborts_it() {
        let task = tokio::spawn(async { std::future::pending::<()>().await });
        let mut group = ConnGroup::new(1, PoolKey::of(&[host("db")], "me"), Reconnect::initial(), AuthMemory::new());
        group.attempt = Some(Attempt::new(task.abort_handle(), Vec::new()));
        group.prompting = true;
        assert!(group.has_open_prompt());
        drop(group);
        assert!(task.await.unwrap_err().is_cancelled(), "closing the last tab cancels the connection task");
    }

    #[tokio::test]
    async fn dropping_a_group_shuts_its_sftp_link_down() {
        let (link, changes, _events) = SftpLink::new(tokio::runtime::Handle::current());
        let mut group = ConnGroup::new(1, PoolKey::of(&[host("db")], "me"), Reconnect::connected(), AuthMemory::new());
        group.link = Some(link);
        assert!(!changes.is_closed(), "the link is alive while the group is");
        drop(group);
        // Closing the last tab drops the group; with the link gone nothing holds the SFTP session
        // (and through it the connection) open any more.
        assert!(changes.recv().await.is_err(), "nobody is left to report changes");
    }

    #[tokio::test]
    async fn stopping_a_group_cancels_the_attempt_and_forgets_the_prompt() {
        let task = tokio::spawn(async { std::future::pending::<()>().await });
        let mut group = ConnGroup::new(1, PoolKey::of(&[host("db")], "me"), Reconnect::initial(), AuthMemory::new());
        group.attempt = Some(Attempt::new(task.abort_handle(), Vec::new()));
        group.prompting = true;
        group.stop();
        assert!(!group.has_open_prompt());
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn channel_numbers_count_up_per_connection() {
        let mut group = ConnGroup::new(1, PoolKey::of(&[host("db")], "me"), Reconnect::connected(), AuthMemory::new());
        assert_eq!((group.next_channel(), group.next_channel(), group.next_channel()), (1, 2, 3));
    }

    #[test]
    fn a_group_without_a_connection_has_none_to_share() {
        let group = ConnGroup::new(1, PoolKey::of(&[host("db")], "me"), Reconnect::connected(), AuthMemory::new());
        assert!(group.live_connection().is_none());
    }
}
