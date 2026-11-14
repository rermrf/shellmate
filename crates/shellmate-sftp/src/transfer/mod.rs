//! The transfer engine: uploads and downloads of files and directories over an SFTP session,
//! queued per connection.
//!
//! One [`TransferQueue`] belongs to one connection. Each thing the user starts is an *item*: a
//! selection of files and directories going to one directory. Items run on the SSH runtime, at
//! most [`MAX_RUNNING`] at a time (the rest wait in order); the files of an item go one after the
//! other, each with many requests in flight. Everything is reported through an
//! [`async_channel`] of [`TransferEvent`]s; [`TransferQueue::snapshot`] gives the whole picture.
//!
//! # How a transfer behaves
//!
//! * The selection is walked first for the total size and file count. Inside directories,
//!   symlinks and special files are skipped and counted; a link the user selected directly is
//!   followed once.
//! * Uploads write `.<name>.shellmate-upload` next to the target (created exclusively) and replace
//!   the target when done, so a target is never half written. Overwriting keeps the permission
//!   bits of the file being replaced, and tries (only root can succeed) to keep its owner. Unlike
//!   the editor, an upload does not fall back to writing in place when the owner cannot be kept:
//!   that would give up both the atomic replace and resuming. Only when the directory has no room
//!   for a temporary file does an overwrite go straight into the existing file, as the editor
//!   does; that is reported in [`TransferView::in_place`], is not atomic, and starts over when
//!   resumed. Existing top-level names are asked about once per item
//!   ([`TransferState::AwaitingClash`], [`Choice`]). Downloads never ask: they write
//!   `<name>.shellmate-download` and pick a free local name (`name (1)`).
//! * Pausing stops at a chunk boundary and keeps the temporary file; resuming continues from its
//!   length, after checking that the source still has the size and time it had and that the
//!   temporary file is still a regular file no longer than the source. If not, that file starts
//!   over. Cancelling removes the temporary file; files of a directory that were already
//!   finished stay.
//! * A disconnect pauses running items ([`PauseReason::Disconnected`]); handing the queue a new
//!   session ([`TransferQueue::set_session`]) resumes exactly those. A session that stops
//!   answering (a request times out) counts as lost; an error that comes back while the session is
//!   being taken away is not the file's failure. When a transfer finds the session it was given
//!   unusable, the queue says so once ([`TransferEvent::SessionLost`]), because the SFTP
//!   subsystem can die while the SSH connection lives on. The engine never reconnects by itself.
//! * Uploads resume from the part of the temporary file that is known to be written without gaps
//!   (servers may apply requests out of order), rewriting what lies above.
//! * The queue lives in memory: it is not saved across restarts of the application.

mod control;
mod names;
mod plan;
mod queue;
mod run;
mod speed;

pub use names::{candidates, first_free, numbered};
pub use queue::TransferQueue;
pub use speed::SpeedMeter;

use std::path::PathBuf;

/// At most this many items run at the same time on one connection.
pub const MAX_RUNNING: usize = 2;

/// An item keeps the first this many failures (and counts the rest), so that a transfer of a
/// whole tree on a broken server does not carry an endless list around.
pub const MAX_FAILURES_KEPT: usize = 100;

/// Identifies an item of a queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub u64);

/// Which way an item moves files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From this machine to the server.
    Upload,
    /// From the server to this machine.
    Download,
}

/// What to do about names that already exist at the destination of an upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Skip those names.
    Skip,
    /// Upload them as `name (1)`, `name (2)`, …
    KeepBoth,
    /// Replace files; directories are merged and the files inside overwritten.
    Overwrite,
}

/// Why an item is paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    /// The user paused it.
    User,
    /// The connection was lost; it resumes by itself when a new session is set.
    Disconnected,
}

/// Where an item stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferState {
    /// 排队中: waiting for its turn (or for a session).
    Queued,
    /// 等待处理同名: names exist at the destination; waiting for [`TransferQueue::answer_clash`].
    AwaitingClash,
    /// 进行中.
    Running,
    /// 已暂停 (by the user) / 已暂停 · 连接断开.
    Paused(PauseReason),
    /// 已完成.
    Done,
    /// 失败, with the reason.
    Failed(String),
    /// 已取消.
    Cancelled,
}

impl TransferState {
    /// The state as the queue shows it.
    pub fn label(&self) -> String {
        match self {
            Self::Queued => "排队中".into(),
            Self::AwaitingClash => "等待处理同名".into(),
            Self::Running => "进行中".into(),
            Self::Paused(PauseReason::User) => "已暂停".into(),
            Self::Paused(PauseReason::Disconnected) => "已暂停 · 连接断开".into(),
            Self::Done => "已完成".into(),
            Self::Failed(reason) => format!("失败：{reason}"),
            Self::Cancelled => "已取消".into(),
        }
    }

    /// Whether the item is over (done, failed or cancelled).
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Done | Self::Failed(_) | Self::Cancelled)
    }
}

/// Top-level names of an upload that already exist at the destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clash {
    /// The destination directory on the server.
    pub dir: String,
    /// How many names exist.
    pub count: usize,
    /// The names (up to five, for the dialog).
    pub names: Vec<String>,
}

impl Clash {
    /// The dialog title: `3 个文件已存在`.
    pub fn title(&self) -> String {
        format!("{} 个文件已存在", self.count)
    }
}

/// What was left out of an item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    /// Symbolic links inside directories.
    pub symlinks: u32,
    /// Devices, pipes and sockets.
    pub special: u32,
    /// Top-level names left out because of [`Choice::Skip`].
    pub existing: u32,
}

impl Skipped {
    /// `跳过 3 个符号链接、2 个特殊文件`, or `None` if nothing was skipped.
    pub fn note(&self) -> Option<String> {
        let parts: Vec<String> = [(self.symlinks, "符号链接"), (self.special, "特殊文件"), (self.existing, "同名项")]
            .into_iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, what)| format!("{count} 个{what}"))
            .collect();
        (!parts.is_empty()).then(|| format!("跳过 {}", parts.join("、")))
    }
}

/// A file that could not be transferred.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferFailure {
    /// The path concerned.
    pub path: String,
    /// What went wrong.
    pub reason: String,
}

/// The reason an item with failures ends with: the count, the first reason and up to five paths.
/// `kept` are the failures that were kept (the first ones), `total` how many there were.
pub fn failure_summary(kept: &[TransferFailure], total: usize) -> String {
    match kept {
        [] => String::new(),
        [only] if total <= 1 => only.reason.clone(),
        [first, ..] => {
            let shown: Vec<&str> = kept.iter().take(5).map(|f| f.path.as_str()).collect();
            let more = if total > 5 { format!(" 等 {total} 个") } else { String::new() };
            format!("{total} 个文件传输失败，首个原因：{}（{}{more}）", first.reason, shown.join("、"))
        }
    }
}

/// Everything the UI shows about one item.
#[derive(Clone, Debug, PartialEq)]
pub struct TransferView {
    /// The item.
    pub id: ItemId,
    /// Which way it goes.
    pub direction: Direction,
    /// What to call it: the name of the one thing selected, or `3 个项目`.
    pub name: String,
    /// The destination: a directory on the server (uploads) or on this machine (downloads).
    pub destination: String,
    /// Where it stands.
    pub state: TransferState,
    /// Bytes done (files that were finished, plus the one in progress).
    pub bytes_done: u64,
    /// Bytes in all (0 until the walk is over).
    pub bytes_total: u64,
    /// Files finished.
    pub files_done: u64,
    /// Files in all.
    pub files_total: u64,
    /// The selection is still being walked.
    pub scanning: bool,
    /// Bytes per second over the last few seconds (0 unless running).
    pub speed: f64,
    /// The file being transferred.
    pub current: Option<String>,
    /// What was left out.
    pub skipped: Skipped,
    /// Files of an upload that were written over the existing file because the directory has no
    /// room for a temporary file (not atomic).
    pub in_place: u32,
    /// The files that failed (the first [`MAX_FAILURES_KEPT`] of them).
    pub failures: Vec<TransferFailure>,
    /// How many files failed in all.
    pub failure_total: usize,
    /// The names to ask about, while [`TransferState::AwaitingClash`].
    pub clash: Option<Clash>,
    /// For a finished download: where the (first) thing ended up, to show in Finder.
    pub result: Option<PathBuf>,
    /// For an upload: the temporary file on the server that the file in progress is being written
    /// to (or, while paused, was), so that a file panel can leave it out of the listing. `None`
    /// when nothing is in progress, and after the file is in place.
    pub temp: Option<String>,
}

impl TransferView {
    /// `2 个文件已直接写回（目录不可写）`, or `None`.
    pub fn in_place_note(&self) -> Option<String> {
        (self.in_place > 0).then(|| format!("{} 个文件已直接写回（目录不可写）", self.in_place))
    }

    /// Progress from 0 to 1 (0 while the total is not known).
    pub fn fraction(&self) -> f64 {
        if self.bytes_total == 0 { 0.0 } else { (self.bytes_done as f64 / self.bytes_total as f64).min(1.0) }
    }
}

/// What the queue reports.
#[derive(Clone, Debug, PartialEq)]
pub enum TransferEvent {
    /// A new item entered the queue.
    Added(TransferView),
    /// An item changed state (the view has the details).
    State(TransferView),
    /// Progress of a running item; at most about five per second per item.
    Progress(TransferView),
    /// Items were removed by [`TransferQueue::clear_finished`].
    Removed(Vec<ItemId>),
    /// Something changed in this directory on the server: the file panel may refresh.
    RemoteChanged(String),
    /// A transfer found the session unusable (the connection is gone, or the SFTP subsystem died or
    /// stopped answering) while the queue still holds it. Sent once per session: the owner should
    /// open a new session if the connection is alive and hand it to
    /// [`TransferQueue::set_session`]; the paused items then resume by themselves.
    SessionLost,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_have_their_labels() {
        assert_eq!(TransferState::Queued.label(), "排队中");
        assert_eq!(TransferState::AwaitingClash.label(), "等待处理同名");
        assert_eq!(TransferState::Running.label(), "进行中");
        assert_eq!(TransferState::Paused(PauseReason::User).label(), "已暂停");
        assert_eq!(TransferState::Paused(PauseReason::Disconnected).label(), "已暂停 · 连接断开");
        assert_eq!(TransferState::Done.label(), "已完成");
        assert_eq!(TransferState::Failed("权限不足".into()).label(), "失败：权限不足");
        assert_eq!(TransferState::Cancelled.label(), "已取消");
        assert!(TransferState::Done.is_finished() && TransferState::Cancelled.is_finished());
        assert!(TransferState::Failed(String::new()).is_finished());
        assert!(!TransferState::Running.is_finished() && !TransferState::Paused(PauseReason::User).is_finished());
    }

    #[test]
    fn the_skipped_note_lists_what_was_left_out() {
        assert_eq!(Skipped::default().note(), None);
        assert_eq!(Skipped { symlinks: 3, ..Default::default() }.note().as_deref(), Some("跳过 3 个符号链接"));
        assert_eq!(
            Skipped { symlinks: 3, special: 2, existing: 1 }.note().as_deref(),
            Some("跳过 3 个符号链接、2 个特殊文件、1 个同名项")
        );
    }

    #[test]
    fn failures_are_summarized_with_count_first_reason_and_up_to_five_paths() {
        let failure =
            |n: usize| TransferFailure { path: format!("/d/f{n}"), reason: format!("权限不足：/d/f{n}") };
        assert_eq!(failure_summary(&[], 0), "");
        assert_eq!(failure_summary(&[failure(1)], 1), "权限不足：/d/f1");
        assert_eq!(
            failure_summary(&[failure(1), failure(2)], 2),
            "2 个文件传输失败，首个原因：权限不足：/d/f1（/d/f1、/d/f2）"
        );
        let many: Vec<_> = (1..=8).map(failure).collect();
        assert_eq!(
            failure_summary(&many, 8),
            "8 个文件传输失败，首个原因：权限不足：/d/f1（/d/f1、/d/f2、/d/f3、/d/f4、/d/f5 等 8 个）"
        );
        // Only the first ones are kept; the count is the real one.
        assert_eq!(
            failure_summary(&many[..2], 500),
            "500 个文件传输失败，首个原因：权限不足：/d/f1（/d/f1、/d/f2 等 500 个）"
        );
        assert_eq!(failure_summary(&many[..1], 40), "40 个文件传输失败，首个原因：权限不足：/d/f1（/d/f1 等 40 个）");
    }

    #[test]
    fn a_clash_has_the_prototype_title() {
        let clash = Clash { dir: "/srv".into(), count: 3, names: vec!["a".into()] };
        assert_eq!(clash.title(), "3 个文件已存在");
    }

    #[test]
    fn the_fraction_is_clamped_and_zero_without_a_total() {
        let mut view = TransferView {
            id: ItemId(1),
            direction: Direction::Upload,
            name: "a".into(),
            destination: "/".into(),
            state: TransferState::Running,
            bytes_done: 50,
            bytes_total: 0,
            files_done: 0,
            files_total: 0,
            scanning: true,
            speed: 0.0,
            current: None,
            skipped: Skipped::default(),
            in_place: 0,
            failures: Vec::new(),
            failure_total: 0,
            clash: None,
            result: None,
            temp: None,
        };
        assert_eq!(view.fraction(), 0.0);
        assert_eq!(view.in_place_note(), None);
        view.in_place = 2;
        assert_eq!(view.in_place_note().as_deref(), Some("2 个文件已直接写回（目录不可写）"));
        view.bytes_total = 200;
        assert_eq!(view.fraction(), 0.25);
        view.bytes_done = 500;
        assert_eq!(view.fraction(), 1.0);
    }
}
