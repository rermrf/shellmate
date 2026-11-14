//! The SFTP session and the operations on it that the file panel and the editor need.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::stream::{self, StreamExt as _};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as RawError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode};
use shellmate_ssh::Connection;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::entry::{Entry, EntryKind, Link, Revision, sort_entries};
use crate::error::SftpError;
use crate::path;

/// Size of one read or write request when the server names no limit.
const DEFAULT_CHUNK: usize = 32 * 1024;
/// Largest request we send, whatever the server allows.
const MAX_CHUNK: usize = 255 * 1024;
/// Roughly how many bytes of requests are in flight at once for one file: enough to keep a fast
/// link with some delay busy, little enough that a pause or cancel takes effect soon.
const IN_FLIGHT_BYTES: usize = 2 * 1024 * 1024;
/// Size of one read or write request for small files (text for the editor).
const CHUNK: usize = DEFAULT_CHUNK;
/// Requests in flight at once for one file.
const WINDOW: usize = 8;
/// Seconds a request may wait for its answer.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Links followed before giving up (a loop of links).
pub(crate) const MAX_LINK_HOPS: usize = 16;
/// Links resolved at the same time when listing a directory.
const LINK_PARALLELISM: usize = 16;

/// `posix-rename@openssh.com`: rename that replaces an existing target atomically.
pub const POSIX_RENAME: &str = "posix-rename@openssh.com";
const FSYNC: &str = "fsync@openssh.com";
const LIMITS: &str = "limits@openssh.com";

/// Request size limits of the server (`limits@openssh.com`); `None` where it names none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServerLimits {
    /// Largest packet.
    pub packet_len: Option<u64>,
    /// Largest read request.
    pub read_len: Option<u64>,
    /// Largest write request.
    pub write_len: Option<u64>,
}

/// What the server told us in its version reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// The SFTP protocol version.
    pub version: u32,
    /// Advertised extensions with their versions (`posix-rename@openssh.com` → `1`).
    pub extensions: BTreeMap<String, String>,
    /// The server's request size limits, if it advertises them.
    pub limits: Option<ServerLimits>,
}

/// How big and how many requests to keep in flight when moving file contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tuning {
    /// Bytes per read request.
    pub read_chunk: usize,
    /// Bytes per write request.
    pub write_chunk: usize,
    /// Read requests in flight.
    pub read_window: usize,
    /// Write requests in flight.
    pub write_window: usize,
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

    /// Request sizes for moving file contents: as large as the server allows (but no more than
    /// 255 KB, what OpenSSH allows), 32 KB when it names no limit, with about 2 MB in flight.
    pub(crate) fn tuning(&self) -> Tuning {
        let limits = self.limits.unwrap_or_default();
        let size = |limit: Option<u64>| limit.map_or(DEFAULT_CHUNK, |limit| limit.clamp(1, MAX_CHUNK as u64) as usize);
        let window = |chunk: usize| (IN_FLIGHT_BYTES / chunk).clamp(8, 32);
        let (read_chunk, write_chunk) = (size(limits.read_len), size(limits.write_len));
        Tuning { read_chunk, write_chunk, read_window: window(read_chunk), write_window: window(write_chunk) }
    }
}

/// An SFTP connection to one server.
///
/// Created on the SSH runtime (the client library drives the stream with tasks of the current
/// tokio runtime); the methods are plain async functions and may be awaited from anywhere.
pub struct SftpSession {
    pub(crate) raw: Arc<RawSftpSession>,
    info: ServerInfo,
    timeout_secs: AtomicU64,
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
        let mut raw = raw;
        let version = raw.init().await.map_err(|err| SftpError::from_raw(err, ""))?;
        let extensions: BTreeMap<String, String> = version.extensions.into_iter().collect();
        let mut limits = None;
        if extensions.get(LIMITS).is_some_and(|version| version == "1") {
            match raw.limits().await {
                Ok(found) => {
                    let known = Limits::from(found);
                    limits = Some(ServerLimits {
                        packet_len: known.packet_len,
                        read_len: known.read_len,
                        write_len: known.write_len,
                    });
                    raw.set_limits(known);
                }
                // Not fatal: without limits we send modest requests.
                Err(err) => log::warn!("limits@openssh.com failed: {err}"),
            }
        }
        let info = ServerInfo { version: version.version, extensions, limits };
        Ok(Self { raw: Arc::new(raw), info, timeout_secs: AtomicU64::new(REQUEST_TIMEOUT_SECS) })
    }

    /// How long a request may wait for its answer before the session counts as stalled
    /// ([`SftpError::Timeout`]); 30 seconds by default.
    pub fn set_request_timeout(&self, seconds: u64) {
        self.timeout_secs.store(seconds, Ordering::Relaxed);
        self.raw.set_timeout(seconds);
    }

    /// The current request timeout.
    pub(crate) fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.load(Ordering::Relaxed))
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

    pub(crate) async fn lstat_plain(&self, path: &str) -> Result<Entry, SftpError> {
        let attrs = self.raw.lstat(path).await.map_err(|err| SftpError::from_raw(err, path))?.attrs;
        Ok(entry_at(path, "", &attrs))
    }

    /// The text of the symlink at `path`.
    pub(crate) async fn read_link(&self, path: &str) -> Result<String, SftpError> {
        let name = self.raw.readlink(path).await.map_err(|err| SftpError::from_raw(err, path))?;
        Ok(name.files.into_iter().next().map(|file| file.filename).unwrap_or_default())
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
        for _ in 0..MAX_LINK_HOPS {
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

    /// The file a write to `path` goes to and what is there now: a symlink is followed to its
    /// target, a dangling one to the place it points at (so the link stays a link); a path with
    /// nothing at it is its own destination. The entry is `None` when nothing is there yet.
    pub(crate) async fn resolve_for_write(&self, path: &str) -> Result<(String, Option<Entry>), SftpError> {
        match self.resolve(path).await {
            Ok((target, entry)) => Ok((target, Some(entry))),
            Err(SftpError::NotFound { .. }) => Ok((self.creation_target(path).await?, None)),
            Err(err) => Err(err),
        }
    }

    /// Where a file written to `start` has to be created when nothing is there: `start` itself,
    /// unless it is a (dangling) symlink, in which case the link must stay a link and the file goes
    /// where it points. Follows the chain of links, each one relative to the directory of the
    /// previous.
    pub(crate) async fn creation_target(&self, start: &str) -> Result<String, SftpError> {
        let mut current = start.to_string();
        for _ in 0..MAX_LINK_HOPS {
            match self.lstat_plain(&current).await {
                Ok(entry) if entry.kind == EntryKind::Symlink => {
                    let target = self.read_link(&current).await?;
                    let base = path::parent(&current).unwrap_or_else(|| "/".to_string());
                    current = path::normalize(&path::join(&base, &target));
                }
                Ok(_) | Err(SftpError::NotFound { .. }) => return Ok(current),
                Err(err) => return Err(err),
            }
        }
        Err(SftpError::Failed { path: start.to_string(), message: "符号链接层数过多".into() })
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

    /// Creates a directory with the given permission bits (`0o755`; applied for real after
    /// creation, because the server's umask applies to the mode given at creation). Fails if
    /// something is there already.
    pub async fn mkdir_with_mode(&self, dir: &str, mode: u32) -> Result<(), SftpError> {
        self.ensure_absent(dir).await?;
        let attrs = FileAttributes { permissions: Some(mode & 0o7777), ..FileAttributes::empty() };
        self.raw.mkdir(dir, attrs).await.map_err(|err| SftpError::from_raw(err, dir))?;
        self.chmod(dir, mode).await
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

    /// Checks that we may write to the existing file at `path`, without changing it: opens it for
    /// writing (no create, no truncate) and closes it again. Replacing a file by renaming another
    /// over it needs no permission on the file itself, so anything that replaces files has to ask
    /// this first, or it would write through a read-only file's back.
    ///
    /// Fails with [`SftpError::PermissionDenied`] if the server refuses.
    pub async fn ensure_writable(&self, path: &str) -> Result<(), SftpError> {
        let handle = self.open_handle(path, OpenFlags::WRITE, FileAttributes::empty()).await?;
        self.close_handle(&handle, path).await
    }

    /// Creates a new file at `path` and opens it for writing. The name must not be in use: the
    /// file is created exclusively, so an existing file is never truncated and a symlink planted
    /// at the name is never followed (which would send our write to another file).
    ///
    /// If something is there, it is unlinked (unlinking does not follow links) and the creation
    /// retried once. If that is refused (a sticky directory with someone else's file), the error is
    /// [`SftpError::PermissionDenied`], as for a directory that does not allow new files.
    pub(crate) async fn create_exclusive(&self, path: &str, attrs: FileAttributes) -> Result<String, SftpError> {
        let flags = OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE;
        let first = match self.open_handle(path, flags, attrs.clone()).await {
            Ok(handle) => return Ok(handle),
            Err(error) => error,
        };
        // SFTP v3 has no "already exists" status (OpenSSH says "failure"): look.
        if !matches!(first, SftpError::Failed { .. }) || self.lstat_plain(path).await.is_err() {
            return Err(first);
        }
        self.remove_file(path).await?;
        self.open_handle(path, flags, attrs).await
    }

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

    /// Sets attributes (mode, times, ...) on an open file.
    pub(crate) async fn set_handle_attrs(
        &self,
        handle: &str,
        attrs: FileAttributes,
        path: &str,
    ) -> Result<(), SftpError> {
        self.raw.fsetstat(handle, attrs).await.map_err(|err| SftpError::from_raw(err, path))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info(limits: Option<ServerLimits>) -> ServerInfo {
        ServerInfo { version: 3, extensions: BTreeMap::new(), limits }
    }

    #[test]
    fn without_limits_requests_are_32_kb_with_a_wide_window() {
        let tuning = info(None).tuning();
        assert_eq!((tuning.read_chunk, tuning.write_chunk), (32 * 1024, 32 * 1024));
        assert_eq!((tuning.read_window, tuning.write_window), (32, 32));
    }

    #[test]
    fn requests_are_as_large_as_the_server_allows_up_to_255_kb() {
        let openssh = ServerLimits { packet_len: Some(262_144), read_len: Some(261_120), write_len: Some(261_120) };
        let tuning = info(Some(openssh)).tuning();
        assert_eq!((tuning.read_chunk, tuning.write_chunk), (255 * 1024, 255 * 1024), "capped at 255 KB");
        assert_eq!((tuning.read_window, tuning.write_window), (8, 8), "about 2 MB in flight");
        let huge = ServerLimits { packet_len: None, read_len: Some(10_000_000), write_len: Some(10_000_000) };
        assert_eq!(info(Some(huge)).tuning().read_chunk, 255 * 1024);
    }

    #[test]
    fn small_limits_are_respected_and_reads_and_writes_are_separate() {
        let limits = ServerLimits { packet_len: None, read_len: Some(16_384), write_len: Some(4_096) };
        let tuning = info(Some(limits)).tuning();
        assert_eq!((tuning.read_chunk, tuning.write_chunk), (16_384, 4_096));
        assert_eq!((tuning.read_window, tuning.write_window), (32, 32), "never more than 32 requests");
        let only_reads = ServerLimits { packet_len: None, read_len: Some(65_536), write_len: None };
        let tuning = info(Some(only_reads)).tuning();
        assert_eq!((tuning.read_chunk, tuning.write_chunk), (65_536, 32 * 1024));
    }
}
