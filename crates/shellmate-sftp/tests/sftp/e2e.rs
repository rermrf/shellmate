//! Tests against a real OpenSSH server (`#[ignore]`d; see the SSH crate's e2e tests for how to
//! start the container). Everything happens in a scratch directory `sftp-e2e-<random>` in the
//! user's home, which is removed afterwards.
//!
//! Settings (environment): `SHELLMATE_E2E_PORT` (2222), `SHELLMATE_E2E_USER` (tester),
//! `SHELLMATE_E2E_KEY` (path of the private key), `SHELLMATE_E2E_CONTAINER` (shellmate-e2e, used
//! to prepare files, symlinks and permissions with `docker exec`, and to check results).

use std::process::Command;

use shellmate_core::AuthMethod;
use shellmate_sftp::{
    EntryKind, InPlaceReason, LineEnding, SaveFormat, SaveOutcome, SftpError, SftpSession, WriteMode,
};
use shellmate_ssh::{Connection, Route, SshHost};

use crate::support::Env;

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn container() -> String {
    setting("SHELLMATE_E2E_CONTAINER", "shellmate-e2e")
}

/// Runs a shell snippet in the container, as the SFTP user (so files get the owner a real
/// session would give them), and returns what it printed.
fn docker(script: &str) -> String {
    let user = setting("SHELLMATE_E2E_USER", "tester");
    let output = Command::new("docker")
        .args(["exec", "-u", &user, &container(), "sh", "-c", script])
        .output()
        .expect("docker is installed");
    assert!(output.status.success(), "docker exec failed: {script}\n{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Runs a shell snippet in the container as root (to set up files owned by somebody else).
fn docker_root(script: &str) -> String {
    let output = Command::new("docker")
        .args(["exec", "-u", "root", &container(), "sh", "-c", script])
        .output()
        .expect("docker is installed");
    assert!(output.status.success(), "docker exec failed: {script}\n{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A scratch directory in the server user's home; removed when dropped (and only ever that
/// directory: the path must look like one of ours).
struct Scratch {
    path: String,
}

impl Scratch {
    fn new(home: &str) -> Self {
        let id: u64 = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
        let path = format!("{}/sftp-e2e-{id:x}", home.trim_end_matches('/'));
        docker(&format!("mkdir -p '{path}'"));
        Self { path }
    }

    fn at(&self, name: &str) -> String {
        format!("{}/{name}", self.path)
    }

    /// Runs a snippet with `$D` set to the scratch directory.
    fn sh(&self, script: &str) -> String {
        docker(&format!("D='{}'; {script}", self.path))
    }

    /// Like [`sh`](Self::sh), as root.
    fn sh_root(&self, script: &str) -> String {
        docker_root(&format!("D='{}'; {script}", self.path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let name = self.path.rsplit('/').next().unwrap_or_default();
        if name.starts_with("sftp-e2e-") && !self.path.contains("..") {
            let user = setting("SHELLMATE_E2E_USER", "tester");
            let _ = Command::new("docker")
                .args([
                    "exec",
                    "-u",
                    &user,
                    &container(),
                    "sh",
                    "-c",
                    &format!("chmod -R u+rwx '{}' 2>/dev/null; rm -rf '{}'", self.path, self.path),
                ])
                .status();
        }
    }
}

async fn session() -> (SftpSession, Connection, Env) {
    let env = Env::new();
    let port = setting("SHELLMATE_E2E_PORT", "2222").parse().expect("SHELLMATE_E2E_PORT is a port number");
    let key = std::env::var("SHELLMATE_E2E_KEY").expect("set SHELLMATE_E2E_KEY to the private key the server accepts");
    let host = SshHost::new("127.0.0.1", port, setting("SHELLMATE_E2E_USER", "tester"), AuthMethod::PrivateKey)
        .with_identity_file(key);
    let connection = Connection::connect(Route::direct(host), env.trusting()).await.expect("connect");
    let session = SftpSession::open(&connection).await.expect("sftp subsystem");
    (session, connection, env)
}

const LF: SaveFormat = SaveFormat { line_ending: LineEnding::Lf, bom: false };

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_the_server_lists_its_extensions_and_a_directory() {
    let (sftp, _connection, _env) = session().await;
    let info = sftp.info();
    eprintln!("SFTP version {}; extensions advertised by this server:", info.version);
    for (name, version) in &info.extensions {
        eprintln!("  {name} = {version}");
    }
    assert!(info.posix_rename(), "OpenSSH offers posix-rename@openssh.com");

    let home = sftp.home().await.unwrap();
    assert!(home.starts_with('/'));
    let scratch = Scratch::new(&home);
    scratch.sh(
        r#"mkdir "$D/Zdir" "$D/adir" "$D/file dir"; echo hi > "$D/b.txt"; echo hi > "$D/file10"; echo hi > "$D/file2";
           echo hi > "$D/.hidden"; echo hi > "$D/报告 终稿.md"; ln -s b.txt "$D/link-to-file"; ln -s adir "$D/link-to-dir";
           ln -s nowhere "$D/dangling"; chmod 640 "$D/b.txt""#,
    );
    let entries = sftp.list(&scratch.path).await.unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "adir",
            "file dir",
            "link-to-dir",
            "Zdir",
            ".hidden",
            "b.txt",
            "dangling",
            "file2",
            "file10",
            "link-to-file",
            "报告 终稿.md"
        ]
    );
    let b = entries.iter().find(|e| e.name == "b.txt").unwrap();
    assert_eq!((b.kind, b.mode, b.size), (EntryKind::File, Some(0o640), 3));
    let user = setting("SHELLMATE_E2E_USER", "tester");
    eprintln!("owner as listed for b.txt: {:?} {:?} (from the longname)", b.owner, b.group);
    assert_eq!(b.owner, user, "OpenSSH puts user names in the longname");
    assert!(b.uid.is_some_and(|uid| uid > 0), "and the numeric ids are in the attributes");
    assert!(entries.iter().find(|e| e.name == "link-to-dir").unwrap().opens_as_dir());
    assert!(entries.iter().find(|e| e.name == "dangling").unwrap().is_dangling());
    assert_eq!(entries.iter().find(|e| e.name == "link-to-file").unwrap().link.as_ref().unwrap().target, "b.txt");
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_saving_keeps_permissions_owner_and_line_endings() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    let file = scratch.at("notes.txt");
    scratch.sh(r#"printf 'one\r\ntwo\r\n' > "$D/notes.txt"; chmod 640 "$D/notes.txt""#);
    let before = scratch.sh(r#"stat -c '%a %U %G %i' "$D/notes.txt""#);

    let opened = sftp.open_text(&file).await.unwrap();
    assert_eq!((opened.text.as_str(), opened.line_ending, opened.mode), ("one\ntwo\n", LineEnding::CrLf, Some(0o640)));
    let outcome = sftp
        .save_text(&file, "one\ntwo\nthree 中文\n", SaveFormat::from(&opened), Some(opened.revision))
        .await
        .unwrap();
    let SaveOutcome::Saved { revision, mode } = outcome else { panic!("{outcome:?}") };
    assert_eq!(mode, WriteMode::Atomic);

    assert!(scratch.sh(r#"od -c "$D/notes.txt" | head -3"#).contains("\\r  \\n"), "CRLF kept");
    let after = scratch.sh(r#"stat -c '%a %U %G %i' "$D/notes.txt""#);
    let (b, a): (Vec<&str>, Vec<&str>) = (before.split_whitespace().collect(), after.split_whitespace().collect());
    assert_eq!(&a[..3], &b[..3], "mode, owner and group unchanged");
    assert_ne!(a[3], b[3], "replaced by a new file, not rewritten in place");
    assert_eq!(scratch.sh(r#"ls -A "$D""#).trim(), "notes.txt", "no temporary file left");
    assert_eq!(sftp.open_text(&file).await.unwrap().revision, revision);
    assert_eq!(scratch.sh(r#"cat "$D/notes.txt""#), "one\r\ntwo\r\nthree 中文\r\n");

    // Several modes, including ones the server's umask would change on creation.
    for mode in ["600", "664", "755"] {
        scratch.sh(&format!(r#"chmod {mode} "$D/notes.txt""#));
        let current = sftp.open_text(&file).await.unwrap();
        sftp.save_text(&file, "x\n", SaveFormat::from(&current), Some(current.revision)).await.unwrap();
        assert_eq!(scratch.sh(r#"stat -c '%a' "$D/notes.txt""#).trim(), mode);
    }
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_editing_a_symlink_writes_through_to_its_target() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    scratch.sh(r#"mkdir "$D/real"; echo original > "$D/real/config.ini"; chmod 600 "$D/real/config.ini"; ln -s real/config.ini "$D/config.ini""#);
    let link = scratch.at("config.ini");
    let opened = sftp.open_text(&link).await.unwrap();
    assert_eq!(opened.text, "original\n");
    assert_eq!(opened.target, scratch.at("real/config.ini"));
    sftp.save_text(&link, "changed\n", LF, Some(opened.revision)).await.unwrap();
    assert_eq!(scratch.sh(r#"readlink "$D/config.ini""#).trim(), "real/config.ini", "still a link to the same place");
    assert_eq!(scratch.sh(r#"cat "$D/real/config.ini""#), "changed\n");
    assert_eq!(scratch.sh(r#"stat -c '%a' "$D/real/config.ini""#).trim(), "600");
    assert_eq!(
        scratch
            .sh(r#"ls -A "$D" "$D/real" | tr '\n' ' '"#)
            .split_whitespace()
            .filter(|n| n.contains("shellmate"))
            .count(),
        0
    );
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_a_directory_that_does_not_allow_new_files_falls_back_to_in_place() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    scratch.sh(r#"mkdir "$D/ro"; echo old > "$D/ro/file.txt"; chmod 644 "$D/ro/file.txt"; chmod 555 "$D/ro""#);
    let file = scratch.at("ro/file.txt");
    let inode_before = scratch.sh(r#"stat -c '%i' "$D/ro/file.txt""#);
    let opened = sftp.open_text(&file).await.unwrap();
    let outcome = sftp.save_text(&file, "rewritten in place\n", LF, Some(opened.revision)).await.unwrap();
    assert!(
        matches!(outcome, SaveOutcome::Saved { mode: WriteMode::InPlace(InPlaceReason::DirNotWritable), .. }),
        "{outcome:?}"
    );
    assert_eq!(scratch.sh(r#"cat "$D/ro/file.txt""#), "rewritten in place\n");
    assert_eq!(scratch.sh(r#"stat -c '%i' "$D/ro/file.txt""#), inode_before);
    assert_eq!(scratch.sh(r#"ls -A "$D/ro""#).trim(), "file.txt");
    // Making a new file there is refused plainly.
    let error = sftp.save_text(&scratch.at("ro/new.txt"), "x", LF, None).await.unwrap_err();
    assert!(matches!(error, SftpError::PermissionDenied { .. }), "{error:?}");
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_a_directory_without_permission_gives_a_clear_error() {
    let (sftp, _connection, _env) = session().await;
    let error = sftp.list("/root").await.unwrap_err();
    assert_eq!(error, SftpError::PermissionDenied { path: "/root".into() });
    assert_eq!(error.to_string(), "权限不足：/root");
    let missing = sftp.list("/definitely/not/here").await.unwrap_err();
    assert!(missing.is_not_found());
    let file_error = sftp.list("/etc/passwd").await.unwrap_err();
    assert_eq!(file_error, SftpError::NotADirectory { path: "/etc/passwd".into() });
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_conflicts_are_detected_against_changes_made_elsewhere() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    scratch.sh(r#"echo mine > "$D/shared.txt""#);
    let file = scratch.at("shared.txt");
    let opened = sftp.open_text(&file).await.unwrap();
    scratch.sh(r#"sleep 1; echo "someone else was here" > "$D/shared.txt""#);
    match sftp.save_text(&file, "my edit\n", LF, Some(opened.revision)).await.unwrap() {
        SaveOutcome::Conflict { remote_text, remote_mtime, .. } => {
            assert_eq!(remote_text.as_deref(), Some("someone else was here\n"));
            assert!(remote_mtime.unwrap() >= opened.revision.mtime.unwrap());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(scratch.sh(r#"cat "$D/shared.txt""#), "someone else was here\n");
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_directory_operations_work_on_a_real_server() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    let (a, b) = (scratch.at("新目录"), scratch.at("renamed"));
    sftp.mkdir(&a).await.unwrap();
    assert_eq!(sftp.mkdir(&a).await.unwrap_err(), SftpError::AlreadyExists { path: a.clone() });
    scratch.sh(r#"echo x > "$D/新目录/f.txt"; mkdir "$D/新目录/deep"; echo y > "$D/新目录/deep/g.txt"; echo z > "$D/other.txt""#);
    assert_eq!(
        sftp.rename(&a, &scratch.at("other.txt")).await.unwrap_err(),
        SftpError::AlreadyExists { path: scratch.at("other.txt") }
    );
    sftp.rename(&a, &b).await.unwrap();
    sftp.chmod(&b, 0o750).await.unwrap();
    assert_eq!(scratch.sh(r#"stat -c '%a' "$D/renamed""#).trim(), "750");
    sftp.remove(&b).await.unwrap();
    assert_eq!(scratch.sh(r#"ls -A "$D""#).trim(), "other.txt");
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_a_group_writable_file_of_somebody_else_keeps_its_owner() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    // A shared directory and file belonging to root, writable by the group tester is in.
    let group = docker("id -gn").trim().to_string();
    scratch.sh_root(&format!(
        r#"mkdir "$D/shared"; chown root:{group} "$D/shared"; chmod 775 "$D/shared";
           echo old > "$D/shared/file.txt"; chown root:{group} "$D/shared/file.txt"; chmod 664 "$D/shared/file.txt""#
    ));
    let file = scratch.at("shared/file.txt");
    let inode = scratch.sh(r#"stat -c '%i' "$D/shared/file.txt""#);
    let opened = sftp.open_text(&file).await.unwrap();
    let outcome = sftp.save_text(&file, "edited by tester\n", LF, Some(opened.revision)).await.unwrap();
    assert!(
        matches!(outcome, SaveOutcome::Saved { mode: WriteMode::InPlace(InPlaceReason::KeepOwner), .. }),
        "a new file would have been tester's: {outcome:?}"
    );
    assert_eq!(scratch.sh(r#"stat -c '%U:%G %a' "$D/shared/file.txt""#).trim(), format!("root:{group} 664"));
    assert_eq!(scratch.sh(r#"cat "$D/shared/file.txt""#), "edited by tester\n");
    assert_eq!(scratch.sh(r#"stat -c '%i' "$D/shared/file.txt""#), inode, "the same file");
    assert_eq!(scratch.sh(r#"ls -A "$D/shared""#).trim(), "file.txt", "the unused temporary file is gone");
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_a_read_only_file_in_a_writable_directory_is_not_replaced() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    scratch.sh(r#"echo "do not touch" > "$D/readonly.txt"; chmod 444 "$D/readonly.txt""#);
    let file = scratch.at("readonly.txt");
    let opened = sftp.open_text(&file).await.unwrap();
    let error = sftp.save_text(&file, "overwritten\n", LF, Some(opened.revision)).await.unwrap_err();
    assert_eq!(error, SftpError::PermissionDenied { path: file.clone() });
    assert_eq!(
        scratch.sh(r#"cat "$D/readonly.txt"; stat -c '%a' "$D/readonly.txt"; ls -A "$D""#),
        "do not touch\n444\nreadonly.txt\n"
    );
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_a_symlink_planted_at_the_temporary_name_is_not_followed() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    scratch.sh(r#"echo mine > "$D/doc.txt"; echo "victim content" > "$D/victim.txt"; ln -s victim.txt "$D/.doc.txt.shellmate-edit""#);
    let outcome = sftp.save_text(&scratch.at("doc.txt"), "saved\n", LF, None).await.unwrap();
    assert!(matches!(outcome, SaveOutcome::Saved { mode: WriteMode::Atomic, .. }), "{outcome:?}");
    assert_eq!(scratch.sh(r#"cat "$D/doc.txt" "$D/victim.txt""#), "saved\nvictim content\n");
    assert_eq!(scratch.sh(r#"ls -A "$D""#), "doc.txt\nvictim.txt\n");
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_overwriting_through_a_dangling_link_keeps_the_link() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    scratch.sh(r#"mkdir "$D/links" "$D/targets"; ln -s ../targets/new.txt "$D/links/hop"; ln -s links/hop "$D/entry""#);
    let outcome = sftp.save_text(&scratch.at("entry"), "created\n", LF, None).await.unwrap();
    assert!(matches!(outcome, SaveOutcome::Saved { mode: WriteMode::Atomic, .. }), "{outcome:?}");
    assert_eq!(
        scratch.sh(r#"readlink "$D/entry"; readlink "$D/links/hop"; cat "$D/targets/new.txt""#),
        "links/hop\n../targets/new.txt\ncreated\n"
    );
}
