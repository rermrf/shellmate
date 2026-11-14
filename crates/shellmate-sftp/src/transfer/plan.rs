//! What an item is going to transfer: the selection walked into a list of directories and files.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use super::{Skipped, TransferFailure};
use crate::entry::EntryKind;
use crate::path;
use crate::session::SftpSession;

/// One thing the user selected.
#[derive(Clone, Debug)]
pub(crate) struct Top {
    /// The name it gets at the destination (after numbering).
    pub name: String,
    /// Where it comes from on the server (downloads).
    pub remote: Option<String>,
    /// Whether it is a directory (a link to one counts as one).
    pub dir: bool,
    /// Where it goes on this machine (downloads; chosen when the item starts).
    pub dest: Option<PathBuf>,
}

/// A directory to create or a file to copy, below one of the selected things.
#[derive(Clone, Debug)]
pub(crate) struct PlanEntry {
    /// Index into [`Plan::tops`].
    pub top: usize,
    /// Names below the selected thing (empty for the thing itself).
    pub rel: Vec<String>,
    pub dir: bool,
    pub size: u64,
    /// Permission bits (`& 0o777` for files, as the source has them).
    pub mode: u32,
    /// The file on this machine (uploads).
    pub local: Option<PathBuf>,
}

impl PlanEntry {
    /// The relative path with `/`, empty for the selected thing itself.
    pub fn rel_path(&self) -> String {
        self.rel.join("/")
    }
}

/// The walked selection: totals known, entries in the order they are transferred (a directory
/// before what is in it).
#[derive(Clone, Debug, Default)]
pub(crate) struct Plan {
    pub tops: Vec<Top>,
    pub entries: Vec<PlanEntry>,
    pub total_bytes: u64,
    pub total_files: u64,
}

impl Plan {
    pub fn new(tops: Vec<Top>, entries: Vec<PlanEntry>) -> Self {
        let files = entries.iter().filter(|entry| !entry.dir);
        let (total_bytes, total_files) = files.fold((0, 0), |(bytes, count), entry| (bytes + entry.size, count + 1));
        Self { tops, entries, total_bytes, total_files }
    }

    /// The name to show for an entry: its own, or the selected thing's.
    pub fn display_name(&self, entry: &PlanEntry) -> String {
        entry.rel.last().cloned().unwrap_or_else(|| self.tops[entry.top].name.clone())
    }

    /// Keeps only the selected things for which `keep` says so, renumbering the entries.
    pub fn retain_tops(&mut self, mut keep: impl FnMut(usize, &Top) -> bool) {
        let mut remap = vec![None; self.tops.len()];
        let mut kept = Vec::new();
        for (index, top) in std::mem::take(&mut self.tops).into_iter().enumerate() {
            if keep(index, &top) {
                remap[index] = Some(kept.len());
                kept.push(top);
            }
        }
        self.tops = kept;
        self.entries.retain_mut(|entry| match remap[entry.top] {
            Some(new) => {
                entry.top = new;
                true
            }
            None => false,
        });
        *self = Self::new(std::mem::take(&mut self.tops), std::mem::take(&mut self.entries));
    }
}

/// The result of walking a selection.
#[derive(Default)]
pub(crate) struct Walked {
    pub tops: Vec<Top>,
    pub entries: Vec<PlanEntry>,
    pub skipped: Skipped,
    pub failures: Vec<TransferFailure>,
    /// The connection broke during the walk (remote only).
    pub disconnected: bool,
}

impl Walked {
    fn fail(&mut self, path: impl Into<String>, reason: impl Into<String>) {
        self.failures.push(TransferFailure { path: path.into(), reason: reason.into() });
    }
}

/// The name a selected path gets, or why it cannot be transferred.
fn top_name(path: &Path) -> Option<String> {
    path.file_name().map(|name| name.to_string_lossy().into_owned())
}

// ---- this machine -------------------------------------------------------------------------------

/// Walks files and directories on this machine. A link the user selected is followed once; links
/// and special files inside directories are skipped (and counted).
pub(crate) fn walk_local(selection: &[PathBuf]) -> Walked {
    let mut walked = Walked::default();
    for source in selection {
        let shown = source.display().to_string();
        let Some(name) = top_name(source) else {
            walked.fail(shown, "无法确定名称");
            continue;
        };
        let metadata = match std::fs::metadata(source) {
            Ok(metadata) => metadata,
            Err(err) => {
                let reason = match err.kind() {
                    std::io::ErrorKind::NotFound => "不存在或链接指向不存在的位置".to_string(),
                    std::io::ErrorKind::PermissionDenied => "权限不足".to_string(),
                    _ => err.to_string(),
                };
                walked.fail(shown, reason);
                continue;
            }
        };
        let top = walked.tops.len();
        if metadata.is_dir() {
            walked.tops.push(Top { name, remote: None, dir: true, dest: None });
            walked.entries.push(local_entry(top, Vec::new(), source, &metadata, true));
            walk_local_dir(source, top, &mut Vec::new(), &mut walked);
        } else if metadata.is_file() {
            walked.tops.push(Top { name, remote: None, dir: false, dest: None });
            walked.entries.push(local_entry(top, Vec::new(), source, &metadata, false));
        } else {
            walked.skipped.special += 1;
        }
    }
    walked
}

fn local_entry(top: usize, rel: Vec<String>, local: &Path, metadata: &std::fs::Metadata, dir: bool) -> PlanEntry {
    PlanEntry {
        top,
        rel,
        dir,
        size: if dir { 0 } else { metadata.len() },
        mode: metadata.mode() & 0o777,
        local: Some(local.to_path_buf()),
    }
}

fn walk_local_dir(dir: &Path, top: usize, rel: &mut Vec<String>, walked: &mut Walked) {
    let children = match std::fs::read_dir(dir) {
        Ok(children) => children,
        Err(err) => {
            walked.fail(dir.display().to_string(), format!("无法读取目录：{err}"));
            return;
        }
    };
    let mut children: Vec<_> = children.filter_map(Result::ok).collect();
    children.sort_by_key(|child| child.file_name());
    for child in children {
        let (child_path, name) = (child.path(), child.file_name().to_string_lossy().into_owned());
        let metadata = match std::fs::symlink_metadata(&child_path) {
            Ok(metadata) => metadata,
            Err(err) => {
                walked.fail(child_path.display().to_string(), err.to_string());
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            walked.skipped.symlinks += 1;
        } else if metadata.is_dir() {
            rel.push(name);
            walked.entries.push(local_entry(top, rel.clone(), &child_path, &metadata, true));
            walk_local_dir(&child_path, top, rel, walked);
            rel.pop();
        } else if metadata.is_file() {
            let mut below = rel.clone();
            below.push(name);
            walked.entries.push(local_entry(top, below, &child_path, &metadata, false));
        } else {
            walked.skipped.special += 1;
        }
    }
}

// ---- the server -----------------------------------------------------------------------------------

/// Walks files and directories on the server, like [`walk_local`].
pub(crate) async fn walk_remote(session: &SftpSession, selection: &[String]) -> Walked {
    let mut walked = Walked::default();
    for source in selection {
        let Some(name) = path::file_name(source) else {
            walked.fail(source.clone(), "无法确定名称");
            continue;
        };
        let entry = match session.stat(source).await {
            Ok(entry) => entry,
            Err(err) => {
                walked.disconnected |= err.is_session_lost();
                walked.fail(source.clone(), err.to_string());
                continue;
            }
        };
        let top = walked.tops.len();
        let remote_entry = |rel: Vec<String>, dir: bool| PlanEntry {
            top,
            rel,
            dir,
            size: if dir { 0 } else { entry.size },
            mode: entry.mode.unwrap_or(0o644) & 0o777,
            local: None,
        };
        match entry.kind {
            EntryKind::Dir => {
                walked.tops.push(Top { name: name.to_string(), remote: Some(source.clone()), dir: true, dest: None });
                walked.entries.push(remote_entry(Vec::new(), true));
                walk_remote_dir(session, source, top, Vec::new(), &mut walked).await;
            }
            EntryKind::File => {
                walked.tops.push(Top { name: name.to_string(), remote: Some(source.clone()), dir: false, dest: None });
                walked.entries.push(remote_entry(Vec::new(), false));
            }
            _ => walked.skipped.special += 1,
        }
    }
    walked
}

fn walk_remote_dir<'a>(
    session: &'a SftpSession,
    dir: &'a str,
    top: usize,
    rel: Vec<String>,
    walked: &'a mut Walked,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let children = match session.list(dir).await {
            Ok(children) => children,
            Err(err) => {
                walked.disconnected |= err.is_session_lost();
                walked.fail(dir.to_string(), err.to_string());
                return;
            }
        };
        for child in children {
            let mut below = rel.clone();
            below.push(child.name.clone());
            let entry = |dir: bool| PlanEntry {
                top,
                rel: below.clone(),
                dir,
                size: if dir { 0 } else { child.size },
                mode: child.mode.unwrap_or(0o644) & 0o777,
                local: None,
            };
            match child.kind {
                EntryKind::Symlink => walked.skipped.symlinks += 1,
                EntryKind::Dir => {
                    walked.entries.push(entry(true));
                    walk_remote_dir(session, &child.path, top, below.clone(), walked).await;
                }
                EntryKind::File => walked.entries.push(entry(false)),
                EntryKind::Other => walked.skipped.special += 1,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use super::*;

    fn write(path: &Path, content: &str, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn rels(walked: &Walked) -> Vec<(usize, String, bool)> {
        walked.entries.iter().map(|e| (e.top, e.rel_path(), e.dir)).collect()
    }

    #[test]
    fn a_directory_is_walked_depth_first_with_directories_before_their_contents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        write(&root.join("b.txt"), "bb", 0o644);
        write(&root.join("a/inner.txt"), "i", 0o600);
        write(&root.join("a/deep/x.txt"), "xxx", 0o755);
        std::fs::create_dir_all(root.join("empty")).unwrap();
        let walked = walk_local(std::slice::from_ref(&root));
        assert_eq!(walked.tops[0].name, "proj");
        assert_eq!(
            rels(&walked),
            [
                (0, "".into(), true),
                (0, "a".into(), true),
                (0, "a/deep".into(), true),
                (0, "a/deep/x.txt".into(), false),
                (0, "a/inner.txt".into(), false),
                (0, "b.txt".into(), false),
                (0, "empty".into(), true)
            ]
        );
        let plan = Plan::new(walked.tops, walked.entries);
        assert_eq!((plan.total_files, plan.total_bytes), (3, 6));
        let modes: Vec<u32> = plan.entries.iter().filter(|e| !e.dir).map(|e| e.mode).collect();
        assert_eq!(modes, [0o755, 0o600, 0o644]);
    }

    #[test]
    fn links_and_special_files_inside_directories_are_skipped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        write(&root.join("real.txt"), "r", 0o644);
        symlink("real.txt", root.join("link-to-file")).unwrap();
        symlink(".", root.join("link-to-dir")).unwrap();
        symlink("nowhere", root.join("dangling")).unwrap();
        let fifo = root.join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status().map(|s| s.success()).unwrap_or(false);
        let walked = walk_local(&[root]);
        assert_eq!(walked.skipped.symlinks, 3);
        assert_eq!(walked.skipped.special, u32::from(made));
        assert_eq!(rels(&walked), [(0, "".into(), true), (0, "real.txt".into(), false)]);
        assert!(walked.failures.is_empty());
    }

    #[test]
    fn a_link_selected_directly_is_followed_once() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("target/file.txt"), "t", 0o644);
        write(&dir.path().join("plain.txt"), "p", 0o644);
        symlink("target", dir.path().join("dirlink")).unwrap();
        symlink("plain.txt", dir.path().join("filelink")).unwrap();
        symlink("missing", dir.path().join("brokenlink")).unwrap();
        let walked =
            walk_local(&[dir.path().join("dirlink"), dir.path().join("filelink"), dir.path().join("brokenlink")]);
        assert_eq!(
            walked.tops.iter().map(|t| (t.name.as_str(), t.dir)).collect::<Vec<_>>(),
            [("dirlink", true), ("filelink", false)]
        );
        assert_eq!(rels(&walked), [(0, "".into(), true), (0, "file.txt".into(), false), (1, "".into(), false)]);
        assert_eq!(walked.failures.len(), 1, "a dangling link cannot be followed");
        assert!(walked.failures[0].path.ends_with("brokenlink"));
    }

    #[test]
    fn a_missing_selection_is_a_failure_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("ok.txt"), "x", 0o644);
        let walked = walk_local(&[dir.path().join("gone.txt"), dir.path().join("ok.txt")]);
        assert_eq!(walked.tops.len(), 1);
        assert_eq!(walked.failures.len(), 1);
        assert!(walked.failures[0].reason.contains("不存在"));
    }

    #[test]
    fn an_unreadable_subdirectory_fails_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        write(&root.join("fine/a.txt"), "a", 0o644);
        write(&root.join("locked/b.txt"), "b", 0o644);
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = std::fs::read_dir(root.join("locked")).is_err();
        let walked = walk_local(std::slice::from_ref(&root));
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(rels(&walked).contains(&(0, "fine/a.txt".into(), false)));
        if unreadable {
            assert_eq!(walked.failures.len(), 1);
            assert!(walked.failures[0].path.ends_with("locked"));
        }
    }

    #[test]
    fn removing_selected_things_renumbers_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.txt"), "aa", 0o644);
        write(&dir.path().join("b/x.txt"), "xxx", 0o644);
        write(&dir.path().join("c.txt"), "c", 0o644);
        let walked = walk_local(&[dir.path().join("a.txt"), dir.path().join("b"), dir.path().join("c.txt")]);
        let mut plan = Plan::new(walked.tops, walked.entries);
        assert_eq!((plan.tops.len(), plan.total_files, plan.total_bytes), (3, 3, 6));
        plan.retain_tops(|_, top| top.name != "b");
        assert_eq!(plan.tops.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["a.txt", "c.txt"]);
        assert_eq!(plan.entries.iter().map(|e| e.top).collect::<Vec<_>>(), [0, 1]);
        assert_eq!((plan.total_files, plan.total_bytes), (2, 3));
    }
}
