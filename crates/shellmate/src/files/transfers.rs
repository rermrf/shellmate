//! The transfer queue as the panel shows it: the items of the connection's queue kept up to date
//! from its events, and what each row says (status line, bar, sizes, buttons). No gpui here, so
//! all of it is tested without a window.

use std::path::PathBuf;

use shellmate_sftp::format_size;
use shellmate_sftp::transfer::{Direction, ItemId, PauseReason, TransferEvent, TransferState, TransferView};

/// The items of one connection's queue, oldest first (the order the queue keeps).
#[derive(Debug, Default)]
pub struct TransferList {
    items: Vec<TransferView>,
}

impl TransferList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes a queue event in. Events that are not about items (`RemoteChanged`, `SessionLost`) are
    /// none of the list's business.
    pub fn apply(&mut self, event: &TransferEvent) {
        match event {
            TransferEvent::Added(view) | TransferEvent::State(view) | TransferEvent::Progress(view) => {
                match self.items.iter_mut().find(|item| item.id == view.id) {
                    Some(item) => *item = view.clone(),
                    None => self.items.push(view.clone()),
                }
            }
            TransferEvent::Removed(ids) => self.items.retain(|item| !ids.contains(&item.id)),
            TransferEvent::RemoteChanged(_) | TransferEvent::SessionLost => {}
        }
    }

    /// The items, oldest first.
    pub fn items(&self) -> &[TransferView] {
        &self.items
    }

    /// The queue is empty: the card is not shown.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, id: ItemId) -> Option<&TransferView> {
        self.items.iter().find(|item| item.id == id)
    }

    /// Items waiting for the answer to a name clash, oldest first.
    pub fn awaiting_clash(&self) -> impl Iterator<Item = &TransferView> {
        self.items.iter().filter(|item| item.state == TransferState::AwaitingClash)
    }

    /// Items that are not over: queued, running, waiting for an answer or paused.
    pub fn unfinished(&self) -> usize {
        self.items.iter().filter(|item| !item.state.is_finished()).count()
    }

    /// Items that finished well.
    pub fn done(&self) -> usize {
        self.items.iter().filter(|item| item.state == TransferState::Done).count()
    }

    /// `2 个进行中 · 1 个完成`
    pub fn summary(&self) -> String {
        format!("{} 个进行中 · {} 个完成", self.unfinished(), self.done())
    }

    /// Whether "清除已完成" has anything to remove (done, failed and cancelled items go).
    pub fn has_finished(&self) -> bool {
        self.items.iter().any(|item| item.state.is_finished())
    }
}

/// The one main button of a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowAction {
    /// 暂停: a running or queued item.
    Pause,
    /// 续传: a paused item.
    Resume,
    /// 处理: an item waiting for the answer about names that exist.
    Handle,
    /// 打开: a finished item. A download is shown in Finder, an upload's folder is opened in the panel.
    Open,
}

impl RowAction {
    /// The button's label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pause => "暂停",
            Self::Resume => "续传",
            Self::Handle => "处理",
            Self::Open => "打开",
        }
    }
}

/// The main button of the row, if it has one. Items that failed or were cancelled have none
/// (there is nothing to open, and "清除已完成" removes them).
pub fn main_action(view: &TransferView) -> Option<RowAction> {
    match view.state {
        TransferState::Queued | TransferState::Running => Some(RowAction::Pause),
        TransferState::Paused(_) => Some(RowAction::Resume),
        TransferState::AwaitingClash => Some(RowAction::Handle),
        TransferState::Done => Some(RowAction::Open),
        TransferState::Failed(_) | TransferState::Cancelled => None,
    }
}

/// The small "取消" button: for everything that is not over.
pub fn can_cancel(view: &TransferView) -> bool {
    !view.state.is_finished()
}

/// How a piece of text or the bar is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// The usual: grey text, blue bar.
    Normal,
    /// Done (green).
    Ok,
    /// Paused or waiting for the user (amber).
    Warn,
    /// Failed (red).
    Err,
    /// Cancelled (faint).
    Faint,
}

/// The state's tone.
pub fn tone(view: &TransferView) -> Tone {
    match view.state {
        TransferState::Done => Tone::Ok,
        TransferState::Paused(_) | TransferState::AwaitingClash => Tone::Warn,
        TransferState::Failed(_) => Tone::Err,
        TransferState::Cancelled => Tone::Faint,
        TransferState::Queued | TransferState::Running => Tone::Normal,
    }
}

/// The second line of a row: the smoothed speed while running, else the state's label (a failure
/// carries its summary), followed by what was skipped and what was written in place.
pub fn status_line(view: &TransferView) -> String {
    let base = match &view.state {
        TransferState::Running if view.scanning => "正在统计文件…".to_string(),
        TransferState::Running if view.speed > 0.0 => format!("{}/s", format_size(view.speed as u64)),
        TransferState::Running => "…".to_string(),
        TransferState::Paused(PauseReason::User) => "已暂停 · 可断点续传".to_string(),
        other => other.label(),
    };
    let mut parts = vec![base];
    parts.extend(view.skipped.note());
    parts.extend(view.in_place_note());
    parts.join(" · ")
}

/// The sizes at the right of a row: `29.7 MB / 48.2 MB`, or just the size once it is done.
pub fn sizes_text(view: &TransferView) -> String {
    match view.state {
        TransferState::Done => format_size(view.bytes_done),
        _ if view.bytes_total == 0 => format_size(view.bytes_done),
        _ => format!("{} / {}", format_size(view.bytes_done), format_size(view.bytes_total)),
    }
}

/// The glyph in the tile at the left of a row.
pub fn arrow(view: &TransferView) -> &'static str {
    match view.direction {
        Direction::Upload => "↑",
        Direction::Download => "↓",
    }
}

/// Where "打开" takes a finished item. `None` for an item that cannot be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reveal {
    /// Show a local file or folder in Finder.
    Finder(PathBuf),
    /// Show a directory of the server in the panel.
    Remote(String),
}

/// What "打开" does for a finished item: a download is shown in Finder (the file it produced, or
/// the folder it went to), an upload takes the panel to its destination directory.
pub fn reveal(view: &TransferView) -> Option<Reveal> {
    match view.direction {
        Direction::Download => {
            Some(Reveal::Finder(view.result.clone().unwrap_or_else(|| PathBuf::from(&view.destination))))
        }
        Direction::Upload => Some(Reveal::Remote(view.destination.clone())),
    }
}

/// Where downloads go: `$SHELLMATE_DOWNLOADS` if it is set (reviews and tests use a scratch
/// directory), else `~/Downloads`.
pub fn downloads_dir(env_override: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match env_override.filter(|value| !value.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => home.map(|home| home.join("Downloads")),
    }
}

/// The downloads directory, created if it is missing.
pub fn ensure_downloads_dir() -> Result<PathBuf, String> {
    let dir = downloads_dir(std::env::var_os("SHELLMATE_DOWNLOADS"), std::env::home_dir())
        .ok_or_else(|| "找不到下载目录".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|err| format!("无法创建下载目录 {}：{err}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use shellmate_sftp::transfer::{Clash, Skipped};

    use super::*;

    fn view(id: u64, direction: Direction, state: TransferState) -> TransferView {
        TransferView {
            id: ItemId(id),
            direction,
            name: format!("item-{id}"),
            destination: "/srv/app".into(),
            state,
            bytes_done: 0,
            bytes_total: 0,
            files_done: 0,
            files_total: 0,
            scanning: false,
            speed: 0.0,
            current: None,
            skipped: Skipped::default(),
            in_place: 0,
            failures: Vec::new(),
            failure_total: 0,
            clash: None,
            result: None,
        }
    }

    fn up(id: u64, state: TransferState) -> TransferView {
        view(id, Direction::Upload, state)
    }

    #[test]
    fn events_keep_the_list_in_step_with_the_queue() {
        let mut list = TransferList::new();
        assert!(list.is_empty());
        list.apply(&TransferEvent::Added(up(1, TransferState::Queued)));
        list.apply(&TransferEvent::Added(up(2, TransferState::Queued)));
        let mut running = up(1, TransferState::Running);
        running.bytes_done = 10;
        list.apply(&TransferEvent::State(running.clone()));
        running.bytes_done = 20;
        list.apply(&TransferEvent::Progress(running));
        assert_eq!(list.items().len(), 2);
        assert_eq!(list.get(ItemId(1)).map(|v| (v.bytes_done, v.state.clone())), Some((20, TransferState::Running)));
        // An item the list never heard of (it started listening late) is taken in as it is.
        list.apply(&TransferEvent::Progress(up(3, TransferState::Running)));
        assert_eq!(list.items().iter().map(|v| v.id.0).collect::<Vec<_>>(), [1, 2, 3]);
        // Removal, and events that are not about items.
        list.apply(&TransferEvent::Removed(vec![ItemId(2)]));
        list.apply(&TransferEvent::RemoteChanged("/srv".into()));
        list.apply(&TransferEvent::SessionLost);
        assert_eq!(list.items().iter().map(|v| v.id.0).collect::<Vec<_>>(), [1, 3]);
    }

    #[test]
    fn the_header_counts_unfinished_items_and_finished_ones() {
        let mut list = TransferList::new();
        assert_eq!(list.summary(), "0 个进行中 · 0 个完成");
        for (id, state) in [
            (1, TransferState::Running),
            (2, TransferState::Queued),
            (3, TransferState::Paused(PauseReason::User)),
            (4, TransferState::AwaitingClash),
            (5, TransferState::Done),
            (6, TransferState::Done),
            (7, TransferState::Failed("权限不足".into())),
            (8, TransferState::Cancelled),
        ] {
            list.apply(&TransferEvent::Added(up(id, state)));
        }
        assert_eq!(
            list.summary(),
            "4 个进行中 · 2 个完成",
            "paused and waiting items are not over; failed ones are not done"
        );
        assert!(list.has_finished());
        assert_eq!(list.awaiting_clash().map(|v| v.id.0).collect::<Vec<_>>(), [4]);
        list.apply(&TransferEvent::Removed(vec![ItemId(5), ItemId(6), ItemId(7), ItemId(8)]));
        assert!(!list.has_finished());
        assert_eq!(list.summary(), "4 个进行中 · 0 个完成");
    }

    #[test]
    fn each_state_has_the_right_main_button_and_cancel() {
        let cases = [
            (TransferState::Queued, Some(RowAction::Pause), true),
            (TransferState::Running, Some(RowAction::Pause), true),
            (TransferState::Paused(PauseReason::User), Some(RowAction::Resume), true),
            (TransferState::Paused(PauseReason::Disconnected), Some(RowAction::Resume), true),
            (TransferState::AwaitingClash, Some(RowAction::Handle), true),
            (TransferState::Done, Some(RowAction::Open), false),
            (TransferState::Failed("x".into()), None, false),
            (TransferState::Cancelled, None, false),
        ];
        for (state, action, cancel) in cases {
            let item = up(1, state.clone());
            assert_eq!((main_action(&item), can_cancel(&item)), (action, cancel), "{state:?}");
        }
        let labels: Vec<_> =
            [RowAction::Pause, RowAction::Resume, RowAction::Handle, RowAction::Open].map(RowAction::label).into();
        assert_eq!(labels, ["暂停", "续传", "处理", "打开"]);
    }

    #[test]
    fn the_status_line_says_the_speed_while_running_and_the_state_otherwise() {
        let mut item = up(1, TransferState::Running);
        item.speed = 7.3 * 1024.0 * 1024.0;
        assert_eq!(status_line(&item), "7.3 MB/s");
        item.speed = 0.0;
        assert_eq!(status_line(&item), "…", "no speed measured yet");
        item.scanning = true;
        assert_eq!(status_line(&item), "正在统计文件…");
        assert_eq!(status_line(&up(1, TransferState::Queued)), "排队中");
        assert_eq!(status_line(&up(1, TransferState::Paused(PauseReason::User))), "已暂停 · 可断点续传");
        assert_eq!(status_line(&up(1, TransferState::Paused(PauseReason::Disconnected))), "已暂停 · 连接断开");
        assert_eq!(status_line(&up(1, TransferState::AwaitingClash)), "等待处理同名");
        assert_eq!(status_line(&up(1, TransferState::Done)), "已完成");
        assert_eq!(status_line(&up(1, TransferState::Cancelled)), "已取消");
        let failed =
            up(1, TransferState::Failed("3 个文件传输失败，首个原因：权限不足：/srv/a（/srv/a、/srv/b）".into()));
        assert_eq!(status_line(&failed), "失败：3 个文件传输失败，首个原因：权限不足：/srv/a（/srv/a、/srv/b）");
    }

    #[test]
    fn skipped_names_and_in_place_writes_follow_the_state() {
        let mut item = up(1, TransferState::Done);
        item.skipped = Skipped { symlinks: 2, special: 0, existing: 1 };
        item.in_place = 3;
        assert_eq!(status_line(&item), "已完成 · 跳过 2 个符号链接、1 个同名项 · 3 个文件已直接写回（目录不可写）");
        item.state = TransferState::Running;
        item.speed = 1024.0;
        assert_eq!(status_line(&item), "1.0 KB/s · 跳过 2 个符号链接、1 个同名项 · 3 个文件已直接写回（目录不可写）");
    }

    #[test]
    fn the_bar_and_text_take_their_colour_from_the_state() {
        let tone_of = |state| tone(&up(1, state));
        assert_eq!(tone_of(TransferState::Running), Tone::Normal);
        assert_eq!(tone_of(TransferState::Queued), Tone::Normal);
        assert_eq!(tone_of(TransferState::Done), Tone::Ok);
        assert_eq!(tone_of(TransferState::Paused(PauseReason::User)), Tone::Warn);
        assert_eq!(tone_of(TransferState::AwaitingClash), Tone::Warn);
        assert_eq!(tone_of(TransferState::Failed("x".into())), Tone::Err);
        assert_eq!(tone_of(TransferState::Cancelled), Tone::Faint);
    }

    #[test]
    fn sizes_show_done_over_total_and_only_the_size_when_over() {
        let mut item = up(1, TransferState::Running);
        item.bytes_done = 29_700_000;
        item.bytes_total = 48_200_000;
        assert_eq!(sizes_text(&item), format!("{} / {}", format_size(29_700_000), format_size(48_200_000)));
        item.bytes_total = 0;
        assert_eq!(sizes_text(&item), format_size(29_700_000), "the walk is not over: no total yet");
        item.state = TransferState::Done;
        item.bytes_total = 48_200_000;
        item.bytes_done = 48_200_000;
        assert_eq!(sizes_text(&item), format_size(48_200_000));
        assert_eq!(arrow(&item), "↑");
        assert_eq!(arrow(&view(2, Direction::Download, TransferState::Done)), "↓");
    }

    #[test]
    fn a_finished_download_is_shown_in_finder_and_an_upload_opens_its_directory() {
        let mut download = view(1, Direction::Download, TransferState::Done);
        download.destination = "/tmp/downloads".into();
        assert_eq!(
            reveal(&download),
            Some(Reveal::Finder(PathBuf::from("/tmp/downloads"))),
            "no single result: the folder"
        );
        download.result = Some(PathBuf::from("/tmp/downloads/report (1).pdf"));
        assert_eq!(reveal(&download), Some(Reveal::Finder(PathBuf::from("/tmp/downloads/report (1).pdf"))));
        let upload = up(2, TransferState::Done);
        assert_eq!(reveal(&upload), Some(Reveal::Remote("/srv/app".into())));
    }

    #[test]
    fn downloads_go_to_the_override_or_the_downloads_folder_of_the_user() {
        let home = Some(PathBuf::from("/Users/me"));
        assert_eq!(downloads_dir(None, home.clone()), Some(PathBuf::from("/Users/me/Downloads")));
        assert_eq!(downloads_dir(Some("/tmp/scratch".into()), home.clone()), Some(PathBuf::from("/tmp/scratch")));
        assert_eq!(downloads_dir(Some("".into()), home), Some(PathBuf::from("/Users/me/Downloads")), "empty is unset");
        assert_eq!(downloads_dir(None, None), None);
    }

    #[test]
    fn a_clash_can_be_read_off_the_item() {
        let mut item = up(1, TransferState::AwaitingClash);
        item.clash = Some(Clash { dir: "/srv/app".into(), count: 2, names: vec!["a.txt".into(), "b.txt".into()] });
        assert_eq!(item.clash.as_ref().map(Clash::title).as_deref(), Some("2 个文件已存在"));
    }
}
