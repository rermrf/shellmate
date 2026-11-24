//! Descriptor-relative operations: checking a path must not open a symlink race.
use std::{
    ffi::{CStr, CString, OsString},
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::OpenOptionsExt,
        },
    },
    path::Path,
};

pub(super) struct Dir(File);
pub(super) struct Entry {
    pub name: OsString,
    pub directory: bool,
    pub bytes: u64,
    pub modified: std::time::SystemTime,
}
fn name(value: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| super::invalid("路径不能包含 NUL"))
}
fn result(code: libc::c_int) -> io::Result<libc::c_int> {
    if code < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(code)
    }
}
impl Dir {
    pub fn open(data_root: &Path, path: &Path, create: bool) -> io::Result<Self> {
        let base = data_root.canonicalize()?;
        let relative = path
            .strip_prefix(&base)
            .map_err(|_| super::invalid("记忆路径越界"))?;
        let mut dir = Self(
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(base)?,
        );
        for part in relative.components() {
            let std::path::Component::Normal(part) = part else {
                return Err(super::invalid("记忆路径越界"));
            };
            dir = dir.child(part, create)?;
        }
        Ok(dir)
    }
    fn child(&self, child: &std::ffi::OsStr, create: bool) -> io::Result<Self> {
        let child = name(child)?;
        if create {
            // SAFETY: the fd is owned by self; child is a valid NUL-terminated single component.
            let code = unsafe { libc::mkdirat(self.0.as_raw_fd(), child.as_ptr(), 0o700) };
            if code < 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: same valid fd/name; O_NOFOLLOW applies to every path component.
        let fd = result(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                child.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        // SAFETY: openat returned a new owned descriptor.
        Ok(Self(unsafe { File::from_raw_fd(fd) }))
    }
    pub fn read(&self, child: &std::ffi::OsStr) -> io::Result<File> {
        let child = name(child)?;
        // SAFETY: valid owned fd and single component. Nonblocking avoids hanging on FIFOs.
        let fd = result(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                child.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        })?;
        // SAFETY: openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Err(super::invalid("只允许普通 .md 文件"));
        }
        Ok(file)
    }
    pub fn create(&self, child: &std::ffi::OsStr) -> io::Result<File> {
        let child = name(child)?;
        // SAFETY: valid owned fd/name; O_EXCL prevents replacing an existing entry.
        let fd = result(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                child.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        })?;
        // SAFETY: openat returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub fn rename(&self, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> io::Result<()> {
        let (from, to) = (name(from)?, name(to)?);
        // SAFETY: valid owned fd/names. A racing symlink is replaced, never followed.
        result(unsafe {
            libc::renameat(
                self.0.as_raw_fd(),
                from.as_ptr(),
                self.0.as_raw_fd(),
                to.as_ptr(),
            )
        })
        .map(|_| ())
    }
    pub fn remove(&self, child: &std::ffi::OsStr, directory: bool) -> io::Result<()> {
        let child = name(child)?;
        // SAFETY: valid owned fd/name. unlinkat never follows a final symlink.
        result(unsafe {
            libc::unlinkat(
                self.0.as_raw_fd(),
                child.as_ptr(),
                if directory { libc::AT_REMOVEDIR } else { 0 },
            )
        })
        .map(|_| ())
    }
    pub fn entries(&self) -> io::Result<Vec<Entry>> {
        // openat(".") gets an independent directory offset (dup would share it).
        let listing = self.child(std::ffi::OsStr::new("."), false)?;
        use std::os::fd::IntoRawFd;
        let fd = listing.0.into_raw_fd();
        // SAFETY: fd is owned and refers to a directory; fdopendir takes ownership on success.
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: ownership was not transferred on failure.
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }
        struct Listing(*mut libc::DIR);
        impl Drop for Listing {
            fn drop(&mut self) {
                // SAFETY: this guard exclusively owns the directory stream and its fd.
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let listing = Listing(stream);
        let mut entries = Vec::new();
        loop {
            // SAFETY: errno is thread-local; distinguish EOF from a failed directory read.
            #[cfg(target_vendor = "apple")]
            unsafe {
                *libc::__error() = 0;
            }
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            // SAFETY: listing owns a valid stream; consume d_name before the next readdir.
            let entry = unsafe { libc::readdir(listing.0) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error().is_some_and(|code| code != 0) {
                    return Err(error);
                }
                break;
            }
            // SAFETY: POSIX guarantees d_name is a NUL-terminated name in this live dirent.
            let entry_name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if matches!(entry_name.to_bytes(), b"." | b"..") {
                continue;
            }
            // SAFETY: stat is initialized and writable; fd/name are valid.
            let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
            result(unsafe {
                libc::fstatat(
                    self.0.as_raw_fd(),
                    entry_name.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            })?;
            let kind = stat.st_mode & libc::S_IFMT;
            if kind == libc::S_IFLNK {
                return Err(super::invalid("不允许符号链接"));
            }
            if !matches!(kind, libc::S_IFREG | libc::S_IFDIR) {
                return Err(super::invalid("只允许普通文件或目录"));
            }
            entries.push(Entry {
                name: OsString::from_vec(entry_name.to_bytes().to_vec()),
                directory: kind == libc::S_IFDIR,
                bytes: stat.st_size.max(0) as u64,
                modified: std::time::UNIX_EPOCH
                    + std::time::Duration::from_secs(stat.st_mtime.max(0) as u64),
            });
        }
        Ok(entries)
    }
    pub fn count_files(&self) -> io::Result<usize> {
        let mut count = 0;
        let mut pending = vec![self.child(std::ffi::OsStr::new("."), false)?];
        while let Some(dir) = pending.pop() {
            for entry in dir.entries()? {
                if entry.directory {
                    pending.push(dir.child(&entry.name, false)?);
                } else {
                    count += 1;
                }
            }
        }
        Ok(count)
    }
    pub fn remove_tree(&self, child: &std::ffi::OsStr) -> io::Result<()> {
        let dir = self.child(child, false)?;
        for entry in dir.entries()? {
            if entry.directory {
                dir.remove_tree(&entry.name)?;
            } else {
                dir.remove(&entry.name, false)?;
            }
        }
        self.remove(child, true)
    }
}
