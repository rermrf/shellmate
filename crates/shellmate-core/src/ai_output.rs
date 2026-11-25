//! AI output spools. Small logs are ordinary bytes. Beyond 64 MiB the first 32 MiB
//! stay in `.log` and the second half is an on-disk circular tail. A tiny `.meta`
//! index records total bytes and the ring cursor. Readers reconstruct chronological
//! head / omission marker / tail, including while a writer is running. Completion
//! linearizes the file with a fixed-size copy buffer and removes the ring representation.
//! stdout/stderr chunks are recorded in observed arrival order; every stderr line
//! (and a resumed stderr fragment after stdout) starts with `[stderr] `.
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, SystemTime},
};

pub const KEEP_BYTES: u64 = 64 * 1024 * 1024;
pub const KILL_BYTES: u64 = 5_000_000_000;
const HALF: u64 = KEEP_BYTES / 2;
pub const CLEARED: &str = "该输出已被清理，可重新执行命令查看";

type Writers = Mutex<HashMap<PathBuf, Weak<Mutex<Writer>>>>;
fn writers() -> &'static Writers {
    static WRITERS: OnceLock<Writers> = OnceLock::new();
    WRITERS.get_or_init(Default::default)
}

/// Storage completeness is independent of a model's preview/read-back budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completeness {
    Complete,
    HeadTail,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub total_bytes: u64,
    pub retained_bytes: u64,
    pub completeness: Completeness,
    pub finished: bool,
}
fn snapshot_info(index: Index, finished: bool, verified: bool) -> SnapshotInfo {
    SnapshotInfo {
        total_bytes: index.total,
        retained_bytes: index.size,
        completeness: if !verified {
            Completeness::Unknown
        } else if index.ring || index.total > KEEP_BYTES || index.size > KEEP_BYTES {
            // A finalized ring includes a generated gap marker in addition to its retained bytes.
            Completeness::HeadTail
        } else {
            Completeness::Complete
        },
        finished,
    }
}
/// Cheap change detection for visible live readers; does not read log contents.
pub fn inspect(root: &Path, conversation: &str, id: &str) -> io::Result<SnapshotInfo> {
    let path = path(root, conversation, id)?;
    let live = writers().lock().unwrap().get(&path).and_then(Weak::upgrade);
    if let Some(live) = live {
        let writer = live.lock().unwrap();
        fs::metadata(&path)?;
        Ok(snapshot_info(writer.index, writer.finished, true))
    } else {
        let index = Index::read(&path)?;
        Ok(snapshot_info(
            index,
            true,
            verified_index(&path, index.size),
        ))
    }
}
/// Incremental reader for a verified, still-contiguous log. Rings require a fresh snapshot.
pub fn snapshot_since(
    root: &Path,
    conversation: &str,
    id: &str,
    offset: u64,
    out: &mut impl Write,
) -> io::Result<Option<SnapshotInfo>> {
    let path = path(root, conversation, id)?;
    let live = writers().lock().unwrap().get(&path).and_then(Weak::upgrade);
    let copy = |index: Index,
                finished: bool,
                verified: bool,
                out: &mut dyn Write|
     -> io::Result<Option<SnapshotInfo>> {
        let info = snapshot_info(index, finished, verified);
        if info.completeness != Completeness::Complete || offset > index.size {
            return Ok(None);
        }
        io::copy(&mut extent(&path, offset, index.size - offset)?, out)?;
        Ok(Some(info))
    };
    if let Some(live) = live {
        let writer = live.lock().unwrap();
        copy(writer.index, writer.finished, true, out)
    } else {
        let index = Index::read(&path)?;
        copy(index, true, verified_index(&path, index.size), out)
    }
}

/// Copy a consistent, chronological snapshot, without a model/tool output limit.
/// Live writers (including child registries) are locked only for this local copy.
/// Call off the UI thread. The destination is owned by the reader, never by the conversation.
pub fn snapshot(
    root: &Path,
    conversation: &str,
    id: &str,
    out: &mut impl Write,
) -> io::Result<SnapshotInfo> {
    let path = path(root, conversation, id)?;
    let live = {
        let mut all = writers().lock().unwrap();
        all.retain(|_, w| w.strong_count() > 0);
        all.get(&path).and_then(Weak::upgrade)
    };
    if let Some(live) = live {
        let writer = live.lock().unwrap();
        // Retaining a writer must not resurrect a deleted conversation.
        let mut reader = reader_at(&path, writer.index)?;
        io::copy(&mut reader, out)?;
        return Ok(snapshot_info(writer.index, writer.finished, true));
    }
    let index = Index::read(&path)?;
    let verified = verified_index(&path, index.size);
    let mut reader = reader_at(&path, index)?;
    io::copy(&mut reader, out)?;
    Ok(snapshot_info(index, true, verified))
}
fn verified_index(path: &Path, size: u64) -> bool {
    let Ok(text) = fs::read_to_string(path.with_extension("meta")) else {
        return false;
    };
    let values: Result<Vec<u64>, _> = text.split_whitespace().map(str::parse).collect();
    values.is_ok_and(|v| {
        v.len() == 4
            && v[1] <= size
            && v[2] < HALF
            && (v[3] == 0 || (v[3] == 1 && v[1] == KEEP_BYTES))
    })
}

pub fn directory(root: &Path, conversation: &str) -> io::Result<PathBuf> {
    validate(conversation)?;
    Ok(root.join("ai-outputs").join(conversation))
}
pub fn path(root: &Path, conversation: &str, id: &str) -> io::Result<PathBuf> {
    validate(id)?;
    Ok(directory(root, conversation)?.join(format!("{id}.log")))
}
fn validate(id: &str) -> io::Result<()> {
    if id.is_empty()
        || id.len() > 160
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "无效的输出 id"));
    }
    Ok(())
}
#[derive(Clone)]
pub struct Spool(Arc<Mutex<Writer>>);
struct Writer {
    file: Option<File>,
    meta: Option<File>,
    path: PathBuf,
    index: Index,
    stderr_line: bool,
    last_stderr: bool,
    line_open: bool,
    finished: bool,
}
#[derive(Clone, Copy, Default)]
struct Index {
    total: u64,
    size: u64,
    cursor: u64,
    ring: bool,
}
impl Index {
    fn write(self, file: &mut File) -> io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        let text = format!(
            "{} {} {} {}\n",
            self.total,
            self.size,
            self.cursor,
            u8::from(self.ring)
        );
        file.write_all(text.as_bytes())?;
        file.set_len(text.len() as u64)
    }
    fn read(path: &Path) -> io::Result<Self> {
        let size = fs::metadata(path)?.len();
        let Ok(text) = fs::read_to_string(path.with_extension("meta")) else {
            return Ok(Self {
                total: size,
                size,
                ..Self::default()
            });
        };
        let v: Vec<u64> = text
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if v.len() != 4 || v[1] > size || v[2] >= HALF || (v[3] != 0 && v[1] != KEEP_BYTES) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "输出索引损坏"));
        }
        Ok(Self {
            total: v[0],
            size: v[1],
            cursor: v[2],
            ring: v[3] != 0,
        })
    }
}
impl Spool {
    pub fn create(root: &Path, conversation: &str, id: &str) -> io::Result<Self> {
        let path = path(root, conversation, id)?;
        fs::create_dir_all(path.parent().unwrap())?;
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)?;
        let mut meta = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path.with_extension("meta"))?;
        Index::default().write(&mut meta)?;
        let writer = Arc::new(Mutex::new(Writer {
            file: Some(file),
            meta: Some(meta),
            path: path.clone(),
            index: Index::default(),
            stderr_line: true,
            last_stderr: false,
            line_open: false,
            finished: false,
        }));
        let mut all = writers().lock().unwrap();
        all.retain(|_, writer| writer.strong_count() > 0);
        all.insert(path, Arc::downgrade(&writer));
        Ok(Self(writer))
    }
    pub fn path(&self) -> PathBuf {
        self.0.lock().unwrap().path.clone()
    }
    pub fn total(&self) -> u64 {
        self.0.lock().unwrap().index.total
    }
    /// False means the 5 GB safety ceiling was reached; the caller must stop execution.
    pub fn write(&self, bytes: &[u8], stderr: bool) -> io::Result<bool> {
        let mut w = self.0.lock().unwrap();
        if w.finished {
            return Ok(false);
        }
        w.index.total = w.index.total.saturating_add(bytes.len() as u64);
        if w.index.total > KILL_BYTES {
            return Ok(false);
        }
        if !bytes.is_empty() {
            if stderr && !w.last_stderr && w.line_open {
                w.keep(b"\n")?;
                w.stderr_line = true;
            }
            for part in bytes.split_inclusive(|b| *b == b'\n') {
                if stderr && (w.stderr_line || !w.last_stderr) {
                    w.keep(b"[stderr] ")?;
                }
                w.keep(part)?;
                w.line_open = !part.ends_with(b"\n");
                if stderr {
                    w.stderr_line = part.ends_with(b"\n");
                }
                w.last_stderr = stderr;
            }
        }
        let index = w.index;
        index.write(w.meta.as_mut().unwrap())?;
        Ok(true)
    }
    pub fn window(&self, bytes: usize) -> io::Result<Window> {
        let w = self.0.lock().unwrap();
        window_at(&w.path, w.index, bytes)
    }
    pub fn lines(
        &self,
        offset: usize,
        limit: usize,
        grep: Option<&str>,
        bytes: usize,
    ) -> io::Result<Window> {
        let w = self.0.lock().unwrap();
        lines_at(&w.path, w.index, offset, limit, grep, bytes)
    }
    pub fn finish(&self) -> io::Result<()> {
        let mut w = self.0.lock().unwrap();
        if w.finished {
            return Ok(());
        }
        if w.index.ring {
            let tmp = w.path.with_extension("linear");
            let mut out = File::create(&tmp)?;
            let mut reader = reader_at(&w.path, w.index)?;
            io::copy(&mut reader, &mut out)?;
            let size = out.metadata()?.len();
            fs::rename(&tmp, &w.path)?;
            w.index.size = size;
            w.index.ring = false;
            w.index.cursor = 0;
        }
        let index = w.index;
        index.write(w.meta.as_mut().unwrap())?;
        w.file.take();
        w.meta.take();
        w.finished = true;
        Ok(())
    }
}
impl Writer {
    fn keep(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if self.index.size < KEEP_BYTES {
            let n = bytes.len().min((KEEP_BYTES - self.index.size) as usize);
            self.file
                .as_mut()
                .unwrap()
                .seek(SeekFrom::Start(self.index.size))?;
            self.file.as_mut().unwrap().write_all(&bytes[..n])?;
            self.index.size += n as u64;
            bytes = &bytes[n..];
        }
        while !bytes.is_empty() {
            self.index.ring = true;
            let n = bytes.len().min((HALF - self.index.cursor) as usize);
            self.file
                .as_mut()
                .unwrap()
                .seek(SeekFrom::Start(HALF + self.index.cursor))?;
            self.file.as_mut().unwrap().write_all(&bytes[..n])?;
            self.index.cursor = (self.index.cursor + n as u64) % HALF;
            bytes = &bytes[n..];
        }
        Ok(())
    }
}
pub struct Window {
    pub bytes: Vec<u8>,
    pub total: u64,
    pub truncated: bool,
}
fn extent(path: &Path, offset: u64, len: u64) -> io::Result<Box<dyn Read + Send>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    Ok(Box::new(f.take(len)))
}
struct Parts(std::collections::VecDeque<Box<dyn Read + Send>>);
impl Read for Parts {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while let Some(p) = self.0.front_mut() {
            let n = p.read(buf)?;
            if n != 0 {
                return Ok(n);
            }
            self.0.pop_front();
        }
        Ok(0)
    }
}
fn reader_at(path: &Path, index: Index) -> io::Result<Parts> {
    let mut parts = std::collections::VecDeque::new();
    if index.ring {
        parts.push_back(extent(path, 0, HALF)?);
        let marker = format!(
            "\n[省略中间输出；总计 {} 字节，保留头尾 64 MiB]\n",
            index.total
        );
        parts.push_back(Box::new(io::Cursor::new(marker.into_bytes())) as Box<dyn Read + Send>);
        parts.push_back(extent(path, HALF + index.cursor, HALF - index.cursor)?);
        parts.push_back(extent(path, HALF, index.cursor)?);
    } else {
        parts.push_back(extent(path, 0, index.size)?);
    }
    Ok(Parts(parts))
}
pub fn read_window(path: &Path, bytes: usize) -> io::Result<Window> {
    window_at(path, Index::read(path)?, bytes)
}
fn window_at(path: &Path, index: Index, bytes: usize) -> io::Result<Window> {
    let mut reader = reader_at(path, index)?;
    let mut head = Vec::new();
    reader.by_ref().take(bytes as u64).read_to_end(&mut head)?;
    // Fixed memory, even for an unterminated line or a running 64 MiB ring.
    let mut tail = std::collections::VecDeque::with_capacity(bytes / 2);
    tail.extend(head.iter().skip(bytes / 2).copied());
    let mut buf = [0; 65536];
    let mut more = false;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        more = true;
        let keep = bytes / 2;
        if n >= keep {
            tail.clear();
            tail.extend(&buf[n - keep..n]);
        } else {
            let excess = (tail.len() + n).saturating_sub(keep);
            tail.drain(..excess);
            tail.extend(&buf[..n]);
        }
    }
    if more {
        head.truncate(bytes / 2);
        head.extend_from_slice(b"\n[... omitted ...]\n");
        head.extend(tail);
    }
    Ok(Window {
        bytes: head,
        total: index.total,
        truncated: more || index.ring,
    })
}
pub fn read_lines(
    path: &Path,
    offset: usize,
    limit: usize,
    grep: Option<&str>,
    bytes: usize,
) -> io::Result<Window> {
    lines_at(path, Index::read(path)?, offset, limit, grep, bytes)
}
fn lines_at(
    path: &Path,
    index: Index,
    offset: usize,
    limit: usize,
    grep: Option<&str>,
    bytes: usize,
) -> io::Result<Window> {
    use io::BufRead;
    let mut reader = BufReader::new(reader_at(path, index)?);
    let needle = grep.unwrap_or("").as_bytes();
    let mut line = Vec::new();
    let mut overlap = Vec::new();
    let mut found = needle.is_empty();
    let (mut number, mut matches) = (0usize, 0usize);
    let mut out = Vec::new();
    let mut truncated = false;
    loop {
        let buf = reader.fill_buf()?;
        let eof = buf.is_empty();
        let n = buf
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buf.len(), |i| i + 1);
        let ended = eof || buf[..n].ends_with(b"\n");
        if !found && number >= offset {
            overlap.extend_from_slice(&buf[..n]);
            found = overlap.windows(needle.len()).any(|w| w == needle);
            let excess = overlap.len().saturating_sub(needle.len().saturating_sub(1));
            overlap.drain(..excess);
        }
        let room = bytes.saturating_sub(line.len());
        line.extend_from_slice(&buf[..n.min(room)]);
        truncated |= n > room && number >= offset;
        reader.consume(n);
        if ended {
            if number >= offset && found && !line.is_empty() {
                let room = bytes.saturating_sub(out.len());
                out.extend_from_slice(&line[..line.len().min(room)]);
                truncated |= line.len() > room;
                matches += 1;
                if matches >= limit.clamp(1, 2000) || out.len() >= bytes {
                    break;
                }
            }
            number = number.saturating_add(1);
            line.clear();
            overlap.clear();
            found = needle.is_empty();
        }
        if eof {
            break;
        }
    }
    Ok(Window {
        bytes: out,
        total: index.total,
        truncated,
    })
}
pub fn delete_conversation(root: &Path, conversation: &str) -> io::Result<()> {
    match fs::remove_dir_all(directory(root, conversation)?) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
/// Age only; never a total-size quota. Symlinks are not traversed.
pub fn sweep(root: &Path, now: SystemTime) -> io::Result<usize> {
    let base = root.join("ai-outputs");
    let dirs = match fs::read_dir(base) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for dir in dirs {
        let dir = dir?;
        if !dir.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(dir.path())? {
            let file = file?;
            if !file.file_type()?.is_file() {
                continue;
            }
            if now
                .duration_since(file.metadata()?.modified()?)
                .unwrap_or_default()
                > Duration::from_secs(30 * 86400)
            {
                fs::remove_file(file.path())?;
                removed += 1;
            }
        }
        let _ = fs::remove_dir(dir.path());
    }
    Ok(removed)
}

/// Close/replace policy shared by the drawer and headless lifecycle tests.
pub fn close_conversation(root: &Path, conversation: &str, save: bool) -> io::Result<()> {
    if save {
        Ok(())
    } else {
        delete_conversation(root, conversation)
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    #[test]
    fn snapshots_are_scoped_consistent_and_do_not_recreate_deleted_logs() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::create(root.path(), "c", "out").unwrap();
        spool.write(b"hello\n", false).unwrap();
        spool.write(b"failure\n", true).unwrap();
        let mut text = vec![];
        let live = snapshot(root.path(), "c", "out", &mut text).unwrap();
        assert_eq!(text, b"hello\n[stderr] failure\n");
        assert!(!live.finished);
        assert_eq!(live.completeness, Completeness::Complete);
        spool.finish().unwrap();
        text.clear();
        assert!(
            snapshot(root.path(), "c", "out", &mut text)
                .unwrap()
                .finished
        );
        assert!(snapshot(root.path(), "other", "out", &mut vec![]).is_err());
        assert!(snapshot(root.path(), "c", "../out", &mut vec![]).is_err());
        delete_conversation(root.path(), "c").unwrap();
        assert_eq!(
            snapshot(root.path(), "c", "out", &mut vec![])
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert!(!directory(root.path(), "c").unwrap().exists());
    }
    #[test]
    fn missing_or_corrupt_metadata_never_claims_completeness() {
        let root = tempfile::tempdir().unwrap();
        let path = path(root.path(), "c", "out").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"available").unwrap();
        for meta in [None, Some("bad metadata"), Some("9 9 0 0")] {
            if let Some(meta) = meta {
                fs::write(path.with_extension("meta"), meta).unwrap();
            }
            let mut text = vec![];
            let result = snapshot(root.path(), "c", "out", &mut text);
            if meta == Some("bad metadata") {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
                continue;
            }
            let info = result.unwrap();
            assert_eq!(text, b"available");
            assert_eq!(
                info.completeness,
                if meta == Some("9 9 0 0") {
                    Completeness::Complete
                } else {
                    Completeness::Unknown
                }
            );
        }
    }
    #[test]
    fn live_and_finalized_rings_preserve_order_and_report_the_storage_gap() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::create(root.path(), "c", "ring").unwrap();
        spool.write(b"HEAD\n", false).unwrap();
        let block = vec![b'x'; 1024 * 1024];
        for _ in 0..65 {
            spool.write(&block, false).unwrap();
        }
        spool.write(b"\nTAIL\n", false).unwrap();
        let mut before = vec![];
        let info = snapshot(root.path(), "c", "ring", &mut before).unwrap();
        assert_eq!(info.completeness, Completeness::HeadTail);
        assert!(before.starts_with(b"HEAD\n"));
        assert!(before.ends_with(b"\nTAIL\n"));
        spool.finish().unwrap();
        drop(spool);
        let mut after = vec![];
        let info = snapshot(root.path(), "c", "ring", &mut after).unwrap();
        assert_eq!(info.completeness, Completeness::HeadTail);
        assert_eq!(before, after);
    }
}
