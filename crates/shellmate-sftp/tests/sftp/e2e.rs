//! Tests against a real OpenSSH server (`#[ignore]`d; see the SSH crate's e2e tests for how to
//! start the container). Everything happens in a scratch directory `sftp-e2e-<random>` in the
//! user's home, which is removed afterwards.
//!
//! Settings (environment): `SHELLMATE_E2E_PORT` (2222), `SHELLMATE_E2E_USER` (tester),
//! `SHELLMATE_E2E_KEY` (path of the private key), `SHELLMATE_E2E_CONTAINER` (shellmate-e2e, used
//! to prepare files, symlinks and permissions with `docker exec`, and to check results).

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

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
        // Unique per process (pid), per call (counter) and per run (time): tests start together,
        // and the clock alone has microsecond resolution at best.
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let id = format!("{:x}-{:x}-{nanos:x}", std::process::id(), CALLS.fetch_add(1, Ordering::Relaxed));
        let path = format!("{}/sftp-e2e-{id}", home.trim_end_matches('/'));
        // Plain mkdir: a name somebody else already holds is an error, never shared.
        docker(&format!("mkdir '{path}'"));
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

// ---- transfers ------------------------------------------------------------------------------------

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use shellmate_core::{PtyDims, TerminalBackend};
use shellmate_sftp::transfer::{PauseReason, TransferQueue, TransferState, TransferView};

use crate::fixture::pattern;
use crate::support::Recorder;

/// `sha256` of a file on this machine.
fn local_sha256(path: &Path) -> String {
    let output = Command::new("shasum").args(["-a", "256"]).arg(path).output().expect("shasum is installed");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).split_whitespace().next().unwrap_or_default().to_string()
}

/// `sha256` of a file in the container.
fn remote_sha256(scratch: &Scratch, remote: &str) -> String {
    scratch.sh(&format!("sha256sum '{remote}'")).split_whitespace().next().unwrap_or_default().to_string()
}

/// Writes `megabytes` MiB of reproducible data.
fn big_file(dir: &Path, name: &str, megabytes: usize, seed: u64) -> PathBuf {
    let path = dir.join(name);
    let mut data = Vec::with_capacity(megabytes << 20);
    for part in 0..megabytes as u64 {
        data.extend(pattern(1 << 20, seed * 1000 + part));
    }
    std::fs::write(&path, data).unwrap();
    path
}

async fn wait_for(
    queue: &TransferQueue,
    id: shellmate_sftp::transfer::ItemId,
    what: &str,
    check: impl Fn(&TransferView) -> bool,
) -> TransferView {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let view = queue.view(id).expect("the item exists");
        if check(&view) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {:?}", view.state);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn megabytes_per_second(bytes: u64, took: Duration) -> f64 {
    bytes as f64 / (1 << 20) as f64 / took.as_secs_f64()
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_a_50_mb_file_goes_up_and_down_with_matching_checksums() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    let local = tempfile::tempdir().unwrap();
    let downloads = local.path().join("downloads");
    std::fs::create_dir(&downloads).unwrap();
    let big = big_file(local.path(), "big.bin", 50, 1);
    let expected = local_sha256(&big);

    let (queue, _events) = TransferQueue::new(tokio::runtime::Handle::current());
    queue.set_session(Some(Arc::new(sftp)));

    let began = Instant::now();
    let id = queue.enqueue_upload(vec![big.clone()], &scratch.path);
    let view = wait_for(&queue, id, "the upload", |v| v.state.is_finished()).await;
    let up = megabytes_per_second(view.bytes_total, began.elapsed());
    assert_eq!(view.state, TransferState::Done, "{view:?}");
    assert_eq!(remote_sha256(&scratch, &scratch.at("big.bin")), expected, "the uploaded file is intact");
    assert_eq!(scratch.sh(r#"ls -A "$D""#).trim(), "big.bin", "no temporary file left");

    let began = Instant::now();
    let id = queue.enqueue_download(vec![scratch.at("big.bin")], downloads.clone());
    let view = wait_for(&queue, id, "the download", |v| v.state.is_finished()).await;
    let down = megabytes_per_second(view.bytes_total, began.elapsed());
    assert_eq!(view.state, TransferState::Done, "{view:?}");
    assert_eq!(local_sha256(&downloads.join("big.bin")), expected, "the downloaded file is intact");
    eprintln!("THROUGHPUT 50 MB file over loopback to the container: upload {up:.1} MB/s, download {down:.1} MB/s");
    assert!(up >= 10.0 && down >= 10.0, "slower than 10 MB/s: up {up:.1}, down {down:.1}");
}

/// 1000 small files in 20 directories.
fn many_small_files(dir: &Path) {
    for d in 0..20u64 {
        let sub = dir.join(format!("d{d:02}"));
        std::fs::create_dir_all(&sub).unwrap();
        for f in 0..50u64 {
            std::fs::write(
                sub.join(format!("file{f:02}.dat")),
                pattern(500 + ((d * 50 + f) as usize % 3000), d * 100 + f),
            )
            .unwrap();
        }
    }
}

fn local_tree_sum(dir: &Path) -> String {
    let script = r#"cd "$1" && find . -type f | LC_ALL=C sort | xargs cat | shasum -a 256 | cut -d' ' -f1"#;
    let output = Command::new("sh").args(["-c", script, "sh"]).arg(dir).output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_a_directory_of_a_thousand_small_files_goes_up_and_down() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    let local = tempfile::tempdir().unwrap();
    let downloads = local.path().join("downloads");
    std::fs::create_dir(&downloads).unwrap();
    many_small_files(&local.path().join("many"));
    let expected = local_tree_sum(&local.path().join("many"));

    let (queue, _events) = TransferQueue::new(tokio::runtime::Handle::current());
    queue.set_session(Some(Arc::new(sftp)));

    let began = Instant::now();
    let id = queue.enqueue_upload(vec![local.path().join("many")], &scratch.path);
    let view = wait_for(&queue, id, "the upload", |v| v.state.is_finished()).await;
    let up = began.elapsed();
    assert_eq!((view.state.clone(), view.files_done, view.files_total), (TransferState::Done, 1000, 1000), "{view:?}");
    assert_eq!(scratch.sh(r#"find "$D/many" -type f | wc -l"#).trim(), "1000");
    assert_eq!(scratch.sh(r#"find "$D/many" -name '*.shellmate-*' | wc -l"#).trim(), "0", "no temporary files");
    let remote_sum =
        scratch.sh(r#"cd "$D/many" && find . -type f | LC_ALL=C sort | xargs cat | sha256sum | cut -d' ' -f1"#);
    assert_eq!(remote_sum.trim(), expected, "every file arrived intact");

    let began = Instant::now();
    let id = queue.enqueue_download(vec![scratch.at("many")], downloads.clone());
    let view = wait_for(&queue, id, "the download", |v| v.state.is_finished()).await;
    let down = began.elapsed();
    assert_eq!((view.state.clone(), view.files_done), (TransferState::Done, 1000), "{view:?}");
    assert_eq!(local_tree_sum(&downloads.join("many")), expected);
    eprintln!(
        "THROUGHPUT 1000 small files over loopback to the container: upload {:.1} s ({:.0} files/s), download {:.1} s ({:.0} files/s)",
        up.as_secs_f64(),
        1000.0 / up.as_secs_f64(),
        down.as_secs_f64(),
        1000.0 / down.as_secs_f64()
    );
}

/// The pid of the process serving this connection (the parent of its shell), found from inside a shell on it.
async fn session_process(connection: &Connection) -> u32 {
    let dims = PtyDims::default();
    let (recorder, sink) = Recorder::sink();
    let mut shell = connection.open_shell(dims).await.unwrap().start(sink, dims);
    recorder.wait_for_text("$").await;
    shell.write(b"echo SESSION-$PPID-END\r").unwrap();
    let pid =
        crate::support::eventually("the session's process id", || number_between(&recorder.text(), "SESSION-", "-END"))
            .await;
    // Leave the shell open: it keeps the connection busy like a terminal tab would.
    std::mem::forget(shell);
    pid
}

/// Kills the SFTP server process (`internal-sftp`) of the connection served by `session_pid`, and nothing else.
fn kill_sftp_server(session_pid: u32) {
    let script = format!(
        r#"for p in /proc/[0-9]*; do pid=${{p#/proc/}}; parent=$(awk '/^PPid:/{{print $2}}' $p/status 2>/dev/null);
           if [ "$parent" = "{session_pid}" ]; then case "$(tr '\0' ' ' < $p/cmdline 2>/dev/null)" in *internal-sftp*) kill -9 $pid; echo killed $pid;; esac; fi; done"#
    );
    let killed = docker_root(&script);
    assert!(killed.contains("killed"), "no SFTP server process under session {session_pid}: {killed:?}");
}

fn number_between(text: &str, marker: &str, end: &str) -> Option<u32> {
    text.match_indices(marker)
        .filter_map(|(at, _)| {
            let rest = &text[at + marker.len()..];
            rest[..rest.find(end)?].parse().ok()
        })
        .next()
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server and docker access to its container (see the SSH crate's e2e docs)"]
async fn e2e_transfers_resume_after_the_servers_sftp_process_is_killed() {
    let (sftp, connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    let local = tempfile::tempdir().unwrap();
    let downloads = local.path().join("downloads");
    std::fs::create_dir(&downloads).unwrap();
    let big = big_file(local.path(), "up.bin", 120, 2);
    let expected_up = local_sha256(&big);
    scratch.sh(r#"head -c 125829120 /dev/urandom > "$D/down.bin""#);
    let expected_down = remote_sha256(&scratch, &scratch.at("down.bin"));
    let session_pid = session_process(&connection).await;

    let (queue, _events) = TransferQueue::new(tokio::runtime::Handle::current());
    queue.set_session(Some(Arc::new(sftp)));
    let up = queue.enqueue_upload(vec![big], &scratch.path);
    let down = queue.enqueue_download(vec![scratch.at("down.bin")], downloads.clone());
    let total = 120u64 << 20;
    wait_for(&queue, up, "a bit of the upload", |v| v.bytes_done > total / 10 && v.state == TransferState::Running)
        .await;
    wait_for(&queue, down, "a bit of the download", |v| v.bytes_done > total / 10 && v.state == TransferState::Running)
        .await;

    // The server side of the SFTP session dies while both transfers are under way.
    kill_sftp_server(session_pid);
    let paused_up =
        wait_for(&queue, up, "the upload to notice", |v| v.state == TransferState::Paused(PauseReason::Disconnected))
            .await;
    let paused_down = wait_for(&queue, down, "the download to notice", |v| {
        v.state == TransferState::Paused(PauseReason::Disconnected)
    })
    .await;
    assert!(paused_up.bytes_done < total && paused_down.bytes_done < total, "they were interrupted, not finished");
    eprintln!(
        "interrupted at {} MB (upload) and {} MB (download) of 120",
        paused_up.bytes_done >> 20,
        paused_down.bytes_done >> 20
    );

    // A new SFTP session on the same connection; the queue carries on by itself.
    queue.set_session(Some(Arc::new(SftpSession::open(&connection).await.expect("a new sftp session"))));
    let done_up = wait_for(&queue, up, "the upload to finish", |v| v.state.is_finished()).await;
    let done_down = wait_for(&queue, down, "the download to finish", |v| v.state.is_finished()).await;
    assert_eq!((done_up.state, done_down.state), (TransferState::Done, TransferState::Done));
    assert_eq!(remote_sha256(&scratch, &scratch.at("up.bin")), expected_up, "the resumed upload is intact");
    assert_eq!(local_sha256(&downloads.join("down.bin")), expected_down, "the resumed download is intact");
    assert_eq!(scratch.sh(r#"ls -A "$D""#).split_whitespace().filter(|n| n.contains("shellmate")).count(), 0);
    assert_eq!(std::fs::read_dir(&downloads).unwrap().count(), 1, "no temporary file in the download directory");
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the SSH crate's e2e docs)"]
async fn e2e_an_overwriting_upload_into_an_unwritable_directory_goes_in_place() {
    let (sftp, _connection, _env) = session().await;
    let scratch = Scratch::new(&sftp.home().await.unwrap());
    scratch.sh(r#"mkdir "$D/ro"; echo "old and longer content" > "$D/ro/file.txt"; chmod 644 "$D/ro/file.txt"; chmod 555 "$D/ro""#);
    let inode = scratch.sh(r#"stat -c '%i' "$D/ro/file.txt""#);
    let local = tempfile::tempdir().unwrap();
    std::fs::write(local.path().join("file.txt"), "new\n").unwrap();
    let (queue, _events) = TransferQueue::new(tokio::runtime::Handle::current());
    queue.set_session(Some(Arc::new(sftp)));
    let id = queue.enqueue_upload(vec![local.path().join("file.txt")], &scratch.at("ro"));
    wait_for(&queue, id, "the question", |v| v.state == TransferState::AwaitingClash).await;
    queue.answer_clash(id, shellmate_sftp::transfer::Choice::Overwrite);
    let view = wait_for(&queue, id, "the upload", |v| v.state.is_finished()).await;
    assert_eq!((view.state.clone(), view.in_place), (TransferState::Done, 1), "{view:?}");
    assert_eq!(scratch.sh(r#"cat "$D/ro/file.txt""#), "new\n");
    assert_eq!(scratch.sh(r#"stat -c '%i' "$D/ro/file.txt""#), inode);
    assert_eq!(scratch.sh(r#"ls -A "$D/ro""#).trim(), "file.txt");
}
