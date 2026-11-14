//! Text files for the editor: reading with the checks the editor needs, and saving with an
//! atomic replace and conflict detection.

use russh_sftp::protocol::{FileAttributes, OpenFlags};

use crate::entry::{Entry, EntryKind, Revision};
use crate::error::SftpError;
use crate::path;
use crate::session::{MAX_LINK_HOPS, SftpSession};

/// The biggest file the editor opens.
pub const MAX_TEXT_SIZE: u64 = 5 * 1024 * 1024;

/// The temporary name used while saving: next to the target, so the final rename stays on one
/// filesystem.
pub fn save_temp_path(target: &str) -> String {
    sibling(target, |name| format!(".{name}.shellmate-edit"))
}

/// How lines of a file end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    /// `\n`
    Lf,
    /// `\r\n`
    CrLf,
}

impl LineEnding {
    /// `LF` or `CRLF`, for the status bar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
        }
    }
}

/// A text file as the editor sees it: always with `\n` line ends in [`text`](Self::text); how it
/// was stored is remembered in [`line_ending`](Self::line_ending) and [`bom`](Self::bom).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFile {
    /// The path that was opened.
    pub path: String,
    /// The file the text really lives in: the same path, or where a symlink points. Saving goes here.
    pub target: String,
    /// The contents, line ends normalized to `\n`, without a BOM.
    pub text: String,
    /// What the file looked like when it was read.
    pub revision: Revision,
    /// The file's line ending style.
    pub line_ending: LineEnding,
    /// Whether the file started with a UTF-8 byte order mark.
    pub bom: bool,
    /// Permission bits (`0o644`).
    pub mode: Option<u32>,
}

/// The format to write a text back in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveFormat {
    /// Line ending style.
    pub line_ending: LineEnding,
    /// Write a UTF-8 byte order mark first.
    pub bom: bool,
}

impl From<&TextFile> for SaveFormat {
    fn from(file: &TextFile) -> Self {
        Self { line_ending: file.line_ending, bom: file.bom }
    }
}

/// How a save reached the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    /// Temporary file, then replace: readers never see half a file.
    Atomic,
    /// The file itself was rewritten in place (not atomic), for the given reason.
    InPlace(InPlaceReason),
}

/// Why a save rewrote the file in place instead of swapping in a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InPlaceReason {
    /// The directory does not allow new files (or the temporary name could not be cleared).
    DirNotWritable,
    /// A new file would have a different owner or group than the original, and only root may
    /// change that; rewriting keeps them.
    KeepOwner,
}

impl InPlaceReason {
    /// The reason as the status line says it: 「已直接写回（目录不可写）」 / 「已直接写回（保留属主）」.
    pub fn label(self) -> &'static str {
        match self {
            Self::DirNotWritable => "目录不可写",
            Self::KeepOwner => "保留属主",
        }
    }
}

/// What came of a save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Written; this is the file's new state, to be used as the expected revision of the next save.
    Saved {
        /// The new size and time.
        revision: Revision,
        /// Whether the atomic path or the fallback was used.
        mode: WriteMode,
    },
    /// The file on the server changed since it was opened; nothing was written.
    Conflict {
        /// The file as it is now (`\n` line ends); `None` if it cannot be shown as text any more
        /// (too large, binary, not UTF-8).
        remote_text: Option<String>,
        /// Its modification time, seconds since the epoch.
        remote_mtime: Option<i64>,
        /// Its size and time, to overwrite knowingly.
        remote_revision: Revision,
    },
}

/// Turns file contents into text: `\n` line ends and no BOM, plus how it was stored. Rejects what
/// the editor does not open.
pub fn decode_text(bytes: &[u8]) -> Result<(String, LineEnding, bool), SftpError> {
    // UTF-16 text is full of NUL bytes: say what it is rather than "binary".
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Err(SftpError::NotUtf8);
    }
    if bytes.contains(&0) {
        return Err(SftpError::Binary);
    }
    let (bom, body) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };
    let text = std::str::from_utf8(body).map_err(|_| SftpError::NotUtf8)?;
    let line_ending = detect_line_ending(text);
    let text = match line_ending {
        LineEnding::CrLf => text.replace("\r\n", "\n"),
        LineEnding::Lf => text.to_string(),
    };
    Ok((text, line_ending, bom))
}

/// The style most lines use; CRLF only when it is not outnumbered by bare LF.
pub fn detect_line_ending(text: &str) -> LineEnding {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf > 0 && crlf >= lf { LineEnding::CrLf } else { LineEnding::Lf }
}

/// The bytes to store for `text` (which has `\n` line ends).
pub fn encode_text(text: &str, format: SaveFormat) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 3);
    if format.bom {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    match format.line_ending {
        LineEnding::Lf => out.extend_from_slice(text.as_bytes()),
        LineEnding::CrLf => {
            // Whatever line ends the text has, the file gets exactly one CRLF per line.
            out.extend_from_slice(text.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
        }
    }
    out
}

/// `<dir>/<make(name)>` next to `target`.
pub(crate) fn sibling(target: &str, make: impl FnOnce(&str) -> String) -> String {
    let name = path::file_name(target).unwrap_or("file");
    let dir = path::parent(target).unwrap_or_else(|| "/".to_string());
    path::join(&dir, &make(name))
}

impl SftpSession {
    /// Reads a text file for editing. A symlink is followed; the result says where the text
    /// really is. Fails with [`SftpError::TooLarge`], [`SftpError::Binary`] or
    /// [`SftpError::NotUtf8`] for files the editor does not open.
    pub async fn open_text(&self, path: &str) -> Result<TextFile, SftpError> {
        let (target, entry) = self.resolve(path).await?;
        match entry.kind {
            EntryKind::Dir => return Err(SftpError::IsADirectory { path: path.to_string() }),
            EntryKind::File => {}
            // Opening a pipe or device could block the server's SFTP process for good.
            _ => return Err(SftpError::NotAFile { path: path.to_string() }),
        }
        if entry.size > MAX_TEXT_SIZE {
            return Err(SftpError::TooLarge { size: entry.size });
        }
        let (bytes, revision) = self.read_file(&target, MAX_TEXT_SIZE).await?;
        let (text, line_ending, bom) = decode_text(&bytes)?;
        Ok(TextFile { path: path.to_string(), target, text, revision, line_ending, bom, mode: entry.mode })
    }

    /// Writes `text` to the file at `path` (following a symlink to its target).
    ///
    /// `expected` is the revision from when the file was opened: if the file on the server no
    /// longer matches, nothing is written and [`SaveOutcome::Conflict`] comes back. `None`
    /// overwrites whatever is there (the user chose "覆盖远端"), and creates the file if it is gone.
    ///
    /// The text is written to a temporary file next to the target and swapped in, keeping the
    /// target's permissions and (if the server lets us) its owner. If the directory does not allow
    /// a temporary file, the target is rewritten in place ([`WriteMode::InPlace`]).
    pub async fn save_text(
        &self,
        path: &str,
        text: &str,
        format: SaveFormat,
        expected: Option<Revision>,
    ) -> Result<SaveOutcome, SftpError> {
        let (target, current) = match self.resolve(path).await {
            Ok((target, entry)) => (target, Some(entry)),
            // Nothing there yet (or a link to nothing): create the file where the path leads.
            Err(SftpError::NotFound { .. }) if expected.is_none() => (self.creation_target(path).await?, None),
            Err(err) => return Err(err),
        };
        if let Some(entry) = &current {
            match entry.kind {
                EntryKind::Dir => return Err(SftpError::IsADirectory { path: path.to_string() }),
                EntryKind::File => {}
                _ => return Err(SftpError::NotAFile { path: path.to_string() }),
            }
            if let Some(expected) = expected
                && entry.revision() != expected
            {
                return Ok(self.conflict(&target, entry).await);
            }
            // Swapping a new file in would bypass the file's own permissions: only save what we
            // could write.
            self.ensure_writable(&target).await?;
        }

        let bytes = encode_text(text, format);
        let temp = save_temp_path(&target);
        match self.write_temp(&temp, &bytes, current.as_ref()).await.map_err(|err| err.at(&target))? {
            Temp::Ready => {
                if let Err(err) = self.replace_file(&temp, &target).await {
                    let _ = self.remove_file(&temp).await;
                    return Err(err.at(&target));
                }
                let revision = self.stat(&target).await?.revision();
                Ok(SaveOutcome::Saved { revision, mode: WriteMode::Atomic })
            }
            // No room for a temporary file next to the target: rewrite the target itself.
            Temp::DirNotWritable if current.is_some() => {
                self.rewrite_in_place(&target, &bytes, InPlaceReason::DirNotWritable).await
            }
            Temp::DirNotWritable => Err(SftpError::PermissionDenied { path: target.clone() }),
            // The new file would belong to somebody else: rewrite the original, which keeps its owner.
            Temp::OwnerDiffers => self.rewrite_in_place(&target, &bytes, InPlaceReason::KeepOwner).await,
        }
    }

    /// Where a file saved to `path` has to be created when nothing is there: `path` itself, unless
    /// it is a (dangling) symlink, in which case the link must stay a link and the file goes where
    /// it points. Follows the chain of links, each one relative to the directory of the previous.
    async fn creation_target(&self, start: &str) -> Result<String, SftpError> {
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

    /// Rewrites the file at `target` itself and reports the new revision.
    async fn rewrite_in_place(
        &self,
        target: &str,
        bytes: &[u8],
        reason: InPlaceReason,
    ) -> Result<SaveOutcome, SftpError> {
        self.write_in_place(target, bytes).await?;
        let revision = self.stat(target).await?.revision();
        Ok(SaveOutcome::Saved { revision, mode: WriteMode::InPlace(reason) })
    }

    async fn conflict(&self, target: &str, entry: &Entry) -> SaveOutcome {
        let remote_text = if entry.size <= MAX_TEXT_SIZE && entry.kind == EntryKind::File {
            match self.read_file(target, MAX_TEXT_SIZE).await {
                Ok((bytes, _)) => decode_text(&bytes).ok().map(|(text, _, _)| text),
                Err(_) => None,
            }
        } else {
            None
        };
        SaveOutcome::Conflict { remote_text, remote_mtime: entry.modified, remote_revision: entry.revision() }
    }

    /// Creates `temp` with `bytes`, carrying over the permissions and owner of `original`.
    ///
    /// The file is created exclusively (see [`SftpSession::create_exclusive`]). Whatever is
    /// reported other than [`Temp::Ready`] leaves no temporary file behind.
    async fn write_temp(&self, temp: &str, bytes: &[u8], original: Option<&Entry>) -> Result<Temp, SftpError> {
        let mode = original.and_then(|entry| entry.mode);
        let create = FileAttributes { permissions: mode, ..FileAttributes::empty() };
        let handle = match self.create_exclusive(temp, create).await {
            Ok(handle) => handle,
            // We could not create it (nor clear the name): nothing of ours to clean up.
            Err(SftpError::PermissionDenied { .. }) => return Ok(Temp::DirNotWritable),
            Err(err) => return Err(err),
        };
        let written = async {
            self.write_all(&handle, bytes, temp).await?;
            // Changing the owner can drop setuid/setgid bits: do it before the final chmod.
            if !self.owner_kept(&handle, original, temp).await? {
                return Ok(Temp::OwnerDiffers);
            }
            if let Some(mode) = mode {
                // The server's umask applies to the mode given at creation: set it for real.
                self.set_handle_mode(&handle, mode, temp).await?;
            }
            self.sync_handle(&handle).await;
            Ok(Temp::Ready)
        }
        .await;
        let closed = self.close_handle(&handle, temp).await;
        match (written, closed) {
            (Ok(Temp::Ready), Ok(())) => Ok(Temp::Ready),
            (written, closed) => {
                // The temporary file is ours and is not going to be used.
                let _ = self.remove_file(temp).await;
                written.and_then(|outcome| closed.map(|()| outcome))
            }
        }
    }

    /// Gives the new file the owner and group of `original` and checks that it worked. Only root
    /// may chown, so for anybody else this holds exactly when the original is already theirs
    /// (or in a group of theirs and owned by them).
    async fn owner_kept(&self, handle: &str, original: Option<&Entry>, path: &str) -> Result<bool, SftpError> {
        let Some(Entry { uid: Some(uid), gid: Some(gid), .. }) = original else { return Ok(true) };
        self.try_set_handle_owner(handle, original).await;
        let attrs = self.raw.fstat(handle).await.map_err(|err| SftpError::from_raw(err, path))?.attrs;
        Ok(attrs.uid.is_none_or(|actual| actual == *uid) && attrs.gid.is_none_or(|actual| actual == *gid))
    }

    async fn write_in_place(&self, target: &str, bytes: &[u8]) -> Result<(), SftpError> {
        let handle = self.open_handle(target, OpenFlags::WRITE | OpenFlags::TRUNCATE, FileAttributes::empty()).await?;
        let written = self.write_all(&handle, bytes, target).await;
        let closed = self.close_handle(&handle, target).await;
        written.and(closed)
    }
}

/// How writing the temporary file of a save went.
enum Temp {
    /// Written, with the original's permissions and owner.
    Ready,
    /// It could not be created next to the target.
    DirNotWritable,
    /// It was written, but it would not have the original's owner: it has been removed.
    OwnerDiffers,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LF: SaveFormat = SaveFormat { line_ending: LineEnding::Lf, bom: false };
    const CRLF: SaveFormat = SaveFormat { line_ending: LineEnding::CrLf, bom: false };

    #[test]
    fn plain_text_round_trips() {
        let (text, ending, bom) = decode_text("一行\nsecond\n".as_bytes()).unwrap();
        assert_eq!((text.as_str(), ending, bom), ("一行\nsecond\n", LineEnding::Lf, false));
        assert_eq!(encode_text(&text, LF), "一行\nsecond\n".as_bytes());
        let (empty, ending, _) = decode_text(b"").unwrap();
        assert_eq!((empty.as_str(), ending), ("", LineEnding::Lf));
    }

    #[test]
    fn crlf_files_edit_as_lf_and_save_as_crlf() {
        let original = b"one\r\ntwo\r\nthree";
        let (text, ending, _) = decode_text(original).unwrap();
        assert_eq!((text.as_str(), ending), ("one\ntwo\nthree", LineEnding::CrLf));
        assert_eq!(encode_text(&text, CRLF), original, "an untouched file comes back byte for byte");
        assert_eq!(encode_text("one\ntwo\r\nthree\n", CRLF), b"one\r\ntwo\r\nthree\r\n", "never CR CR LF");
    }

    #[test]
    fn the_dominant_line_ending_wins() {
        assert_eq!(detect_line_ending("a\r\nb\r\nc\n"), LineEnding::CrLf);
        assert_eq!(detect_line_ending("a\nb\nc\r\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("a\r\nb\nc\r\nd\n"), LineEnding::CrLf, "a tie goes to CRLF");
        assert_eq!(detect_line_ending("no newline"), LineEnding::Lf);
        assert_eq!(detect_line_ending("lone\rcr"), LineEnding::Lf);
        assert_eq!(LineEnding::CrLf.label(), "CRLF");
    }

    #[test]
    fn a_byte_order_mark_is_kept_aside() {
        let (text, ending, bom) = decode_text(b"\xEF\xBB\xBFhello\r\n").unwrap();
        assert_eq!((text.as_str(), ending, bom), ("hello\n", LineEnding::CrLf, true));
        let saved = encode_text(&text, SaveFormat { line_ending: ending, bom });
        assert_eq!(saved, b"\xEF\xBB\xBFhello\r\n");
        assert_eq!(encode_text("x", SaveFormat { line_ending: LineEnding::Lf, bom: true }), b"\xEF\xBB\xBFx");
    }

    #[test]
    fn binary_and_foreign_encodings_are_refused() {
        assert_eq!(decode_text(b"\x7fELF\x02\x01\x00\x00"), Err(SftpError::Binary));
        assert_eq!(decode_text(b"abc\0def"), Err(SftpError::Binary));
        assert_eq!(decode_text(b"\xFF\xFEh\0i\0"), Err(SftpError::NotUtf8), "UTF-16 LE");
        assert_eq!(decode_text(b"\xFE\xFF\0h\0i"), Err(SftpError::NotUtf8), "UTF-16 BE");
        assert_eq!(decode_text(b"caf\xE9 latin-1"), Err(SftpError::NotUtf8));
        assert_eq!(decode_text(&[0xE4, 0xB8]), Err(SftpError::NotUtf8), "a Chinese character cut in half");
        assert!(decode_text("GBK 之外的 UTF-8 中文".as_bytes()).is_ok());
    }

    #[test]
    fn in_place_reasons_have_status_texts() {
        assert_eq!(InPlaceReason::DirNotWritable.label(), "目录不可写");
        assert_eq!(InPlaceReason::KeepOwner.label(), "保留属主");
    }

    #[test]
    fn the_save_temp_lives_next_to_the_target() {
        assert_eq!(save_temp_path("/etc/app/config.yml"), "/etc/app/.config.yml.shellmate-edit");
        assert_eq!(save_temp_path("/notes.txt"), "/.notes.txt.shellmate-edit");
        assert_eq!(save_temp_path("/home/用户/报告.md"), "/home/用户/.报告.md.shellmate-edit");
    }
}
