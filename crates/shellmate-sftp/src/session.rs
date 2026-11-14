//! The SFTP session and the operations on it that the file panel and the editor need.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::{self, StreamExt as _};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as RawError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode};
use shellmate_ssh::Connection;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::entry::{Entry, EntryKind, Link, Revision, sort_entries};
use crate::error::SftpError;
use crate::path;

/// Size of one read or write request.
const CHUNK: usize = 32 * 1024;
/// Requests in flight at once for one file.
const WINDOW: usize = 8;
/// Seconds a request may wait for its answer.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Links resolved at the same time when listing a directory.
const LINK_PARALLELISM: usize = 16;

/// `posix-rename@openssh.com`: rename that replaces an existing target atomically.
pub const POSIX_RENAME: &str = "posix-rename@openssh.com";
const FSYNC: &str = "fsync@openssh.com";

/// What the server told us in its version reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// The SFTP protocol version.
    pub version: u32,
    /// Advertised extensions with their versions (`posix-rename@openssh.com` → `1`).
    pub extensions: BTreeMap<String, String>,
}

impl ServerInfo {
    /// Whether the server advertises the extension.
    pub fn has(&self, extension: &str) -> bool {
        self.extensions.contains_key(extension)
    }

    /// Whether the server can replace a file by renaming over it.
    pub fn posix_rename(&self) -> bool {
        self.extensions.get(POSIX_RENAME).is_some_and(|version| version == "1")
    }
}

/// An SFTP connection to one server.
///
/// Created on the SSH runtime (the client library drives the stream with tasks of the current
/// tokio runtime); the methods are plain async functions and may be awaited from anywhere.
pub struct SftpSession {
    pub(crate) raw: Arc<RawSftpSession>,
    info: ServerInfo,
}

impl SftpSession {
    /// Opens the `sftp` subsystem on a channel of `connection` and starts the session.
    pub async fn open(connection: &Connection) -> Result<Self, SftpError> {
        let stream = connection.open_subsystem("sftp").await?;
        Self::from_stream(stream).await
    }

    /// Starts a session on a stream that already speaks to an SFTP server.
    pub async fn from_stream<S>(stream: S) -> Result<Self, SftpError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let raw = RawSftpSession::new(stream);
        raw.set_timeout(REQUEST_TIMEOUT_SECS);
        let version = raw.init().await.map_err(|err| SftpError::from_raw(err, ""))?;
        let info = ServerInfo { version: version.version, extensions: version.extensions.into_iter().collect() };
        Ok(Self { raw: Arc::new(raw), info })
    }

    /// What the server advertised.
    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    // ---- paths and metadata ----------------------------------------------------------------

    /// The absolute, symlink-free form of `path` as the server sees it (`.` is the home directory).
    pub async fn canonicalize(&self, path: &str) -> Result<String, SftpError> {
        let name = self.raw.realpath(path).await.map_err(|err| SftpError::from_raw(err, path))?;
        name.files
            .into_iter()
            .next()
            .map(|file| file.filename)
            .ok_or_else(|| SftpError::Protocol("服务器没有返回路径".into()))
    }

    /// The directory a new session starts in: the user's home.
    pub async fn home(&self) -> Result<String, SftpError> {
        self.canonicalize(".").await
    }

    /// Metadata of `path`, following a symlink to what it points at.
    pub async fn stat(&self, path: &str) -> Result<Entry, SftpError> {
        let attrs = self.raw.stat(path).await.map_err(|err| SftpError::from_raw(err, path))?.attrs;
        Ok(entry_at(path, "", &attrs))
    }

    /// Metadata of `path` itself; for a symlink, the link (with its target filled in).
    pub async fn lstat(&self, path: &str) -> Result<Entry, SftpError> {
        let mut entry = self.lstat_plain(path).await?;
        if entry.kind == EntryKind::Symlink {
            entry.link = Some(self.describe_link(path).await);
        }
        Ok(entry)
    }

    async fn lstat_plain(&self, path: &str) -> Result<Entry, SftpError> {
        let attrs = self.raw.lstat(path).await.map_err(|err| SftpError::from_raw(err, path))?.attrs;
        Ok(entry_at(path, "", &attrs))
    }

    /// Where the link at `path` points and what is there.
    async fn describe_link(&self, path: &str) -> Link {
        let target = match self.raw.readlink(path).await {
            Ok(name) => name.files.into_iter().next().map(|file| file.filename).unwrap_or_default(),
            Err(_) => String::new(),
        };
        let resolved = match self.raw.stat(path).await {
            Ok(attrs) => Some(attrs.attrs.permissions.map_or(EntryKind::Other, EntryKind::from_mode)),
            Err(_) => None,
        };
        Link { target, resolved }
    }

    /// The file `path` really is: itself, or where a symlink (chain) ends, with its metadata.
    pub async fn resolve(&self, path: &str) -> Result<(String, Entry), SftpError> {
        let first = self.lstat_plain(path).await?;
        if first.kind != EntryKind::Symlink {
            return Ok((path.to_string(), first));
        }
        // Let the server resolve it; if it hands back another link (servers that only normalize
        // paths), follow the links ourselves.
        let mut target = self.canonicalize(path).await?;
        for _ in 0..16 {
            let entry = self.stat(&target).await?;
            let here = self.lstat_plain(&target).await?;
            if here.kind != EntryKind::Symlink {
                return Ok((target, entry));
            }
            let link = self.raw.readlink(&target).await.map_err(|err| SftpError::from_raw(err, &target))?;
            let next = link.files.into_iter().next().map(|file| file.filename).unwrap_or_default();
            target = path::normalize(&path::join(&path::parent(&target).unwrap_or_else(|| "/".into()), &next));
        }
        Err(SftpError::Failed { path: path.to_string(), message: "符号链接层数过多".into() })
    }

    // ---- directories -------------------------------------------------------------------------

    /// The entries of a directory, directories first and then by name (case-insensitive, numbers
    /// by value). Hidden files are included; `.` and `..` are not. Links carry their target.
    pub async fn list(&self, dir: &str) -> Result<Vec<Entry>, SftpError> {
        let handle = match self.raw.opendir(dir).await {
            Ok(handle) => handle.handle,
            Err(err) => return Err(self.explain_dir_error(dir, err).await),
        };
        let mut files = Vec::new();
        let read = loop {
            match self.raw.readdir(handle.as_str()).await {
                Ok(batch) => files.extend(batch.files),
                Err(RawError::Status(status)) if status.status_code == StatusCode::Eof => break Ok(()),
                Err(err) => break Err(SftpError::from_raw(err, dir)),
            }
        };
        let _ = self.raw.close(handle).await;
        read?;

        let mut entries: Vec<Entry> = files
            .iter()
            .filter(|file| file.filename != "." && file.filename != "..")
            .map(|file| Entry::from_attrs(dir, &file.filename, &file.longname, &file.attrs))
            .collect();
        let links: Vec<(usize, String)> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.kind == EntryKind::Symlink)
            .map(|(ix, entry)| (ix, entry.path.clone()))
            .collect();
        let described: Vec<(usize, Link)> = stream::iter(links)
            .map(|(ix, path)| async move { (ix, self.describe_link(&path).await) })
            .buffer_unordered(LINK_PARALLELISM)
            .collect()
            .await;
        for (ix, link) in described {
            entries[ix].link = Some(link);
        }
        sort_entries(&mut entries);
        Ok(entries)
    }

    /// SFTP v3 has no "not a directory" status (OpenSSH answers "no such file" or "failure"):
    /// ask what the path is to say something useful.
    async fn explain_dir_error(&self, dir: &str, err: RawError) -> SftpError {
        let error = SftpError::from_raw(err, dir);
        if matches!(error, SftpError::Failed { .. } | SftpError::NotFound { .. })
            && let Ok(entry) = self.stat(dir).await
            && !entry.is_dir()
        {
            return SftpError::NotADirectory { path: dir.to_string() };
        }
        error
    }

    /// Creates a directory. Fails with [`SftpError::AlreadyExists`] if something is there.
    pub async fn mkdir(&self, dir: &str) -> Result<(), SftpError> {
        self.ensure_absent(dir).await?;
        self.raw.mkdir(dir, FileAttributes::empty()).await.map_err(|err| SftpError::from_raw(err, dir))?;
        Ok(())
    }

    /// Renames `from` to `to`. Never overwrites: if `to` exists, fails with
    /// [`SftpError::AlreadyExists`] (SFTP leaves it to the server whether a rename replaces; some do).
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), SftpError> {
        if from == to {
            return Ok(());
        }
        self.ensure_absent(to).await?;
        self.raw.rename(from, to).await.map_err(|err| SftpError::from_raw(err, from))?;
        Ok(())
    }

    /// Sets the permission bits (`0o644`) of a file or directory (of the target, for a link).
    pub async fn chmod(&self, path: &str, mode: u32) -> Result<(), SftpError> {
        let attrs = FileAttributes { permissions: Some(mode & 0o7777), ..FileAttributes::empty() };
        self.raw.setstat(path, attrs).await.map_err(|err| SftpError::from_raw(err, path))?;
        Ok(())
    }

    /// Deletes a file or link (not following it) or a directory with everything in it.
    /// Refuses the root and the current directory.
    pub async fn remove(&self, path: &str) -> Result<(), SftpError> {
        let normalized = path::normalize(path);
        if normalized == "/" || normalized == "." {
            return Err(SftpError::Refused { path: path.to_string() });
        }
        self.remove_tree(&normalized).await
    }

    fn remove_tree<'a>(
        &'a self,
        path: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SftpError>> + Send + 'a>> {
        Box::pin(async move {
            let entry = self.lstat_plain(path).await?;
            if entry.kind != EntryKind::Dir {
                return self.remove_file(path).await;
            }
            for child in self.list(path).await? {
                self.remove_tree(&child.path).await?;
            }
            self.raw.rmdir(path).await.map_err(|err| SftpError::from_raw(err, path))?;
            Ok(())
        })
    }

    /// Deletes a single file or link.
    pub async fn remove_file(&self, path: &str) -> Result<(), SftpError> {
        self.raw.remove(path).await.map_err(|err| SftpError::from_raw(err, path))?;
        Ok(())
    }

    async fn ensure_absent(&self, path: &str) -> Result<(), SftpError> {
        match self.lstat_plain(path).await {
            Ok(_) => Err(SftpError::AlreadyExists { path: path.to_string() }),
            Err(SftpError::NotFound { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    // ---- replacing a file ----------------------------------------------------------------------

    /// Makes `temp` the file at `target`, replacing what is there without a moment in which
    /// `target` is missing or half written.
    ///
    /// With `posix-rename@openssh.com` that is one request. Otherwise the old file is moved aside
    /// (`.<name>.shellmate-bak`), the new one moved in, and the old one deleted; if moving the new
    /// one in fails, the old one is moved back. `temp` is left alone on failure.
    pub async fn replace_file(&self, temp: &str, target: &str) -> Result<(), SftpError> {
        if self.info.posix_rename() {
            return self.posix_rename(temp, target).await;
        }
        let exists = match self.lstat_plain(target).await {
            Ok(_) => true,
            Err(SftpError::NotFound { .. }) => false,
            Err(err) => return Err(err),
        };
        if !exists {
            return self.raw.rename(temp, target).await.map(|_| ()).map_err(|err| SftpError::from_raw(err, target));
        }
        let backup = crate::text::sibling(target, |name| format!(".{name}.shellmate-bak"));
        // A backup left by an earlier crash would block the move: it is older than the file we hold.
        let _ = self.remove_file(&backup).await;
        self.raw.rename(target, &backup).await.map_err(|err| SftpError::from_raw(err, target))?;
        match self.raw.rename(temp, target).await {
            Ok(_) => {
                if let Err(err) = self.remove_file(&backup).await {
                    log::warn!("could not delete the backup {backup}: {err}");
                }
                Ok(())
            }
            Err(err) => {
                let error = SftpError::from_raw(err, target);
                match self.raw.rename(&backup, target).await {
                    Ok(_) => Err(error),
                    Err(restore) => Err(SftpError::Failed {
                        path: target.to_string(),
                        message: format!("替换失败（{error}），且无法还原原文件，原文件保存在 {backup}（{restore}）"),
                    }),
                }
            }
        }
    }

    async fn posix_rename(&self, from: &str, to: &str) -> Result<(), SftpError> {
        let mut data = Vec::with_capacity(8 + from.len() + to.len());
        for text in [from, to] {
            data.extend_from_slice(&(text.len() as u32).to_be_bytes());
            data.extend_from_slice(text.as_bytes());
        }
        match self.raw.extended(POSIX_RENAME, data).await.map_err(|err| SftpError::from_raw(err, to))? {
            Packet::Status(status) if status.status_code == StatusCode::Ok => Ok(()),
            Packet::Status(status) => Err(SftpError::from_raw(RawError::Status(status), to)),
            _ => Err(SftpError::Protocol("服务器对 posix-rename 返回了意外的数据包".into())),
        }
    }

    // ---- file contents -------------------------------------------------------------------------

    pub(crate) async fn open_handle(
        &self,
        path: &str,
        flags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<String, SftpError> {
        let handle = self.raw.open(path, flags, attrs).await.map_err(|err| SftpError::from_raw(err, path))?;
        Ok(handle.handle)
    }

    pub(crate) async fn close_handle(&self, handle: &str, path: &str) -> Result<(), SftpError> {
        self.raw.close(handle).await.map_err(|err| SftpError::from_raw(err, path))?;
        Ok(())
    }

    /// Reads a whole file (at most `max` bytes: bigger ones fail with [`SftpError::TooLarge`]) and
    /// returns what it read together with the revision the server reported for it.
    pub async fn read_file(&self, path: &str, max: u64) -> Result<(Vec<u8>, Revision), SftpError> {
        let handle = self.open_handle(path, OpenFlags::READ, FileAttributes::empty()).await?;
        let read = self.read_open_file(&handle, path, max).await;
        let _ = self.raw.close(handle).await;
        read
    }

    async fn read_open_file(&self, handle: &str, path: &str, max: u64) -> Result<(Vec<u8>, Revision), SftpError> {
        let attrs = self.raw.fstat(handle).await.map_err(|err| SftpError::from_raw(err, path))?.attrs;
        let size = attrs.size.unwrap_or(0);
        if size > max {
            return Err(SftpError::TooLarge { size });
        }
        let mut buf = Vec::with_capacity(size as usize);
        'reading: while (buf.len() as u64) < size {
            let start = buf.len() as u64;
            let requests: Vec<(u64, u32)> = (0..WINDOW as u64)
                .map(|i| start + i * CHUNK as u64)
                .take_while(|offset| *offset < size)
                .map(|offset| (offset, (size - offset).min(CHUNK as u64) as u32))
                .collect();
            let results =
                futures::future::join_all(requests.iter().map(|(offset, len)| self.raw.read(handle, *offset, *len)))
                    .await;
            for ((_, len), result) in requests.iter().zip(results) {
                match result {
                    Ok(data) => {
                        let got = data.data.len();
                        buf.extend_from_slice(&data.data);
                        // A short answer (allowed by the protocol) breaks the run of offsets:
                        // ask again from where we are.
                        if got < *len as usize {
                            if got == 0 {
                                break 'reading;
                            }
                            continue 'reading;
                        }
                    }
                    Err(RawError::Status(status)) if status.status_code == StatusCode::Eof => break 'reading,
                    Err(err) => return Err(SftpError::from_raw(err, path)),
                }
            }
        }
        Ok((buf, Revision { size, mtime: attrs.mtime.map(i64::from) }))
    }

    /// Writes `data` at the start of the open file `handle`.
    pub(crate) async fn write_all(&self, handle: &str, data: &[u8], path: &str) -> Result<(), SftpError> {
        let mut offset = 0usize;
        for batch in data.chunks(CHUNK * WINDOW) {
            let writes = batch
                .chunks(CHUNK)
                .enumerate()
                .map(|(i, chunk)| self.raw.write(handle, (offset + i * CHUNK) as u64, chunk.to_vec()));
            futures::future::try_join_all(writes).await.map_err(|err| SftpError::from_raw(err, path))?;
            offset += batch.len();
        }
        Ok(())
    }

    pub(crate) async fn set_handle_mode(&self, handle: &str, mode: u32, path: &str) -> Result<(), SftpError> {
        let attrs = FileAttributes { permissions: Some(mode & 0o7777), ..FileAttributes::empty() };
        self.raw.fsetstat(handle, attrs).await.map_err(|err| SftpError::from_raw(err, path))?;
        Ok(())
    }

    /// Gives the file the owner of `original`; only root may, so a refusal is ignored.
    pub(crate) async fn try_set_handle_owner(&self, handle: &str, original: Option<&Entry>) {
        let Some(Entry { uid: Some(uid), gid: Some(gid), .. }) = original else { return };
        let attrs = FileAttributes { uid: Some(*uid), gid: Some(*gid), ..FileAttributes::empty() };
        let _ = self.raw.fsetstat(handle, attrs).await;
    }

    /// Asks the server to flush the file to disk, if it can.
    pub(crate) async fn sync_handle(&self, handle: &str) {
        if self.info.has(FSYNC) {
            let _ = self.raw.fsync(handle).await;
        }
    }
}

/// The entry for `path`, named by its last component.
fn entry_at(path: &str, longname: &str, attrs: &FileAttributes) -> Entry {
    let name = path::file_name(path).unwrap_or("/");
    let dir = path::parent(path).unwrap_or_else(|| "/".to_string());
    let mut entry = Entry::from_attrs(&dir, name, longname, attrs);
    entry.path = path.to_string();
    entry
}
