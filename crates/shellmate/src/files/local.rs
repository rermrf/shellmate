//! Blocking filesystem operations for a local tab. Callers run these on the blocking pool.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use objc2_foundation::{NSFileManager, NSURL};
use shellmate_sftp::transfer::candidates;
use shellmate_sftp::{
    Entry, EntryKind, Link, Listed, MAX_TEXT_SIZE, Revision, SaveFormat, SaveOutcome, SftpError, TextFile, WriteMode,
    decode_text, encode_text, sort_entries,
};

pub(super) type Result<T> = std::result::Result<T, SftpError>;
type Trash = dyn Fn(&Path) -> Result<()> + Send + Sync;

pub(crate) struct LocalFs {
    pub home: PathBuf,
    trash: Box<Trash>,
    names: Mutex<HashMap<(bool, u32), String>>,
}

pub(super) fn io_error(path: &Path, err: io::Error) -> SftpError {
    let path = path.display().to_string();
    match err.kind() {
        io::ErrorKind::NotFound => SftpError::NotFound { path },
        io::ErrorKind::NotADirectory => SftpError::NotADirectory { path },
        io::ErrorKind::PermissionDenied => SftpError::PermissionDenied { path },
        io::ErrorKind::AlreadyExists => SftpError::AlreadyExists { path },
        _ => SftpError::Failed {
            path,
            message: err.to_string(),
        },
    }
}

fn system_trash(path: &Path) -> Result<()> {
    let url = NSURL::from_file_path(path).ok_or_else(|| io_error(path, io::ErrorKind::InvalidInput.into()))?;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|err| SftpError::Failed {
            path: path.display().to_string(),
            message: err.localizedDescription().to_string(),
        })
}

fn account_name(group: bool, id: u32) -> String {
    let mut buffer = vec![0; 4096];
    loop {
        let (status, name) = unsafe {
            if group {
                let mut record = std::mem::MaybeUninit::uninit();
                let mut found = std::ptr::null_mut();
                let status = libc::getgrgid_r(id, record.as_mut_ptr(), buffer.as_mut_ptr(), buffer.len(), &mut found);
                (status, (status == 0 && !found.is_null()).then(|| (*found).gr_name))
            } else {
                let mut record = std::mem::MaybeUninit::uninit();
                let mut found = std::ptr::null_mut();
                let status = libc::getpwuid_r(id, record.as_mut_ptr(), buffer.as_mut_ptr(), buffer.len(), &mut found);
                (status, (status == 0 && !found.is_null()).then(|| (*found).pw_name))
            }
        };
        if status == libc::ERANGE && buffer.len() < 1 << 20 {
            buffer.resize(buffer.len() * 2, 0);
        } else {
            return name.filter(|name| status == 0 && !name.is_null()).map_or_else(
                || id.to_string(),
                |name| unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned(),
            );
        }
    }
}

pub(super) fn rename_with(from: &Path, to: &Path, flags: u32) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), flags) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl LocalFs {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            trash: Box::new(system_trash),
            names: Mutex::default(),
        }
    }

    fn entry(&self, path: &Path) -> io::Result<Entry> {
        let metadata = fs::symlink_metadata(path)?;
        let kind = EntryKind::from_mode(metadata.mode());
        let link = if kind == EntryKind::Symlink {
            Some(Link {
                target: fs::read_link(path)?.display().to_string(),
                resolved: fs::metadata(path)
                    .ok()
                    .map(|target| EntryKind::from_mode(target.mode())),
            })
        } else {
            None
        };
        let mut names = self.names.lock().unwrap();
        let mut name = |group, id| {
            names
                .entry((group, id))
                .or_insert_with(|| account_name(group, id))
                .clone()
        };
        let full = path.to_str().ok_or(io::ErrorKind::InvalidData)?;
        Ok(Entry {
            name: shellmate_sftp::path::file_name(full).unwrap_or("/").into(),
            path: full.into(),
            kind,
            size: metadata.len(),
            modified: Some(metadata.mtime()),
            mode: Some(metadata.mode() & 0o7777),
            uid: Some(metadata.uid()),
            gid: Some(metadata.gid()),
            owner: name(false, metadata.uid()),
            group: name(true, metadata.gid()),
            link,
        })
    }

    pub fn listing(&self, dir: &str) -> Listed {
        let dir = if dir == "." { &self.home } else { Path::new(dir) };
        let entries = (|| {
            let mut entries = Vec::new();
            for child in fs::read_dir(dir)? {
                match self.entry(&child?.path()) {
                    Ok(entry) => entries.push(entry),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err),
                }
            }
            sort_entries(&mut entries);
            Ok(entries)
        })()
        .map_err(|err| io_error(dir, err));
        Listed {
            dir: Some(dir.display().to_string()),
            entries,
            stat: self.entry(dir).ok(),
        }
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        rename_with(from.as_ref(), to.as_ref(), libc::RENAME_EXCL).map_err(|err| io_error(to.as_ref(), err))
    }

    pub fn create_dir_unique(&self, dir: &str, base: &str) -> Result<String> {
        for name in candidates(base, false) {
            let target = Path::new(dir).join(name);
            match fs::create_dir(&target) {
                Ok(()) => return Ok(target.display().to_string()),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(io_error(&target, err)),
            }
        }
        Err(SftpError::AlreadyExists {
            path: shellmate_sftp::path::join(dir, base),
        })
    }

    pub fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777)).map_err(|err| io_error(path.as_ref(), err))
    }

    pub fn trash_selected(&self, paths: &[String]) -> Result<()> {
        for path in paths {
            let target = Path::new(path);
            if !target.is_absolute() || target.file_name().is_none() {
                return Err(SftpError::Refused { path: path.clone() });
            }
            (self.trash)(target)?;
        }
        Ok(())
    }

    pub fn open_text(&self, path: &str) -> Result<TextFile> {
        let error = |err| io_error(path.as_ref(), err);
        let target = fs::canonicalize(path).map_err(error)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&target)
            .map_err(error)?;
        let metadata = file.metadata().map_err(error)?;
        regular(path, &metadata)?;
        if metadata.len() > MAX_TEXT_SIZE {
            return Err(SftpError::TooLarge { size: metadata.len() });
        }
        let mut bytes = Vec::new();
        file.take(MAX_TEXT_SIZE + 1).read_to_end(&mut bytes).map_err(error)?;
        if bytes.len() as u64 > MAX_TEXT_SIZE {
            return Err(SftpError::TooLarge {
                size: bytes.len() as u64,
            });
        }
        let (text, line_ending, bom) = decode_text(&bytes)?;
        Ok(TextFile {
            path: path.into(),
            target: target.display().to_string(),
            text,
            line_ending,
            bom,
            revision: revision(&metadata),
            mode: Some(metadata.mode() & 0o7777),
        })
    }

    pub fn save_text(&self, path: &str, text: &str, format: SaveFormat, expected: Revision) -> Result<SaveOutcome> {
        let error = |err| io_error(path.as_ref(), err);
        let target = fs::canonicalize(path).map_err(error)?;
        let check = || -> Result<(Metadata, Option<SaveOutcome>)> {
            let meta = fs::metadata(&target).map_err(error)?;
            regular(path, &meta)?;
            let current = revision(&meta);
            let conflict = (expected != current).then(|| SaveOutcome::Conflict {
                remote_revision: current,
                remote_mtime: Some(meta.mtime()),
                remote_text: self.open_text(path).ok().map(|file| file.text),
            });
            Ok((meta, conflict))
        };
        let (meta, conflict) = check()?;
        if let Some(outcome) = conflict {
            return Ok(outcome);
        }
        OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&target)
            .map_err(error)?;
        let bytes = encode_text(text, format);
        if bytes.len() as u64 > MAX_TEXT_SIZE {
            return Err(SftpError::TooLarge {
                size: bytes.len() as u64,
            });
        }
        let mut temp = tempfile::NamedTempFile::new_in(target.parent().unwrap()).map_err(error)?;
        temp.write_all(&bytes).map_err(error)?;
        let created = temp.as_file().metadata().map_err(error)?;
        if (created.uid(), created.gid()) != (meta.uid(), meta.gid())
            && unsafe { libc::fchown(temp.as_raw_fd(), meta.uid(), meta.gid()) } != 0
        {
            return Err(error(io::Error::last_os_error()));
        }
        temp.as_file().set_permissions(meta.permissions()).map_err(error)?;
        if let (_, Some(outcome)) = check()? {
            return Ok(outcome);
        }
        let saved: File = temp.persist(target).map_err(|err| error(err.error))?;
        Ok(SaveOutcome::Saved {
            revision: revision(&saved.metadata().map_err(error)?),
            mode: WriteMode::Atomic,
        })
    }
}

fn regular(path: &str, meta: &Metadata) -> Result<()> {
    if meta.is_file() {
        Ok(())
    } else if meta.is_dir() {
        Err(SftpError::IsADirectory { path: path.into() })
    } else {
        Err(SftpError::NotAFile { path: path.into() })
    }
}

fn revision(meta: &Metadata) -> Revision {
    Revision {
        size: meta.len(),
        mtime: Some(
            meta.mtime()
                .saturating_mul(1_000_000_000)
                .saturating_add(meta.mtime_nsec()),
        ),
    }
}

#[cfg(test)]
#[path = "local_tests.rs"]
mod tests;
