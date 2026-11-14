//! Reading text for the editor and saving it back.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::time::{Duration, SystemTime};

use shellmate_sftp::{
    InPlaceReason, LineEnding, MAX_TEXT_SIZE, Revision, SaveFormat, SaveOutcome, SftpError, WriteMode, save_temp_path,
};

use crate::fixture::{Fixture, SftpOptions};

const LF: SaveFormat = SaveFormat { line_ending: LineEnding::Lf, bom: false };

fn is_root() -> bool {
    // Permission tests are meaningless for a process that ignores permissions.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, "x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::read(&file).is_ok()
}

fn saved(outcome: SaveOutcome) -> (Revision, WriteMode) {
    match outcome {
        SaveOutcome::Saved { revision, mode } => (revision, mode),
        other => panic!("expected a save, got {other:?}"),
    }
}

/// Moves a file's modification time by `seconds` without touching its content.
fn shift_mtime(fx: &Fixture, remote: &str, seconds: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(fx.host(remote)).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(seconds)).unwrap();
}

// ---- reading -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_text_file_opens_with_its_revision_and_mode() {
    let fx = Fixture::default().await;
    fx.file("/etc/app.conf", "name = 中文\nport = 22\n", 0o640);
    let opened = fx.session.open_text("/etc/app.conf").await.unwrap();
    assert_eq!(opened.text, "name = 中文\nport = 22\n");
    assert_eq!((opened.line_ending, opened.bom, opened.mode), (LineEnding::Lf, false, Some(0o640)));
    assert_eq!((opened.path.as_str(), opened.target.as_str()), ("/etc/app.conf", "/etc/app.conf"));
    let meta = std::fs::metadata(fx.host("/etc/app.conf")).unwrap();
    assert_eq!(opened.revision, Revision { size: meta.len(), mtime: Some(meta.mtime()) });
    assert_eq!(
        opened.revision,
        fx.session.stat("/etc/app.conf").await.unwrap().revision(),
        "what the editor remembers is what stat reports"
    );
}

#[tokio::test]
async fn line_endings_and_byte_order_marks_are_detected() {
    let fx = Fixture::default().await;
    fx.file("/crlf.txt", "a\r\nb\r\n", 0o644);
    fx.file("/bom.txt", "\u{feff}héllo\n", 0o644);
    fx.file("/mixed.txt", "a\r\nb\nc\nd\n", 0o644);
    let crlf = fx.session.open_text("/crlf.txt").await.unwrap();
    assert_eq!((crlf.text.as_str(), crlf.line_ending), ("a\nb\n", LineEnding::CrLf));
    let bom = fx.session.open_text("/bom.txt").await.unwrap();
    assert_eq!((bom.text.as_str(), bom.bom), ("héllo\n", true));
    assert_eq!(fx.session.open_text("/mixed.txt").await.unwrap().line_ending, LineEnding::Lf);
}

#[tokio::test]
async fn files_the_editor_does_not_open_are_refused_with_distinct_errors() {
    let fx = Fixture::default().await;
    fx.file("/bin.dat", b"\x7fELF\x00\x01\x02", 0o755);
    fx.file("/latin1.txt", b"caf\xe9\n", 0o644);
    fx.file("/utf16.txt", b"\xff\xfeh\x00i\x00", 0o644);
    fx.file("/big.txt", vec![b'a'; MAX_TEXT_SIZE as usize + 1], 0o644);
    fx.dir("/folder");
    assert_eq!(fx.session.open_text("/bin.dat").await.unwrap_err(), SftpError::Binary);
    assert_eq!(fx.session.open_text("/latin1.txt").await.unwrap_err(), SftpError::NotUtf8);
    assert_eq!(fx.session.open_text("/utf16.txt").await.unwrap_err(), SftpError::NotUtf8);
    assert_eq!(fx.session.open_text("/big.txt").await.unwrap_err(), SftpError::TooLarge { size: MAX_TEXT_SIZE + 1 });
    assert_eq!(fx.session.open_text("/folder").await.unwrap_err(), SftpError::IsADirectory { path: "/folder".into() });
    assert_eq!(
        fx.session.open_text("/missing.txt").await.unwrap_err(),
        SftpError::NotFound { path: "/missing.txt".into() }
    );
    assert!(fx.session.open_text("/big.txt").await.unwrap_err().to_string().contains("请下载后查看"));
}

#[tokio::test]
async fn the_size_limit_is_inclusive() {
    let fx = Fixture::default().await;
    fx.file("/limit.txt", vec![b'x'; MAX_TEXT_SIZE as usize], 0o644);
    let opened = fx.session.open_text("/limit.txt").await.unwrap();
    assert_eq!(opened.text.len() as u64, MAX_TEXT_SIZE);
}

#[tokio::test]
async fn a_large_file_is_read_completely_and_in_order() {
    let fx = Fixture::default().await;
    // Many chunks with distinct content, so a misplaced chunk shows.
    let content: String = (0..120_000).map(|i| format!("line {i} 中\n")).collect();
    assert!(content.len() > 1_500_000);
    fx.file("/large.txt", &content, 0o644);
    let opened = fx.session.open_text("/large.txt").await.unwrap();
    assert_eq!(opened.text.len(), content.len());
    assert!(opened.text == content);
}

#[tokio::test]
async fn pipes_are_not_opened() {
    let fx = Fixture::default().await;
    let fifo = fx.host("/pipe");
    let made = std::process::Command::new("mkfifo").arg(&fifo).status().map(|s| s.success()).unwrap_or(false);
    if made {
        assert_eq!(fx.session.open_text("/pipe").await.unwrap_err(), SftpError::NotAFile { path: "/pipe".into() });
    }
}

#[tokio::test]
async fn an_unreadable_file_gives_permission_denied() {
    if is_root() {
        return;
    }
    let fx = Fixture::default().await;
    fx.file("/secret.txt", "x", 0o000);
    assert_eq!(
        fx.session.open_text("/secret.txt").await.unwrap_err(),
        SftpError::PermissionDenied { path: "/secret.txt".into() }
    );
}

// ---- saving: the atomic path, with and without posix-rename -------------------------------------

async fn both_servers() -> [(&'static str, Fixture); 2] {
    [
        ("posix-rename", Fixture::default().await),
        ("backup swap", Fixture::start(SftpOptions { posix_rename: false, ..Default::default() }).await),
    ]
}

#[tokio::test]
async fn saving_replaces_the_file_atomically_and_keeps_its_permissions() {
    for (what, fx) in both_servers().await {
        for mode in [0o644, 0o600, 0o640, 0o755, 0o664] {
            fx.file("/s/app.conf", "old\n", mode);
            let inode = std::fs::metadata(fx.host("/s/app.conf")).unwrap().ino();
            let opened = fx.session.open_text("/s/app.conf").await.unwrap();
            let outcome = fx.session.save_text("/s/app.conf", "new 内容\n", LF, Some(opened.revision)).await.unwrap();
            let (revision, write) = saved(outcome);
            assert_eq!(write, WriteMode::Atomic, "{what}");
            assert_eq!(fx.text("/s/app.conf"), "new 内容\n", "{what}");
            assert_eq!(fx.mode("/s/app.conf"), mode, "{what}: the mode {mode:o} survives, umask or not");
            assert_ne!(
                std::fs::metadata(fx.host("/s/app.conf")).unwrap().ino(),
                inode,
                "{what}: a new file was swapped in"
            );
            assert_eq!(fx.names("/s"), ["app.conf"], "{what}: no temporary or backup file is left");
            assert_eq!(revision, fx.session.stat("/s/app.conf").await.unwrap().revision(), "{what}");
        }
    }
}

#[tokio::test]
async fn the_revision_returned_by_a_save_is_good_for_the_next_one() {
    let fx = Fixture::default().await;
    fx.file("/n.txt", "v0", 0o644);
    let opened = fx.session.open_text("/n.txt").await.unwrap();
    let (first, _) = saved(fx.session.save_text("/n.txt", "v1", LF, Some(opened.revision)).await.unwrap());
    let (second, _) = saved(fx.session.save_text("/n.txt", "v2 longer", LF, Some(first)).await.unwrap());
    assert_eq!(fx.text("/n.txt"), "v2 longer");
    assert_eq!(second.size, 9);
    let reopened = fx.session.open_text("/n.txt").await.unwrap();
    assert_eq!(reopened.revision, second);
}

#[tokio::test]
async fn saving_keeps_line_endings_and_the_byte_order_mark() {
    let fx = Fixture::default().await;
    fx.file("/win.txt", "\u{feff}one\r\ntwo\r\n", 0o644);
    let opened = fx.session.open_text("/win.txt").await.unwrap();
    assert_eq!(opened.text, "one\ntwo\n");
    let text = format!("{}three\n", opened.text);
    saved(fx.session.save_text("/win.txt", &text, SaveFormat::from(&opened), Some(opened.revision)).await.unwrap());
    assert_eq!(fx.read("/win.txt"), "\u{feff}one\r\ntwo\r\nthree\r\n".as_bytes());
    // Typing the other kind of line end (or pasting CRLF text) does not double up.
    saved(
        fx.session
            .save_text("/win.txt", "a\r\nb\nc", SaveFormat { line_ending: LineEnding::CrLf, bom: false }, None)
            .await
            .unwrap(),
    );
    assert_eq!(fx.read("/win.txt"), b"a\r\nb\r\nc");
}

#[tokio::test]
async fn saving_an_empty_text_or_a_big_one_works() {
    let fx = Fixture::default().await;
    fx.file("/e.txt", "something", 0o644);
    saved(fx.session.save_text("/e.txt", "", LF, None).await.unwrap());
    assert!(fx.read("/e.txt").is_empty());
    let big: String = (0..90_000).map(|i| format!("row {i}\n")).collect();
    saved(fx.session.save_text("/e.txt", &big, LF, None).await.unwrap());
    assert!(fx.text("/e.txt") == big, "many requests in flight, one file");
}

#[tokio::test]
async fn a_new_file_can_be_created_by_a_save_without_an_expectation() {
    for (what, fx) in both_servers().await {
        fx.dir("/fresh");
        let (revision, write) = saved(fx.session.save_text("/fresh/new.txt", "hello\n", LF, None).await.unwrap());
        assert_eq!((write, revision.size), (WriteMode::Atomic, 6), "{what}");
        assert_eq!(fx.text("/fresh/new.txt"), "hello\n", "{what}");
        assert_eq!(fx.names("/fresh"), ["new.txt"], "{what}");
    }
}

#[tokio::test]
async fn refusing_to_chown_does_not_fail_the_save() {
    let fx = Fixture::start(SftpOptions { deny_chown: true, ..Default::default() }).await;
    fx.file("/o.txt", "old", 0o640);
    // The file is already ours, so the new one has the right owner without a chown: still atomic.
    let (_, write) = saved(fx.session.save_text("/o.txt", "new", LF, None).await.unwrap());
    assert_eq!(write, WriteMode::Atomic);
    assert_eq!((fx.text("/o.txt").as_str(), fx.mode("/o.txt")), ("new", 0o640));
}

// ---- saving: failures roll back -----------------------------------------------------------------

#[tokio::test]
async fn a_failed_replace_leaves_the_original_untouched_and_cleans_up() {
    for (what, posix) in [("posix-rename", true), ("backup swap", false)] {
        let fx = Fixture::start(SftpOptions {
            posix_rename: posix,
            fail_rename_from: Some(".shellmate-edit"),
            ..Default::default()
        })
        .await;
        fx.file("/r/keep.txt", "original\n", 0o640);
        let inode = std::fs::metadata(fx.host("/r/keep.txt")).unwrap().ino();
        let opened = fx.session.open_text("/r/keep.txt").await.unwrap();
        let error = fx.session.save_text("/r/keep.txt", "lost\n", LF, Some(opened.revision)).await.unwrap_err();
        assert!(matches!(error, SftpError::Failed { .. }), "{what}: {error:?}");
        assert_eq!(fx.text("/r/keep.txt"), "original\n", "{what}");
        assert_eq!(fx.mode("/r/keep.txt"), 0o640, "{what}");
        assert_eq!(std::fs::metadata(fx.host("/r/keep.txt")).unwrap().ino(), inode, "{what}: the very same file");
        assert_eq!(fx.names("/r"), ["keep.txt"], "{what}: temporary and backup files are gone");
    }
}

#[tokio::test]
async fn the_backup_swap_puts_the_original_back_when_the_second_step_fails() {
    // Failing only the move-in: the original is moved aside first and must come back.
    let fx = Fixture::start(SftpOptions {
        posix_rename: false,
        fail_rename_from: Some(".shellmate-edit"),
        ..Default::default()
    })
    .await;
    fx.file("/q/a.txt", "A", 0o600);
    fx.file("/q/b.txt", "B", 0o644);
    assert!(fx.session.save_text("/q/a.txt", "changed", LF, None).await.is_err());
    assert_eq!((fx.text("/q/a.txt").as_str(), fx.mode("/q/a.txt")), ("A", 0o600));
    assert_eq!(fx.names("/q"), ["a.txt", "b.txt"]);
}

#[tokio::test]
async fn replace_file_replaces_a_target_and_creates_a_missing_one() {
    for (what, fx) in both_servers().await {
        fx.file("/m/target", "old", 0o644);
        fx.file("/m/temp-1", "new", 0o644);
        fx.session.replace_file("/m/temp-1", "/m/target").await.unwrap();
        assert_eq!((fx.text("/m/target").as_str(), fx.names("/m")), ("new", vec!["target".to_string()]), "{what}");
        fx.file("/m/temp-2", "fresh", 0o644);
        fx.session.replace_file("/m/temp-2", "/m/created").await.unwrap();
        assert_eq!(fx.text("/m/created"), "fresh", "{what}");
        assert_eq!(fx.names("/m"), ["created", "target"], "{what}");
    }
}

#[tokio::test]
async fn a_stale_backup_from_an_earlier_crash_does_not_block_a_save() {
    let fx = Fixture::start(SftpOptions { posix_rename: false, ..Default::default() }).await;
    fx.file("/z/doc.txt", "current", 0o644);
    fx.file("/z/.doc.txt.shellmate-bak", "stale", 0o644);
    saved(fx.session.save_text("/z/doc.txt", "saved", LF, None).await.unwrap());
    assert_eq!(fx.text("/z/doc.txt"), "saved");
    assert_eq!(fx.names("/z"), ["doc.txt"], "the stale backup was cleared away");
}

// ---- conflicts ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_changed_file_is_a_conflict_and_nothing_is_written() {
    for (what, fx) in both_servers().await {
        fx.file("/c/shared.txt", "mine\nline2\n", 0o644);
        let opened = fx.session.open_text("/c/shared.txt").await.unwrap();
        // Somebody else saves a different text.
        fx.file("/c/shared.txt", "theirs, longer\nline2\n", 0o644);
        let outcome = fx.session.save_text("/c/shared.txt", "my edit\n", LF, Some(opened.revision)).await.unwrap();
        let SaveOutcome::Conflict { remote_text, remote_mtime, remote_revision } = outcome else {
            panic!("{what}: {outcome:?}")
        };
        assert_eq!(remote_text.as_deref(), Some("theirs, longer\nline2\n"), "{what}");
        assert_eq!(remote_revision, fx.session.stat("/c/shared.txt").await.unwrap().revision(), "{what}");
        assert_eq!(remote_mtime, remote_revision.mtime, "{what}");
        assert_eq!(fx.text("/c/shared.txt"), "theirs, longer\nline2\n", "{what}: untouched");
        assert_eq!(fx.names("/c"), ["shared.txt"], "{what}: no temporary file was left behind");

        // "覆盖远端": the user chose to overwrite knowingly.
        saved(fx.session.save_text("/c/shared.txt", "my edit\n", LF, Some(remote_revision)).await.unwrap());
        assert_eq!(fx.text("/c/shared.txt"), "my edit\n", "{what}");
    }
}

#[tokio::test]
async fn a_change_of_the_time_alone_is_a_conflict() {
    let fx = Fixture::default().await;
    fx.file("/t.txt", "same size", 0o644);
    let opened = fx.session.open_text("/t.txt").await.unwrap();
    shift_mtime(&fx, "/t.txt", 3600);
    let outcome = fx.session.save_text("/t.txt", "x", LF, Some(opened.revision)).await.unwrap();
    let SaveOutcome::Conflict { remote_text, remote_mtime, .. } = outcome else { panic!("{outcome:?}") };
    assert_eq!(remote_text.as_deref(), Some("same size"));
    assert!(remote_mtime.unwrap() > opened.revision.mtime.unwrap());
}

#[tokio::test]
async fn a_conflict_with_a_file_that_is_no_longer_text_says_so() {
    let fx = Fixture::default().await;
    fx.file("/g.txt", "text", 0o644);
    let opened = fx.session.open_text("/g.txt").await.unwrap();
    fx.file("/g.txt", b"\x00\x01\x02binary now", 0o644);
    let outcome = fx.session.save_text("/g.txt", "mine", LF, Some(opened.revision)).await.unwrap();
    let SaveOutcome::Conflict { remote_text, remote_revision, .. } = outcome else { panic!("{outcome:?}") };
    assert_eq!(remote_text, None);
    assert_eq!(remote_revision.size, 13);
    fx.file("/g.txt", vec![b'x'; MAX_TEXT_SIZE as usize + 10], 0o644);
    let SaveOutcome::Conflict { remote_text, .. } =
        fx.session.save_text("/g.txt", "mine", LF, Some(opened.revision)).await.unwrap()
    else {
        panic!("expected a conflict")
    };
    assert_eq!(remote_text, None, "too large to show");
}

#[tokio::test]
async fn a_file_deleted_in_the_meantime_is_not_silently_recreated() {
    let fx = Fixture::default().await;
    fx.file("/d.txt", "here", 0o644);
    let opened = fx.session.open_text("/d.txt").await.unwrap();
    std::fs::remove_file(fx.host("/d.txt")).unwrap();
    let error = fx.session.save_text("/d.txt", "mine", LF, Some(opened.revision)).await.unwrap_err();
    assert_eq!(error, SftpError::NotFound { path: "/d.txt".into() });
    assert!(!fx.exists("/d.txt"));
    // Overwriting on purpose recreates it.
    saved(fx.session.save_text("/d.txt", "mine", LF, None).await.unwrap());
    assert_eq!(fx.text("/d.txt"), "mine");
}

#[tokio::test]
async fn saving_over_a_directory_is_refused() {
    let fx = Fixture::default().await;
    fx.dir("/folder");
    assert_eq!(
        fx.session.save_text("/folder", "x", LF, None).await.unwrap_err(),
        SftpError::IsADirectory { path: "/folder".into() }
    );
}

// ---- directories that do not allow new files ---------------------------------------------------

#[tokio::test]
async fn without_room_for_a_temporary_file_the_target_is_rewritten_in_place() {
    for posix in [true, false] {
        let fx = Fixture::start(SftpOptions { dir_unwritable: true, posix_rename: posix, ..Default::default() }).await;
        fx.file("/ro/conf.txt", "old\n", 0o640);
        let inode = std::fs::metadata(fx.host("/ro/conf.txt")).unwrap().ino();
        let opened = fx.session.open_text("/ro/conf.txt").await.unwrap();
        let (revision, write) = saved(
            fx.session
                .save_text("/ro/conf.txt", "new content that is longer\n", LF, Some(opened.revision))
                .await
                .unwrap(),
        );
        assert_eq!(write, WriteMode::InPlace(InPlaceReason::DirNotWritable), "posix {posix}");
        assert_eq!(fx.text("/ro/conf.txt"), "new content that is longer\n");
        assert_eq!(std::fs::metadata(fx.host("/ro/conf.txt")).unwrap().ino(), inode, "the same file, rewritten");
        assert_eq!(fx.mode("/ro/conf.txt"), 0o640);
        assert_eq!(fx.names("/ro"), ["conf.txt"]);
        assert_eq!(revision, fx.session.stat("/ro/conf.txt").await.unwrap().revision());
        // A shorter text must not leave the tail of the old one behind.
        saved(fx.session.save_text("/ro/conf.txt", "short\n", LF, None).await.unwrap());
        assert_eq!(fx.text("/ro/conf.txt"), "short\n");
    }
}

#[tokio::test]
async fn a_new_file_in_a_directory_that_does_not_allow_it_is_refused() {
    let fx = Fixture::start(SftpOptions { dir_unwritable: true, ..Default::default() }).await;
    fx.dir("/ro");
    let error = fx.session.save_text("/ro/new.txt", "x", LF, None).await.unwrap_err();
    assert_eq!(error, SftpError::PermissionDenied { path: "/ro/new.txt".into() });
    assert!(fx.names("/ro").is_empty());
}

#[tokio::test]
async fn a_file_that_cannot_be_written_either_is_a_permission_error() {
    if is_root() {
        return;
    }
    let fx = Fixture::start(SftpOptions { dir_unwritable: true, ..Default::default() }).await;
    fx.file("/ro/locked.txt", "old", 0o444);
    let error = fx.session.save_text("/ro/locked.txt", "new", LF, None).await.unwrap_err();
    assert_eq!(error, SftpError::PermissionDenied { path: "/ro/locked.txt".into() });
    assert_eq!(fx.text("/ro/locked.txt"), "old");
}

// ---- symlinks -----------------------------------------------------------------------------------

#[tokio::test]
async fn editing_a_link_edits_its_target_and_keeps_the_link() {
    for (what, fx) in both_servers().await {
        fx.file("/links/real/settings.ini", "a=1\n", 0o640);
        fx.link("/links/settings.ini", "real/settings.ini");
        let opened = fx.session.open_text("/links/settings.ini").await.unwrap();
        assert_eq!(opened.text, "a=1\n", "{what}");
        assert_eq!(
            (opened.path.as_str(), opened.target.as_str()),
            ("/links/settings.ini", "/links/real/settings.ini"),
            "{what}"
        );

        saved(fx.session.save_text("/links/settings.ini", "a=2\n", LF, Some(opened.revision)).await.unwrap());
        assert!(
            std::fs::symlink_metadata(fx.host("/links/settings.ini")).unwrap().file_type().is_symlink(),
            "{what}: still a link"
        );
        assert_eq!(
            std::fs::read_link(fx.host("/links/settings.ini")).unwrap().to_str(),
            Some("real/settings.ini"),
            "{what}"
        );
        assert_eq!(fx.text("/links/real/settings.ini"), "a=2\n", "{what}");
        assert_eq!(fx.mode("/links/real/settings.ini"), 0o640, "{what}");
        assert_eq!(
            fx.names("/links/real"),
            ["settings.ini"],
            "{what}: the temporary file lived next to the target and is gone"
        );
        assert_eq!(fx.names("/links"), ["real", "settings.ini"], "{what}");
    }
}

#[tokio::test]
async fn a_chain_of_links_ends_at_the_real_file() {
    let fx = Fixture::default().await;
    fx.file("/chain/data/final.txt", "end", 0o644);
    fx.link("/chain/one", "two");
    fx.link("/chain/two", "data/final.txt");
    let opened = fx.session.open_text("/chain/one").await.unwrap();
    assert_eq!(opened.target, "/chain/data/final.txt");
    saved(fx.session.save_text("/chain/one", "changed", LF, Some(opened.revision)).await.unwrap());
    assert_eq!(fx.text("/chain/data/final.txt"), "changed");
    assert!(std::fs::symlink_metadata(fx.host("/chain/one")).unwrap().file_type().is_symlink());
    assert!(std::fs::symlink_metadata(fx.host("/chain/two")).unwrap().file_type().is_symlink());
}

#[tokio::test]
async fn a_conflict_through_a_link_looks_at_the_target() {
    let fx = Fixture::default().await;
    fx.file("/lk/target.txt", "v1", 0o644);
    fx.link("/lk/alias.txt", "target.txt");
    let opened = fx.session.open_text("/lk/alias.txt").await.unwrap();
    fx.file("/lk/target.txt", "v2 by someone", 0o644);
    let outcome = fx.session.save_text("/lk/alias.txt", "mine", LF, Some(opened.revision)).await.unwrap();
    assert!(matches!(outcome, SaveOutcome::Conflict { remote_text: Some(ref text), .. } if text == "v2 by someone"));
    assert_eq!(fx.text("/lk/target.txt"), "v2 by someone");
}

#[tokio::test]
async fn a_dangling_link_cannot_be_opened() {
    let fx = Fixture::default().await;
    fx.link("/dead", "nowhere.txt");
    assert!(fx.session.open_text("/dead").await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn the_temporary_name_is_the_documented_one() {
    let fx = Fixture::start(SftpOptions {
        fail_rename_from: Some(".shellmate-edit"),
        posix_rename: false,
        ..Default::default()
    })
    .await;
    fx.file("/n/doc.txt", "x", 0o644);
    assert_eq!(save_temp_path("/n/doc.txt"), "/n/.doc.txt.shellmate-edit");
    // The failure injection triggers on exactly that name: the save used it.
    assert!(fx.session.save_text("/n/doc.txt", "y", LF, None).await.is_err());
    assert_eq!(fx.text("/n/doc.txt"), "x");
}

// ---- files we may not write -----------------------------------------------------------------------

#[tokio::test]
async fn a_read_only_file_is_not_replaced_by_renaming_over_it() {
    if is_root() {
        return;
    }
    for (what, fx) in both_servers().await {
        fx.file("/w/readonly.txt", "keep me\n", 0o444);
        let opened = fx.session.open_text("/w/readonly.txt").await.unwrap();
        for expected in [Some(opened.revision), None] {
            let error = fx.session.save_text("/w/readonly.txt", "overwritten\n", LF, expected).await.unwrap_err();
            assert_eq!(error, SftpError::PermissionDenied { path: "/w/readonly.txt".into() }, "{what}");
            assert_eq!(fx.text("/w/readonly.txt"), "keep me\n", "{what}");
            assert_eq!(fx.mode("/w/readonly.txt"), 0o444, "{what}");
            assert_eq!(fx.names("/w"), ["readonly.txt"], "{what}: nothing was written, no temporary file");
        }
    }
}

#[tokio::test]
async fn ensure_writable_checks_without_changing_anything() {
    if is_root() {
        return;
    }
    let fx = Fixture::default().await;
    fx.file("/e/ok.txt", "content", 0o644);
    fx.file("/e/locked.txt", "content", 0o444);
    let before = std::fs::metadata(fx.host("/e/ok.txt")).unwrap();
    fx.session.ensure_writable("/e/ok.txt").await.unwrap();
    let after = std::fs::metadata(fx.host("/e/ok.txt")).unwrap();
    assert_eq!((before.len(), before.mtime()), (after.len(), after.mtime()), "not truncated, not touched");
    assert_eq!(fx.text("/e/ok.txt"), "content");
    assert_eq!(
        fx.session.ensure_writable("/e/locked.txt").await.unwrap_err(),
        SftpError::PermissionDenied { path: "/e/locked.txt".into() }
    );
    assert!(fx.session.ensure_writable("/e/ghost.txt").await.unwrap_err().is_not_found());
    assert_eq!(fx.names("/e"), ["locked.txt", "ok.txt"], "and nothing was created");
}

// ---- keeping the owner ----------------------------------------------------------------------------

fn process_ids(fx: &Fixture) -> (u32, u32) {
    let meta = std::fs::metadata(fx.host("/")).unwrap();
    (meta.uid(), meta.gid())
}

#[tokio::test]
async fn a_file_owned_by_somebody_else_is_rewritten_in_place_to_keep_its_owner() {
    for (what, posix, other_user, other_group) in [
        ("other user, posix-rename", true, true, false),
        ("other user, backup swap", false, true, false),
        ("other group", true, false, true),
    ] {
        // The server reports `owned.txt` as belonging to somebody else and refuses chown, like sftp-server for non-root.
        let probe = Fixture::default().await;
        let (uid, gid) = process_ids(&probe);
        let fake = ("owned.txt", uid + other_user as u32, gid + other_group as u32);
        let fx = Fixture::start(SftpOptions {
            posix_rename: posix,
            deny_chown: true,
            fake_owner: Some(fake),
            ..Default::default()
        })
        .await;
        fx.file("/o/owned.txt", "old\n", 0o664);
        let inode = std::fs::metadata(fx.host("/o/owned.txt")).unwrap().ino();
        let opened = fx.session.open_text("/o/owned.txt").await.unwrap();
        let outcome =
            fx.session.save_text("/o/owned.txt", "new text that is longer\n", LF, Some(opened.revision)).await.unwrap();
        let (revision, write) = saved(outcome);
        assert_eq!(write, WriteMode::InPlace(InPlaceReason::KeepOwner), "{what}");
        assert_eq!(fx.text("/o/owned.txt"), "new text that is longer\n", "{what}");
        assert_eq!(
            std::fs::metadata(fx.host("/o/owned.txt")).unwrap().ino(),
            inode,
            "{what}: the very same file, so the same owner"
        );
        assert_eq!(fx.mode("/o/owned.txt"), 0o664, "{what}");
        assert_eq!(fx.names("/o"), ["owned.txt"], "{what}: the unused temporary file was removed");
        assert_eq!(revision, fx.session.stat("/o/owned.txt").await.unwrap().revision(), "{what}");
        // A shorter text leaves nothing of the old one.
        saved(fx.session.save_text("/o/owned.txt", "s\n", LF, Some(revision)).await.unwrap());
        assert_eq!(fx.text("/o/owned.txt"), "s\n", "{what}");
    }
}

#[tokio::test]
async fn the_owner_check_does_not_apply_to_files_whose_owner_is_unknown_or_ours() {
    // Ours: the new file already has the owner (the chown is refused, and not needed).
    let fx = Fixture::start(SftpOptions { deny_chown: true, ..Default::default() }).await;
    fx.file("/k/mine.txt", "x", 0o644);
    let (_, write) = saved(fx.session.save_text("/k/mine.txt", "y", LF, None).await.unwrap());
    assert_eq!(write, WriteMode::Atomic);
    // A new file has no original to keep.
    let (_, write) = saved(fx.session.save_text("/k/brand-new.txt", "y", LF, None).await.unwrap());
    assert_eq!(write, WriteMode::Atomic);
}

// ---- the temporary file is created exclusively -----------------------------------------------------

#[tokio::test]
async fn a_symlink_planted_at_the_temporary_name_does_not_redirect_the_write() {
    for (what, fx) in both_servers().await {
        fx.file("/p/doc.txt", "mine\n", 0o644);
        fx.file("/p/victim.txt", "victim content", 0o644);
        fx.link("/p/.doc.txt.shellmate-edit", "victim.txt");
        let (_, write) = saved(fx.session.save_text("/p/doc.txt", "saved\n", LF, None).await.unwrap());
        assert_eq!(write, WriteMode::Atomic, "{what}: the save still lands, atomically");
        assert_eq!(fx.text("/p/doc.txt"), "saved\n", "{what}");
        assert_eq!(fx.text("/p/victim.txt"), "victim content", "{what}: the link was not followed");
        assert_eq!(fx.names("/p"), ["doc.txt", "victim.txt"], "{what}");
    }
}

#[tokio::test]
async fn a_dangling_link_at_the_temporary_name_is_not_followed_either() {
    let fx = Fixture::default().await;
    fx.file("/q/doc.txt", "mine\n", 0o644);
    fx.link("/q/.doc.txt.shellmate-edit", "created-through-the-link.txt");
    saved(fx.session.save_text("/q/doc.txt", "saved\n", LF, None).await.unwrap());
    assert_eq!(fx.names("/q"), ["doc.txt"], "nothing was created at the link's destination");
}

#[tokio::test]
async fn a_stale_temporary_file_is_cleared_away() {
    for (what, fx) in both_servers().await {
        fx.file("/s2/doc.txt", "mine\n", 0o600);
        fx.file("/s2/.doc.txt.shellmate-edit", "leftover of a crashed save", 0o644);
        let (_, write) = saved(fx.session.save_text("/s2/doc.txt", "saved\n", LF, None).await.unwrap());
        assert_eq!(
            (write, fx.text("/s2/doc.txt").as_str(), fx.mode("/s2/doc.txt")),
            (WriteMode::Atomic, "saved\n", 0o600),
            "{what}"
        );
        assert_eq!(fx.names("/s2"), ["doc.txt"], "{what}");
    }
}

#[tokio::test]
async fn when_the_temporary_name_cannot_be_cleared_the_save_goes_in_place_and_leaves_it_alone() {
    // A directory that refuses to unlink (a sticky directory with somebody else's file): the name
    // stays taken, and the file that is not ours is not touched.
    let fx = Fixture::start(SftpOptions { dir_unwritable: true, ..Default::default() }).await;
    fx.file("/t/doc.txt", "mine\n", 0o644);
    fx.file("/t/.doc.txt.shellmate-edit", "somebody else's file", 0o644);
    let (_, write) = saved(fx.session.save_text("/t/doc.txt", "saved\n", LF, None).await.unwrap());
    assert_eq!(write, WriteMode::InPlace(InPlaceReason::DirNotWritable));
    assert_eq!(fx.text("/t/doc.txt"), "saved\n");
    assert_eq!(fx.text("/t/.doc.txt.shellmate-edit"), "somebody else's file");
}

// ---- dangling links ---------------------------------------------------------------------------------

#[tokio::test]
async fn overwriting_through_a_dangling_link_creates_the_file_where_it_points() {
    for (what, fx) in both_servers().await {
        fx.dir("/dl/real");
        fx.link("/dl/link.txt", "real/new.txt");
        let (_, write) = saved(fx.session.save_text("/dl/link.txt", "created\n", LF, None).await.unwrap());
        assert_eq!(write, WriteMode::Atomic, "{what}");
        assert!(
            std::fs::symlink_metadata(fx.host("/dl/link.txt")).unwrap().file_type().is_symlink(),
            "{what}: still a link"
        );
        assert_eq!(std::fs::read_link(fx.host("/dl/link.txt")).unwrap().to_str(), Some("real/new.txt"), "{what}");
        assert_eq!(fx.text("/dl/real/new.txt"), "created\n", "{what}");
        assert_eq!(fx.names("/dl"), ["link.txt", "real"], "{what}");
        assert_eq!(fx.names("/dl/real"), ["new.txt"], "{what}");
        // And it opens through the link now.
        assert_eq!(fx.session.open_text("/dl/link.txt").await.unwrap().text, "created\n", "{what}");
    }
}

#[tokio::test]
async fn each_hop_of_a_dangling_chain_is_relative_to_its_own_link() {
    let fx = Fixture::default().await;
    fx.dir("/chain/links");
    fx.dir("/chain/targets");
    fx.link("/chain/entry", "links/hop");
    fx.link("/chain/links/hop", "../targets/final.txt");
    saved(fx.session.save_text("/chain/entry", "end of the chain\n", LF, None).await.unwrap());
    assert_eq!(fx.text("/chain/targets/final.txt"), "end of the chain\n");
    for link in ["/chain/entry", "/chain/links/hop"] {
        assert!(std::fs::symlink_metadata(fx.host(link)).unwrap().file_type().is_symlink(), "{link}");
    }
    assert_eq!(fx.names("/chain/links"), ["hop"]);
}

#[tokio::test]
async fn a_loop_of_links_is_an_error_and_nothing_is_written() {
    let fx = Fixture::default().await;
    fx.dir("/loop");
    fx.link("/loop/a", "b");
    fx.link("/loop/b", "a");
    let error = fx.session.save_text("/loop/a", "x", LF, None).await.unwrap_err();
    assert!(matches!(error, SftpError::Failed { ref message, .. } if message.contains("层数过多")), "{error:?}");
    assert_eq!(fx.names("/loop"), ["a", "b"]);
    for link in ["/loop/a", "/loop/b"] {
        assert!(std::fs::symlink_metadata(fx.host(link)).unwrap().file_type().is_symlink());
    }
}

#[tokio::test]
async fn a_dangling_link_into_a_missing_directory_is_an_error_and_the_link_survives() {
    let fx = Fixture::default().await;
    fx.dir("/md");
    fx.link("/md/link.txt", "no-such-dir/x.txt");
    let error = fx.session.save_text("/md/link.txt", "x", LF, None).await.unwrap_err();
    assert_eq!(error, SftpError::NotFound { path: "/md/no-such-dir/x.txt".into() });
    assert!(std::fs::symlink_metadata(fx.host("/md/link.txt")).unwrap().file_type().is_symlink());
    assert_eq!(fx.names("/md"), ["link.txt"]);
}

#[tokio::test]
async fn with_an_expectation_a_dangling_link_is_just_not_found() {
    let fx = Fixture::default().await;
    fx.dir("/ex");
    fx.link("/ex/link.txt", "gone.txt");
    let revision = Revision { size: 1, mtime: Some(1) };
    let error = fx.session.save_text("/ex/link.txt", "x", LF, Some(revision)).await.unwrap_err();
    assert!(error.is_not_found(), "{error:?}");
    assert_eq!(fx.names("/ex"), ["link.txt"], "nothing was created");
}
