//! Portable, versioned snapshots and deterministic merge previews. No deletions
//! are propagated. All SQL identifiers come from the fixed schema below.
use crate::vault::{read_all, restore_options, snapshot, write_secret, write_verified};
use crate::{Backend, CredentialStore, SecretError, SecretKey};
use rusqlite::{
    Connection, params_from_iter,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::{Map, Value, json};
use shellmate_core::Secret;
use std::collections::{HashMap, HashSet};
use zeroize::{Zeroize, Zeroizing};

type Result<T> = std::result::Result<T, String>;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Group,
    Host,
    CommandGroup,
    Command,
    Rule,
    Credential,
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Group => "分组",
            Self::Host => "主机",
            Self::CommandGroup => "命令分组",
            Self::Command => "快捷命令",
            Self::Rule => "转发规则",
            Self::Credential => "凭据",
        }
    }
    fn tag(self) -> &'static str {
        match self {
            Self::Group => "groups",
            Self::Host => "hosts",
            Self::CommandGroup => "snippet_groups",
            Self::Command => "snippets",
            Self::Rule => "forward_rules",
            Self::Credential => "credentials",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        [
            Self::Group,
            Self::Host,
            Self::CommandGroup,
            Self::Command,
            Self::Rule,
            Self::Credential,
        ]
        .into_iter()
        .find(|k| k.tag() == s)
        .ok_or_else(|| "配置包包含未知项目类型".into())
    }
    fn columns(self) -> &'static [&'static str] {
        match self {
            Self::Group => &["name", "tone", "sort"],
            Self::Host => &[
                "name",
                "addr",
                "port",
                "username",
                "group_id",
                "favorite",
                "protocol",
                "auth",
                "identity_file",
                "serial_baud",
                "serial_bits",
                "serial_parity",
                "serial_stop",
                "serial_flow",
                "serial_newline",
            ],
            Self::CommandGroup => &["name", "sort"],
            Self::Command => &["group_id", "name", "command", "sort"],
            Self::Rule => &[
                "host_id",
                "kind",
                "name",
                "listen_host",
                "listen_port",
                "target_host",
                "target_port",
                "enabled",
                "sort",
            ],
            Self::Credential => &[],
        }
    }
    fn id_column(self) -> &'static str {
        if matches!(self, Self::CommandGroup | Self::Command) {
            "stable_id"
        } else {
            "id"
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contents {
    pub hosts: bool,
    pub groups: bool,
    pub commands: bool,
    pub rules: bool,
    pub credentials: bool,
}
impl Default for Contents {
    fn default() -> Self {
        Self {
            hosts: true,
            groups: true,
            commands: true,
            rules: true,
            credentials: true,
        }
    }
}
impl Contents {
    fn includes(self, k: Kind) -> bool {
        match k {
            Kind::Group => self.groups,
            Kind::Host => self.hosts,
            Kind::CommandGroup | Kind::Command => self.commands,
            Kind::Rule => self.rules,
            Kind::Credential => self.credentials,
        }
    }
}
/// Deliberately no Debug: a payload can own credential plaintext.
#[derive(Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: Kind,
    pub id: String,
    pub fields: Map<String, Value>,
    pub secret: Option<Secret>,
}
impl Item {
    pub fn name(&self) -> String {
        self.fields
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&self.id)
            .to_owned()
    }
    fn same(&self, other: &Self) -> bool {
        self.kind == other.kind && self.fields == other.fields && self.secret == other.secret
    }
    fn natural(&self) -> Vec<Value> {
        let cols: &[&str] = match self.kind {
            Kind::Group | Kind::CommandGroup => &["name"],
            Kind::Host => &["name", "addr", "port", "username"],
            Kind::Command => &["group_id", "name"],
            Kind::Rule => &["host_id", "kind", "listen_host", "listen_port"],
            Kind::Credential => &[],
        };
        if self.kind == Kind::Credential {
            vec![json!(self.id)]
        } else {
            cols.iter()
                .map(|c| self.fields.get(*c).cloned().unwrap_or(Value::Null))
                .collect()
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityAlias {
    pub kind: Kind,
    pub remote: String,
    pub local: String,
}
#[derive(Clone, Default)]
pub struct Bundle {
    pub items: Vec<Item>,
    /// Known identities from earlier natural-key merges travel with the item.
    pub aliases: Vec<IdentityAlias>,
}
struct SensitiveJson(Value);
impl Drop for SensitiveJson {
    fn drop(&mut self) {
        fn wipe(v: &mut Value) {
            match v {
                Value::String(s) => s.zeroize(),
                Value::Array(a) => a.iter_mut().for_each(wipe),
                Value::Object(o) => o.values_mut().for_each(wipe),
                _ => {}
            }
        }
        wipe(&mut self.0)
    }
}
impl Bundle {
    pub fn encrypt(&self, password: &str, params: shellmate_vault::KdfParams) -> Result<Vec<u8>> {
        // Stream borrowed secrets directly into a wiping buffer: never build
        // an intermediate serde Value or ordinary String containing plaintext.
        use std::io::Write;
        let capacity = self.items.iter().try_fold(32usize, |size, item| {
            let fields = serde_json::to_vec(&item.fields)
                .map_err(|_| "无法编码配置包")?
                .len();
            let secret = item.secret.as_ref().map_or(0, |s| s.expose().len());
            size.checked_add(256 + fields + 6 * item.id.len() + 6 * secret)
                .filter(|n| *n <= 64 * 1024 * 1024)
                .ok_or("配置包过大")
        })?;
        let alias_json = json!(
            self.aliases
                .iter()
                .map(|a| json!([a.kind.tag(), a.remote, a.local]))
                .collect::<Vec<_>>()
        );
        let alias_bytes = serde_json::to_vec(&alias_json).map_err(|_| "无法编码配置包")?;
        let capacity = capacity
            .checked_add(alias_bytes.len() + 32)
            .filter(|n| *n <= 64 * 1024 * 1024)
            .ok_or("配置包过大")?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
        bytes.extend_from_slice(b"{\"version\":1,\"items\":[");
        for (index, item) in self.items.iter().enumerate() {
            if index > 0 {
                bytes.push(b',');
            }
            bytes.extend_from_slice(b"{\"kind\":");
            serde_json::to_writer(&mut *bytes, item.kind.tag()).map_err(|_| "无法编码配置包")?;
            bytes.write_all(b",\"id\":").map_err(|_| "无法编码配置包")?;
            serde_json::to_writer(&mut *bytes, &item.id).map_err(|_| "无法编码配置包")?;
            bytes.extend_from_slice(b",\"fields\":");
            serde_json::to_writer(&mut *bytes, &item.fields).map_err(|_| "无法编码配置包")?;
            bytes.extend_from_slice(b",\"secret\":");
            serde_json::to_writer(&mut *bytes, &item.secret.as_ref().map(Secret::expose))
                .map_err(|_| "无法编码配置包")?;
            bytes.push(b'}');
        }
        bytes.extend_from_slice(b"],\"aliases\":");
        bytes.extend_from_slice(&alias_bytes);
        bytes.push(b'}');
        shellmate_vault::encrypt_bundle(password, params, &bytes).map_err(|e| e.to_string())
    }
    pub fn decrypt(password: &str, bytes: &[u8]) -> Result<Self> {
        let bytes = shellmate_vault::decrypt_bundle(password, bytes).map_err(|e| e.to_string())?;
        let value = SensitiveJson(serde_json::from_slice(&bytes).map_err(|_| "配置包内容损坏")?);
        if value.0["version"] != 1 {
            return Err("不支持此配置包内容版本，请升级 Shellmate".into());
        }
        let rows = value.0["items"].as_array().ok_or("配置包内容损坏")?;
        if rows.len() > 100_000 {
            return Err("配置包项目过多".into());
        }
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        for row in rows {
            let kind = Kind::parse(row["kind"].as_str().ok_or("缺少项目类型")?)?;
            let id = row["id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 4096)
                .ok_or("项目标识无效")?
                .to_owned();
            if !seen.insert((kind, id.clone())) {
                return Err("配置包含重复项目标识".into());
            }
            let fields = row["fields"].as_object().ok_or("项目内容无效")?.clone();
            let secret = row["secret"].as_str().map(Secret::new);
            let item = Item {
                kind,
                id,
                fields,
                secret,
            };
            validate(&item)?;
            items.push(item);
        }
        items.sort_by_key(|i| i.kind);
        let mut aliases = Vec::new();
        let mut alias_keys = HashSet::new();
        if let Some(rows) = value.0.get("aliases") {
            let rows = rows.as_array().ok_or("配置包身份映射无效")?;
            if rows.len() > 100_000 {
                return Err("身份映射过多".into());
            }
            for row in rows {
                let fields = row
                    .as_array()
                    .filter(|a| a.len() == 3)
                    .ok_or("身份映射无效")?;
                let kind = Kind::parse(fields[0].as_str().ok_or("身份映射无效")?)?;
                let remote = fields[1]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 4096)
                    .ok_or("身份映射无效")?
                    .to_owned();
                let local = fields[2].as_str().ok_or("身份映射无效")?.to_owned();
                if !alias_keys.insert((kind, remote.clone()))
                    || !items.iter().any(|i| i.kind == kind && i.id == local)
                {
                    return Err("身份映射重复或缺少目标".into());
                }
                aliases.push(IdentityAlias {
                    kind,
                    remote,
                    local,
                });
            }
        }
        Ok(Self { items, aliases })
    }
}
fn validate(i: &Item) -> Result<()> {
    if i.kind == Kind::Credential {
        if i.secret.is_none() || !i.fields.is_empty() {
            return Err("凭据项目无效".into());
        }
        return Ok(());
    }
    if i.secret.is_some() {
        return Err("非凭据项目含秘密数据".into());
    }
    let mut allowed = i.kind.columns().to_vec();
    if i.kind == Kind::Host {
        allowed.extend(["tags", "jumps"])
    }
    if i.fields.len() != allowed.len() || allowed.iter().any(|k| !i.fields.contains_key(*k)) {
        return Err("配置包字段缺失或不支持".into());
    }
    for (k, v) in &i.fields {
        if matches!(k.as_str(), "tags" | "jumps") {
            if !v
                .as_array()
                .is_some_and(|a| a.len() <= 1000 && a.iter().all(Value::is_string))
            {
                return Err("主机关系字段无效".into());
            }
        } else if matches!(
            k.as_str(),
            "sort"
                | "port"
                | "favorite"
                | "serial_baud"
                | "serial_bits"
                | "serial_stop"
                | "listen_port"
                | "target_port"
                | "enabled"
        ) {
            if v.is_null() && k == "target_port" {
                continue;
            }
            if !v.is_i64() {
                return Err("配置包数值字段无效".into());
            }
            if k.ends_with("port") && !(0..=65535).contains(&v.as_i64().unwrap()) {
                return Err("配置包端口超出范围".into());
            }
        } else if !v.is_string()
            && !(v.is_null()
                && matches!(
                    k.as_str(),
                    "username" | "group_id" | "identity_file" | "target_host"
                ))
        {
            return Err("配置包文本字段无效".into());
        }
    }
    Ok(())
}
fn config(conn: &Connection) -> Result<Bundle> {
    let mut items = Vec::new();
    for kind in [
        Kind::Group,
        Kind::Host,
        Kind::CommandGroup,
        Kind::Command,
        Kind::Rule,
    ] {
        let sql = format!(
            "SELECT {},{} FROM {} ORDER BY rowid",
            kind.id_column(),
            kind.columns().join(","),
            kind.tag()
        );
        let mut q = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = q.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let id: String = row.get(0).map_err(|e| e.to_string())?;
            let mut fields = Map::new();
            for (n, col) in kind.columns().iter().enumerate() {
                let value = match row.get_ref(n + 1).map_err(|e| e.to_string())? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(v) => json!(v),
                    ValueRef::Text(v) => json!(std::str::from_utf8(v).map_err(|_| "文本编码损坏")?),
                    _ => return Err("主机库字段类型损坏".into()),
                };
                fields.insert((*col).into(), value);
            }
            if kind == Kind::Command {
                let stable: String = conn
                    .query_row(
                        "SELECT stable_id FROM snippet_groups WHERE id=?1",
                        [fields["group_id"].as_i64().ok_or("命令分组损坏")?],
                        |r| r.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                fields.insert("group_id".into(), json!(stable));
            }
            if kind == Kind::Host {
                for (table, col, key) in [
                    ("host_tags", "tag", "tags"),
                    ("host_jumps", "jump_id", "jumps"),
                ] {
                    let mut q = conn
                        .prepare(&format!(
                            "SELECT {col} FROM {table} WHERE host_id=?1 ORDER BY position"
                        ))
                        .map_err(|e| e.to_string())?;
                    let values = q
                        .query_map([&id], |r| r.get::<_, String>(0))
                        .map_err(|e| e.to_string())?
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map_err(|e| e.to_string())?;
                    fields.insert(key.into(), json!(values));
                }
            }
            items.push(Item {
                kind,
                id,
                fields,
                secret: None,
            });
        }
    }
    let mut q = conn
        .prepare("SELECT kind,remote_id,local_id FROM bundle_aliases ORDER BY kind,remote_id")
        .map_err(|e| e.to_string())?;
    let raw = q
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    let aliases = raw
        .into_iter()
        .map(|(kind, remote, local)| {
            Ok(IdentityAlias {
                kind: Kind::parse(&kind)?,
                remote,
                local,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Bundle { items, aliases })
}
impl CredentialStore {
    pub fn export_bundle(&self, contents: Contents) -> Result<Bundle> {
        let _op = self.operation.lock().map_err(|_| "凭据操作不可用")?;
        let mut bundle = config(&self.db.borrow().conn)?;
        bundle.items.retain(|i| contents.includes(i.kind));
        if contents.credentials {
            for (k, s) in read_all(self.active()).map_err(|e| e.to_string())? {
                bundle.items.push(Item {
                    kind: Kind::Credential,
                    id: k.account(),
                    fields: Map::new(),
                    secret: Some(s),
                });
            }
        }
        // Omitted groups detach hosts; omitted hosts keep rule/jump references
        // for matching on another device. Missing dependencies are reported before commit.
        if !contents.groups {
            for i in &mut bundle.items {
                if i.kind == Kind::Host {
                    i.fields.insert("group_id".into(), Value::Null);
                }
            }
        }
        bundle.aliases.retain(|a| {
            bundle
                .items
                .iter()
                .any(|i| i.kind == a.kind && i.id == a.local)
        });
        Ok(bundle)
    }
    pub fn preview_bundle(&self, incoming: Bundle) -> Result<Preview> {
        let contents = Contents {
            credentials: incoming.items.iter().any(|i| i.kind == Kind::Credential),
            ..Contents::default()
        };
        let local = self.export_bundle(contents)?;
        Preview::new(local, incoming)
    }
    pub fn apply_bundle(&self, preview: &Preview) -> Result<usize> {
        let _op = self.operation.lock().map_err(|_| "凭据操作不可用")?;
        let changes = preview.materialize()?;
        let credentials = changes
            .iter()
            .filter(|i| i.kind == Kind::Credential)
            .map(|i| {
                (
                    SecretKey::from_account(&i.id),
                    i.secret.clone().expect("validated credential"),
                )
            })
            .collect::<Vec<_>>();
        let backend = self.backend();
        let active = self.active();
        let before =
            snapshot(active, credentials.iter().map(|(k, _)| k)).map_err(|e| e.to_string())?;
        // Reject stale previews so a connection/editor cannot be overwritten silently.
        let mut current = config(&self.db.borrow().conn)?;
        if preview
            .local
            .items
            .iter()
            .any(|i| i.kind == Kind::Credential)
        {
            for (k, s) in read_all(active).map_err(|e| e.to_string())? {
                current.items.push(Item {
                    kind: Kind::Credential,
                    id: k.account(),
                    fields: Map::new(),
                    secret: Some(s),
                });
            }
        }
        if !same_snapshot(&current, &preview.local) {
            return Err("本机内容已变化，请重新预览配置包".into());
        }
        let encrypted = if backend == Backend::Master {
            credentials
                .iter()
                .map(|(k, s)| {
                    self.vault
                        .encrypt(k, s)
                        .map(|e| (k.clone(), e))
                        .map_err(|e| e.to_string())
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let mut db = self.db.borrow_mut();
        let tx = db.conn.transaction().map_err(|e| e.to_string())?;
        tx.pragma_update(None, "defer_foreign_keys", true)
            .map_err(|e| e.to_string())?;
        for item in changes.iter().filter(|i| i.kind != Kind::Credential) {
            write_item(&tx, item)?;
        }
        // Validate complete resulting library, including jump cycles and protocol fields.
        super::hosts::load_hosts(&tx, None)
            .map_err(|e| e.to_string())?
            .iter()
            .try_for_each(|h| {
                h.serial.validate().map_err(str::to_owned)?;
                super::hosts::resolve(&tx, &h.id)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })?;
        let bad_rules:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM forward_rules WHERE listen_port NOT BETWEEN 0 AND 65535 OR (kind IN ('L','R') AND (target_host IS NULL OR target_port IS NULL OR target_port NOT BETWEEN 0 AND 65535)))",[],|r|r.get(0)).map_err(|e|e.to_string())?;
        if bad_rules {
            return Err("转发规则的目标或端口无效".into());
        }
        for row in &preview.rows {
            if let Some(existing) = &row.existing
                && row.choice != Choice::KeepBoth
            {
                for remote in std::iter::once(row.incoming.id.as_str()).chain(
                    preview
                        .incoming_aliases
                        .iter()
                        .filter(|a| a.kind == row.incoming.kind && a.local == row.incoming.id)
                        .map(|a| a.remote.as_str()),
                ) {
                    if remote != existing.id {
                        tx.execute("INSERT INTO bundle_aliases VALUES(?1,?2,?3) ON CONFLICT(kind,remote_id) DO UPDATE SET local_id=excluded.local_id",rusqlite::params![row.incoming.kind.tag(),remote,existing.id]).map_err(|e|e.to_string())?;
                    }
                }
            } else if row.classification == Classification::New {
                for alias in preview
                    .incoming_aliases
                    .iter()
                    .filter(|a| a.kind == row.incoming.kind && a.local == row.incoming.id)
                {
                    tx.execute("INSERT INTO bundle_aliases VALUES(?1,?2,?3) ON CONFLICT(kind,remote_id) DO UPDATE SET local_id=excluded.local_id",rusqlite::params![alias.kind.tag(),alias.remote,alias.local]).map_err(|e|e.to_string())?;
                }
            }
        }
        for (k, e) in &encrypted {
            write_secret(&tx, k, e).map_err(|e| e.to_string())?;
        }
        if backend == Backend::Keychain
            && let Err(e) = write_verified(active, &credentials)
        {
            return Err(restore_options(active, &before, e).to_string());
        }
        if let Err(e) = tx.commit() {
            return Err(if backend == Backend::Keychain {
                restore_options(active, &before, SecretError(e.to_string())).to_string()
            } else {
                e.to_string()
            });
        }
        Ok(changes.len())
    }
}
fn same_snapshot(a: &Bundle, b: &Bundle) -> bool {
    a.items.len() == b.items.len() && a.items.iter().all(|i| b.items.iter().any(|j| i == j))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Classification {
    New,
    Identical,
    Conflict,
}
impl Classification {
    pub fn label(self) -> &'static str {
        match self {
            Self::New => "新增",
            Self::Identical => "相同",
            Self::Conflict => "冲突",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Skip,
    Overwrite,
    KeepBoth,
}
impl Choice {
    pub fn label(self) -> &'static str {
        match self {
            Self::Skip => "跳过",
            Self::Overwrite => "覆盖",
            Self::KeepBoth => "另存一份",
        }
    }
}
pub struct PreviewRow {
    pub incoming: Item,
    pub existing: Option<Item>,
    pub classification: Classification,
    pub choice: Choice,
}
impl PreviewRow {
    pub fn diff(&self) -> Vec<String> {
        if self.classification != Classification::Conflict {
            return Vec::new();
        }
        let Some(old) = &self.existing else {
            return Vec::new();
        };
        if self.incoming.kind == Kind::Credential {
            return vec!["凭据内容不同（不显示明文）".into()];
        }
        self.incoming
            .fields
            .iter()
            .filter(|(k, v)| old.fields.get(*k) != Some(*v))
            .map(|(k, v)| {
                format!(
                    "{}：{} → {}",
                    field_label(k),
                    compact(old.fields.get(k).unwrap_or(&Value::Null)),
                    compact(v)
                )
            })
            .collect()
    }
}
fn field_label(field: &str) -> &str {
    match field {
        "name" => "名称",
        "addr" => "地址",
        "port" => "端口",
        "username" => "用户名",
        "group_id" => "分组",
        "favorite" => "收藏",
        "protocol" => "协议",
        "auth" => "认证方式",
        "identity_file" => "私钥路径",
        "serial_baud" => "波特率",
        "serial_bits" => "数据位",
        "serial_parity" => "校验",
        "serial_stop" => "停止位",
        "serial_flow" => "流控",
        "serial_newline" => "换行",
        "tags" => "标签",
        "jumps" => "跳板",
        "tone" => "颜色",
        "sort" => "顺序",
        "command" => "命令",
        "host_id" => "主机",
        "kind" => "转发类型",
        "listen_host" => "监听地址",
        "listen_port" => "监听端口",
        "target_host" => "目标地址",
        "target_port" => "目标端口",
        "enabled" => "启用",
        _ => field,
    }
}
fn compact(v: &Value) -> String {
    let s = v
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string());
    let mut out = s.chars().take(90).collect::<String>();
    if s.chars().count() > 90 {
        out.push('…')
    }
    out
}
pub const PREVIEW_FOOTER: &str =
    "按稳定 ID 合并，未改动的项目自动跳过。删除不会传播（无删除标记）；本机已有项目会保留。";
pub struct Preview {
    pub rows: Vec<PreviewRow>,
    incoming_aliases: Vec<IdentityAlias>,
    local: Bundle,
}
impl Preview {
    pub fn new(local: Bundle, mut incoming: Bundle) -> Result<Self> {
        incoming.items.sort_by_key(|i| i.kind);
        let mut rows = Vec::new();
        let mut mapping = HashMap::new();
        for original in incoming.items {
            validate(&original)?;
            let mut normalized = original.clone();
            remap(&mut normalized, &mapping);
            let identities = std::iter::once(original.id.as_str())
                .chain(
                    incoming
                        .aliases
                        .iter()
                        .filter(|a| a.kind == original.kind && a.local == original.id)
                        .map(|a| a.remote.as_str()),
                )
                .collect::<HashSet<_>>();
            let matches = local
                .items
                .iter()
                .filter(|i| {
                    i.kind == original.kind
                        && (identities.contains(i.id.as_str())
                            || local.aliases.iter().any(|a| {
                                a.kind == i.kind
                                    && a.local == i.id
                                    && identities.contains(a.remote.as_str())
                            }))
                })
                .collect::<Vec<_>>();
            let direct = local
                .items
                .iter()
                .find(|i| i.kind == original.kind && i.id == original.id);
            if direct.is_none() && matches.len() > 1 {
                return Err(format!(
                    "「{}」的跨设备身份指向多个本机项目，请整理重复项目",
                    original.name()
                ));
            }
            let by_id = direct.or_else(|| matches.first().copied());
            let candidates = local
                .items
                .iter()
                .filter(|i| i.kind == normalized.kind && i.natural() == normalized.natural())
                .collect::<Vec<_>>();
            if by_id.is_none() && candidates.len() > 1 {
                return Err(format!(
                    "「{}」有多个自然键匹配，请先整理本机重复项目",
                    original.name()
                ));
            }
            let existing = by_id.or_else(|| candidates.first().copied()).cloned();
            let classification = match &existing {
                None => Classification::New,
                Some(e) if normalized.same(e) => Classification::Identical,
                Some(_) => Classification::Conflict,
            };
            mapping.insert(
                (original.kind, original.id.clone()),
                existing
                    .as_ref()
                    .map_or_else(|| original.id.clone(), |e| e.id.clone()),
            );
            rows.push(PreviewRow {
                incoming: original,
                existing,
                classification,
                choice: if classification == Classification::New {
                    Choice::Overwrite
                } else {
                    Choice::Skip
                },
            });
        }
        for row in &mut rows {
            let mut item = row.incoming.clone();
            remap(&mut item, &mapping);
            if let Some(existing) = &row.existing {
                row.classification = if item.same(existing) {
                    Classification::Identical
                } else {
                    Classification::Conflict
                };
                row.choice = Choice::Skip;
            }
        }
        Ok(Self {
            rows,
            local,
            incoming_aliases: incoming.aliases,
        })
    }
    pub fn choose_all(&mut self, choice: Choice) {
        for row in &mut self.rows {
            if row.classification == Classification::Conflict {
                row.choice = choice;
            }
        }
    }
    pub fn counts(&self) -> [usize; 3] {
        let mut n = [0; 3];
        for r in &self.rows {
            n[match r.classification {
                Classification::New => 0,
                Classification::Identical => 1,
                Classification::Conflict => 2,
            }] += 1;
        }
        n
    }
    fn materialize(&self) -> Result<Vec<Item>> {
        let mut mapping = HashMap::new();
        for row in &self.rows {
            let id = if row.classification != Classification::Identical
                && row.choice == Choice::KeepBoth
            {
                uuid::Uuid::new_v4().to_string()
            } else {
                row.existing
                    .as_ref()
                    .map_or_else(|| row.incoming.id.clone(), |e| e.id.clone())
            };
            mapping.insert((row.incoming.kind, row.incoming.id.clone()), id);
        }
        let mut result = Vec::new();
        let mut names: HashSet<(Kind, String)> = self
            .local
            .items
            .iter()
            .map(|i| (i.kind, i.name()))
            .collect();
        for row in &self.rows {
            let mut item = row.incoming.clone();
            if item.kind != Kind::Credential {
                item.id = mapping[&(item.kind, item.id.clone())].clone();
            }
            remap(&mut item, &mapping);
            let copied_credential = item.kind == Kind::Credential
                && item.id
                    != row
                        .existing
                        .as_ref()
                        .map_or(row.incoming.id.as_str(), |e| e.id.as_str());
            if (row.classification == Classification::Identical || row.choice == Choice::Skip)
                && !copied_credential
            {
                continue;
            }
            if item.kind == Kind::Credential && !copied_credential {
                item.id = mapping[&(item.kind, row.incoming.id.clone())].clone();
            }
            if row.choice == Choice::KeepBoth {
                if item.kind == Kind::Credential {
                    // Host passwords follow a copied host; orphan/key-file copies
                    // are retained under a new account and never replace the original.
                    if !copied_credential {
                        item.id = format!(
                            "import:{}:{}（导入）",
                            uuid::Uuid::new_v4(),
                            row.incoming.id
                        );
                    }
                } else {
                    let base = format!("{}（导入）", item.name());
                    let mut name = base.clone();
                    let mut n = 2;
                    while names.contains(&(item.kind, name.clone())) {
                        name = format!("{base} {n}");
                        n += 1;
                    }
                    names.insert((item.kind, name.clone()));
                    item.fields.insert("name".into(), json!(name));
                }
            }
            result.push(item);
        }
        // Identity remapping can converge two items; never silently let the last win.
        let mut ids = HashSet::new();
        if result.iter().any(|i| !ids.insert((i.kind, i.id.clone()))) {
            return Err("导入选择产生重复目标，请调整冲突选项".into());
        }
        Ok(result)
    }
}
fn remap(i: &mut Item, map: &HashMap<(Kind, String), String>) {
    let refs: &[(&str, Kind)] = match i.kind {
        Kind::Host => &[("group_id", Kind::Group)],
        Kind::Command => &[("group_id", Kind::CommandGroup)],
        Kind::Rule => &[("host_id", Kind::Host)],
        _ => &[],
    };
    for (field, kind) in refs {
        if let Some(old) = i.fields.get(*field).and_then(Value::as_str)
            && let Some(id) = map.get(&(*kind, old.to_owned()))
        {
            i.fields.insert((*field).into(), json!(id));
        }
    }
    if i.kind == Kind::Host
        && let Some(jumps) = i.fields.get_mut("jumps").and_then(Value::as_array_mut)
    {
        for jump in jumps {
            if let Some(s) = jump.as_str()
                && let Some(id) = map.get(&(Kind::Host, s.to_owned()))
            {
                *jump = json!(id);
            }
        }
    }
    if i.kind == Kind::Credential
        && let SecretKey::HostPassword(host) = SecretKey::from_account(&i.id)
        && let Some(id) = map.get(&(Kind::Host, host.as_str().to_owned()))
    {
        i.id = SecretKey::HostPassword(shellmate_core::HostId::from_raw(id)).account();
    }
}
fn sql_value(v: &Value) -> Result<SqlValue> {
    match v {
        Value::Null => Ok(SqlValue::Null),
        Value::String(s) => Ok(SqlValue::Text(s.clone())),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .ok_or("数值超出范围".into()),
        _ => Err("字段类型无效".into()),
    }
}
fn write_item(conn: &Connection, i: &Item) -> Result<()> {
    let columns = i.kind.columns();
    let id_col = i.kind.id_column();
    let mut fields = i.fields.clone();
    if i.kind == Kind::Command {
        let id: i64 = conn
            .query_row(
                "SELECT id FROM snippet_groups WHERE stable_id=?1",
                [fields["group_id"].as_str().ok_or("命令分组无效")?],
                |r| r.get(0),
            )
            .map_err(|_| "缺少命令分组，请同时导出分组")?;
        fields.insert("group_id".into(), json!(id));
    }
    let values = std::iter::once(Ok(SqlValue::Text(i.id.clone())))
        .chain(columns.iter().map(|c| sql_value(&fields[*c])))
        .collect::<Result<Vec<_>>>()?;
    let sql = format!(
        "INSERT INTO {} ({id_col},{}) VALUES({}) ON CONFLICT({id_col}) DO UPDATE SET {}",
        i.kind.tag(),
        columns.join(","),
        vec!["?"; columns.len() + 1].join(","),
        columns
            .iter()
            .map(|c| format!("{c}=excluded.{c}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    conn.execute(&sql, params_from_iter(values))
        .map_err(|e| format!("无法应用{}：{e}", i.kind.label()))?;
    if i.kind == Kind::Host {
        for (table, col, key) in [
            ("host_tags", "tag", "tags"),
            ("host_jumps", "jump_id", "jumps"),
        ] {
            conn.execute(&format!("DELETE FROM {table} WHERE host_id=?1"), [&i.id])
                .map_err(|e| e.to_string())?;
            for (pos, value) in fields[key]
                .as_array()
                .ok_or("主机关系损坏")?
                .iter()
                .enumerate()
            {
                conn.execute(
                    &format!("INSERT INTO {table}(host_id,position,{col}) VALUES(?1,?2,?3)"),
                    rusqlite::params![i.id, pos as i64, value.as_str().ok_or("主机关系损坏")?],
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::tests::{fast, setup};
    use crate::{SecretStore, Snippet, SnippetGroup};
    use shellmate_core::{Group, GroupId, GroupTone, Host};
    fn populated() -> (crate::SharedStore, CredentialStore) {
        let (db, _, m) = setup();
        let group = Group {
            id: GroupId::generate(),
            name: "production".into(),
            tone: GroupTone::Accent,
            sort: 0,
        };
        let mut host = Host::new("web", "web.example.test");
        host.group = Some(group.id.clone());
        host.tags = vec!["prod".into()];
        db.borrow_mut().save_group(&group).unwrap();
        db.borrow_mut().save_host(&host).unwrap();
        db.borrow_mut()
            .save_snippets(&[SnippetGroup {
                id: "group-stable".into(),
                name: "ops".into(),
                items: vec![Snippet {
                    id: "command-stable".into(),
                    name: "status".into(),
                    command: "uptime".into(),
                }],
            }])
            .unwrap();
        db.borrow().conn.execute("INSERT INTO forward_rules(id,host_id,kind,name,listen_host,listen_port,target_host,target_port) VALUES('rule-stable',?1,'L','app','127.0.0.1',8080,'web',80)",[host.id.as_str()]).unwrap();
        m.set(
            &SecretKey::HostPassword(host.id),
            &Secret::new("fixture secret"),
        )
        .unwrap();
        (db, m)
    }
    #[test]
    fn all_content_roundtrip_and_corruption() {
        let (_, m) = populated();
        let bundle = m.export_bundle(Contents::default()).unwrap();
        assert_eq!(bundle.items.len(), 6);
        let cipher = bundle.encrypt("Bundle9!Meadow", fast()).unwrap();
        let plain = Bundle::decrypt("Bundle9!Meadow", &cipher).unwrap();
        assert!(same_snapshot(&bundle, &plain));
        assert!(Bundle::decrypt("wrong", &cipher).is_err());
        let mut broken = cipher;
        let n = broken.len() - 1;
        broken[n] ^= 1;
        assert!(Bundle::decrypt("Bundle9!Meadow", &broken).is_err());
        let (db, _, b) = setup();
        let p = b.preview_bundle(plain).unwrap();
        assert_eq!(p.counts(), [6, 0, 0]);
        assert_eq!(b.apply_bundle(&p).unwrap(), 6);
        assert_eq!(db.borrow().hosts().unwrap().len(), 1);
        assert_eq!(
            db.borrow().snippets().unwrap()[0].items[0].id,
            "command-stable"
        );
        assert!(same_snapshot(
            &bundle,
            &b.export_bundle(Contents::default()).unwrap()
        ));
    }
    #[test]
    fn manual_sync_edit_on_b_updates_a_without_duplicates() {
        let (a_db, a) = populated();
        let (b_db, _, b) = setup();
        let p = b
            .preview_bundle(a.export_bundle(Contents::default()).unwrap())
            .unwrap();
        b.apply_bundle(&p).unwrap();
        let mut host = b_db.borrow().hosts().unwrap().remove(0);
        host.addr = "new.example.test".into();
        b_db.borrow_mut().save_host(&host).unwrap();
        let mut groups = b_db.borrow().snippets().unwrap();
        groups[0].name = "renamed".into();
        groups[0].items[0].name = "status renamed".into();
        groups[0].items[0].command = "hostname".into();
        b_db.borrow_mut().save_snippets(&groups).unwrap();
        let mut p = a
            .preview_bundle(b.export_bundle(Contents::default()).unwrap())
            .unwrap();
        assert_eq!(p.counts(), [0, 3, 3]);
        p.choose_all(Choice::Overwrite);
        a.apply_bundle(&p).unwrap();
        assert_eq!(a_db.borrow().hosts().unwrap().len(), 1);
        assert_eq!(a_db.borrow().hosts().unwrap()[0].addr, "new.example.test");
        assert_eq!(
            a_db.borrow().snippets().unwrap()[0].items[0].command,
            "hostname"
        );
        assert_eq!(
            a.preview_bundle(b.export_bundle(Contents::default()).unwrap())
                .unwrap()
                .counts(),
            [0, 6, 0]
        );
    }
    #[test]
    fn conflict_skip_overwrite_keep_both() {
        for choice in [Choice::Skip, Choice::Overwrite, Choice::KeepBoth] {
            let (db, m) = populated();
            let mut bundle = m
                .export_bundle(Contents {
                    credentials: false,
                    ..Default::default()
                })
                .unwrap();
            let host = bundle
                .items
                .iter_mut()
                .find(|i| i.kind == Kind::Host)
                .unwrap();
            host.fields.insert("addr".into(), json!("changed.test"));
            let mut p = m.preview_bundle(bundle).unwrap();
            p.choose_all(choice);
            m.apply_bundle(&p).unwrap();
            let hosts = db.borrow().hosts().unwrap();
            assert_eq!(hosts.len(), if choice == Choice::KeepBoth { 2 } else { 1 });
            if choice == Choice::Overwrite {
                assert_eq!(hosts[0].addr, "changed.test")
            }
            if choice == Choice::KeepBoth {
                assert!(hosts.iter().any(|h| h.name == "web（导入）"));
            }
        }
    }
    #[test]
    fn natural_key_match_preserves_local_id() {
        let (_, a) = populated();
        let (db, _, b) = setup();
        let host = Host::new("web", "web.example.test");
        db.borrow_mut().save_host(&host).unwrap();
        let p = b
            .preview_bundle(
                a.export_bundle(Contents {
                    groups: false,
                    credentials: false,
                    commands: false,
                    rules: false,
                    hosts: true,
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.rows[0].existing.as_ref().unwrap().id, host.id.as_str());
        let mut p = p;
        p.choose_all(Choice::Overwrite);
        b.apply_bundle(&p).unwrap();
        assert_eq!(db.borrow().hosts().unwrap().len(), 1);
        assert_eq!(db.borrow().hosts().unwrap()[0].id, host.id);
    }
    #[test]
    fn missing_dependency_rolls_back_and_stale_preview_rejected() {
        let (_, a) = populated();
        let (db, _, b) = setup();
        let rules = a
            .export_bundle(Contents {
                hosts: false,
                groups: false,
                commands: false,
                rules: true,
                credentials: false,
            })
            .unwrap();
        let p = b.preview_bundle(rules).unwrap();
        assert!(b.apply_bundle(&p).is_err());
        assert!(db.borrow().hosts().unwrap().is_empty());
        let p = b
            .preview_bundle(a.export_bundle(Contents::default()).unwrap())
            .unwrap();
        db.borrow_mut()
            .save_host(&Host::new("local edit", "local.test"))
            .unwrap();
        assert!(b.apply_bundle(&p).unwrap_err().contains("重新预览"));
        assert_eq!(db.borrow().hosts().unwrap().len(), 1);
    }
    #[test]
    fn import_credentials_into_locked_vault_fails_without_writes() {
        let (_, a) = populated();
        let (db, _, b) = setup();
        b.switch_to_master("Master9!Meadow", fast()).unwrap();
        let p = b
            .preview_bundle(a.export_bundle(Contents::default()).unwrap())
            .unwrap();
        b.lock();
        assert!(b.apply_bundle(&p).is_err());
        assert!(db.borrow().hosts().unwrap().is_empty());
        b.unlock("Master9!Meadow").unwrap();
        b.apply_bundle(&p).unwrap();
        assert_eq!(b.keys().unwrap().len(), 1);
    }
    #[test]
    fn natural_identity_alias_survives_later_rename_in_both_directions() {
        let (a_db, _, a) = setup();
        let (b_db, _, b) = setup();
        let host_a = Host::new("web", "same.test");
        let host_b = Host::new("web", "same.test");
        a_db.borrow_mut().save_host(&host_a).unwrap();
        b_db.borrow_mut().save_host(&host_b).unwrap();
        let options = Contents {
            credentials: false,
            ..Default::default()
        };
        let p = b.preview_bundle(a.export_bundle(options).unwrap()).unwrap();
        assert_eq!(p.counts(), [0, 1, 0]);
        b.apply_bundle(&p).unwrap();
        let mut changed = host_b.clone();
        changed.name = "new name".into();
        changed.addr = "new.test".into();
        b_db.borrow_mut().save_host(&changed).unwrap();
        let exported = b.export_bundle(options).unwrap();
        assert_eq!(exported.aliases.len(), 1);
        let bytes = exported.encrypt("Bundle9!Meadow", fast()).unwrap();
        let mut p = a
            .preview_bundle(Bundle::decrypt("Bundle9!Meadow", &bytes).unwrap())
            .unwrap();
        assert_eq!(p.counts(), [0, 0, 1]);
        p.choose_all(Choice::Overwrite);
        a.apply_bundle(&p).unwrap();
        assert_eq!(a_db.borrow().hosts().unwrap().len(), 1);
        assert_eq!(a_db.borrow().hosts().unwrap()[0].id, host_a.id);
        changed = a_db.borrow().hosts().unwrap().remove(0);
        changed.name = "renamed on A".into();
        a_db.borrow_mut().save_host(&changed).unwrap();
        let mut p = b.preview_bundle(a.export_bundle(options).unwrap()).unwrap();
        assert_eq!(p.counts(), [0, 0, 1]);
        p.choose_all(Choice::Overwrite);
        b.apply_bundle(&p).unwrap();
        assert_eq!(b_db.borrow().hosts().unwrap().len(), 1);
        assert_eq!(b_db.borrow().hosts().unwrap()[0].name, "renamed on A");
    }
    #[test]
    fn keep_both_host_copies_its_password_to_new_identity() {
        let (db, m) = populated();
        let mut incoming = m.export_bundle(Contents::default()).unwrap();
        incoming
            .items
            .iter_mut()
            .find(|i| i.kind == Kind::Host)
            .unwrap()
            .fields
            .insert("addr".into(), json!("changed.test"));
        let mut p = m.preview_bundle(incoming).unwrap();
        p.choose_all(Choice::KeepBoth);
        m.apply_bundle(&p).unwrap();
        let copied = db
            .borrow()
            .hosts()
            .unwrap()
            .into_iter()
            .find(|h| h.name.ends_with("（导入）"))
            .unwrap();
        assert_eq!(
            m.get(&SecretKey::HostPassword(copied.id))
                .unwrap()
                .unwrap()
                .expose(),
            "fixture secret"
        );
        assert_eq!(m.keys().unwrap().len(), 2);
    }
    #[test]
    fn keychain_import_nth_write_failure_rolls_back_entire_config() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Fail {
            store: crate::MemoryStore,
            writes: AtomicUsize,
        }
        impl SecretStore for Fail {
            fn keys(&self) -> std::result::Result<Vec<SecretKey>, SecretError> {
                self.store.keys()
            }
            fn get(&self, k: &SecretKey) -> std::result::Result<Option<Secret>, SecretError> {
                self.store.get(k)
            }
            fn set(&self, k: &SecretKey, s: &Secret) -> std::result::Result<(), SecretError> {
                if self.writes.fetch_add(1, Ordering::SeqCst) == 1 {
                    return Err(SecretError("injected second write".into()));
                }
                self.store.set(k, s)
            }
            fn delete(&self, k: &SecretKey) -> std::result::Result<(), SecretError> {
                self.store.delete(k)
            }
        }
        let (_, a) = populated();
        a.set(&SecretKey::Account("extra".into()), &Secret::new("second"))
            .unwrap();
        let db = crate::SharedStore::new(crate::HostStore::open_in_memory().unwrap());
        let external = Arc::new(Fail {
            store: crate::MemoryStore::new(),
            writes: AtomicUsize::new(0),
        });
        let b = CredentialStore::new(
            db.clone(),
            external.clone(),
            Arc::new(crate::vault::tests::Auth),
            Arc::new(crate::vault::tests::Keys::default()),
        );
        let preview = b
            .preview_bundle(a.export_bundle(Contents::default()).unwrap())
            .unwrap();
        assert!(b.apply_bundle(&preview).is_err());
        assert!(db.borrow().hosts().unwrap().is_empty());
        assert!(db.borrow().groups().unwrap().is_empty());
        assert!(db.borrow().snippets().unwrap().is_empty());
        assert!(external.keys().unwrap().is_empty());
    }
}
