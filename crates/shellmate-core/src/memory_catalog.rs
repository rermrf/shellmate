//! Structured UI access to the same protected memory filesystem as the model tool.
use super::*;
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub path: String,
    pub name: String,
    pub description: String,
    pub kind: String,
    pub modified: SystemTime,
}
impl Memory {
    /// Editor upsert; all model-tool limits, stamps and atomic-write rules still apply.
    pub fn save(&self, path: &str, text: &str) -> io::Result<String> {
        let _lock = WRITES.lock().unwrap();
        let target = self.resolve(path, true)?;
        let new_slot = !target.path.exists();
        self.write(
            &target,
            text,
            new_slot,
            if new_slot {
                "已保存记忆"
            } else {
                "已更新记忆"
            },
        )
    }
    pub fn scope_exists(&self, host: bool) -> io::Result<bool> {
        let path = if host {
            "/memories/host"
        } else {
            "/memories/global"
        };
        let target = self.resolve(path, false)?;
        match disk::Dir::open(&self.data_root, &target.path, false) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    pub fn files(&self, host: bool) -> io::Result<Vec<FileInfo>> {
        let scope = if host {
            "/memories/host"
        } else {
            "/memories/global"
        };
        let target = self.resolve(scope, false)?;
        if !target.path.exists() {
            return Ok(Vec::new());
        }
        let mut pending = vec![(target.path, scope.to_owned())];
        let mut files = Vec::new();
        while let Some((path, virtual_path)) = pending.pop() {
            for entry in disk::Dir::open(&self.data_root, &path, false)?.entries()? {
                let name = entry
                    .name
                    .to_str()
                    .ok_or_else(|| invalid("记忆文件名必须是 UTF-8"))?;
                let next = format!("{virtual_path}/{name}");
                if entry.directory {
                    pending.push((path.join(name), next));
                } else if name.ends_with(".md") {
                    let text = self.read_prefix(&next, FILE_BYTES)?;
                    let field = |key: &str| frontmatter(&text, key).unwrap_or_default();
                    let title = field("name");
                    files.push(FileInfo {
                        path: next,
                        name: if title.is_empty() { name.into() } else { title },
                        description: field("description"),
                        kind: field("type"),
                        modified: timestamp(&field("modified")).unwrap_or(entry.modified),
                    });
                }
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }
    /// Creates only the protected root, for opening it with the system file manager.
    pub fn directory(&self) -> io::Result<PathBuf> {
        let root = self.root()?;
        disk::Dir::open(&self.data_root, &root, true)?;
        Ok(root)
    }
    /// Explicit UI clear-all, separate from output/conversation retention.
    pub fn clear_all(&self) -> io::Result<()> {
        let _lock = WRITES.lock().unwrap();
        let root = match self.root() {
            Ok(root) => root,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        match disk::Dir::open(&self.data_root, &root, false) {
            Ok(dir) => {
                dir.count_files()?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        disk::Dir::open(&self.data_root, root.parent().unwrap(), false)?
            .remove_tree(std::ffi::OsStr::new("memory"))
    }
}
fn frontmatter(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()? != "---" {
        return None;
    }
    for line in lines.take_while(|l| *l != "---") {
        if let Some(value) = line.strip_prefix(&format!("{key}:")) {
            let value = value.trim();
            if let Some(s) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                return Some(s.to_owned());
            }
            if let Some(s) = value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
                return Some(s.replace("''", "'"));
            }
            return Some(value.split(" #").next().unwrap_or_default().trim().into());
        }
    }
    None
}
fn timestamp(value: &str) -> Option<SystemTime> {
    // CC's timestamp is ISO 8601 with an explicit offset (written by Memory::save).
    if value.len() != 25 || !value.is_ascii() {
        return None;
    }
    let number = |start, end| value.get(start..end)?.parse::<i32>().ok();
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let sign = match &value[19..20] {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let offset = sign * (number(20, 22)? * 3600 + number(23, 25)? * 60);
    // SAFETY: tm is initialized, valid and exclusively owned; timegm ignores the process timezone.
    let seconds = unsafe {
        let mut tm = std::mem::zeroed::<libc::tm>();
        tm.tm_year = year - 1900;
        tm.tm_mon = month - 1;
        tm.tm_mday = day;
        tm.tm_hour = hour;
        tm.tm_min = minute;
        tm.tm_sec = second;
        libc::timegm(&mut tm) - i64::from(offset)
    };
    (seconds >= 0).then(|| UNIX_EPOCH + std::time::Duration::from_secs(seconds as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editor_save_and_structured_listing_share_limits_metadata_and_atomic_writes() {
        let root = tempfile::tempdir().unwrap();
        let memory = Memory::new(root.path(), Some("local"));
        assert!(!memory.scope_exists(true).unwrap());
        memory.save("/memories/host/sub/deploy.md","---\nname: 'Deploy layout'\ndescription: Uses compose\ntype: project # durable\n---\nbody").unwrap();
        let files = memory.files(true).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "Deploy layout");
        assert_eq!(files[0].description, "Uses compose");
        assert_eq!(files[0].kind, "project");
        assert!(
            SystemTime::now().duration_since(files[0].modified).unwrap()
                < std::time::Duration::from_secs(60)
        );
        assert!(
            memory
                .save("/memories/host/sub/deploy.md", &"x".repeat(FILE_BYTES + 1))
                .is_err()
        );
        assert!(
            memory
                .read("/memories/host/sub/deploy.md")
                .unwrap()
                .ends_with("body")
        );
        memory.save("/memories/host/sub/deploy.md", "new").unwrap();
        assert_eq!(memory.read("/memories/host/sub/deploy.md").unwrap(), "new");
        assert!(
            memory
                .save("/memories/host/../../outside.md", "bad")
                .is_err()
        );
    }
    #[test]
    fn clear_and_listing_reject_symlinks_without_touching_outside_data() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let memory = Memory::new(root.path(), None);
        memory.directory().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("memory/global")).unwrap();
        std::fs::write(outside.path().join("keep.md"), "keep").unwrap();
        assert!(memory.clear_all().is_err());
        assert!(memory.files(false).is_err());
        assert!(memory.save("/memories/global/keep.md", "bad").is_err());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("keep.md")).unwrap(),
            "keep"
        );
    }
    #[test]
    fn frontmatter_modified_respects_iso_timezone_offset() {
        assert_eq!(
            timestamp("2026-11-24T10:00:00+08:00"),
            timestamp("2026-11-24T02:00:00+00:00")
        );
        assert_eq!(
            timestamp("2026-11-23T21:00:00-05:00"),
            timestamp("2026-11-24T02:00:00+00:00")
        );
        assert!(timestamp("invalid").is_none());
    }
}
