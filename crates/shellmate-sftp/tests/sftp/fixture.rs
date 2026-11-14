//! An SFTP server for the tests: the `sftp` subsystem of the in-process SSH server, served by
//! `russh_sftp::server` over a temporary directory that it never leaves.
//!
//! It behaves like OpenSSH where it matters to the client: `FAILURE` for a rename onto an
//! existing file or a mkdir of an existing directory, no uid change without privilege, the
//! process umask on create. Options switch the features the client has to cope with.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::{self, Metadata, OpenOptions};
use std::os::unix::fs::{FileExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh_sftp::protocol::{
    Attrs, Data, File as SftpFile, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode, Version,
};
use russh_sftp::server::{Handler, StatusReply};
use shellmate_core::AuthMethod;
use shellmate_sftp::SftpSession;
use shellmate_ssh::{Connection, Route, Secret, SshHost};

use crate::support::{Env, ServerOptions, SubsystemRunner, TestServer};

/// How the server words the `ls -l` line it sends with every directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Longnames {
    /// `-rw-r--r-- 1 tester staff ...`
    Names,
    /// `-rw-r--r-- 1 1000 1000 ...` (numeric ids, as some servers do)
    Numeric,
    /// Nothing at all.
    Missing,
}

/// What the server can do and where it misbehaves.
#[derive(Clone, Debug)]
pub struct SftpOptions {
    /// Advertise and serve `posix-rename@openssh.com`.
    pub posix_rename: bool,
    /// Directories do not allow creating, renaming or removing anything in them (existing files
    /// can still be written).
    pub dir_unwritable: bool,
    /// A plain rename onto an existing path replaces it (some servers do) instead of failing.
    pub plain_rename_replaces: bool,
    /// Renames (plain or posix) whose source path ends with this fail.
    pub fail_rename_from: Option<&'static str>,
    /// Changing the owner of a file is refused (as for any user but root).
    pub deny_chown: bool,
    /// Paths ending with this report the given uid and gid (a file somebody else owns).
    pub fake_owner: Option<(&'static str, u32, u32)>,
    /// Entries per `readdir` reply.
    pub page: usize,
    /// The format of the `longname` of directory entries.
    pub longnames: Longnames,
    /// Where `realpath(".")` leads, relative to the root.
    pub home: &'static str,
}

impl Default for SftpOptions {
    fn default() -> Self {
        Self {
            posix_rename: true,
            dir_unwritable: false,
            plain_rename_replaces: false,
            fail_rename_from: None,
            deny_chown: false,
            fake_owner: None,
            page: 7,
            longnames: Longnames::Names,
            home: "/home/tester",
        }
    }
}

enum Open {
    File { file: fs::File, path: PathBuf },
    Dir { entries: Vec<SftpFile>, sent: usize },
}

struct FsHandler {
    root: PathBuf,
    options: SftpOptions,
    handles: HashMap<String, Open>,
    next: u64,
}

fn status(id: u32) -> Status {
    Status { id, status_code: StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() }
}

/// OpenSSH's errno mapping: "not a directory" is reported as "no such file".
fn reply(error: std::io::Error) -> StatusReply {
    use std::io::ErrorKind::*;
    match error.kind() {
        NotFound | NotADirectory => StatusCode::NoSuchFile.into(),
        PermissionDenied => StatusCode::PermissionDenied.into(),
        _ => match error.raw_os_error() {
            Some(libc::EPERM) => StatusCode::PermissionDenied.into(),
            Some(libc::ELOOP | libc::EBADF) => StatusCode::NoSuchFile.into(),
            _ => StatusCode::Failure.into(),
        },
    }
}

fn attrs_of(metadata: &Metadata) -> FileAttributes {
    FileAttributes {
        size: Some(metadata.len()),
        uid: Some(metadata.uid()),
        gid: Some(metadata.gid()),
        permissions: Some(metadata.mode()),
        atime: Some(metadata.atime() as u32),
        mtime: Some(metadata.mtime() as u32),
        ..Default::default()
    }
}

fn mode_text(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        _ => '-',
    };
    let bits: String =
        "rwxrwxrwx".chars().enumerate().map(|(i, c)| if mode & (0o400 >> i) != 0 { c } else { '-' }).collect();
    format!("{kind}{bits}")
}

impl FsHandler {
    fn new(root: PathBuf, options: SftpOptions) -> Self {
        Self { root, options, handles: HashMap::new(), next: 0 }
    }

    /// The path on the host for a path asked for by the client; `..` cannot climb out of the root.
    fn host(&self, remote: &str) -> PathBuf {
        let absolute =
            if remote.starts_with('/') { remote.to_string() } else { format!("{}/{remote}", self.options.home) };
        let relative = shellmate_sftp::path::normalize(&absolute);
        self.root.join(relative.trim_start_matches('/'))
    }

    fn remote(&self, host: &Path) -> String {
        let relative = host.strip_prefix(&self.root).unwrap_or(Path::new(""));
        format!("/{}", relative.display())
    }

    /// The attributes of the file at `path`, with the pretend owner if one is configured for it.
    fn attrs(&self, path: &Path, metadata: &Metadata) -> FileAttributes {
        let mut attrs = attrs_of(metadata);
        if let Some((suffix, uid, gid)) = self.options.fake_owner
            && path.to_string_lossy().ends_with(suffix)
        {
            (attrs.uid, attrs.gid) = (Some(uid), Some(gid));
        }
        attrs
    }

    fn entry(&self, path: &Path, name: &str, metadata: &Metadata) -> SftpFile {
        let attrs = self.attrs(path, metadata);
        let longname = match self.options.longnames {
            Longnames::Names => {
                format!("{} 1 tester staff {} Nov 12 08:55 {name}", mode_text(metadata.mode()), metadata.len())
            }
            Longnames::Numeric => {
                format!(
                    "{} 1 {} {} {} Nov 12 08:55 {name}",
                    mode_text(metadata.mode()),
                    metadata.uid(),
                    metadata.gid(),
                    metadata.len()
                )
            }
            Longnames::Missing => String::new(),
        };
        SftpFile { filename: name.to_string(), longname, attrs }
    }

    fn new_handle(&mut self, open: Open) -> String {
        self.next += 1;
        let handle = format!("h{}", self.next);
        self.handles.insert(handle.clone(), open);
        handle
    }

    fn deny_if_unwritable(&self) -> Result<(), StatusReply> {
        if self.options.dir_unwritable { Err(StatusCode::PermissionDenied.into()) } else { Ok(()) }
    }

    fn rename_to(&self, from: &str, to: &str, replace: bool) -> Result<(), StatusReply> {
        self.deny_if_unwritable()?;
        if self.options.fail_rename_from.is_some_and(|suffix| from.ends_with(suffix)) {
            return Err(StatusCode::Failure.with_message("injected failure"));
        }
        let (from, to) = (self.host(from), self.host(to));
        if !replace && fs::symlink_metadata(&to).is_ok() {
            return Err(StatusCode::Failure.into());
        }
        fs::rename(from, to).map_err(reply)
    }

    fn apply(&self, path: &Path, attrs: &FileAttributes) -> Result<(), StatusReply> {
        if let Some(mode) = attrs.permissions {
            fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777)).map_err(reply)?;
        }
        if attrs.uid.is_some() || attrs.gid.is_some() {
            if self.options.deny_chown {
                return Err(StatusCode::PermissionDenied.into());
            }
            std::os::unix::fs::chown(path, attrs.uid, attrs.gid).map_err(reply)?;
        }
        if let Some(mtime) = attrs.mtime {
            let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime.into());
            OpenOptions::new().write(true).open(path).and_then(|file| file.set_modified(time)).map_err(reply)?;
        }
        Ok(())
    }
}

impl Handler for FsHandler {
    type Error = StatusReply;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported.into()
    }

    async fn init(&mut self, _version: u32, _extensions: HashMap<String, String>) -> Result<Version, Self::Error> {
        let mut version = Version::new();
        version.extensions.insert("fsync@openssh.com".into(), "1".into());
        if self.options.posix_rename {
            version.extensions.insert("posix-rename@openssh.com".into(), "1".into());
        }
        Ok(version)
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = self.host(&filename);
        if pflags.contains(OpenFlags::CREATE) && !path.exists() {
            self.deny_if_unwritable()?;
        }
        let mut options: OpenOptions = pflags.into();
        options.mode(attrs.permissions.map_or(0o666, |mode| mode & 0o7777));
        let file = options.open(&path).map_err(reply)?;
        let handle = self.new_handle(Open::File { file, path });
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle).map(|_| status(id)).ok_or_else(|| StatusCode::Failure.into())
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        let Some(Open::File { file, .. }) = self.handles.get(&handle) else { return Err(StatusCode::Failure.into()) };
        let mut buf = vec![0; len as usize];
        let got = file.read_at(&mut buf, offset).map_err(reply)?;
        if got == 0 && len > 0 {
            return Err(StatusCode::Eof.into());
        }
        buf.truncate(got);
        Ok(Data { id, data: buf })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        let Some(Open::File { file, .. }) = self.handles.get(&handle) else { return Err(StatusCode::Failure.into()) };
        file.write_all_at(&data, offset).map_err(reply)?;
        Ok(status(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let host = self.host(&path);
        let metadata = fs::symlink_metadata(&host).map_err(reply)?;
        Ok(Attrs { id, attrs: self.attrs(&host, &metadata) })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let host = self.host(&path);
        let metadata = fs::metadata(&host).map_err(reply)?;
        Ok(Attrs { id, attrs: self.attrs(&host, &metadata) })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let Some(Open::File { file, path }) = self.handles.get(&handle) else { return Err(StatusCode::Failure.into()) };
        Ok(Attrs { id, attrs: self.attrs(path, &file.metadata().map_err(reply)?) })
    }

    async fn setstat(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        self.apply(&self.host(&path), &attrs)?;
        Ok(status(id))
    }

    async fn fsetstat(&mut self, id: u32, handle: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        let Some(Open::File { path, .. }) = self.handles.get(&handle) else { return Err(StatusCode::Failure.into()) };
        self.apply(path, &attrs)?;
        Ok(status(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let dir = self.host(&path);
        let mut entries = Vec::new();
        for name in [".", ".."] {
            entries.push(self.entry(&dir, name, &fs::metadata(&dir).map_err(reply)?));
        }
        for item in fs::read_dir(&dir).map_err(reply)? {
            let item = item.map_err(reply)?;
            let metadata = fs::symlink_metadata(item.path()).map_err(reply)?;
            entries.push(self.entry(&item.path(), &item.file_name().to_string_lossy(), &metadata));
        }
        let handle = self.new_handle(Open::Dir { entries, sent: 0 });
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let page = self.options.page;
        let Some(Open::Dir { entries, sent }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure.into());
        };
        if *sent >= entries.len() {
            return Err(StatusCode::Eof.into());
        }
        let end = (*sent + page).min(entries.len());
        let files = entries[*sent..end].to_vec();
        *sent = end;
        Ok(Name { id, files })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        self.deny_if_unwritable()?;
        fs::remove_file(self.host(&filename)).map_err(reply)?;
        Ok(status(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
        self.deny_if_unwritable()?;
        fs::create_dir(self.host(&path)).map_err(reply)?;
        Ok(status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.deny_if_unwritable()?;
        fs::remove_dir(self.host(&path)).map_err(reply)?;
        Ok(status(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let host = self.host(&path);
        // Like OpenSSH: symlinks are resolved, and a last component that does not exist is fine.
        let resolved = match fs::canonicalize(&host) {
            Ok(real) => self.remote(&real),
            Err(_) => self.remote(&host),
        };
        Ok(Name { id, files: vec![SftpFile::dummy(resolved)] })
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, Self::Error> {
        self.rename_to(&oldpath, &newpath, self.options.plain_rename_replaces)?;
        Ok(status(id))
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let target = fs::read_link(self.host(&path)).map_err(reply)?;
        Ok(Name { id, files: vec![SftpFile::dummy(target.to_string_lossy())] })
    }

    async fn symlink(&mut self, id: u32, linkpath: String, targetpath: String) -> Result<Status, Self::Error> {
        self.deny_if_unwritable()?;
        std::os::unix::fs::symlink(targetpath, self.host(&linkpath)).map_err(reply)?;
        Ok(status(id))
    }

    async fn extended(&mut self, id: u32, request: String, data: Vec<u8>) -> Result<Packet, Self::Error> {
        match request.as_str() {
            "posix-rename@openssh.com" if self.options.posix_rename => {
                let mut rest = data.as_slice();
                let mut next = || -> Option<String> {
                    let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
                    let text = String::from_utf8(rest.get(4..4 + len)?.to_vec()).ok()?;
                    rest = &rest[4 + len..];
                    Some(text)
                };
                let (Some(from), Some(to)) = (next(), next()) else { return Err(StatusCode::BadMessage.into()) };
                self.rename_to(&from, &to, true)?;
                Ok(Packet::Status(status(id)))
            }
            "fsync@openssh.com" => {
                let handle = String::from_utf8_lossy(data.get(4..).unwrap_or_default()).into_owned();
                match self.handles.get(&handle) {
                    Some(Open::File { file, .. }) => file.sync_all().map_err(reply)?,
                    _ => return Err(StatusCode::Failure.into()),
                }
                Ok(Packet::Status(status(id)))
            }
            _ => Err(StatusCode::OpUnsupported.into()),
        }
    }
}

/// The `sftp` subsystem over `root`.
pub fn runner(root: PathBuf, options: SftpOptions) -> SubsystemRunner {
    Arc::new(move |stream| {
        let handler = FsHandler::new(root.clone(), options.clone());
        tokio::spawn(russh_sftp::server::run(stream, handler));
    })
}

/// A running server, a connection to it and an SFTP session, with the directory the server serves.
pub struct Fixture {
    pub session: SftpSession,
    pub connection: Connection,
    pub server: TestServer,
    /// The server's root on the host (canonical).
    pub root: PathBuf,
    _env: Env,
    _dir: tempfile::TempDir,
}

impl Fixture {
    pub async fn start(options: SftpOptions) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(options.home.trim_start_matches('/'))).unwrap();
        let server = TestServer::start(ServerOptions {
            subsystems: vec![("sftp", runner(root.clone(), options))],
            ..Default::default()
        })
        .await;
        let env = Env::new();
        let host =
            SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"));
        let connection = Connection::connect(Route::direct(host), env.trusting()).await.expect("connect");
        let session = SftpSession::open(&connection).await.expect("sftp session");
        Self { session, connection, server, root, _env: env, _dir: dir }
    }

    pub async fn default() -> Self {
        Self::start(SftpOptions::default()).await
    }

    /// The host path of a remote path.
    pub fn host(&self, remote: &str) -> PathBuf {
        self.root.join(remote.trim_start_matches('/'))
    }

    /// Creates a file (and its directories) with the content and mode.
    pub fn file(&self, remote: &str, content: impl AsRef<[u8]>, mode: u32) {
        let path = self.host(remote);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }

    pub fn dir(&self, remote: &str) {
        fs::create_dir_all(self.host(remote)).unwrap();
    }

    /// A symlink at `remote` pointing at `target` (relative to the link's directory).
    pub fn link(&self, remote: &str, target: &str) {
        std::os::unix::fs::symlink(target, self.host(remote)).unwrap();
    }

    pub fn read(&self, remote: &str) -> Vec<u8> {
        fs::read(self.host(remote)).unwrap()
    }

    pub fn text(&self, remote: &str) -> String {
        String::from_utf8(self.read(remote)).unwrap()
    }

    pub fn mode(&self, remote: &str) -> u32 {
        fs::metadata(self.host(remote)).unwrap().permissions().mode() & 0o7777
    }

    pub fn exists(&self, remote: &str) -> bool {
        fs::symlink_metadata(self.host(remote)).is_ok()
    }

    /// Names in a host directory, sorted (to check for leftovers such as temporary files).
    pub fn names(&self, remote: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.host(remote))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}
