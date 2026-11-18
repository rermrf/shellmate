//! Deterministic local directory for debug scenes.
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::Path;

pub(crate) const SELECTION: [&str; 2] = ["notes.txt", "report.md"];
const MTIME: i64 = 1_794_473_700;

pub(crate) fn prepare(data: &Path) -> io::Result<()> {
    fs::create_dir_all(data)?;
    let root = data.join("files-local");
    for directory in [&root, &root.join("Documents"), &root.join("Projects")] {
        match fs::create_dir(directory) {
            Ok(()) => (),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && fs::symlink_metadata(directory)?.is_dir() => (),
            Err(err) => return Err(err),
        }
    }
    for (name, bytes) in [
        ("notes.txt", &b"Local notes\n"[..]),
        ("report.md", b"# Weekly report\n"),
        (".settings", b"theme=light\n"),
    ] {
        let mut file = tempfile::NamedTempFile::new_in(&root)?;
        file.write_all(bytes)?;
        file.persist(root.join(name))?;
    }
    let link = root.join("latest");
    match symlink("report.md", &link) {
        Ok(()) => (),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists && fs::read_link(&link)? == Path::new("report.md") => (),
        Err(err) => return Err(err),
    }
    for name in [
        "Documents",
        "Projects",
        "notes.txt",
        "report.md",
        ".settings",
        "latest",
        "",
    ] {
        let path = CString::new(root.join(name).as_os_str().as_bytes())?;
        let times = [libc::timespec {
            tv_sec: MTIME,
            tv_nsec: 0,
        }; 2];
        if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn fixture_is_repeatable_including_link_mtime() {
        let data = tempfile::tempdir().unwrap();
        for _ in 0..2 {
            prepare(data.path()).unwrap();
            let root = data.path().join("files-local");
            assert_eq!(fs::read_dir(&root).unwrap().count(), 6);
            for child in fs::read_dir(&root).unwrap() {
                assert_eq!(fs::symlink_metadata(child.unwrap().path()).unwrap().mtime(), MTIME);
            }
            assert_eq!(fs::metadata(root.join("notes.txt")).unwrap().len(), 12);
        }
    }
}
