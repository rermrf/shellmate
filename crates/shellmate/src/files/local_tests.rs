use super::*;
use crate::files::local_copy::CopyPlan;
use crate::files::model::{FilesModel, LinkStatus};
use shellmate_sftp::transfer::Choice;
use std::os::unix::fs::symlink;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

fn fixture() -> (tempfile::TempDir, LocalFs) {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = LocalFs::new(dir.path().to_owned());
    fs.trash = Box::new(|_| panic!("unexpected trash operation"));
    (dir, fs)
}

#[test]
fn listing_includes_hidden_fields_sorted_names_and_symlink_targets() {
    let (dir, backend) = fixture();
    let root = dir.path();
    fs::create_dir(root.join("docs")).unwrap();
    symlink("docs", root.join("shortcut")).unwrap();
    for name in ["file10", "file2", ".hidden"] {
        fs::write(root.join(name), b"123").unwrap();
    }
    let file = File::open(root.join("file2")).unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o640)).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        .unwrap();
    let meta = file.metadata().unwrap();
    let list = backend.listing(".");
    assert_eq!(list.dir.as_deref(), root.to_str());
    let entries = list.entries.unwrap();
    assert_eq!(
        entries.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(),
        ["docs", "shortcut", ".hidden", "file2", "file10"]
    );
    assert_eq!(
        entries[1].link,
        Some(Link {
            target: "docs".into(),
            resolved: Some(EntryKind::Dir)
        })
    );
    let entry = &entries[3];
    assert_eq!(
        (entry.mode, entry.modified, entry.size),
        (Some(0o640), Some(1_700_000_000), 3)
    );
    assert_eq!((entry.uid, entry.gid), (Some(meta.uid()), Some(meta.gid())));
    assert!(!entry.owner.is_empty() && !entry.group.is_empty());
}

#[test]
fn listing_errors_use_shared_messages() {
    let (dir, backend) = fixture();
    let file = dir.path().join("not-a-directory");
    let path = file.to_str().unwrap();
    assert_eq!(
        backend.listing(path).entries.unwrap_err().to_string(),
        format!("不存在：{path}")
    );
    fs::write(&file, b"").unwrap();
    assert_eq!(
        backend.listing(path).entries.unwrap_err().to_string(),
        format!("不是目录：{path}")
    );
    assert_eq!(
        io_error(&file, io::Error::from_raw_os_error(libc::EACCES)).to_string(),
        format!("权限不足：{path}")
    );
}

#[test]
fn mkdir_unique_rename_no_clobber_and_chmod() {
    let (dir, backend) = fixture();
    let root = dir.path().to_str().unwrap();
    let first = backend.create_dir_unique(root, "Folder").unwrap();
    let second = backend.create_dir_unique(root, "Folder").unwrap();
    assert_eq!(second, format!("{root}/Folder (1)"));
    assert!(matches!(
        backend.rename(&first, &second),
        Err(SftpError::AlreadyExists { .. })
    ));
    let new = format!("{root}/renamed");
    backend.rename(&first, &new).unwrap();
    assert!(!Path::new(&first).exists());
    backend.chmod(&new, 0o750).unwrap();
    assert_eq!(fs::metadata(new).unwrap().mode() & 0o777, 0o750);
}

#[test]
fn trash_receives_only_selected_paths_and_failure_never_unlinks() {
    let (dir, mut backend) = fixture();
    let mut model = FilesModel::new();
    for name in ["one", "two", "untouched"] {
        fs::write(dir.path().join(name), name).unwrap();
    }
    let request = model.set_link(LinkStatus::Ready).unwrap();
    model.apply_listed(request.id, backend.listing("."));
    model.select_row(1);
    model.toggle_row(2);
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    backend.trash = Box::new(move |path| {
        sink.lock().unwrap().push(path.to_owned());
        if path.ends_with("two") {
            Err(io_error(path, io::ErrorKind::PermissionDenied.into()))
        } else {
            Ok(())
        }
    });
    assert!(matches!(
        backend.trash_selected(&model.selected_paths()),
        Err(SftpError::PermissionDenied { .. })
    ));
    assert_eq!(
        *received.lock().unwrap(),
        [dir.path().join("one"), dir.path().join("two")]
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
}

#[test]
fn finder_copy_skip_keep_both_and_replace() {
    let (dir, _) = fixture();
    let target = dir.path().join("target");
    fs::create_dir(&target).unwrap();
    let source = dir.path().join("a.txt");
    fs::write(&source, b"new").unwrap();
    let old = target.join("a.txt");
    fs::write(&old, b"old").unwrap();
    let plan = || CopyPlan::new(vec![source.clone()], target.display().to_string()).unwrap();
    assert_eq!(plan().clash.names, ["a.txt"]);
    assert_eq!(plan().execute(Choice::Skip).unwrap(), 0);
    assert_eq!(plan().execute(Choice::KeepBoth).unwrap(), 1);
    assert_eq!(fs::read(&old).unwrap(), b"old");
    assert_eq!(fs::read(target.join("a (1).txt")).unwrap(), b"new");
    assert_eq!(plan().execute(Choice::Overwrite).unwrap(), 1);
    assert_eq!(fs::read(&old).unwrap(), b"new");
    assert_eq!(fs::read(source).unwrap(), b"new");
}

#[test]
fn finder_copy_recurses_without_following_links_or_copying_into_itself() {
    let (dir, _) = fixture();
    let root = dir.path().join("source");
    let dest = dir.path().join("dest");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::create_dir(&dest).unwrap();
    fs::write(root.join("nested/doc"), b"hi").unwrap();
    symlink("..", root.join("nested/up")).unwrap();
    let plan = |dir: &Path| CopyPlan::new(vec![root.clone()], dir.display().to_string()).unwrap();
    plan(&dest).execute(Choice::Skip).unwrap();
    assert_eq!(fs::read(dest.join("source/nested/doc")).unwrap(), b"hi");
    assert_eq!(fs::read_link(dest.join("source/nested/up")).unwrap(), Path::new(".."));
    assert!(plan(&root.join("nested")).execute(Choice::KeepBoth).is_err());
    symlink(&root, dest.join("alias")).unwrap();
    assert!(plan(&dest.join("alias/nested")).execute(Choice::Overwrite).is_err());
}

#[test]
fn editor_atomic_save_preserves_mode_link_bom_and_detects_same_size_conflict() {
    let (dir, backend) = fixture();
    let file = dir.path().join("notes");
    fs::write(&file, b"\xef\xbb\xbfabc\r\n").unwrap();
    let time = UNIX_EPOCH + Duration::from_secs(1_700_000_001);
    File::open(&file).unwrap().set_modified(time).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    let alias = dir.path().join("alias");
    symlink("notes", &alias).unwrap();
    let path = alias.to_str().unwrap();
    let opened = backend.open_text(path).unwrap();
    assert_eq!(opened.text, "abc\n");
    let format = SaveFormat::from(&opened);
    let old = File::open(&file).unwrap();
    fs::write(&file, b"\xef\xbb\xbfext\r\n").unwrap();
    old.set_modified(time + Duration::from_millis(5)).unwrap();
    let conflict = backend.save_text(path, "mine\n", format, opened.revision).unwrap();
    let SaveOutcome::Conflict {
        remote_revision,
        remote_text,
        ..
    } = conflict
    else {
        panic!("expected a conflict")
    };
    assert_eq!(remote_text.as_deref(), Some("ext\n"));
    let saved = backend.save_text(path, "mine\n", format, remote_revision).unwrap();
    assert!(matches!(
        saved,
        SaveOutcome::Saved {
            mode: WriteMode::Atomic,
            ..
        }
    ));
    assert_eq!(fs::read_link(alias).unwrap(), Path::new("notes"));
    assert_eq!(fs::read(&file).unwrap(), b"\xef\xbb\xbfmine\r\n");
    let new = fs::metadata(&file).unwrap();
    assert_ne!(old.metadata().unwrap().ino(), new.ino());
    assert_eq!(new.mode() & 0o7777, 0o640);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn editor_rejects_oversize_binary_and_invalid_encoding() {
    let (dir, backend) = fixture();
    let file = dir.path().join("refused");
    let path = file.to_str().unwrap();
    File::create(&file).unwrap().set_len(MAX_TEXT_SIZE + 1).unwrap();
    assert!(matches!(backend.open_text(path), Err(SftpError::TooLarge { .. })));
    for (bytes, error) in [(b"\0".as_slice(), SftpError::Binary), (&[0xff], SftpError::NotUtf8)] {
        fs::write(&file, bytes).unwrap();
        assert_eq!(backend.open_text(path), Err(error));
    }
}
