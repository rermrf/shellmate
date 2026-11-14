//! Directory entries and their ordering.

use std::cmp::Ordering;

use russh_sftp::protocol::FileAttributes;

use crate::path;

/// What an entry is (a symlink is a symlink here; see [`Link`] for what it points at).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link.
    Symlink,
    /// A device, pipe, socket, or a server that did not say.
    Other,
}

impl EntryKind {
    /// The kind in the `st_mode` type bits.
    pub fn from_mode(mode: u32) -> Self {
        match mode & 0o170000 {
            0o040000 => Self::Dir,
            0o100000 => Self::File,
            0o120000 => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// Where a symbolic link points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The link's text as stored (`../shared`, `/etc/hosts`).
    pub target: String,
    /// What the target is; `None` when it does not exist (a dangling link) or could not be checked.
    pub resolved: Option<EntryKind>,
}

/// The size and modification time of a file as the server reported them: what the editor
/// remembers when it opens a file and compares before saving. SFTP v3 times have a resolution of
/// one second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revision {
    /// Size in bytes.
    pub size: u64,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: Option<i64>,
}

/// One file, directory or link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The name in its directory (`/` for the root).
    pub name: String,
    /// The full path.
    pub path: String,
    /// What it is.
    pub kind: EntryKind,
    /// Size in bytes (of the link itself for a symlink).
    pub size: u64,
    /// Modification time, seconds since the Unix epoch.
    pub modified: Option<i64>,
    /// Permission bits (`0o755`), without the file type.
    pub mode: Option<u32>,
    /// Numeric user id, if the server reported one.
    pub uid: Option<u32>,
    /// Numeric group id, if the server reported one.
    pub gid: Option<u32>,
    /// Owning user: the name if the server's listing has it, else the numeric id; empty if unknown.
    pub owner: String,
    /// Owning group, likewise.
    pub group: String,
    /// For a symlink: where it points.
    pub link: Option<Link>,
}

impl Entry {
    /// Builds an entry from the attributes of `name` in `dir`. `longname` is the server's
    /// `ls -l` style line, used for the user and group names (which SFTP v3 attributes lack).
    pub fn from_attrs(dir: &str, name: &str, longname: &str, attrs: &FileAttributes) -> Self {
        let kind = attrs.permissions.map_or(EntryKind::Other, EntryKind::from_mode);
        let (owner, group) = owner_names(longname).unwrap_or_else(|| {
            (
                attrs.uid.map(|uid| uid.to_string()).unwrap_or_default(),
                attrs.gid.map(|gid| gid.to_string()).unwrap_or_default(),
            )
        });
        Self {
            name: name.to_string(),
            path: path::join(dir, name),
            kind,
            size: attrs.size.unwrap_or(0),
            modified: attrs.mtime.map(i64::from),
            mode: attrs.permissions.map(|mode| mode & 0o7777),
            uid: attrs.uid,
            gid: attrs.gid,
            owner,
            group,
            link: None,
        }
    }

    /// Whether this is a directory (not a link to one).
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    /// Whether opening it should enter it: a directory, or a link that points at one.
    pub fn opens_as_dir(&self) -> bool {
        self.is_dir() || self.link.as_ref().is_some_and(|link| link.resolved == Some(EntryKind::Dir))
    }

    /// Whether this is a symlink whose target does not exist.
    pub fn is_dangling(&self) -> bool {
        self.link.as_ref().is_some_and(|link| link.resolved.is_none())
    }

    /// The remembered state for conflict detection.
    pub fn revision(&self) -> Revision {
        Revision { size: self.size, mtime: self.modified }
    }

    /// `tester:staff`, or `1000:1000` when the server gave only numbers (empty when unknown).
    pub fn owner_display(&self) -> String {
        match (self.owner.is_empty(), self.group.is_empty()) {
            (true, true) => String::new(),
            _ => format!("{}:{}", self.owner, self.group),
        }
    }
}

/// The user and group of an `ls -l` style line (`-rw-r--r-- 1 tester staff 1234 Nov 12 08:55 a b`),
/// or `None` if the line does not look like one.
fn owner_names(longname: &str) -> Option<(String, String)> {
    let mut parts = longname.split_whitespace();
    let mode = parts.next()?;
    let links = parts.next()?;
    let user = parts.next()?;
    let group = parts.next()?;
    let looks_like_mode = mode.len() >= 10 && mode.starts_with(['-', 'd', 'l', 'c', 'b', 'p', 's']);
    (looks_like_mode && !links.is_empty() && links.bytes().all(|b| b.is_ascii_digit()))
        .then(|| (user.to_string(), group.to_string()))
}

/// Directories first, then names in case-insensitive natural order (`file2` before `file10`).
/// Links to directories count as directories.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| b.opens_as_dir().cmp(&a.opens_as_dir()).then_with(|| natural_cmp(&a.name, &b.name)));
}

/// Case-insensitive comparison in which runs of digits compare by value. Names equal in that
/// sense fall back to a plain comparison so the order is total and stable.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut left, mut right) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (run_x, run_y) = (take_digits(&mut left), take_digits(&mut right));
                let (trim_x, trim_y) = (run_x.trim_start_matches('0'), run_y.trim_start_matches('0'));
                let order = trim_x.len().cmp(&trim_y.len()).then_with(|| trim_x.cmp(trim_y));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                left.next();
                right.next();
            }
        }
    }
}

fn take_digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut run = String::new();
    while let Some(c) = chars.peek().copied().filter(char::is_ascii_digit) {
        run.push(c);
        chars.next();
    }
    run
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(mode: u32, size: u64, mtime: u32) -> FileAttributes {
        FileAttributes {
            size: Some(size),
            uid: Some(1000),
            gid: Some(100),
            permissions: Some(mode),
            mtime: Some(mtime),
            ..Default::default()
        }
    }

    #[test]
    fn kind_comes_from_the_type_bits() {
        assert_eq!(EntryKind::from_mode(0o100644), EntryKind::File);
        assert_eq!(EntryKind::from_mode(0o040755), EntryKind::Dir);
        assert_eq!(EntryKind::from_mode(0o120777), EntryKind::Symlink);
        assert_eq!(EntryKind::from_mode(0o060660), EntryKind::Other, "a block device is not a directory");
        assert_eq!(EntryKind::from_mode(0o140755), EntryKind::Other, "nor is a socket");
        assert_eq!(EntryKind::from_mode(0o644), EntryKind::Other, "no type bits");
    }

    #[test]
    fn an_entry_takes_names_from_the_longname() {
        let line = "-rwxr-xr-x    1 tester   staff        2048 Nov 12 08:55 my notes.txt";
        let entry = Entry::from_attrs("/home/tester", "my notes.txt", line, &attrs(0o100755, 2048, 1_700_000_000));
        assert_eq!(entry.path, "/home/tester/my notes.txt");
        assert_eq!((entry.owner.as_str(), entry.group.as_str()), ("tester", "staff"));
        assert_eq!(entry.owner_display(), "tester:staff");
        assert_eq!(
            (entry.kind, entry.size, entry.modified, entry.mode),
            (EntryKind::File, 2048, Some(1_700_000_000), Some(0o755))
        );
    }

    #[test]
    fn without_a_usable_longname_the_numeric_ids_are_shown() {
        for line in ["", "garbage", "total 12", "-rw-r--r-- x tester staff 1 Nov 12 08:55 a", "short 1 a b"] {
            let entry = Entry::from_attrs("/", "a", line, &attrs(0o100644, 1, 0));
            assert_eq!(entry.owner_display(), "1000:100", "{line:?}");
        }
        let bare = Entry::from_attrs("/", "a", "", &FileAttributes::default());
        assert_eq!((bare.owner_display().as_str(), bare.kind, bare.mode), ("", EntryKind::Other, None));
    }

    #[test]
    fn numeric_names_in_the_longname_are_kept() {
        let entry = Entry::from_attrs(
            "/",
            "a",
            "-rw-r--r--   1 1000     1000          5 Nov 12 08:55 a",
            &attrs(0o100644, 5, 0),
        );
        assert_eq!(entry.owner_display(), "1000:1000");
    }

    #[test]
    fn links_to_directories_open_as_directories() {
        let mut link = Entry::from_attrs("/", "docs", "", &attrs(0o120777, 4, 0));
        assert!(!link.is_dir() && !link.opens_as_dir());
        link.link = Some(Link { target: "real/docs".into(), resolved: Some(EntryKind::Dir) });
        assert!(!link.is_dir() && link.opens_as_dir() && !link.is_dangling());
        link.link = Some(Link { target: "nowhere".into(), resolved: None });
        assert!(!link.opens_as_dir() && link.is_dangling());
    }

    #[test]
    fn natural_order_compares_numbers_by_value() {
        fn sorted<'a>(names: &[&'a str]) -> Vec<&'a str> {
            let mut names = names.to_vec();
            names.sort_by(|a, b| natural_cmp(a, b));
            names
        }
        assert_eq!(sorted(&["file10", "file2", "file1"]), ["file1", "file2", "file10"]);
        assert_eq!(
            sorted(&["b", "A", "a", "B"]),
            ["A", "a", "B", "b"],
            "case-insensitive, upper before lower on a tie"
        );
        assert_eq!(
            sorted(&["img007", "img7", "img10"]),
            ["img007", "img7", "img10"],
            "equal values keep a stable order"
        );
        assert_eq!(sorted(&["a1b10", "a1b9", "a01b2"]), ["a01b2", "a1b9", "a1b10"]);
        assert_eq!(sorted(&[".zshrc", "Zebra", "alpha", ".bashrc"]), [".bashrc", ".zshrc", "alpha", "Zebra"]);
        assert_eq!(sorted(&["报告2", "报告10", "报告1"]), ["报告1", "报告2", "报告10"]);
        assert_eq!(sorted(&["a", "a1", ""]), ["", "a", "a1"]);
        assert_eq!(natural_cmp("x", "x"), Ordering::Equal);
        assert_eq!(
            sorted(&["99999999999999999999999", "100000000000000000000000"]),
            ["99999999999999999999999", "100000000000000000000000"],
            "no overflow on long runs"
        );
    }

    #[test]
    fn directories_come_first_and_links_to_directories_with_them() {
        let entry = |name: &str, mode: u32| Entry::from_attrs("/", name, "", &attrs(mode, 0, 0));
        let mut link_to_dir = entry("Lnk", 0o120777);
        link_to_dir.link = Some(Link { target: "d".into(), resolved: Some(EntryKind::Dir) });
        let mut broken = entry("broken", 0o120777);
        broken.link = Some(Link { target: "x".into(), resolved: None });
        let mut entries = vec![
            entry("b.txt", 0o100644),
            entry("Zdir", 0o040755),
            broken,
            entry("a.txt", 0o100644),
            link_to_dir,
            entry("adir", 0o040755),
        ];
        sort_entries(&mut entries);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["adir", "Lnk", "Zdir", "a.txt", "b.txt", "broken"]);
    }
}
