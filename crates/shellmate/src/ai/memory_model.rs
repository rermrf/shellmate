//! Memory UI policies without GPUI. Every file mutation goes through Memory.
use shellmate_core::{
    HostId,
    memory::{FILE_BYTES, FileInfo, Memory},
};
use shellmate_store::HostStore;
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Focus {
    pub host: Option<String>,
    pub path: Option<String>,
}
impl Focus {
    pub fn current(host: Option<&str>) -> Self {
        Self {
            host: host.map(str::to_owned),
            path: None,
        }
    }
    pub fn global_instructions() -> Self {
        Self {
            host: None,
            path: Some("/memories/global/AGENTS.md".into()),
        }
    }
    pub fn from_action(action: &shellmate_ai::memory::Action, host: Option<&str>) -> Option<Self> {
        let parts: Vec<_> = action
            .path
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        if parts.len() < 3
            || parts[0] != "memories"
            || parts.contains(&"..")
            || action.path.contains(['\0', '\\'])
        {
            return None;
        }
        let key = match parts[1] {
            "global" => None,
            "host" => Some(host?.into()),
            _ => return None,
        };
        Some(Self {
            host: key,
            path: (!action.scope_only).then(|| format!("/{}", parts.join("/"))),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub key: Option<String>,
    pub label: String,
}
pub struct Model {
    pub root: PathBuf,
    pub scopes: Vec<Scope>,
    pub selected: usize,
    pub files: Vec<FileInfo>,
    pub remote: Vec<(String, bool)>,
}
impl Model {
    pub fn new(
        root: PathBuf,
        db: &HostStore,
        current: Option<&str>,
        focus: &Focus,
    ) -> Result<Self, String> {
        let mut scopes = vec![
            Scope {
                key: None,
                label: "全局".into(),
            },
            Scope {
                key: Some("local".into()),
                label: "本机终端".into(),
            },
        ];
        for host in db.hosts().map_err(|e| e.to_string())? {
            let key = host.id.to_string();
            if Some(key.as_str()) == current
                || focus.host.as_deref() == Some(&key)
                || Memory::new(&root, Some(&key))
                    .scope_exists(true)
                    .map_err(|e| e.to_string())?
            {
                scopes.push(Scope {
                    key: Some(key),
                    label: host.name,
                });
            }
        }
        let selected = scopes.iter().position(|s| s.key == focus.host).unwrap_or(0);
        let mut model = Self {
            root,
            scopes,
            selected,
            files: Vec::new(),
            remote: Vec::new(),
        };
        model.reload(db)?;
        Ok(model)
    }
    pub fn memory(&self) -> Memory {
        Memory::new(&self.root, self.scopes[self.selected].key.as_deref())
    }
    pub fn prefix(&self) -> &'static str {
        if self.scopes[self.selected].key.is_some() {
            "/memories/host"
        } else {
            "/memories/global"
        }
    }
    pub fn reload(&mut self, db: &HostStore) -> Result<(), String> {
        self.files = self
            .memory()
            .files(self.scopes[self.selected].key.is_some())
            .map_err(|e| e.to_string())?;
        self.remote = match &self.scopes[self.selected].key {
            Some(key) => db.remote_docs(key).map_err(|e| e.to_string())?,
            None => Vec::new(),
        };
        Ok(())
    }
    pub fn read(&self, path: &str) -> Result<String, String> {
        match self.memory().read(path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e.to_string()),
        }
    }
    pub fn save(&self, path: &str, text: &str) -> Result<String, String> {
        self.memory().save(path, text).map_err(|e| e.to_string())
    }
    pub fn create(&self, name: &str, text: &str) -> Result<String, String> {
        if name.trim() != name
            || name.is_empty()
            || name.contains(['/', '\\', '\0', '\n', '\r'])
            || !name.ends_with(".md")
            || matches!(name, "AGENTS.md" | "MEMORY.md" | ".md" | "..md")
        {
            return Err(
                "请输入有效的主题文件名，以 .md 结尾（不能使用 AGENTS.md 或 MEMORY.md）".into(),
            );
        }
        let path = format!("{}/{name}", self.prefix());
        self.memory()
            .create(&path, text)
            .map_err(|e| e.to_string())?;
        Ok(path)
    }
    pub fn delete(&self, path: &str) -> Result<(), String> {
        self.memory()
            .delete(path)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn trust(
        &mut self,
        db: &mut HostStore,
        path: &str,
        decision: Option<bool>,
    ) -> Result<(), String> {
        let key = self.scopes[self.selected]
            .key
            .as_deref()
            .ok_or("全局没有远端信任记录")?;
        match decision {
            Some(value) => db.save_remote_doc_trust(key, &[(path.into(), value)]),
            None => db.remove_remote_doc_trust(key, path),
        }
        .map_err(|e| e.to_string())?;
        self.reload(db)
    }
}
pub const TEMPLATE: &str = "---\nname: 新记忆\ndescription: 一句话说明\ntype: project\n---\n\n";
pub fn clear_all(root: &Path, db: &mut HostStore) -> Result<(), String> {
    Memory::new(root, None)
        .clear_all()
        .map_err(|e| e.to_string())?;
    db.clear_remote_doc_trust().map_err(|e| e.to_string())
}
pub fn save_host_note(root: &Path, db: &HostStore, id: &HostId, text: &str) -> Result<(), String> {
    if db.host(id).map_err(|e| e.to_string())?.is_none() {
        return Err("请先保存主机，再保存 AI 说明".into());
    }
    if text.len() > FILE_BYTES {
        return Err("AI 说明最多 64 KB".into());
    }
    let memory = Memory::new(root, Some(id.as_str()));
    if text.trim().is_empty() {
        match memory.delete("/memories/host/AGENTS.md") {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    } else {
        memory
            .save("/memories/host/AGENTS.md", text)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
pub fn relative_time(modified: SystemTime, now: SystemTime) -> String {
    let elapsed = now
        .duration_since(modified)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    match elapsed {
        0..60 => "刚刚".into(),
        60..3600 => format!("{} 分钟前", elapsed / 60),
        3600..86400 => format!("{} 小时前", elapsed / 3600),
        _ => format!("{} 天前", elapsed / 86400),
    }
}
#[cfg(test)]
#[path = "memory_model_tests.rs"]
mod tests;
