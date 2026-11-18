use super::{read_cmd, valid_label};
use crate::services::launchd::safe_path;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Plist {
    pub label: String,
    pub program: Option<String>,
    pub keep_alive: bool,
    pub run_at_load: bool,
    pub stderr: Option<String>,
    pub stdout: Option<String>,
}

async fn read(path: &str) -> Option<Plist> {
    let text = read_cmd("/usr/bin/plutil", &["-convert", "json", "-o", "-", "--", path]).await?;
    let json: Value = serde_json::from_str(&text).ok()?;
    let dict = json.as_object()?;
    let label = dict.get("Label")?.as_str()?.to_owned();
    if !valid_label(&label) {
        return None;
    }
    let boolean = |key| dict.get(key).map_or(Some(false), Value::as_bool);
    let keep_alive = match dict.get("KeepAlive") {
        Some(Value::Object(_)) => true,
        _ => boolean("KeepAlive")?,
    };
    let path = |key| {
        dict.get(key)?
            .as_str()
            .filter(|path| safe_path(path))
            .map(str::to_owned)
    };
    let program = dict
        .get("Program")
        .and_then(Value::as_str)
        .or_else(|| dict.get("ProgramArguments")?.as_array()?.first()?.as_str())
        .map(str::to_owned);
    Some(Plist {
        label,
        program,
        keep_alive,
        run_at_load: boolean("RunAtLoad")?,
        stderr: path("StandardErrorPath"),
        stdout: path("StandardOutPath"),
    })
}

#[derive(Clone)]
pub(super) struct Entry {
    pub system: bool,
    pub label: String,
    pub path: String,
    pub data: Option<Plist>,
}

pub(super) fn roots(home: Option<PathBuf>) -> Vec<(bool, PathBuf)> {
    home.filter(|home| home.is_absolute())
        .map(|home| (false, home.join("Library/LaunchAgents")))
        .into_iter()
        .chain([
            (false, "/Library/LaunchAgents".into()),
            (true, "/Library/LaunchDaemons".into()),
            (false, "/System/Library/LaunchAgents".into()),
            (true, "/System/Library/LaunchDaemons".into()),
        ])
        .collect()
}

#[derive(Default)]
pub(super) struct Cache(HashMap<String, (Option<SystemTime>, Option<Plist>)>);

impl Cache {
    pub(super) async fn scan(&mut self, roots: Vec<(bool, PathBuf)>) -> Vec<Entry> {
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (system, root) in roots {
            let Ok(dir) = fs::read_dir(root) else { continue };
            let mut paths: Vec<_> = dir
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "plist"))
                .collect();
            paths.sort();
            for path in paths {
                let Some(label) = path.file_stem().and_then(|name| name.to_str()) else {
                    continue;
                };
                let Some(path) = path.to_str() else { continue };
                let metadata = fs::metadata(path).ok();
                let mtime = metadata
                    .filter(|meta| meta.is_file())
                    .and_then(|meta| meta.modified().ok());
                if self.0.get(path).is_none_or(|(old, _)| *old != mtime) {
                    let data = if mtime.is_some() { read(path).await } else { None };
                    self.0.insert(path.to_owned(), (mtime, data));
                }
                seen.insert(path.to_owned());
                let data = self.0[path].1.clone();
                entries.push(Entry {
                    system,
                    label: data.as_ref().map_or(label, |data| &data.label).to_owned(),
                    path: path.into(),
                    data,
                });
            }
        }
        self.0.retain(|path, _| seen.contains(path));
        entries
    }
}

pub(super) fn log_tail(path: &Path) -> Option<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let offset = meta.len().saturating_sub(16 * 1024);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(16 * 1024).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<_> = text.lines().rev().take(5).collect();
    lines.reverse();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[derive(Default)]
pub(super) struct Tails(HashMap<String, CachedTail>);

struct CachedTail {
    identity: (u64, u64, u64, SystemTime),
    text: Option<String>,
    used: bool,
}

impl Tails {
    pub(super) fn read(&mut self, path: &str) -> Option<String> {
        let meta = fs::metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        let identity = (meta.dev(), meta.ino(), meta.len(), meta.modified().ok()?);
        let tail = self.0.entry(path.into()).or_insert_with(|| CachedTail {
            identity,
            text: log_tail(path.as_ref()),
            used: true,
        });
        if tail.identity != identity {
            tail.text = log_tail(path.as_ref());
            tail.identity = identity;
        }
        tail.used = true;
        tail.text.clone()
    }

    pub(super) fn finish_round(&mut self) {
        self.0.retain(|_, tail| std::mem::take(&mut tail.used));
    }
}
