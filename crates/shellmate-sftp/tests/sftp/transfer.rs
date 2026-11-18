//! The transfer engine against the in-process server.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use shellmate_sftp::transfer::{
    Choice, ItemId, PauseReason, TransferEvent, TransferQueue, TransferState, TransferView,
};

use crate::fixture::{Fixture, SftpOptions, pattern, umask};

const SEC: Duration = Duration::from_secs(1);

/// A server, a queue on a session to it, and a scratch area on this machine.
struct Rig {
    fx: Fixture,
    queue: TransferQueue,
    events: async_channel::Receiver<TransferEvent>,
    local: tempfile::TempDir,
}

impl Rig {
    async fn new() -> Self {
        Self::with(SftpOptions::default()).await
    }

    async fn with(options: SftpOptions) -> Self {
        Self::with_timeout(options, None).await
    }

    /// Like [`with`](Self::with), with a short request timeout on the session the queue starts with.
    async fn with_timeout(options: SftpOptions, timeout_secs: Option<u64>) -> Self {
        let fx = Fixture::start(options).await;
        let (queue, events) = TransferQueue::new(tokio::runtime::Handle::current());
        // If the server is to misbehave, the session of the queue is the one to suffer.
        fx.logs.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let session = fx.new_session().await;
        if let Some(seconds) = timeout_secs {
            session.set_request_timeout(seconds).await;
        }
        queue.set_session(Some(Arc::new(session)));
        let local = tempfile::tempdir().unwrap();
        std::fs::create_dir(local.path().join("downloads")).unwrap();
        fx.dir("/up");
        Self { fx, queue, events, local }
    }

    /// A slow server: transfers last long enough to be interrupted.
    async fn slow() -> Self {
        Self::with(SftpOptions {
            read_delay: Duration::from_millis(8),
            write_delay: Duration::from_millis(8),
            ..Default::default()
        })
        .await
    }

    fn src(&self, rel: &str) -> PathBuf {
        self.local.path().join("src").join(rel)
    }

    fn downloads(&self) -> PathBuf {
        self.local.path().join("downloads")
    }

    /// Creates a local source file.
    fn source(&self, rel: &str, content: &[u8], mode: u32) -> PathBuf {
        let path = self.src(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    async fn wait(&self, id: ItemId, what: &str, check: impl Fn(&TransferView) -> bool) -> TransferView {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let view = self.queue.view(id).expect("the item exists");
            if check(&view) {
                return view;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}; the item is {:?}", view.state);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn state(&self, id: ItemId, state: TransferState) -> TransferView {
        self.wait(id, &format!("{state:?}"), |v| v.state == state).await
    }

    async fn finished(&self, id: ItemId) -> TransferView {
        self.wait(id, "the item to finish", |v| v.state.is_finished()).await
    }

    /// Waits until the item has moved some bytes (but is not done).
    async fn started(&self, id: ItemId) -> TransferView {
        self.wait(id, "some progress", |v| v.bytes_done > 0 && v.state == TransferState::Running).await
    }

    fn events(&self) -> Vec<TransferEvent> {
        std::iter::from_fn(|| self.events.try_recv().ok()).collect()
    }
}

fn set_mtime(path: &Path, secs: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

fn mtime(path: &Path) -> i64 {
    std::fs::metadata(path).unwrap().mtime()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn is_root() -> bool {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, "x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::read(&file).is_ok()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

fn fifo(path: &Path) -> bool {
    std::process::Command::new("mkfifo").arg(path).status().map(|s| s.success()).unwrap_or(false)
}

// ---- single files -------------------------------------------------------------------------------

#[tokio::test]
async fn an_upload_goes_through_the_states_and_keeps_mode_and_time() {
    let rig = Rig::new().await;
    let content = pattern(100_000, 1);
    let src = rig.source("report.pdf", &content, 0o640);
    set_mtime(&src, 1_600_000_000);
    let id = rig.queue.enqueue_upload(vec![src], "/up");
    let view = rig.finished(id).await;

    assert_eq!(view.state, TransferState::Done);
    assert_eq!((view.bytes_done, view.bytes_total, view.files_done, view.files_total), (100_000, 100_000, 1, 1));
    assert_eq!((view.name.as_str(), view.destination.as_str(), view.scanning), ("report.pdf", "/up", false));
    assert_eq!(rig.fx.read("/up/report.pdf"), content);
    assert_eq!(rig.fx.mode("/up/report.pdf"), 0o640, "the local permission bits, whatever the umask");
    assert_eq!(mtime(&rig.fx.host("/up/report.pdf")), 1_600_000_000, "the remote time is the local one");
    assert_eq!(rig.fx.names("/up"), ["report.pdf"], "no temporary file is left");

    let events = rig.events();
    assert!(matches!(&events[0], TransferEvent::Added(v) if v.state == TransferState::Queued && v.id == id));
    let mut states: Vec<TransferState> = events
        .iter()
        .filter_map(|e| if let TransferEvent::State(v) = e { Some(v.state.clone()) } else { None })
        .collect();
    states.dedup();
    assert_eq!(states, [TransferState::Running, TransferState::Done]);
    assert!(events.contains(&TransferEvent::RemoteChanged("/up".into())), "the file panel may refresh");
}

#[tokio::test]
async fn a_download_goes_through_the_states_and_keeps_the_time() {
    let rig = Rig::new().await;
    let content = pattern(250_000, 2);
    rig.fx.file("/srv/video.bin", &content, 0o644);
    set_mtime(&rig.fx.host("/srv/video.bin"), 1_500_000_000);
    let id = rig.queue.enqueue_download(vec!["/srv/video.bin".into()], rig.downloads());
    let view = rig.finished(id).await;

    assert_eq!(view.state, TransferState::Done);
    assert_eq!((view.bytes_done, view.bytes_total, view.files_done), (250_000, 250_000, 1));
    let local = rig.downloads().join("video.bin");
    assert_eq!(std::fs::read(&local).unwrap(), content);
    assert_eq!(mtime(&local), 1_500_000_000, "the local time is the remote one");
    assert_eq!(mode(&local), (0o644 | 0o200) & !umask(), "the remote mode plus owner-write, under the umask");
    assert_eq!(names(&rig.downloads()), ["video.bin"], "no temporary file is left");
    assert_eq!(view.result, Some(local));
    let events = rig.events();
    assert!(!events.iter().any(|e| matches!(e, TransferEvent::RemoteChanged(_))), "a download changes nothing remote");
}

#[tokio::test]
async fn empty_files_and_files_of_many_chunks_work() {
    let rig = Rig::new().await;
    let big = pattern(5 * 1024 * 1024 + 123, 3);
    let empty = rig.source("empty.txt", b"", 0o644);
    let large = rig.source("large.bin", &big, 0o600);
    let id = rig.queue.enqueue_upload(vec![empty, large], "/up");
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert_eq!(rig.fx.read("/up/empty.txt"), b"");
    assert!(rig.fx.read("/up/large.bin") == big);

    let id = rig.queue.enqueue_download(vec!["/up/empty.txt".into(), "/up/large.bin".into()], rig.downloads());
    let view = rig.finished(id).await;
    assert_eq!((view.state, view.files_done), (TransferState::Done, 2));
    assert!(std::fs::read(rig.downloads().join("large.bin")).unwrap() == big);
    assert_eq!(std::fs::read(rig.downloads().join("empty.txt")).unwrap(), b"");
    assert_eq!(view.name, "2 个项目");
}

// ---- directories --------------------------------------------------------------------------------

#[tokio::test]
async fn a_directory_upload_recreates_the_tree_and_skips_links_and_special_files() {
    let rig = Rig::new().await;
    rig.source("proj/a.txt", b"a", 0o644);
    rig.source("proj/sub/b.txt", b"bb", 0o600);
    rig.source("proj/sub/deep/c.bin", &pattern(70_000, 4), 0o755);
    std::fs::create_dir_all(rig.src("proj/empty")).unwrap();
    std::fs::set_permissions(rig.src("proj/empty"), std::fs::Permissions::from_mode(0o750)).unwrap();
    symlink("a.txt", rig.src("proj/link-file")).unwrap();
    symlink("sub", rig.src("proj/link-dir")).unwrap();
    let made_fifo = fifo(&rig.src("proj/pipe"));

    let id = rig.queue.enqueue_upload(vec![rig.src("proj")], "/up");
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done);
    assert_eq!((view.files_done, view.files_total, view.bytes_total), (3, 3, 1 + 2 + 70_000));
    assert_eq!((view.skipped.symlinks, view.skipped.special), (2, u32::from(made_fifo)));
    assert!(view.skipped.note().unwrap().starts_with("跳过 2 个符号链接"));
    assert_eq!(rig.fx.names("/up/proj"), ["a.txt", "empty", "sub"]);
    assert_eq!(rig.fx.names("/up/proj/sub"), ["b.txt", "deep"]);
    assert_eq!(rig.fx.text("/up/proj/sub/b.txt"), "bb");
    assert_eq!(rig.fx.read("/up/proj/sub/deep/c.bin"), pattern(70_000, 4));
    assert_eq!(rig.fx.mode("/up/proj/sub/b.txt"), 0o600);
    assert_eq!(rig.fx.mode("/up/proj/sub/deep/c.bin"), 0o755);
    assert_eq!(rig.fx.mode("/up/proj/empty"), 0o750, "new directories take the local directory's mode");
}

#[tokio::test]
async fn a_directory_download_recreates_the_tree_and_skips_links_and_special_files() {
    let rig = Rig::new().await;
    rig.fx.file("/srv/proj/a.txt", "a", 0o644);
    rig.fx.file("/srv/proj/sub/b.txt", "bb", 0o600);
    rig.fx.file("/srv/proj/sub/deep/c.bin", pattern(70_000, 5), 0o755);
    rig.fx.dir("/srv/proj/empty");
    std::fs::set_permissions(rig.fx.host("/srv/proj/empty"), std::fs::Permissions::from_mode(0o750)).unwrap();
    rig.fx.link("/srv/proj/link-file", "a.txt");
    rig.fx.link("/srv/proj/link-dir", "sub");
    rig.fx.link("/srv/proj/dangling", "nowhere");
    let made_fifo = fifo(&rig.fx.host("/srv/proj/pipe"));
    set_mtime(&rig.fx.host("/srv/proj/sub/b.txt"), 1_400_000_000);

    let id = rig.queue.enqueue_download(vec!["/srv/proj".into()], rig.downloads());
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done, "{view:?}");
    assert_eq!((view.files_done, view.files_total), (3, 3));
    assert_eq!((view.skipped.symlinks, view.skipped.special), (3, u32::from(made_fifo)));
    let root = rig.downloads().join("proj");
    assert_eq!(names(&root), ["a.txt", "empty", "sub"]);
    assert_eq!(std::fs::read(root.join("sub/deep/c.bin")).unwrap(), pattern(70_000, 5));
    assert_eq!(mtime(&root.join("sub/b.txt")), 1_400_000_000);
    assert_eq!(mode(&root.join("sub/b.txt")), (0o600 | 0o200) & !umask());
    assert_eq!(mode(&root.join("sub/deep/c.bin")), 0o755 & !umask());
    assert_eq!(mode(&root.join("empty")), (0o750 | 0o700) & !umask());
    assert_eq!(view.result, Some(root));
}

#[tokio::test]
async fn a_link_selected_directly_is_followed_once() {
    let rig = Rig::new().await;
    // Uploads.
    rig.source("real/file.txt", b"inside", 0o644);
    rig.source("real/nested/x.txt", b"x", 0o644);
    symlink("nested", rig.src("real/inner-link")).unwrap();
    rig.source("plain.txt", b"plain", 0o644);
    symlink("real", rig.src("dirlink")).unwrap();
    symlink("plain.txt", rig.src("filelink")).unwrap();
    let id = rig.queue.enqueue_upload(vec![rig.src("dirlink"), rig.src("filelink")], "/up");
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done);
    assert_eq!(rig.fx.names("/up"), ["dirlink", "filelink"]);
    assert_eq!(rig.fx.text("/up/dirlink/file.txt"), "inside", "the contents of the linked directory");
    assert_eq!(rig.fx.text("/up/filelink"), "plain", "the linked file, not a link");
    assert!(!std::fs::symlink_metadata(rig.fx.host("/up/filelink")).unwrap().file_type().is_symlink());
    assert_eq!(view.skipped.symlinks, 1, "the link inside the linked directory is skipped");
    assert!(!rig.fx.exists("/up/dirlink/inner-link"));

    // Downloads.
    rig.fx.file("/srv/real/file.txt", "remote inside", 0o644);
    rig.fx.file("/srv/rplain.txt", "remote plain", 0o644);
    rig.fx.link("/srv/rdirlink", "real");
    rig.fx.link("/srv/rfilelink", "rplain.txt");
    let id = rig.queue.enqueue_download(vec!["/srv/rdirlink".into(), "/srv/rfilelink".into()], rig.downloads());
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert_eq!(std::fs::read_to_string(rig.downloads().join("rdirlink/file.txt")).unwrap(), "remote inside");
    assert_eq!(std::fs::read_to_string(rig.downloads().join("rfilelink")).unwrap(), "remote plain");
    assert!(!std::fs::symlink_metadata(rig.downloads().join("rfilelink")).unwrap().file_type().is_symlink());
}

#[tokio::test]
async fn a_failing_file_does_not_stop_the_directory_and_the_item_ends_as_failed_with_a_summary() {
    if is_root() {
        return;
    }
    let rig = Rig::new().await;
    rig.source("tree/ok1.txt", b"1", 0o644);
    rig.source("tree/locked.txt", b"secret", 0o000);
    rig.source("tree/sub/ok2.txt", b"2", 0o644);
    rig.source("tree/sub/locked2.txt", b"secret", 0o000);
    let id = rig.queue.enqueue_upload(vec![rig.src("tree")], "/up");
    let view = rig.finished(id).await;
    let TransferState::Failed(summary) = &view.state else { panic!("{:?}", view.state) };
    assert!(summary.starts_with("2 个文件传输失败，首个原因：权限不足"), "{summary}");
    assert!(summary.contains("locked.txt") && summary.contains("locked2.txt"), "{summary}");
    assert_eq!(view.failures.len(), 2);
    assert_eq!((view.files_done, view.files_total), (2, 4));
    assert_eq!(view.bytes_done, view.bytes_total, "the bar still reaches the end");
    assert_eq!(rig.fx.text("/up/tree/ok1.txt"), "1");
    assert_eq!(rig.fx.text("/up/tree/sub/ok2.txt"), "2");
    assert_eq!(rig.fx.names("/up/tree"), ["ok1.txt", "sub"], "no temporary files, no half files");
    assert_eq!(rig.fx.names("/up/tree/sub"), ["ok2.txt"]);
}

#[tokio::test]
async fn failures_while_downloading_a_directory_are_collected_too() {
    if is_root() {
        return;
    }
    let rig = Rig::new().await;
    rig.fx.file("/srv/tree/ok.txt", "ok", 0o644);
    rig.fx.file("/srv/tree/secret.txt", "secret", 0o000);
    rig.fx.file("/srv/tree/locked/inside.txt", "x", 0o644);
    std::fs::set_permissions(rig.fx.host("/srv/tree/locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let id = rig.queue.enqueue_download(vec!["/srv/tree".into()], rig.downloads());
    let view = rig.finished(id).await;
    std::fs::set_permissions(rig.fx.host("/srv/tree/locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let TransferState::Failed(summary) = &view.state else { panic!("{:?}", view.state) };
    assert!(summary.starts_with("2 个文件传输失败"), "{summary}");
    assert_eq!(std::fs::read_to_string(rig.downloads().join("tree/ok.txt")).unwrap(), "ok");
    let leftovers = names(&rig.downloads().join("tree"));
    assert!(!leftovers.iter().any(|n| n.ends_with(".shellmate-download")), "{leftovers:?}");
    assert!(!leftovers.contains(&"secret.txt".to_string()));
}

#[tokio::test]
async fn a_selection_that_cannot_be_read_at_all_fails_with_the_reason() {
    let rig = Rig::new().await;
    let id = rig.queue.enqueue_upload(vec![rig.src("missing.txt")], "/up");
    let view = rig.finished(id).await;
    let TransferState::Failed(reason) = &view.state else { panic!("{:?}", view.state) };
    assert!(reason.contains("不存在"), "{reason}");
    let id = rig.queue.enqueue_download(vec!["/nowhere/file".into()], rig.downloads());
    let TransferState::Failed(reason) = rig.finished(id).await.state else { panic!() };
    assert!(reason.contains("不存在"), "{reason}");
    assert!(names(&rig.downloads()).is_empty());
}

#[tokio::test]
async fn a_type_clash_inside_a_merged_directory_fails_that_file_only() {
    let rig = Rig::new().await;
    rig.source("proj/a.txt", b"new a", 0o644);
    rig.source("proj/x.txt", b"file where a directory is", 0o644);
    rig.source("proj/z.txt", b"new z", 0o644);
    rig.fx.file("/up/proj/old.txt", "old", 0o644);
    rig.fx.dir("/up/proj/x.txt");
    let id = rig.queue.enqueue_upload(vec![rig.src("proj")], "/up");
    rig.state(id, TransferState::AwaitingClash).await;
    rig.queue.answer_clash(id, Choice::Overwrite);
    let view = rig.finished(id).await;
    let TransferState::Failed(reason) = &view.state else { panic!("{:?}", view.state) };
    assert!(reason.contains("是目录"), "{reason}");
    assert_eq!(rig.fx.text("/up/proj/a.txt"), "new a");
    assert_eq!(rig.fx.text("/up/proj/z.txt"), "new z");
    assert!(rig.fx.host("/up/proj/x.txt").is_dir(), "the directory was not replaced");
    assert_eq!(rig.fx.text("/up/proj/old.txt"), "old", "merged, not replaced");
}

// ---- pausing, resuming, cancelling ----------------------------------------------------------------

const BIG: usize = 4 * 1024 * 1024;

/// Starts an upload of a big file on the slow server and pauses it part of the way.
async fn paused_upload(rig: &Rig, content: &[u8]) -> (ItemId, PathBuf, u64, usize) {
    let src = rig.source("big.bin", content, 0o644);
    let id = rig.queue.enqueue_upload(vec![src.clone()], "/up");
    let running = rig.wait(id, "a quarter of the file", |v| v.bytes_done > (content.len() / 4) as u64).await;
    // The view says which file on the server is the upload's own temporary file (a panel hides it).
    assert_eq!(running.temp.as_deref(), Some("/up/.big.bin.shellmate-upload"));
    rig.queue.pause(id);
    let paused = rig.state(id, TransferState::Paused(PauseReason::User)).await;
    assert_eq!(paused.temp.as_deref(), Some("/up/.big.bin.shellmate-upload"), "a paused upload keeps it");
    let len = std::fs::metadata(rig.fx.host("/up/.big.bin.shellmate-upload")).unwrap().len();
    assert!(len > 0 && len < content.len() as u64, "the temporary file holds part of the file: {len}");
    assert_eq!(paused.bytes_done, len, "what is shown is what is on the server");
    assert!(!rig.fx.exists("/up/big.bin"), "the target is not touched before the end");
    let log_len = rig.fx.logs.writes.lock().unwrap().len();
    (id, src, len, log_len)
}

#[tokio::test]
async fn pausing_an_upload_keeps_the_temporary_file_and_resuming_continues_from_its_length() {
    let rig = Rig::slow().await;
    let content = pattern(BIG, 7);
    let (id, _, len, log_len) = paused_upload(&rig, &content).await;

    tokio::time::sleep(SEC / 3).await;
    assert_eq!(rig.fx.logs.writes.lock().unwrap().len(), log_len, "nothing moves while paused");
    assert_eq!(rig.queue.view(id).unwrap().speed, 0.0);

    rig.queue.resume(id);
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done);
    assert_eq!(view.temp, None, "the file is in place: no temporary file to hide any more");
    assert!(rig.fx.read("/up/big.bin") == content, "the checksum matches");
    assert_eq!(rig.fx.names("/up"), ["big.bin"]);
    let log = rig.fx.logs.writes.lock().unwrap()[log_len..].to_vec();
    assert!(log.iter().all(|(_, offset, _)| *offset >= len), "it went on from {len}, nothing was sent twice");
    assert_eq!(log.iter().map(|(_, o, l)| *o + *l as u64).max(), Some(BIG as u64));
}

#[tokio::test]
async fn pausing_a_download_keeps_the_temporary_file_and_resuming_continues_from_its_length() {
    let rig = Rig::slow().await;
    let content = pattern(BIG, 8);
    rig.fx.file("/srv/big.bin", &content, 0o644);
    let id = rig.queue.enqueue_download(vec!["/srv/big.bin".into()], rig.downloads());
    rig.wait(id, "a quarter of the file", |v| v.bytes_done > (BIG / 4) as u64).await;
    rig.queue.pause(id);
    let paused = rig.state(id, TransferState::Paused(PauseReason::User)).await;
    let temp = rig.downloads().join("big.bin.shellmate-download");
    let len = std::fs::metadata(&temp).unwrap().len();
    assert!(len > 0 && len < BIG as u64);
    assert_eq!(paused.bytes_done, len);
    assert!(!rig.downloads().join("big.bin").exists());
    let log_len = rig.fx.logs.reads.lock().unwrap().len();

    rig.queue.resume(id);
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done);
    assert!(std::fs::read(rig.downloads().join("big.bin")).unwrap() == content, "the checksum matches");
    assert_eq!(names(&rig.downloads()), ["big.bin"]);
    let log = rig.fx.logs.reads.lock().unwrap()[log_len..].to_vec();
    assert!(log.iter().all(|(_, offset, _)| *offset >= len), "nothing was read twice");
}

#[tokio::test]
async fn an_upload_whose_source_changed_while_paused_starts_that_file_over() {
    for (what, new_content, shift_time) in
        [("other size", pattern(BIG - 50_000, 10), false), ("same size, later time", pattern(BIG, 11), true)]
    {
        let rig = Rig::slow().await;
        let (id, src, len, log_len) = paused_upload(&rig, &pattern(BIG, 9)).await;
        std::fs::write(&src, &new_content).unwrap();
        set_mtime(&src, if shift_time { 1_900_000_000 } else { 1_800_000_000 });
        rig.queue.resume(id);
        let view = rig.finished(id).await;
        assert_eq!(view.state, TransferState::Done, "{what}");
        assert!(rig.fx.read("/up/big.bin") == new_content, "{what}: the new content, not a mix of two");
        let log = rig.fx.logs.writes.lock().unwrap()[log_len..].to_vec();
        assert!(log.iter().any(|(_, offset, _)| *offset == 0), "{what}: restarted from 0 (was at {len})");
        assert_eq!(view.bytes_done, view.bytes_total, "{what}");
        assert_eq!(view.bytes_total, new_content.len() as u64, "{what}: the total follows the new size");
    }
}

#[tokio::test]
async fn a_download_whose_source_changed_while_paused_starts_that_file_over() {
    let rig = Rig::slow().await;
    rig.fx.file("/srv/big.bin", pattern(BIG, 12), 0o644);
    let id = rig.queue.enqueue_download(vec!["/srv/big.bin".into()], rig.downloads());
    rig.wait(id, "a quarter of the file", |v| v.bytes_done > (BIG / 4) as u64).await;
    rig.queue.pause(id);
    rig.state(id, TransferState::Paused(PauseReason::User)).await;
    let log_len = rig.fx.logs.reads.lock().unwrap().len();
    let new_content = pattern(BIG + 777, 13);
    std::fs::write(rig.fx.host("/srv/big.bin"), &new_content).unwrap();
    set_mtime(&rig.fx.host("/srv/big.bin"), 1_800_000_000);
    rig.queue.resume(id);
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done);
    assert!(std::fs::read(rig.downloads().join("big.bin")).unwrap() == new_content);
    assert!(rig.fx.logs.reads.lock().unwrap()[log_len..].iter().any(|(_, offset, _)| *offset == 0), "restarted from 0");
    assert_eq!((view.bytes_done, view.bytes_total), (new_content.len() as u64, new_content.len() as u64));
}

#[tokio::test]
async fn an_upload_temp_replaced_by_a_link_while_paused_is_not_followed() {
    let rig = Rig::slow().await;
    let content = pattern(BIG, 14);
    rig.fx.file("/up/victim.txt", "do not touch", 0o644);
    let (id, _, _, log_len) = paused_upload(&rig, &content).await;
    let temp = rig.fx.host("/up/.big.bin.shellmate-upload");
    std::fs::remove_file(&temp).unwrap();
    symlink("victim.txt", &temp).unwrap();
    rig.queue.resume(id);
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(rig.fx.read("/up/big.bin") == content);
    assert_eq!(rig.fx.text("/up/victim.txt"), "do not touch", "the link's target is untouched");
    assert_eq!(rig.fx.names("/up"), ["big.bin", "victim.txt"]);
    assert!(
        rig.fx.logs.writes.lock().unwrap()[log_len..].iter().any(|(_, offset, _)| *offset == 0),
        "restarted from 0"
    );
}

#[tokio::test]
async fn a_download_temp_replaced_by_a_link_while_paused_is_not_followed() {
    let rig = Rig::slow().await;
    let content = pattern(BIG, 15);
    rig.fx.file("/srv/big.bin", &content, 0o644);
    let victim = rig.local.path().join("victim.txt");
    std::fs::write(&victim, "do not touch").unwrap();
    let id = rig.queue.enqueue_download(vec!["/srv/big.bin".into()], rig.downloads());
    rig.wait(id, "a quarter of the file", |v| v.bytes_done > (BIG / 4) as u64).await;
    rig.queue.pause(id);
    rig.state(id, TransferState::Paused(PauseReason::User)).await;
    let temp = rig.downloads().join("big.bin.shellmate-download");
    std::fs::remove_file(&temp).unwrap();
    symlink(&victim, &temp).unwrap();
    rig.queue.resume(id);
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(std::fs::read(rig.downloads().join("big.bin")).unwrap() == content);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not touch");
    assert_eq!(names(&rig.downloads()), ["big.bin"]);
}

#[tokio::test]
async fn a_temp_longer_than_its_source_is_started_over() {
    // Uploads.
    let rig = Rig::slow().await;
    let content = pattern(BIG, 16);
    let (id, _, _, log_len) = paused_upload(&rig, &content).await;
    let temp = rig.fx.host("/up/.big.bin.shellmate-upload");
    let junk = std::fs::OpenOptions::new().append(true).open(&temp).unwrap();
    junk.set_len(BIG as u64 + 4096).unwrap();
    rig.queue.resume(id);
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(rig.fx.read("/up/big.bin") == content);
    assert!(rig.fx.logs.writes.lock().unwrap()[log_len..].iter().any(|(_, offset, _)| *offset == 0));

    // Downloads.
    rig.fx.file("/srv/d.bin", &content, 0o644);
    let id = rig.queue.enqueue_download(vec!["/srv/d.bin".into()], rig.downloads());
    rig.wait(id, "a quarter of the file", |v| v.bytes_done > (BIG / 4) as u64).await;
    rig.queue.pause(id);
    rig.state(id, TransferState::Paused(PauseReason::User)).await;
    std::fs::OpenOptions::new()
        .write(true)
        .open(rig.downloads().join("d.bin.shellmate-download"))
        .unwrap()
        .set_len(BIG as u64 + 4096)
        .unwrap();
    rig.queue.resume(id);
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(std::fs::read(rig.downloads().join("d.bin")).unwrap() == content);
}

#[tokio::test]
async fn cancelling_removes_the_partial_file() {
    let rig = Rig::slow().await;
    let id = rig.queue.enqueue_upload(vec![rig.source("big.bin", &pattern(BIG, 17), 0o644)], "/up");
    rig.wait(id, "a quarter of the file", |v| v.bytes_done > (BIG / 4) as u64).await;
    assert!(rig.fx.exists("/up/.big.bin.shellmate-upload"));
    rig.queue.cancel(id);
    assert_eq!(rig.state(id, TransferState::Cancelled).await.state, TransferState::Cancelled);
    assert!(rig.fx.names("/up").is_empty(), "neither the temporary file nor the target");
    assert!(rig.events().contains(&TransferEvent::RemoteChanged("/up".into())));

    rig.fx.file("/srv/d.bin", pattern(BIG, 18), 0o644);
    let id = rig.queue.enqueue_download(vec!["/srv/d.bin".into()], rig.downloads());
    rig.wait(id, "a quarter of the file", |v| v.bytes_done > (BIG / 4) as u64).await;
    assert!(rig.downloads().join("d.bin.shellmate-download").exists());
    rig.queue.cancel(id);
    rig.state(id, TransferState::Cancelled).await;
    assert!(names(&rig.downloads()).is_empty());
}

#[tokio::test]
async fn cancelling_a_paused_or_queued_item_cleans_up_too() {
    let rig = Rig::slow().await;
    let (paused, _, _, _) = paused_upload(&rig, &pattern(BIG, 19)).await;
    rig.queue.cancel(paused);
    assert_eq!(rig.queue.view(paused).unwrap().state, TransferState::Cancelled);
    for _ in 0..100 {
        if rig.fx.names("/up").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(rig.fx.names("/up").is_empty(), "the temporary file of the paused upload is gone");

    // A third item waits behind two running ones, and is cancelled before it ever starts.
    let (a, b) = (rig.source("a.bin", &pattern(BIG, 20), 0o644), rig.source("b.bin", &pattern(BIG, 21), 0o644));
    let c = rig.source("c.bin", b"c", 0o644);
    let (first, second) = (rig.queue.enqueue_upload(vec![a], "/up"), rig.queue.enqueue_upload(vec![b], "/up"));
    let third = rig.queue.enqueue_upload(vec![c], "/up");
    rig.wait(third, "the preflight", |v| v.state == TransferState::Queued).await;
    rig.queue.cancel(third);
    assert_eq!(rig.queue.view(third).unwrap().state, TransferState::Cancelled);
    rig.queue.cancel(first);
    rig.queue.cancel(second);
    rig.state(second, TransferState::Cancelled).await;
    rig.state(first, TransferState::Cancelled).await;
    tokio::time::sleep(SEC / 4).await;
    assert!(!rig.fx.exists("/up/c.bin"), "it never started");
    assert!(rig.fx.names("/up").is_empty());
}

#[tokio::test]
async fn cancelling_a_directory_keeps_the_files_that_were_finished() {
    let rig = Rig::slow().await;
    rig.source("tree/a.txt", b"small and quick", 0o644);
    rig.source("tree/b.bin", &pattern(BIG, 22), 0o644);
    rig.source("tree/c.txt", b"never reached", 0o644);
    let id = rig.queue.enqueue_upload(vec![rig.src("tree")], "/up");
    rig.wait(id, "the second file to be under way", |v| v.files_done == 1 && v.bytes_done > 20_000).await;
    rig.queue.cancel(id);
    rig.state(id, TransferState::Cancelled).await;
    assert_eq!(rig.fx.text("/up/tree/a.txt"), "small and quick", "a finished file stays");
    assert_eq!(rig.fx.names("/up/tree"), ["a.txt"], "no partial b.bin, no c.txt");
}

#[tokio::test]
async fn a_resume_taken_back_before_the_pause_landed_keeps_going() {
    let rig = Rig::slow().await;
    let content = pattern(BIG, 23);
    let id = rig.queue.enqueue_upload(vec![rig.source("big.bin", &content, 0o644)], "/up");
    rig.started(id).await;
    rig.queue.pause(id);
    rig.queue.resume(id);
    // Whichever way the race went, resuming again finishes the job.
    for _ in 0..50 {
        let view = rig.queue.view(id).unwrap();
        if view.state == TransferState::Paused(PauseReason::User) {
            rig.queue.resume(id);
        }
        if view.state.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(rig.fx.read("/up/big.bin") == content);
}

// ---- names that exist ---------------------------------------------------------------------------

/// An upload of `report.pdf`, `notes.txt`, the directory `photos` and `fresh.txt` into a
/// directory where the first three exist already.
async fn clash_rig() -> (Rig, ItemId) {
    let rig = Rig::new().await;
    rig.fx.file("/up/report.pdf", "old pdf", 0o644);
    rig.fx.file("/up/notes.txt", "old notes", 0o600);
    rig.fx.file("/up/photos/a.jpg", "old a", 0o644);
    rig.fx.file("/up/photos/keep.jpg", "keep me", 0o644);
    let report = rig.source("report.pdf", b"new pdf", 0o644);
    let notes = rig.source("notes.txt", b"new notes", 0o644);
    rig.source("photos/a.jpg", b"new a", 0o644);
    rig.source("photos/b.jpg", b"new b", 0o644);
    let fresh = rig.source("fresh.txt", b"fresh", 0o644);
    let selection = vec![report, notes, rig.src("photos"), fresh];
    let id = rig.queue.enqueue_upload(selection, "/up");
    let view = rig.state(id, TransferState::AwaitingClash).await;
    let clash = view.clash.clone().expect("the names to ask about");
    assert_eq!((clash.count, clash.dir.as_str()), (3, "/up"));
    assert_eq!(clash.title(), "3 个文件已存在");
    let mut shown = clash.names.clone();
    shown.sort();
    assert_eq!(shown, ["notes.txt", "photos", "report.pdf"]);
    tokio::time::sleep(SEC / 5).await;
    assert!(!rig.fx.exists("/up/fresh.txt"), "nothing is uploaded before the answer");
    assert_eq!(rig.queue.view(id).unwrap().bytes_done, 0);
    (rig, id)
}

#[tokio::test]
async fn skipping_leaves_what_exists_alone_and_uploads_the_rest() {
    let (rig, id) = clash_rig().await;
    rig.queue.answer_clash(id, Choice::Skip);
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done);
    assert_eq!((view.skipped.existing, view.files_total, view.files_done), (3, 1, 1));
    assert!(view.skipped.note().unwrap().contains("3 个同名项"));
    assert_eq!(rig.fx.text("/up/report.pdf"), "old pdf");
    assert_eq!(rig.fx.text("/up/notes.txt"), "old notes");
    assert_eq!(rig.fx.text("/up/photos/a.jpg"), "old a");
    assert!(!rig.fx.exists("/up/photos/b.jpg"), "a skipped directory is not merged");
    assert_eq!(rig.fx.text("/up/fresh.txt"), "fresh");
}

#[tokio::test]
async fn keeping_both_numbers_the_new_names_and_keeps_extensions() {
    let (rig, id) = clash_rig().await;
    rig.fx.file("/up/report (1).pdf", "an earlier copy", 0o644);
    rig.queue.answer_clash(id, Choice::KeepBoth);
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done);
    assert_eq!(rig.fx.text("/up/report.pdf"), "old pdf");
    assert_eq!(rig.fx.text("/up/report (1).pdf"), "an earlier copy");
    assert_eq!(rig.fx.text("/up/report (2).pdf"), "new pdf", "the first free number, extension kept");
    assert_eq!(rig.fx.text("/up/notes.txt"), "old notes");
    assert_eq!(rig.fx.text("/up/notes (1).txt"), "new notes");
    assert_eq!(rig.fx.text("/up/photos/a.jpg"), "old a");
    assert_eq!(rig.fx.text("/up/photos (1)/a.jpg"), "new a", "a directory is numbered without an extension");
    assert_eq!(rig.fx.text("/up/photos (1)/b.jpg"), "new b");
    assert_eq!(rig.fx.text("/up/fresh.txt"), "fresh");
}

#[tokio::test]
async fn overwriting_replaces_files_keeps_their_permissions_and_merges_directories() {
    let (rig, id) = clash_rig().await;
    rig.queue.answer_clash(id, Choice::Overwrite);
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done);
    assert_eq!(rig.fx.text("/up/report.pdf"), "new pdf");
    assert_eq!(rig.fx.text("/up/notes.txt"), "new notes");
    assert_eq!(rig.fx.mode("/up/notes.txt"), 0o600, "the permission bits of the file that was replaced");
    assert_eq!(rig.fx.text("/up/photos/a.jpg"), "new a", "inside a merged directory files are overwritten");
    assert_eq!(rig.fx.text("/up/photos/b.jpg"), "new b");
    assert_eq!(rig.fx.text("/up/photos/keep.jpg"), "keep me", "merged, not replaced");
    assert_eq!(rig.fx.names("/up"), ["fresh.txt", "notes.txt", "photos", "report.pdf"], "no temporary files");
}

#[tokio::test]
async fn overwriting_a_read_only_file_is_refused_not_bypassed() {
    if is_root() {
        return;
    }
    let rig = Rig::new().await;
    rig.fx.file("/up/locked.txt", "precious", 0o444);
    let id = rig.queue.enqueue_upload(vec![rig.source("locked.txt", b"overwritten", 0o644)], "/up");
    rig.state(id, TransferState::AwaitingClash).await;
    rig.queue.answer_clash(id, Choice::Overwrite);
    let view = rig.finished(id).await;
    let TransferState::Failed(reason) = &view.state else { panic!("{:?}", view.state) };
    assert!(reason.contains("权限不足"), "{reason}");
    assert_eq!(rig.fx.text("/up/locked.txt"), "precious");
    assert_eq!(rig.fx.mode("/up/locked.txt"), 0o444);
    assert_eq!(rig.fx.names("/up"), ["locked.txt"], "no temporary file");
}

#[tokio::test]
async fn an_item_waiting_for_an_answer_does_not_hold_up_the_others() {
    let rig = Rig::new().await;
    rig.fx.file("/up/exists.txt", "old", 0o644);
    let waiting = rig.queue.enqueue_upload(vec![rig.source("exists.txt", b"new", 0o644)], "/up");
    rig.state(waiting, TransferState::AwaitingClash).await;
    let other = rig.queue.enqueue_upload(vec![rig.source("other.txt", b"other", 0o644)], "/up");
    assert_eq!(rig.finished(other).await.state, TransferState::Done);
    assert_eq!(rig.queue.view(waiting).unwrap().state, TransferState::AwaitingClash);
    rig.queue.answer_clash(waiting, Choice::Overwrite);
    assert_eq!(rig.finished(waiting).await.state, TransferState::Done);
    assert_eq!(rig.fx.text("/up/exists.txt"), "new");
}

#[tokio::test]
async fn an_answer_to_nothing_is_ignored() {
    let rig = Rig::new().await;
    let id = rig.queue.enqueue_upload(vec![rig.source("a.txt", b"a", 0o644)], "/up");
    rig.queue.answer_clash(id, Choice::Skip);
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert_eq!(rig.fx.text("/up/a.txt"), "a", "the early answer did not turn into a skip");
}

#[tokio::test]
async fn a_name_that_appears_after_the_check_is_asked_about_before_it_is_overwritten() {
    // Two uploads of the same name into the same directory: the second waits for the first, then asks.
    let rig = Rig::slow().await;
    let first = rig.queue.enqueue_upload(vec![rig.source("a/same.bin", &pattern(1_000_000, 24), 0o644)], "/up");
    let second = rig.queue.enqueue_upload(vec![rig.source("b/same.bin", b"second", 0o644)], "/up");
    rig.started(first).await;
    assert_eq!(rig.queue.view(second).unwrap().state, TransferState::Queued, "waits: same target, same temporary file");
    assert_eq!(rig.finished(first).await.state, TransferState::Done);
    rig.state(second, TransferState::AwaitingClash).await;
    assert_eq!(rig.fx.read("/up/same.bin"), pattern(1_000_000, 24), "the first upload is untouched");
    rig.queue.answer_clash(second, Choice::KeepBoth);
    assert_eq!(rig.finished(second).await.state, TransferState::Done);
    assert_eq!(rig.fx.text("/up/same (1).bin"), "second");
}

// ---- downloads pick local names -----------------------------------------------------------------

#[tokio::test]
async fn downloads_never_overwrite_and_never_ask() {
    let rig = Rig::new().await;
    rig.fx.file("/srv/a.txt", "remote a", 0o644);
    rig.fx.file("/srv/proj/f.txt", "remote f", 0o644);
    std::fs::write(rig.downloads().join("a.txt"), "local a").unwrap();
    std::fs::create_dir(rig.downloads().join("proj")).unwrap();
    std::fs::write(rig.downloads().join("proj/mine.txt"), "mine").unwrap();
    let id = rig.queue.enqueue_download(vec!["/srv/a.txt".into(), "/srv/proj".into()], rig.downloads());
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done);
    assert_eq!(std::fs::read_to_string(rig.downloads().join("a.txt")).unwrap(), "local a");
    assert_eq!(std::fs::read_to_string(rig.downloads().join("a (1).txt")).unwrap(), "remote a");
    assert_eq!(std::fs::read_to_string(rig.downloads().join("proj/mine.txt")).unwrap(), "mine");
    assert_eq!(std::fs::read_to_string(rig.downloads().join("proj (1)/f.txt")).unwrap(), "remote f");
    assert_eq!(view.result, Some(rig.downloads().join("a (1).txt")));
}

#[tokio::test]
async fn two_downloads_of_the_same_file_get_different_names() {
    let rig = Rig::slow().await;
    rig.fx.file("/srv/same.bin", pattern(800_000, 25), 0o644);
    let a = rig.queue.enqueue_download(vec!["/srv/same.bin".into()], rig.downloads());
    let b = rig.queue.enqueue_download(vec!["/srv/same.bin".into()], rig.downloads());
    rig.finished(a).await;
    rig.finished(b).await;
    assert_eq!(names(&rig.downloads()), ["same (1).bin", "same.bin"]);
    for name in ["same.bin", "same (1).bin"] {
        assert!(std::fs::read(rig.downloads().join(name)).unwrap() == pattern(800_000, 25), "{name}");
    }
}

// ---- the queue ------------------------------------------------------------------------------------

#[tokio::test]
async fn at_most_two_items_run_and_the_rest_start_in_order() {
    let rig = Rig::slow().await;
    let ids: Vec<ItemId> = (0..5)
        .map(|n| {
            rig.queue.enqueue_upload(vec![rig.source(&format!("f{n}.bin"), &pattern(600_000, 30 + n), 0o644)], "/up")
        })
        .collect();
    let mut max_running = 0;
    let mut started: Vec<ItemId> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let views = rig.queue.snapshot();
        let running: Vec<ItemId> = views.iter().filter(|v| v.state == TransferState::Running).map(|v| v.id).collect();
        max_running = max_running.max(running.len());
        for id in running {
            if !started.contains(&id) {
                started.push(id);
            }
        }
        if views.iter().all(|v| v.state.is_finished()) {
            break;
        }
        assert!(Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(max_running, 2, "two at a time, never more");
    assert_eq!(started, ids, "they start in the order they were queued");
    for n in 0..5 {
        assert!(rig.fx.read(&format!("/up/f{n}.bin")) == pattern(600_000, 30 + n as u64), "f{n}");
    }
}

#[tokio::test]
async fn a_waiting_item_starts_when_a_running_one_is_paused_or_cancelled() {
    let rig = Rig::slow().await;
    let big = |n: &str| rig.source(n, &pattern(BIG, 40), 0o644);
    let (a, b) =
        (rig.queue.enqueue_upload(vec![big("a.bin")], "/up"), rig.queue.enqueue_upload(vec![big("b.bin")], "/up"));
    let c = rig.queue.enqueue_upload(vec![rig.source("c.txt", b"c", 0o644)], "/up");
    rig.started(a).await;
    rig.started(b).await;
    tokio::time::sleep(SEC / 5).await;
    assert_eq!(rig.queue.view(c).unwrap().state, TransferState::Queued, "both slots are taken");
    rig.queue.pause(a);
    rig.state(a, TransferState::Paused(PauseReason::User)).await;
    assert_eq!(rig.finished(c).await.state, TransferState::Done, "a paused item gives up its slot");
    rig.queue.cancel(a);
    assert_eq!(rig.queue.view(a).unwrap().state, TransferState::Cancelled);
    rig.queue.cancel(b);
    rig.finished(b).await;
}

#[tokio::test]
async fn progress_events_are_throttled_and_the_speed_is_reported() {
    let rig = Rig::slow().await;
    let id = rig.queue.enqueue_upload(vec![rig.source("big.bin", &pattern(BIG, 41), 0o644)], "/up");
    let began = Instant::now();
    let mut best_speed: f64 = 0.0;
    let view = loop {
        let view = rig.queue.view(id).unwrap();
        best_speed = best_speed.max(view.speed);
        if view.state.is_finished() {
            break view;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let seconds = began.elapsed().as_secs_f64();
    assert_eq!(view.state, TransferState::Done);
    let progress = rig.events().iter().filter(|e| matches!(e, TransferEvent::Progress(_))).count();
    assert!(progress >= 2, "{progress}");
    assert!(
        progress as f64 <= seconds * 5.0 + 3.0,
        "{progress} progress events in {seconds:.1}s is more than 5 a second"
    );
    assert!(best_speed > 100_000.0, "a speed was reported while running: {best_speed}");
    assert_eq!(rig.queue.view(id).unwrap().speed, 0.0, "and none when it is over");
}

#[tokio::test]
async fn finished_items_can_be_cleared() {
    let rig = Rig::new().await;
    let done = rig.queue.enqueue_upload(vec![rig.source("a.txt", b"a", 0o644)], "/up");
    let failed = rig.queue.enqueue_upload(vec![rig.src("gone.txt")], "/up");
    rig.finished(done).await;
    rig.finished(failed).await;
    rig.fx.file("/srv/x.bin", pattern(BIG, 42), 0o644);
    rig.events();
    // One that is not finished stays.
    let slow = Rig::slow().await;
    let running = slow.queue.enqueue_upload(vec![slow.source("big.bin", &pattern(BIG, 43), 0o644)], "/up");
    slow.started(running).await;
    slow.queue.clear_finished();
    assert_eq!(slow.queue.snapshot().len(), 1);
    slow.queue.cancel(running);

    rig.queue.clear_finished();
    assert!(rig.queue.snapshot().is_empty());
    assert!(rig.queue.view(done).is_none());
    let events = rig.events();
    assert!(
        events.iter().any(|e| matches!(e, TransferEvent::Removed(ids) if ids == &vec![done, failed])),
        "{events:?}"
    );
}

// ---- sessions -----------------------------------------------------------------------------------

#[tokio::test]
async fn losing_the_session_pauses_running_items_and_a_new_one_resumes_only_those() {
    let rig = Rig::slow().await;
    let content = |n: u64| pattern(3 * 1024 * 1024, 50 + n);
    let up = |name: &str, n: u64| rig.queue.enqueue_upload(vec![rig.source(name, &content(n), 0o644)], "/up");
    let (a, b) = (up("a.bin", 1), up("b.bin", 2));
    let (c, d) = (up("c.bin", 3), up("d.bin", 4));
    rig.started(a).await;
    rig.started(b).await;
    rig.queue.pause(c);
    assert_eq!(rig.queue.view(c).unwrap().state, TransferState::Paused(PauseReason::User));

    rig.queue.set_session(None);
    let paused_a = rig.state(a, TransferState::Paused(PauseReason::Disconnected)).await;
    rig.state(b, TransferState::Paused(PauseReason::Disconnected)).await;
    assert_eq!(paused_a.state.label(), "已暂停 · 连接断开");
    assert_eq!(rig.queue.view(c).unwrap().state, TransferState::Paused(PauseReason::User));
    assert_eq!(rig.queue.view(d).unwrap().state, TransferState::Queued);
    tokio::time::sleep(SEC / 3).await;
    assert_eq!(rig.queue.view(d).unwrap().state, TransferState::Queued, "nothing starts without a session");

    // The connection comes back: what was running goes on by itself, what the user paused stays.
    rig.queue.set_session(Some(Arc::new(rig.fx.new_session().await)));
    for id in [a, b, d] {
        assert_eq!(rig.finished(id).await.state, TransferState::Done);
    }
    assert_eq!(rig.queue.view(c).unwrap().state, TransferState::Paused(PauseReason::User), "still paused");
    assert!(!rig.fx.exists("/up/c.bin"));
    assert!(rig.fx.read("/up/a.bin") == content(1));
    assert!(rig.fx.read("/up/b.bin") == content(2));
    assert!(rig.fx.read("/up/d.bin") == content(4));
    rig.queue.resume(c);
    assert_eq!(rig.finished(c).await.state, TransferState::Done);
    assert!(rig.fx.read("/up/c.bin") == content(3));
    assert_eq!(rig.fx.names("/up"), ["a.bin", "b.bin", "c.bin", "d.bin"]);
}

#[tokio::test]
async fn a_dropped_connection_is_noticed_by_the_transfer_and_a_new_session_finishes_it() {
    let rig = Rig::slow().await;
    let content = pattern(BIG, 60);
    rig.fx.file("/srv/down.bin", pattern(BIG, 61), 0o644);
    let up = rig.queue.enqueue_upload(vec![rig.source("up.bin", &content, 0o644)], "/up");
    let down = rig.queue.enqueue_download(vec!["/srv/down.bin".into()], rig.downloads());
    rig.wait(up, "progress", |v| v.bytes_done > (BIG / 5) as u64).await;
    rig.wait(down, "progress", |v| v.bytes_done > (BIG / 5) as u64).await;

    rig.fx.server.state.kill_connections();
    // Nobody told the queue: the transfers find out themselves, and wait.
    rig.state(up, TransferState::Paused(PauseReason::Disconnected)).await;
    let paused = rig.state(down, TransferState::Paused(PauseReason::Disconnected)).await;
    assert!(paused.bytes_done > 0);

    rig.queue.set_session(Some(Arc::new(rig.fx.new_session().await)));
    assert_eq!(rig.finished(up).await.state, TransferState::Done);
    assert_eq!(rig.finished(down).await.state, TransferState::Done);
    assert!(rig.fx.read("/up/up.bin") == content, "the checksum matches after the resume");
    assert!(std::fs::read(rig.downloads().join("down.bin")).unwrap() == pattern(BIG, 61));
    assert_eq!(rig.fx.names("/up"), ["up.bin"]);
    assert_eq!(names(&rig.downloads()), ["down.bin"]);
}

#[tokio::test]
async fn items_wait_for_a_session_and_a_replaced_session_resumes_running_items() {
    let (queue, _events) = TransferQueue::new(tokio::runtime::Handle::current());
    let fx = Fixture::default().await;
    fx.dir("/up");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let id = queue.enqueue_upload(vec![dir.path().join("a.txt")], "/up");
    tokio::time::sleep(SEC / 4).await;
    assert_eq!(queue.view(id).unwrap().state, TransferState::Queued, "no session yet");
    queue.set_session(Some(Arc::new(fx.new_session().await)));
    for _ in 0..500 {
        if queue.view(id).unwrap().state.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(queue.view(id).unwrap().state, TransferState::Done);
    assert_eq!(fx.text("/up/a.txt"), "a");
}

#[tokio::test]
async fn a_session_replaced_without_a_gap_carries_running_items_over() {
    let rig = Rig::slow().await;
    let content = pattern(BIG, 70);
    let id = rig.queue.enqueue_upload(vec![rig.source("big.bin", &content, 0o644)], "/up");
    rig.wait(id, "a quarter", |v| v.bytes_done > (BIG / 4) as u64).await;
    // The reconnect handed in the new session directly, without saying that the old one was gone.
    rig.queue.set_session(Some(Arc::new(rig.fx.new_session().await)));
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(rig.fx.read("/up/big.bin") == content);
}

// ---- how the requests are made --------------------------------------------------------------------

#[tokio::test]
async fn requests_follow_the_servers_limits() {
    let rig = Rig::with(SftpOptions { limits: Some((65_536, 16_384, 20_000)), ..Default::default() }).await;
    assert!(rig.fx.session.info().await.limits.is_some());
    let content = pattern(600_000, 80);
    let id = rig.queue.enqueue_upload(vec![rig.source("a.bin", &content, 0o644)], "/up");
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(rig.fx.read("/up/a.bin") == content);
    let longest_write = rig.fx.logs.writes.lock().unwrap().iter().map(|(_, _, len)| *len).max().unwrap();
    assert_eq!(longest_write, 20_000, "writes are as large as the limit allows");

    let id = rig.queue.enqueue_download(vec!["/up/a.bin".into()], rig.downloads());
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(std::fs::read(rig.downloads().join("a.bin")).unwrap() == content);
    let longest_read = rig.fx.logs.reads.lock().unwrap().iter().map(|(_, _, len)| *len).max().unwrap();
    assert_eq!(longest_read, 16_384);
}

#[tokio::test]
async fn without_limits_requests_are_modest() {
    let rig = Rig::new().await;
    assert!(rig.fx.session.info().await.limits.is_none());
    let id = rig.queue.enqueue_upload(vec![rig.source("a.bin", &pattern(300_000, 81), 0o644)], "/up");
    rig.finished(id).await;
    let longest = rig.fx.logs.writes.lock().unwrap().iter().map(|(_, _, len)| *len).max().unwrap();
    assert_eq!(longest, 32 * 1024);
}

#[tokio::test]
async fn short_reads_do_not_corrupt_a_download() {
    // The protocol lets a server answer with less than was asked; the requests behind it must be redone.
    let rig = Rig::with(SftpOptions { max_read: Some(5_000), ..Default::default() }).await;
    let content = pattern(400_000, 82);
    rig.fx.file("/srv/short.bin", &content, 0o644);
    let id = rig.queue.enqueue_download(vec!["/srv/short.bin".into()], rig.downloads());
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done, "{view:?}");
    assert!(std::fs::read(rig.downloads().join("short.bin")).unwrap() == content);
    assert!(rig.fx.logs.reads.lock().unwrap().iter().all(|(_, _, len)| *len <= 5_000));
}

#[tokio::test]
async fn a_download_of_a_file_that_shrinks_fails_that_file() {
    let rig = Rig::slow().await;
    rig.fx.file("/srv/shrinks.bin", pattern(BIG, 83), 0o644);
    rig.fx.file("/srv/stays.bin", pattern(10_000, 84), 0o644);
    let id = rig.queue.enqueue_download(vec!["/srv/shrinks.bin".into(), "/srv/stays.bin".into()], rig.downloads());
    rig.wait(id, "some progress", |v| v.bytes_done > 100_000).await;
    std::fs::OpenOptions::new().write(true).open(rig.fx.host("/srv/shrinks.bin")).unwrap().set_len(200_000).unwrap();
    let view = rig.finished(id).await;
    let TransferState::Failed(reason) = &view.state else { panic!("{:?}", view.state) };
    assert!(reason.contains("变短"), "{reason}");
    assert_eq!(names(&rig.downloads()), ["stays.bin"], "the failed file leaves nothing behind");
}

// ---- directories that do not allow new files ----------------------------------------------------------

#[tokio::test]
async fn an_overwrite_where_no_temporary_file_can_be_made_goes_straight_into_the_file() {
    let rig = Rig::with(SftpOptions { dir_unwritable: true, ..Default::default() }).await;
    rig.fx.file("/ro/conf.txt", "old content that is rather long", 0o640);
    let inode = std::fs::metadata(rig.fx.host("/ro/conf.txt")).unwrap().ino();
    let src = rig.source("conf.txt", b"new", 0o644);
    set_mtime(&src, 1_700_000_000);
    let id = rig.queue.enqueue_upload(vec![src], "/ro");
    rig.state(id, TransferState::AwaitingClash).await;
    rig.queue.answer_clash(id, Choice::Overwrite);
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done, "{view:?}");
    assert_eq!(view.in_place, 1);
    assert_eq!(view.in_place_note().as_deref(), Some("1 个文件已直接写回（目录不可写）"));
    assert_eq!(rig.fx.text("/ro/conf.txt"), "new", "the tail of the old content is gone");
    assert_eq!(std::fs::metadata(rig.fx.host("/ro/conf.txt")).unwrap().ino(), inode, "the same file");
    assert_eq!(rig.fx.mode("/ro/conf.txt"), 0o640, "its permissions are kept");
    assert_eq!(mtime(&rig.fx.host("/ro/conf.txt")), 1_700_000_000);
    assert_eq!(rig.fx.names("/ro"), ["conf.txt"]);

    // Where there is nothing to overwrite, there is no way in.
    let id = rig.queue.enqueue_upload(vec![rig.source("fresh.txt", b"x", 0o644)], "/ro");
    let view = rig.finished(id).await;
    let TransferState::Failed(reason) = &view.state else { panic!("{:?}", view.state) };
    assert!(reason.contains("权限不足"), "{reason}");
    assert_eq!(view.in_place, 0);
    assert_eq!(rig.fx.names("/ro"), ["conf.txt"]);
}

#[tokio::test]
async fn an_item_waiting_for_an_answer_can_be_cancelled() {
    let rig = Rig::new().await;
    rig.fx.file("/up/a.txt", "old", 0o644);
    let id =
        rig.queue.enqueue_upload(vec![rig.source("a.txt", b"new", 0o644), rig.source("b.txt", b"b", 0o644)], "/up");
    rig.state(id, TransferState::AwaitingClash).await;
    rig.queue.cancel(id);
    let view = rig.queue.view(id).unwrap();
    assert_eq!((view.state, view.clash), (TransferState::Cancelled, None));
    tokio::time::sleep(SEC / 5).await;
    assert_eq!(rig.fx.text("/up/a.txt"), "old");
    assert!(!rig.fx.exists("/up/b.txt"), "nothing was uploaded");
    rig.queue.answer_clash(id, Choice::Overwrite);
    assert_eq!(rig.queue.view(id).unwrap().state, TransferState::Cancelled, "a late answer changes nothing");
}

#[tokio::test]
async fn shutting_the_queue_down_stops_the_transfers_and_lets_go_of_the_session() {
    let rig = Rig::slow().await;
    let id = rig.queue.enqueue_upload(vec![rig.source("big.bin", &pattern(BIG, 90), 0o644)], "/up");
    rig.started(id).await;
    rig.queue.shutdown();
    rig.state(id, TransferState::Paused(PauseReason::Disconnected)).await;
    tokio::time::sleep(SEC / 3).await;
    assert_eq!(
        rig.queue.view(id).unwrap().state,
        TransferState::Paused(PauseReason::Disconnected),
        "nothing restarts it"
    );
}

// ---- gaps, stalls and lost sessions ------------------------------------------------------------------

const CHUNK: u64 = 32 * 1024;

fn session_lost_events(events: &[TransferEvent]) -> usize {
    events.iter().filter(|e| matches!(e, TransferEvent::SessionLost)).count()
}

#[tokio::test]
async fn a_resume_goes_on_from_the_gapless_part_not_from_the_length_of_the_temporary_file() {
    // One write request is lost on its way while the ones after it get through: the temporary file
    // ends up as long as the source, with a hole of zeros in it.
    let hole_at = 5 * CHUNK;
    let rig = Rig::with(SftpOptions { drop_write_at: Some(hole_at), ..Default::default() }).await;
    let content = pattern(1 << 20, 100);
    let id = rig.queue.enqueue_upload(vec![rig.source("big.bin", &content, 0o644)], "/up");
    rig.wait(id, "all but the lost chunk", |v| v.bytes_done == (1 << 20) - CHUNK).await;
    tokio::time::sleep(SEC / 5).await;
    let on_server = std::fs::read(rig.fx.host("/up/.big.bin.shellmate-upload")).unwrap();
    assert_eq!(on_server.len(), 1 << 20, "the temporary file is as long as the source");
    assert!(on_server[hole_at as usize..(hole_at + CHUNK) as usize].iter().all(|b| *b == 0), "with a hole in it");

    rig.fx.server.state.kill_connections();
    let paused = rig.state(id, TransferState::Paused(PauseReason::Disconnected)).await;
    assert_eq!(paused.failure_total, 0);
    let log_len = rig.fx.logs.writes.lock().unwrap().len();
    rig.queue.set_session(Some(Arc::new(rig.fx.new_session().await)));
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(rig.fx.read("/up/big.bin") == content, "no zeros left over from the hole");
    let log = rig.fx.logs.writes.lock().unwrap()[log_len..].to_vec();
    assert_eq!(log.iter().map(|(_, offset, _)| *offset).min(), Some(hole_at), "it went on from the gapless part");
}

#[tokio::test]
async fn a_session_that_stops_answering_pauses_the_item_and_a_new_one_resumes_it_without_resending() {
    let stalls_after = 20;
    let rig =
        Rig::with_timeout(SftpOptions { stall_after_writes: Some(stalls_after), ..Default::default() }, Some(1)).await;
    let content = pattern(2 << 20, 101);
    let id = rig.queue.enqueue_upload(vec![rig.source("big.bin", &content, 0o644)], "/up");
    // Nothing is answered after the 20th write: the requests time out, and the item waits.
    let paused = rig.state(id, TransferState::Paused(PauseReason::Disconnected)).await;
    assert_eq!((paused.failure_total, paused.failures.len()), (0, 0), "a stalled session is not the file's failure");
    assert_eq!(paused.bytes_done, stalls_after as u64 * CHUNK);
    assert_eq!(
        std::fs::metadata(rig.fx.host("/up/.big.bin.shellmate-upload")).unwrap().len(),
        stalls_after as u64 * CHUNK,
        "the temporary file is kept"
    );
    assert_eq!(session_lost_events(&rig.events()), 1, "the owner is told");

    let log_len = rig.fx.logs.writes.lock().unwrap().len();
    rig.queue.set_session(Some(Arc::new(rig.fx.new_session().await)));
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(rig.fx.read("/up/big.bin") == content);
    let log = rig.fx.logs.writes.lock().unwrap()[log_len..].to_vec();
    assert!(
        log.iter().all(|(_, offset, _)| *offset >= stalls_after as u64 * CHUNK),
        "the acknowledged part was not sent again"
    );
}

#[tokio::test]
async fn a_download_from_a_stalled_session_waits_too() {
    let rig =
        Rig::with_timeout(SftpOptions { read_delay: Duration::from_millis(2), ..Default::default() }, Some(1)).await;
    // Reads are not lost by the link; the connection is cut under a download without anybody noticing
    // the transfer itself: it finds out by the first answer that does not come.
    let content = pattern(8 << 20, 102);
    rig.fx.file("/srv/big.bin", &content, 0o644);
    let id = rig.queue.enqueue_download(vec!["/srv/big.bin".into()], rig.downloads());
    rig.wait(id, "a bit of the file", |v| v.bytes_done > (1 << 20)).await;
    rig.fx.server.state.kill_connections();
    let paused = rig.state(id, TransferState::Paused(PauseReason::Disconnected)).await;
    assert_eq!(paused.failure_total, 0);
    assert!(rig.downloads().join("big.bin.shellmate-download").exists(), "the partial file is kept");
    rig.queue.set_session(Some(Arc::new(rig.fx.new_session().await)));
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(std::fs::read(rig.downloads().join("big.bin")).unwrap() == content);
}

#[tokio::test]
async fn taking_the_session_away_stops_a_transfer_at_once_without_waiting_for_answers() {
    let rig = Rig::with(SftpOptions { stall_after_writes: Some(10), ..Default::default() }).await;
    let id = rig.queue.enqueue_upload(vec![rig.source("big.bin", &pattern(2 << 20, 103), 0o644)], "/up");
    rig.wait(id, "ten chunks", |v| v.bytes_done >= 10 * CHUNK).await;
    tokio::time::sleep(SEC / 4).await;
    let began = Instant::now();
    rig.queue.set_session(None);
    let paused = rig.state(id, TransferState::Paused(PauseReason::Disconnected)).await;
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "no draining of requests nobody will answer: {:?}",
        began.elapsed()
    );
    assert_eq!(paused.failure_total, 0);
    assert!(rig.fx.exists("/up/.big.bin.shellmate-upload"), "the temporary file is kept");
    assert_eq!(session_lost_events(&rig.events()), 0, "taking it away is not losing it");
}

#[tokio::test]
async fn a_user_pause_on_a_stalled_session_ends_as_paused_not_failed_and_resumes_cleanly() {
    let stalls_after = 10;
    let rig =
        Rig::with_timeout(SftpOptions { stall_after_writes: Some(stalls_after), ..Default::default() }, Some(2)).await;
    let content = pattern(2 << 20, 104);
    let id = rig.queue.enqueue_upload(vec![rig.source("big.bin", &content, 0o644)], "/up");
    rig.wait(id, "ten chunks", |v| v.bytes_done >= stalls_after as u64 * CHUNK).await;
    rig.queue.pause(id);
    // Waiting for the requests on their way is given up after a while: the pause still lands.
    let paused = rig.state(id, TransferState::Paused(PauseReason::User)).await;
    assert_eq!((paused.failure_total, paused.bytes_done), (0, stalls_after as u64 * CHUNK));

    let log_len = rig.fx.logs.writes.lock().unwrap().len();
    rig.queue.set_session(Some(Arc::new(rig.fx.new_session().await)));
    tokio::time::sleep(SEC / 4).await;
    assert_eq!(rig.queue.view(id).unwrap().state, TransferState::Paused(PauseReason::User), "the user's pause stays");
    rig.queue.resume(id);
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert!(rig.fx.read("/up/big.bin") == content);
    let log = rig.fx.logs.writes.lock().unwrap()[log_len..].to_vec();
    assert!(log.iter().all(|(_, offset, _)| *offset >= stalls_after as u64 * CHUNK), "nothing was sent twice");
}

#[tokio::test]
async fn an_error_that_comes_back_after_the_session_was_taken_away_is_not_the_files_failure() {
    // The server is slow to answer the open of the temporary file, and then refuses: but by then the
    // session is gone, and what comes back from a session being torn down means nothing.
    let rig = Rig::with(SftpOptions { open_delay: Duration::from_millis(400), ..Default::default() }).await;
    let id = rig.queue.enqueue_upload(vec![rig.source("a.txt", b"a", 0o644)], "/up");
    rig.wait(id, "the file to be started", |v| v.state == TransferState::Running && v.current.is_some()).await;
    rig.queue.set_session(None);
    rig.fx.logs.fail_opens.store(true, std::sync::atomic::Ordering::Relaxed);
    let paused = rig.state(id, TransferState::Paused(PauseReason::Disconnected)).await;
    assert_eq!((paused.failure_total, paused.files_done), (0, 0), "no failure was recorded");
    rig.fx.logs.fail_opens.store(false, std::sync::atomic::Ordering::Relaxed);
    rig.queue.set_session(Some(Arc::new(rig.fx.new_session().await)));
    assert_eq!(rig.finished(id).await.state, TransferState::Done);
    assert_eq!(rig.fx.text("/up/a.txt"), "a");
}

#[tokio::test]
async fn the_queue_says_once_for_each_session_when_the_one_it_holds_is_lost() {
    let rig = Rig::slow().await;
    let up =
        |n: u64| rig.queue.enqueue_upload(vec![rig.source(&format!("f{n}.bin"), &pattern(BIG, 110 + n), 0o644)], "/up");
    let (a, b) = (up(1), up(2));
    rig.started(a).await;
    rig.started(b).await;
    rig.events();

    rig.fx.server.state.kill_connections();
    rig.state(a, TransferState::Paused(PauseReason::Disconnected)).await;
    rig.state(b, TransferState::Paused(PauseReason::Disconnected)).await;
    tokio::time::sleep(SEC / 3).await;
    assert_eq!(session_lost_events(&rig.events()), 1, "two items, one session, one message");

    rig.queue.set_session(Some(Arc::new(rig.fx.new_session().await)));
    rig.finished(a).await;
    rig.finished(b).await;
    // The next session is lost in its turn: told again.
    let c = up(3);
    rig.started(c).await;
    rig.events();
    rig.fx.server.state.kill_connections();
    rig.state(c, TransferState::Paused(PauseReason::Disconnected)).await;
    tokio::time::sleep(SEC / 3).await;
    assert_eq!(session_lost_events(&rig.events()), 1);

    // Taking a session away on purpose is not losing it.
    rig.queue.set_session(Some(Arc::new(rig.fx.new_session().await)));
    rig.started(c).await;
    rig.events();
    rig.queue.set_session(None);
    rig.state(c, TransferState::Paused(PauseReason::Disconnected)).await;
    tokio::time::sleep(SEC / 3).await;
    assert_eq!(session_lost_events(&rig.events()), 0);
}

// ---- small things -------------------------------------------------------------------------------------

#[tokio::test]
async fn only_the_first_failures_are_kept_and_the_rest_are_counted() {
    if is_root() {
        return;
    }
    let rig = Rig::new().await;
    for n in 0..130 {
        rig.source(&format!("tree/f{n:03}.txt"), b"x", 0o000);
    }
    let id = rig.queue.enqueue_upload(vec![rig.src("tree")], "/up");
    let view = rig.finished(id).await;
    let TransferState::Failed(summary) = &view.state else { panic!("{:?}", view.state) };
    assert_eq!((view.failure_total, view.failures.len()), (130, 100));
    assert!(summary.starts_with("130 个文件传输失败，首个原因：权限不足"), "{summary}");
    assert!(summary.ends_with("等 130 个）"), "{summary}");
    assert!(view.failures[0].path.ends_with("f000.txt"), "the first ones are the ones kept");
    assert_eq!(rig.fx.names("/up/tree").len(), 0, "no temporary files");
}

#[tokio::test]
async fn the_result_of_a_download_is_the_name_it_really_got() {
    let rig = Rig::slow().await;
    let content = pattern(1 << 20, 120);
    rig.fx.file("/srv/a.txt", &content, 0o644);
    let id = rig.queue.enqueue_download(vec!["/srv/a.txt".into()], rig.downloads());
    rig.started(id).await;
    // Somebody takes the planned name while the download is under way.
    std::fs::write(rig.downloads().join("a.txt"), "somebody else's file").unwrap();
    let view = rig.finished(id).await;
    assert_eq!(view.state, TransferState::Done);
    assert_eq!(std::fs::read_to_string(rig.downloads().join("a.txt")).unwrap(), "somebody else's file");
    assert!(std::fs::read(rig.downloads().join("a (1).txt")).unwrap() == content);
    assert_eq!(view.result, Some(rig.downloads().join("a (1).txt")), "Finder is shown the right file");
}
