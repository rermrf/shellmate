//! Machine-local instructions and memory. Never shares the output spool's lifetime.
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

#[path = "memory_fs.rs"]
mod disk;

pub const FILE_BYTES: usize = 64 * 1024;
pub const SCOPE_FILES: usize = 200;
pub const INDEX_BYTES: usize = 25 * 1024;
pub const INDEX_LINES: usize = 200;
// Serialize tool mutations across drawers, including scope capacity checks.
static WRITES: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug)]
pub struct Memory {
    data_root: PathBuf,
    host_key: Option<String>,
}
#[derive(Debug)]
struct Resolved {
    path: PathBuf,
    scope: PathBuf,
    relative: String,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn path_parts(path: &str) -> io::Result<Vec<&str>> {
    if path.contains(['\0', '\\']) || !path.starts_with('/') {
        return Err(invalid(
            "记忆路径必须以 /memories 开头，不能包含 NUL 或反斜杠",
        ));
    }
    let parts: Vec<_> = path
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.contains(&"..") || parts.len() > 34 {
        return Err(invalid("记忆路径不能包含 .. 或过多目录层级"));
    }
    Ok(parts)
}
impl Memory {
    pub fn new(data_root: impl Into<PathBuf>, host_key: Option<&str>) -> Self {
        Self {
            data_root: data_root.into(),
            host_key: host_key.map(str::to_owned),
        }
    }
    pub fn has_host(&self) -> bool {
        self.host_key.is_some()
    }
    fn root(&self) -> io::Result<PathBuf> {
        // The supplied data root is trusted; platform aliases such as /tmp are OK.
        Ok(self.data_root.canonicalize()?.join("memory"))
    }
    fn resolve(&self, path: &str, file: bool) -> io::Result<Resolved> {
        let parts = path_parts(path)?;
        if parts.first() != Some(&"memories") || parts.len() < 2 {
            return Err(invalid("路径必须位于 /memories/global 或 /memories/host"));
        }
        let root = self.root()?;
        let scope = match parts[1] {
            "global" => root.join("global"),
            "host" => {
                let key = self.host_key.as_deref().ok_or_else(|| {
                    invalid("当前会话没有主机记忆范围（临时或未保存连接），请使用 /memories/global")
                })?;
                if key.is_empty() || key == "." || key == ".." || key.contains(['/', '\\', '\0']) {
                    return Err(invalid("主机记忆 key 无效"));
                }
                root.join("hosts").join(key)
            }
            _ => return Err(invalid("未知记忆范围，只支持 global 和 host")),
        };
        let mut target = scope.clone();
        for part in &parts[2..] {
            target.push(part);
        }
        if file && (parts.len() < 3 || target.extension().is_none_or(|e| e != "md")) {
            return Err(invalid("记忆工具仅允许 .md 文件"));
        }
        check_path(&root, &target)?;
        Ok(Resolved {
            path: target,
            scope,
            relative: parts[1..].join("/"),
        })
    }
    pub fn view(&self, path: &str, range: Option<[usize; 2]>) -> io::Result<String> {
        if path_parts(path)? == ["memories"] {
            return Ok(if self.has_host() {
                "global/\nhost/"
            } else {
                "global/"
            }
            .into());
        }
        let target = self.resolve(path, false)?;
        if target.path.is_dir() || (target.path == target.scope && !target.path.exists()) {
            if range.is_some() {
                return Err(invalid("目录不支持 view_range"));
            }
            let mut lines = Vec::new();
            if target.path.exists() {
                for entry in disk::Dir::open(&self.data_root, &target.path, false)?.entries()? {
                    if entry.directory
                        || Path::new(&entry.name)
                            .extension()
                            .is_some_and(|e| e == "md")
                    {
                        let modified = local_time(entry.modified);
                        lines.push(format!(
                            "{}{}\t{} bytes\tmodified: {modified}",
                            entry.name.to_string_lossy(),
                            if entry.directory { "/" } else { "" },
                            entry.bytes
                        ));
                    }
                }
            }
            lines.sort();
            return Ok(if lines.is_empty() {
                "（空目录）".into()
            } else {
                lines.join("\n")
            });
        }
        let content = self.read(path)?;
        let lines: Vec<_> = content.lines().collect();
        let [start, end] = range.unwrap_or([1, lines.len().max(1)]);
        if start == 0 || end < start || start > lines.len().max(1) {
            return Err(invalid("view_range 必须是有效的 1 起算 [start, end]"));
        }
        Ok(lines
            .iter()
            .enumerate()
            .take(end)
            .skip(start - 1)
            .map(|(i, s)| format!("{}: {s}", i + 1))
            .collect::<Vec<_>>()
            .join("\n"))
    }
    pub fn read(&self, path: &str) -> io::Result<String> {
        let text = self.read_prefix(path, FILE_BYTES + 4)?;
        if text.len() > FILE_BYTES {
            return Err(invalid("记忆文件超过 64 KB"));
        }
        Ok(text)
    }
    /// Bounded reads also support user-edited instructions/indexes larger than the tool cap.
    pub fn read_prefix(&self, path: &str, bytes: usize) -> io::Result<String> {
        let target = self.resolve(path, true)?;
        let file = self
            .parent(&target, false)?
            .read(target.path.file_name().unwrap())?;
        let mut content = Vec::new();
        file.take(bytes as u64).read_to_end(&mut content)?;
        // A byte cap may split the last UTF-8 character; drop only that partial suffix.
        match String::from_utf8(content) {
            Ok(s) => Ok(s),
            Err(e) if e.utf8_error().error_len().is_none() => {
                let end = e.utf8_error().valid_up_to();
                Ok(String::from_utf8(e.into_bytes()[..end].to_vec()).unwrap())
            }
            Err(_) => Err(invalid("记忆文件必须是 UTF-8")),
        }
    }
    pub fn create(&self, path: &str, text: &str) -> io::Result<String> {
        let _lock = WRITES.lock().unwrap();
        let target = self.resolve(path, true)?;
        if target.path.exists() {
            return Err(invalid("文件已存在，请用 str_replace 或 insert 更新"));
        }
        self.write(&target, text, true, "已保存记忆")
    }
    pub fn str_replace(&self, path: &str, old: &str, new: &str) -> io::Result<String> {
        let _lock = WRITES.lock().unwrap();
        let target = self.resolve(path, true)?;
        let text = self.read(path)?;
        // Count overlapping matches too: ambiguous replacements must not silently pass.
        if old.is_empty()
            || text
                .char_indices()
                .filter(|(i, _)| text[*i..].starts_with(old))
                .count()
                != 1
        {
            return Err(invalid("old_str 必须恰好出现一次（当前未找到或出现多次）"));
        }
        self.write(&target, &text.replacen(old, new, 1), false, "已更新记忆")
    }
    pub fn insert(&self, path: &str, line: usize, text: &str) -> io::Result<String> {
        let _lock = WRITES.lock().unwrap();
        let target = self.resolve(path, true)?;
        let old = self.read(path)?;
        let lines: Vec<_> = old.split_inclusive('\n').collect();
        if line > lines.len() {
            return Err(invalid("insert_line 超出文件行数；0 表示文件开头"));
        }
        let offset: usize = lines[..line].iter().map(|s| s.len()).sum();
        let (head, tail) = old.split_at(offset);
        let separator = if !head.is_empty() && !head.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        let ending = if !text.ends_with('\n') && !tail.is_empty() {
            "\n"
        } else {
            ""
        };
        self.write(
            &target,
            &format!("{head}{separator}{text}{ending}{tail}"),
            false,
            "已更新记忆",
        )
    }
    pub fn delete(&self, path: &str) -> io::Result<String> {
        let _lock = WRITES.lock().unwrap();
        let target = self.resolve(path, true)?;
        let parent = self.parent(&target, false)?;
        let name = target.path.file_name().unwrap();
        parent.read(name)?;
        parent.remove(name, false)?;
        Ok(format!("已删除记忆 · {}", target.relative))
    }
    pub fn rename(&self, old: &str, new: &str) -> io::Result<String> {
        let _lock = WRITES.lock().unwrap();
        let from = self.resolve(old, true)?;
        let to = self.resolve(new, true)?;
        if from.path == to.path {
            return Err(invalid("新旧路径相同"));
        }
        if to.path.exists() {
            return Err(invalid("目标文件已存在"));
        }
        let text = self.read(old)?;
        // A rename within a full scope does not consume another slot.
        let result = self.write(&to, &text, from.scope != to.scope, "已更新记忆")?;
        if let Err(error) = self
            .parent(&from, false)
            .and_then(|p| p.remove(from.path.file_name().unwrap(), false))
        {
            if let Ok(parent) = self.parent(&to, false) {
                let _ = parent.remove(to.path.file_name().unwrap(), false);
            }
            return Err(error);
        }
        Ok(result)
    }
    fn write(
        &self,
        target: &Resolved,
        text: &str,
        new_slot: bool,
        verb: &str,
    ) -> io::Result<String> {
        let name = target.path.file_name().unwrap();
        let text = if name == "AGENTS.md" || name == "MEMORY.md" {
            text.to_owned()
        } else {
            stamp(text, &local_time(SystemTime::now()))
        };
        if text.len() > FILE_BYTES {
            return Err(invalid("记忆文件最多 64 KB（包含 modified）"));
        }
        if new_slot && self.count_files(&target.scope)? >= SCOPE_FILES {
            return Err(invalid("每个记忆范围最多 200 个文件"));
        }
        let parent = self.parent(target, true)?;
        // Replace via a unique sibling, so a failed write never truncates the old file.
        let tmp = std::ffi::OsString::from(format!(".memory-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = parent.create(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            parent.rename(&tmp, name)
        })();
        if result.is_err() {
            let _ = parent.remove(&tmp, false);
        }
        result?;
        let warning = if name == "MEMORY.md" {
            index_warning(&text)
        } else {
            String::new()
        };
        Ok(format!("{verb} · {}{warning}", target.relative))
    }
    fn parent(&self, target: &Resolved, create: bool) -> io::Result<disk::Dir> {
        disk::Dir::open(&self.data_root, target.path.parent().unwrap(), create)
    }
    fn count_files(&self, path: &Path) -> io::Result<usize> {
        match disk::Dir::open(&self.data_root, path, false) {
            Ok(dir) => dir.count_files(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }
    /// Call only after the host record was successfully deleted.
    pub fn delete_host(&self) -> io::Result<()> {
        let _lock = WRITES.lock().unwrap();
        let target = self.resolve("/memories/host", false)?;
        if target.path.exists() {
            self.count_files(&target.path)?; // reject symlinks before deleting any entry
            self.parent(&target, false)?
                .remove_tree(target.path.file_name().unwrap())?;
        }
        Ok(())
    }
}
fn check_path(root: &Path, target: &Path) -> io::Result<()> {
    let suffix = target
        .strip_prefix(root)
        .map_err(|_| invalid("记忆路径越界"))?;
    let mut path = root.to_path_buf();
    for part in std::iter::once(None).chain(suffix.components().map(Some)) {
        if let Some(part) = part {
            path.push(part);
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(invalid("不允许符号链接")),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
pub fn index_warning(text: &str) -> String {
    let lines = text.lines().count();
    let bytes = text.len();
    if lines > INDEX_LINES || bytes > INDEX_BYTES {
        "\nMEMORY.md 已超过加载上限，超出部分下次不会加载，请立即重写索引".into()
    } else if lines >= INDEX_LINES * 9 / 10 || bytes >= INDEX_BYTES * 9 / 10 {
        format!(
            "\nMEMORY.md 接近加载上限（{lines} 行 / {:.1} KB）：保持每条一行，把细节移到主题文件，合并或删除过时条目",
            bytes as f64 / 1024.
        )
    } else {
        String::new()
    }
}
fn stamp(text: &str, time: &str) -> String {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines
        .next()
        .filter(|s| s.trim_end_matches(['\r', '\n']) == "---")
    else {
        return text.into();
    };
    let newline = if first.ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut result = first.to_owned();
    let mut found = false;
    for line in lines.by_ref() {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            if !found {
                result.push_str(&format!("modified: {time}{newline}"));
            }
            result.push_str(line);
            result.extend(lines);
            return result;
        }
        if line.starts_with("modified:") {
            if !found {
                result.push_str(&format!("modified: {time}{newline}"));
            }
            found = true;
        } else {
            result.push_str(line);
        }
    }
    text.into() // An unclosed block is not frontmatter.
}
fn local_time(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    // SAFETY: localtime_r writes to our valid tm and strftime writes at most buffer.len().
    unsafe {
        let mut tm = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&seconds, &mut tm).is_null() {
            return String::new();
        }
        let mut buffer = [0u8; 64];
        let n = libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            c"%Y-%m-%dT%H:%M:%S%z".as_ptr(),
            &tm,
        );
        let mut value = String::from_utf8_lossy(&buffer[..n]).into_owned();
        if value.len() >= 5 {
            value.insert(value.len() - 2, ':');
        }
        value
    }
}

#[path = "memory_catalog.rs"]
mod catalog;
pub use catalog::FileInfo;

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
