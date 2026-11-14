//! Creating, renaming, deleting and changing permissions.

use shellmate_sftp::SftpError;

use crate::fixture::{Fixture, SftpOptions};

#[tokio::test]
async fn mkdir_creates_a_directory_and_refuses_what_is_there() {
    let fx = Fixture::default().await;
    fx.session.mkdir("/new").await.unwrap();
    assert!(fx.host("/new").is_dir());
    assert_eq!(fx.session.mkdir("/new").await.unwrap_err(), SftpError::AlreadyExists { path: "/new".into() });
    fx.file("/file", "x", 0o644);
    assert_eq!(fx.session.mkdir("/file").await.unwrap_err(), SftpError::AlreadyExists { path: "/file".into() });
    assert!(fx.session.mkdir("/no/parent/here").await.unwrap_err().is_not_found());
    fx.session.mkdir("/新建 文件夹").await.unwrap();
    assert!(fx.host("/新建 文件夹").is_dir());
}

#[tokio::test]
async fn a_new_folder_takes_the_first_free_name() {
    let fx = Fixture::default().await;
    fx.dir("/work");
    assert_eq!(fx.session.create_dir_unique("/work", "新建文件夹").await.unwrap(), "/work/新建文件夹");
    assert_eq!(fx.session.create_dir_unique("/work", "新建文件夹").await.unwrap(), "/work/新建文件夹 (1)");
    assert_eq!(fx.session.create_dir_unique("/work", "新建文件夹").await.unwrap(), "/work/新建文件夹 (2)");
    assert_eq!(fx.names("/work"), ["新建文件夹", "新建文件夹 (1)", "新建文件夹 (2)"]);
    // A gap is filled, and a file or a link in the way counts as taken like a folder does.
    fx.session.remove("/work/新建文件夹 (1)").await.unwrap();
    assert_eq!(fx.session.create_dir_unique("/work", "新建文件夹").await.unwrap(), "/work/新建文件夹 (1)");
    fx.file("/work/f", "x", 0o644);
    fx.link("/work/f (1)", "f");
    assert_eq!(fx.session.create_dir_unique("/work", "f").await.unwrap(), "/work/f (2)");
    // At the root, and in a directory that is not there.
    assert_eq!(fx.session.create_dir_unique("/", "top").await.unwrap(), "/top");
    assert!(fx.session.create_dir_unique("/nowhere", "x").await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn rename_moves_files_and_directories() {
    let fx = Fixture::default().await;
    fx.file("/a/one.txt", "1", 0o644);
    fx.dir("/a/folder");
    fx.file("/a/folder/inner.txt", "i", 0o644);
    fx.session.rename("/a/one.txt", "/a/uno.txt").await.unwrap();
    fx.session.rename("/a/folder", "/a/dossier").await.unwrap();
    assert_eq!(fx.names("/a"), ["dossier", "uno.txt"]);
    assert_eq!(fx.text("/a/dossier/inner.txt"), "i");
    fx.session.rename("/a/uno.txt", "/a/dossier/moved.txt").await.unwrap();
    assert!(fx.exists("/a/dossier/moved.txt") && !fx.exists("/a/uno.txt"));
    fx.session.rename("/a/dossier", "/a/dossier").await.unwrap();
}

#[tokio::test]
async fn rename_never_overwrites() {
    // The check is the client's: it must hold on a server that would silently replace, too.
    let fx = Fixture::start(SftpOptions { plain_rename_replaces: true, ..Default::default() }).await;
    fx.file("/b/src.txt", "source", 0o644);
    fx.file("/b/dst.txt", "destination", 0o644);
    fx.dir("/b/dir");
    for target in ["/b/dst.txt", "/b/dir"] {
        assert_eq!(
            fx.session.rename("/b/src.txt", target).await.unwrap_err(),
            SftpError::AlreadyExists { path: target.into() }
        );
    }
    assert_eq!(fx.text("/b/src.txt"), "source");
    assert_eq!(fx.text("/b/dst.txt"), "destination");
    fx.link("/b/link", "src.txt");
    assert!(
        matches!(fx.session.rename("/b/src.txt", "/b/link").await, Err(SftpError::AlreadyExists { .. })),
        "an existing link counts"
    );
    assert!(fx.session.rename("/b/ghost", "/b/other").await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn remove_deletes_files_links_and_empty_directories() {
    let fx = Fixture::default().await;
    fx.file("/c/file.txt", "x", 0o644);
    fx.link("/c/link", "file.txt");
    fx.dir("/c/empty");
    fx.session.remove("/c/link").await.unwrap();
    assert!(fx.exists("/c/file.txt"), "deleting a link leaves its target");
    assert!(!fx.exists("/c/link"));
    fx.session.remove("/c/file.txt").await.unwrap();
    fx.session.remove("/c/empty").await.unwrap();
    assert!(fx.names("/c").is_empty());
    assert!(fx.session.remove("/c/ghost").await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn remove_deletes_directories_recursively_without_following_links() {
    let fx = Fixture::default().await;
    fx.file("/keep/precious.txt", "keep me", 0o644);
    for i in 0..12 {
        fx.file(&format!("/tree/sub/deep/f{i}.txt"), "x", 0o644);
    }
    fx.file("/tree/top.txt", "x", 0o444);
    fx.file("/tree/.hidden", "x", 0o644);
    fx.link("/tree/sub/to-keep", "../../keep");
    fx.link("/tree/to-file", "top.txt");
    fx.session.remove("/tree/").await.unwrap();
    assert!(!fx.exists("/tree"));
    assert_eq!(fx.text("/keep/precious.txt"), "keep me", "a link inside the tree is removed, not followed");
}

#[tokio::test]
async fn remove_refuses_the_root_and_the_working_directory() {
    let fx = Fixture::default().await;
    fx.file("/safe.txt", "x", 0o644);
    for path in ["/", "//", "/.", "/a/..", ".", ""] {
        assert!(matches!(fx.session.remove(path).await, Err(SftpError::Refused { .. })), "{path:?}");
    }
    assert!(fx.exists("/safe.txt"));
}

#[tokio::test]
async fn chmod_sets_the_permission_bits() {
    let fx = Fixture::default().await;
    fx.file("/p/script.sh", "#!/bin/sh", 0o644);
    fx.dir("/p/dir");
    fx.session.chmod("/p/script.sh", 0o755).await.unwrap();
    assert_eq!(fx.mode("/p/script.sh"), 0o755);
    fx.session.chmod("/p/script.sh", 0o600).await.unwrap();
    assert_eq!(fx.mode("/p/script.sh"), 0o600);
    fx.session.chmod("/p/dir", 0o700).await.unwrap();
    assert_eq!(fx.mode("/p/dir"), 0o700);
    // File type bits in the argument are not permissions.
    fx.session.chmod("/p/script.sh", 0o100640).await.unwrap();
    assert_eq!(fx.mode("/p/script.sh"), 0o640);
    // A link changes its target.
    fx.link("/p/link", "script.sh");
    fx.session.chmod("/p/link", 0o664).await.unwrap();
    assert_eq!(fx.mode("/p/script.sh"), 0o664);
    assert!(fx.session.chmod("/p/ghost", 0o644).await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn a_directory_that_does_not_allow_changes_gives_permission_denied() {
    let fx = Fixture::start(SftpOptions { dir_unwritable: true, ..Default::default() }).await;
    fx.file("/ro/file.txt", "x", 0o644);
    assert_eq!(fx.session.mkdir("/ro/new").await.unwrap_err(), SftpError::PermissionDenied { path: "/ro/new".into() });
    assert_eq!(
        fx.session.remove("/ro/file.txt").await.unwrap_err(),
        SftpError::PermissionDenied { path: "/ro/file.txt".into() }
    );
    assert!(matches!(fx.session.rename("/ro/file.txt", "/ro/other").await, Err(SftpError::PermissionDenied { .. })));
    assert!(fx.exists("/ro/file.txt"));
    assert_eq!(fx.session.remove("/ro/file.txt").await.unwrap_err().to_string(), "权限不足：/ro/file.txt");
}
