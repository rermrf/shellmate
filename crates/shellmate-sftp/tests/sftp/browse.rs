//! Listing, metadata and paths.

use std::os::unix::fs::MetadataExt as _;

use shellmate_sftp::{EntryKind, SftpError};

use crate::fixture::{Fixture, Longnames, SftpOptions};

fn names(entries: &[shellmate_sftp::Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

#[tokio::test]
async fn the_session_starts_in_the_home_directory() {
    let fx = Fixture::default().await;
    assert_eq!(fx.session.home().await.unwrap(), "/home/tester");
    assert_eq!(fx.session.canonicalize("/home/tester/../tester/.").await.unwrap(), "/home/tester");
    assert_eq!(fx.session.canonicalize("/").await.unwrap(), "/");
    assert_eq!(fx.session.canonicalize("/../..").await.unwrap(), "/", "the root has no parent");
}

#[tokio::test]
async fn the_server_extensions_are_reported() {
    let with = Fixture::default().await;
    assert!(with.session.info().posix_rename());
    assert!(with.session.info().has("fsync@openssh.com"));
    assert_eq!(with.session.info().version, 3);
    let without = Fixture::start(SftpOptions { posix_rename: false, ..Default::default() }).await;
    assert!(!without.session.info().posix_rename());
    assert!(!without.session.info().has("posix-rename@openssh.com"));
}

#[tokio::test]
async fn directories_come_first_then_names_in_natural_order() {
    let fx = Fixture::default().await;
    for name in ["b.txt", "a.txt", "file10", "file2", "File1", ".hidden", "Zeta.md", "10.txt", "9.txt"] {
        fx.file(&format!("/data/{name}"), "x", 0o644);
    }
    fx.dir("/data/Zdir");
    fx.dir("/data/adir");
    fx.dir("/data/.config");
    let entries = fx.session.list("/data").await.unwrap();
    // 12 entries come in three replies of seven: the paging is put together.
    assert_eq!(
        names(&entries),
        [
            ".config", "adir", "Zdir", ".hidden", "9.txt", "10.txt", "a.txt", "b.txt", "File1", "file2", "file10",
            "Zeta.md"
        ]
    );
    assert!(entries.iter().take(3).all(|e| e.is_dir()));
    assert!(entries.iter().all(|e| e.name != "." && e.name != ".."), "no dot entries");
}

#[tokio::test]
async fn entries_carry_type_size_time_permissions_and_owner() {
    let fx = Fixture::default().await;
    fx.file("/docs/notes.txt", "twelve bytes", 0o640);
    fx.dir("/docs/sub");
    let entries = fx.session.list("/docs").await.unwrap();
    let notes = entries.iter().find(|e| e.name == "notes.txt").unwrap();
    assert_eq!(notes.path, "/docs/notes.txt");
    assert_eq!((notes.kind, notes.size, notes.mode), (EntryKind::File, 12, Some(0o640)));
    assert_eq!((notes.owner.as_str(), notes.group.as_str()), ("tester", "staff"), "names from the longname");
    assert_eq!(notes.owner_display(), "tester:staff");
    let host_meta = std::fs::metadata(fx.host("/docs/notes.txt")).unwrap();
    assert_eq!(notes.modified, Some(host_meta.mtime()));
    assert_eq!((notes.uid, notes.gid), (Some(host_meta.uid()), Some(host_meta.gid())));
    let sub = entries.iter().find(|e| e.name == "sub").unwrap();
    assert_eq!((sub.kind, sub.mode), (EntryKind::Dir, Some(0o777 & !current_umask())));
}

fn current_umask() -> u32 {
    // The test process's umask shows in the mode of a directory it makes.
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("probe");
    std::fs::create_dir(&probe).unwrap();
    0o777 & !std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&probe).unwrap().permissions()) & 0o777
}

#[tokio::test]
async fn without_names_in_the_longname_the_ids_are_shown() {
    for style in [Longnames::Missing, Longnames::Numeric] {
        let fx = Fixture::start(SftpOptions { longnames: style, ..Default::default() }).await;
        fx.file("/a.txt", "x", 0o644);
        let meta = std::fs::metadata(fx.host("/a.txt")).unwrap();
        let entries = fx.session.list("/").await.unwrap();
        let a = entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert_eq!(a.owner_display(), format!("{}:{}", meta.uid(), meta.gid()), "{style:?}");
    }
}

#[tokio::test]
async fn links_show_where_they_point_and_links_to_directories_sort_with_directories() {
    let fx = Fixture::default().await;
    fx.file("/d/real.txt", "x", 0o644);
    fx.dir("/d/folder");
    fx.link("/d/to-file", "real.txt");
    fx.link("/d/to-dir", "folder");
    fx.link("/d/dangling", "no/such/thing");
    fx.link("/d/up", "../d/real.txt");
    let entries = fx.session.list("/d").await.unwrap();
    assert_eq!(names(&entries), ["folder", "to-dir", "dangling", "real.txt", "to-file", "up"]);
    let link = |name: &str| entries.iter().find(|e| e.name == name).unwrap();

    assert_eq!(link("to-file").kind, EntryKind::Symlink);
    let to_file = link("to-file").link.as_ref().unwrap();
    assert_eq!((to_file.target.as_str(), to_file.resolved), ("real.txt", Some(EntryKind::File)));
    assert!(!link("to-file").opens_as_dir());

    assert_eq!(link("to-dir").link.as_ref().unwrap().resolved, Some(EntryKind::Dir));
    assert!(link("to-dir").opens_as_dir() && !link("to-dir").is_dir());

    assert!(link("dangling").is_dangling());
    assert_eq!(link("dangling").link.as_ref().unwrap().target, "no/such/thing");
    assert_eq!(link("up").link.as_ref().unwrap().target, "../d/real.txt");
    assert!(link("real.txt").link.is_none());
}

#[tokio::test]
async fn names_with_spaces_and_chinese_characters_survive() {
    let fx = Fixture::default().await;
    fx.file("/文档/报告 (终稿) v2.txt", "内容", 0o644);
    fx.dir("/文档/子目录 一");
    let entries = fx.session.list("/文档").await.unwrap();
    assert_eq!(names(&entries), ["子目录 一", "报告 (终稿) v2.txt"]);
    assert_eq!(entries[1].path, "/文档/报告 (终稿) v2.txt");
    assert_eq!(entries[1].size, "内容".len() as u64);
}

#[tokio::test]
async fn listing_a_bad_path_says_what_is_wrong() {
    let fx = Fixture::default().await;
    fx.file("/plain.txt", "x", 0o644);
    assert_eq!(fx.session.list("/missing").await.unwrap_err(), SftpError::NotFound { path: "/missing".into() });
    assert_eq!(
        fx.session.list("/plain.txt").await.unwrap_err(),
        SftpError::NotADirectory { path: "/plain.txt".into() }
    );
    assert!(fx.session.list("/missing").await.unwrap_err().to_string().starts_with("不存在"));

    fx.dir("/locked");
    std::fs::set_permissions(fx.host("/locked"), std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    if std::fs::read_dir(fx.host("/locked")).is_err() {
        assert_eq!(
            fx.session.list("/locked").await.unwrap_err(),
            SftpError::PermissionDenied { path: "/locked".into() }
        );
    }
    std::fs::set_permissions(fx.host("/locked"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn an_empty_directory_lists_as_empty() {
    let fx = Fixture::default().await;
    fx.dir("/empty");
    assert!(fx.session.list("/empty").await.unwrap().is_empty());
}

#[tokio::test]
async fn stat_follows_links_and_lstat_does_not() {
    let fx = Fixture::default().await;
    fx.file("/t/real.txt", "hello", 0o600);
    fx.link("/t/link", "real.txt");
    let followed = fx.session.stat("/t/link").await.unwrap();
    assert_eq!((followed.kind, followed.size, followed.mode), (EntryKind::File, 5, Some(0o600)));
    assert_eq!(followed.name, "link");
    let link = fx.session.lstat("/t/link").await.unwrap();
    assert_eq!(link.kind, EntryKind::Symlink);
    assert_eq!(link.link.unwrap().target, "real.txt");
    let plain = fx.session.lstat("/t/real.txt").await.unwrap();
    assert!(plain.link.is_none());
    assert_eq!(fx.session.stat("/t/missing").await.unwrap_err(), SftpError::NotFound { path: "/t/missing".into() });
    let root = fx.session.stat("/").await.unwrap();
    assert_eq!((root.name.as_str(), root.kind), ("/", EntryKind::Dir));
}

#[tokio::test]
async fn resolve_follows_chains_of_links() {
    let fx = Fixture::default().await;
    fx.file("/r/deep/file.txt", "x", 0o644);
    fx.link("/r/first", "second");
    fx.link("/r/second", "deep/file.txt");
    let (target, entry) = fx.session.resolve("/r/first").await.unwrap();
    assert_eq!(target, "/r/deep/file.txt");
    assert_eq!((entry.kind, entry.size), (EntryKind::File, 1));
    let (same, _) = fx.session.resolve("/r/deep/file.txt").await.unwrap();
    assert_eq!(same, "/r/deep/file.txt");
    fx.link("/r/broken", "nothing");
    assert!(fx.session.resolve("/r/broken").await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn a_dead_connection_is_reported_as_such() {
    let fx = Fixture::default().await;
    fx.dir("/x");
    assert!(fx.session.list("/x").await.is_ok());
    fx.server.state.kill_connections();
    let error = loop {
        match fx.session.list("/x").await {
            Err(error) => break error,
            Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    };
    assert!(error.is_disconnected(), "{error:?}");
    assert!(error.to_string().starts_with("连接已断开"));
    // And it stays that way, quickly.
    let again = fx.session.stat("/x").await.unwrap_err();
    assert!(again.is_disconnected(), "{again:?}");
}
