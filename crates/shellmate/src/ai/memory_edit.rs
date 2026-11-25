//! Conservative topic form codec and navigation state, independent of GPUI.
use super::memory_model::Model;

pub const AGENTS_PLACEHOLDER: &str =
    "写给 AI 的长期说明，例如：回答用中文；修改生产环境前先说明影响范围。";
pub const RAW_HINT: &str = "此文件包含无法按表单编辑的内容，按原文编辑";
pub const INDEX_HINT: &str = "AI 会自动维护此索引，每条一行：- [标题](文件.md) — 一句话";
pub const TYPES: [(&str, &str); 4] = [
    ("user", "用户"),
    ("feedback", "反馈"),
    ("project", "项目"),
    ("reference", "参考"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topic {
    pub name: String,
    pub description: String,
    pub kind: usize,
    pub body: String,
}
impl Default for Topic {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            kind: 2,
            body: String::new(),
        }
    }
}
impl Topic {
    pub fn parse(text: &str) -> Option<Self> {
        let mut lines = text.split_inclusive('\n');
        if lines.next()?.trim_end_matches(['\r', '\n']) != "---" {
            return None;
        }
        let mut values = std::collections::BTreeMap::new();
        for line in lines.by_ref() {
            let line = line.trim_end_matches(['\r', '\n']);
            if line == "---" {
                let name = values.remove("name")?;
                let description = values.remove("description")?;
                let kind = TYPES.iter().position(|(value, _)| {
                    Some(*value) == values.get("type").map(String::as_str)
                })?;
                let tail = lines.collect::<String>();
                // One separator blank line belongs to frontmatter formatting, not the body.
                let body = tail
                    .strip_prefix("\r\n")
                    .or_else(|| tail.strip_prefix('\n'))
                    .unwrap_or(&tail)
                    .into();
                return Some(Self {
                    name,
                    description,
                    kind,
                    body,
                });
            }
            let (key, value) = line.split_once(':')?;
            if !value.is_empty() && !value.starts_with([' ', '\t']) {
                return None;
            }
            if !matches!(key, "name" | "description" | "type" | "modified")
                || values.contains_key(key)
            {
                return None;
            }
            values.insert(key.to_owned(), scalar(value)?);
        }
        None
    }
    pub fn serialize(&self) -> String {
        format!(
            "---\nname: {}\ndescription: {}\ntype: {}\n---\n\n{}",
            quote(&self.name),
            quote(&self.description),
            TYPES[self.kind].0,
            self.body
        )
    }
}
fn scalar(value: &str) -> Option<String> {
    let value = value.trim();
    let decoded = if value.starts_with('"') {
        serde_json::from_str::<String>(value).ok()?
    } else if let Some(inner) = value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        let mut chars = inner.chars();
        let mut out = String::new();
        while let Some(c) = chars.next() {
            if c == '\'' && chars.next() != Some('\'') {
                return None;
            }
            out.push(c);
        }
        out
    } else {
        // YAML collections, block scalars, aliases, comments and nested mappings need raw editing.
        if value.starts_with([
            '[', ']', '{', '}', '|', '>', '&', '*', '!', '%', '@', '`', '\'', '"', '#',
        ]) || value.contains(": ")
            || value.ends_with(':')
            || value.contains(" #")
            || value.starts_with("- ")
            || value.starts_with("? ")
            || value.chars().any(char::is_control)
        {
            return None;
        }
        value.to_owned()
    };
    // Multiline metadata cannot be represented by the single-line form without losing content.
    if decoded.contains(['\r', '\n']) {
        return None;
    }
    Some(decoded)
}
fn quote(text: &str) -> String {
    let text = text.replace(['\r', '\n'], "");
    if text.chars().any(char::is_control) {
        serde_json::to_string(&text).unwrap()
    } else {
        format!("'{}'", text.replace('\'', "''"))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Instructions(String),
    Index(String),
    Topic(Topic),
    Raw(String),
}
impl Content {
    pub fn text(&self) -> String {
        match self {
            Self::Topic(topic) => topic.serialize(),
            Self::Instructions(text) | Self::Index(text) | Self::Raw(text) => text.clone(),
        }
    }
    pub fn body(&self) -> &str {
        match self {
            Self::Topic(topic) => &topic.body,
            Self::Instructions(text) | Self::Index(text) | Self::Raw(text) => text,
        }
    }
    pub fn body_mut(&mut self) -> &mut String {
        match self {
            Self::Topic(topic) => &mut topic.body,
            Self::Instructions(text) | Self::Index(text) | Self::Raw(text) => text,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub filename: String,
    pub content: Content,
}
#[derive(Clone, Debug)]
pub struct Detail {
    pub path: Option<String>,
    pub exists: bool,
    pub draft: Draft,
    original: Draft,
}
impl Detail {
    pub fn load(path: String, text: String, exists: bool) -> Self {
        let content = match path.rsplit('/').next() {
            Some("AGENTS.md") => Content::Instructions(text),
            Some("MEMORY.md") => Content::Index(text),
            _ => Topic::parse(&text)
                .map(Content::Topic)
                .unwrap_or(Content::Raw(text)),
        };
        let draft = Draft {
            filename: path.rsplit('/').next().unwrap_or_default().into(),
            content,
        };
        Self {
            path: Some(path),
            exists,
            original: draft.clone(),
            draft,
        }
    }
    pub fn new_topic() -> Self {
        let draft = Draft {
            filename: String::new(),
            content: Content::Topic(Topic::default()),
        };
        Self {
            path: None,
            exists: false,
            original: draft.clone(),
            draft,
        }
    }
    pub fn dirty(&self) -> bool {
        self.draft != self.original
    }
    pub fn section(&self) -> &'static str {
        if matches!(self.draft.content, Content::Instructions(_)) {
            "说明"
        } else {
            "记忆"
        }
    }
    pub fn title(&self) -> &str {
        if self.path.is_none() {
            return "新建记忆";
        }
        match &self.draft.content {
            Content::Instructions(_) => "AI 说明",
            Content::Index(_) => "索引",
            Content::Topic(topic) if !topic.name.trim().is_empty() => &topic.name,
            _ => &self.draft.filename,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Navigation {
    Back,
    Scope(usize),
    Close,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavigationResult {
    Applied,
    NeedsConfirmation,
    Close,
}
#[derive(Default)]
pub struct Page {
    pub detail: Option<Detail>,
}
impl Page {
    pub fn from_focus(model: &Model, path: Option<&str>) -> Result<Self, String> {
        let mut page = Self::default();
        if let Some(path) = path {
            page.open(model, path)?;
        }
        Ok(page)
    }
    pub fn open(&mut self, model: &Model, path: &str) -> Result<(), String> {
        let (text, exists) = match model.memory().read(path) {
            Ok(text) => (text, true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
            Err(e) => return Err(e.to_string()),
        };
        self.detail = Some(Detail::load(path.into(), text, exists));
        Ok(())
    }
    pub fn create(&mut self) {
        self.detail = Some(Detail::new_topic());
    }
    pub fn dirty(&self) -> bool {
        self.detail.as_ref().is_some_and(Detail::dirty)
    }
    pub fn navigate(
        &mut self,
        model: &mut Model,
        action: Navigation,
        discard: bool,
    ) -> Result<NavigationResult, String> {
        if matches!(action,Navigation::Scope(i) if i==model.selected) {
            return Ok(NavigationResult::Applied);
        }
        if self.dirty() && !discard {
            return Ok(NavigationResult::NeedsConfirmation);
        }
        if let Navigation::Scope(index) = action {
            if index >= model.scopes.len() {
                return Err("记忆范围不存在".into());
            }
            let old = model.selected;
            model.selected = index;
            if let Err(error) = model.reload() {
                model.selected = old;
                return Err(error);
            }
        }
        self.detail = None;
        Ok(if action == Navigation::Close {
            NavigationResult::Close
        } else {
            NavigationResult::Applied
        })
    }
    pub fn save(&mut self, model: &mut Model) -> Result<String, String> {
        let detail = self.detail.as_ref().ok_or("请先选择文件")?;
        let text = detail.draft.content.text();
        let (path, message) = if let Some(path) = &detail.path {
            (
                path.clone(),
                if matches!(detail.draft.content, Content::Raw(_)) {
                    model
                        .memory()
                        .save_verbatim(path, &text)
                        .map_err(|e| e.to_string())?
                } else {
                    model.save(path, &text)?
                },
            )
        } else {
            (model.create(&detail.draft.filename, &text)?, String::new())
        };
        self.open(model, &path)?;
        model.reload()?;
        Ok(message)
    }
    pub fn delete(&mut self, model: &mut Model) -> Result<(), String> {
        let detail = self
            .detail
            .as_ref()
            .filter(|d| d.exists)
            .ok_or("文件不存在")?;
        model.delete(detail.path.as_deref().unwrap())?;
        self.detail = None;
        model.reload()
    }
    pub fn subtitle(&self, model: &Model) -> String {
        let scope = &model.scopes[model.selected].label;
        match &self.detail {
            Some(detail) if detail.path.is_some() => format!("{scope} · {}", detail.draft.filename),
            Some(_) => scope.clone(),
            None => String::new(),
        }
    }
}
#[cfg(test)]
#[path = "memory_edit_tests.rs"]
mod tests;
