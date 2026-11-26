//! Read-only bounded directory manifests, resumable by opaque result reference.
use super::{session_catalog::Endpoint, target_router::Router, tasks::Tasks};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shellmate_ai::{
    agent::LocalResult,
    session_tools::{Compare, Side},
};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use unicode_normalization::UnicodeNormalization;
const MAX_ENTRIES: usize = 100_000;
const MAX_SAVED_BYTES: u64 = 64 * 1024 * 1024;
const BATCH_ENTRIES: usize = 10_000;
const BATCH_BYTES: u64 = 1024 * 1024 * 1024;
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
#[derive(Clone)]
struct ScanLease {
    cancelled: Arc<AtomicBool>,
    control: shellmate_ai::interaction::Control,
    endpoint: Endpoint,
}
impl ScanLease {
    fn check(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Acquire)
            || self.control.intent() != shellmate_ai::interaction::Intent::None
        {
            return Err("比较已取消".into());
        }
        if !self.endpoint.lease.load(Ordering::Acquire)
            || !self.endpoint.binding.alive.load(Ordering::Acquire)
        {
            return Err("目标已断开；保留已读取的部分结果".into());
        }
        Ok(())
    }
}
fn failure(message: String) -> Item {
    Item {
        kind: "error".into(),
        size: 0,
        modified: None,
        mode: None,
        digest: None,
        link: None,
        error: Some(message),
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Item {
    kind: String,
    size: u64,
    modified: Option<i64>,
    mode: Option<u32>,
    digest: Option<String>,
    link: Option<String>,
    error: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Saved {
    request: Compare,
    left_root: String,
    right_root: String,
    left: BTreeMap<String, Item>,
    right: BTreeMap<String, Item>,
    left_queue: VecDeque<String>,
    right_queue: VecDeque<String>,
    left_done: bool,
    right_done: bool,
    started: i64,
}
#[derive(Serialize)]
struct Difference {
    path: String,
    status: &'static str,
    left: Option<Item>,
    right: Option<Item>,
}
enum Source {
    Local,
    Remote(Arc<shellmate_sftp::SftpSession>),
}
impl Source {
    async fn new(e: &Endpoint) -> Result<Self, String> {
        if e.binding.local {
            Ok(Self::Local)
        } else {
            let c = e
                .binding
                .connection
                .as_ref()
                .ok_or("该终端不支持文件读取")?;
            shellmate_sftp::SftpSession::open(c)
                .await
                .map(|s| Self::Remote(Arc::new(s)))
                .map_err(|e| e.to_string())
        }
    }
    async fn canonical(&self, path: &str) -> Result<String, String> {
        match self {
            Self::Local => {
                let p = path.to_owned();
                tokio::task::spawn_blocking(move || {
                    std::fs::canonicalize(p).map(|p| p.to_string_lossy().into_owned())
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
            }
            Self::Remote(s) => s.canonicalize(path).await.map_err(|e| e.to_string()),
        }
    }
    async fn list(&self, path: &str, lease: &ScanLease) -> Result<Vec<(String, Item)>, String> {
        match self {
            Self::Local => {
                let p = path.to_owned();
                let lease = lease.clone();
                tokio::task::spawn_blocking(move || {
                    use std::os::unix::fs::MetadataExt;
                    if std::fs::canonicalize(&p).map_err(|e| e.to_string())? != Path::new(&p) {
                        return Err("扫描期间目录路径发生变化".into());
                    }
                    if std::fs::symlink_metadata(&p)
                        .map_err(|e| e.to_string())?
                        .file_type()
                        .is_symlink()
                    {
                        return Err("扫描期间目录被替换为链接".into());
                    }
                    let mut rows = vec![];
                    for e in std::fs::read_dir(p).map_err(|e| e.to_string())? {
                        lease.check()?;
                        if rows.len() >= MAX_ENTRIES {
                            return Err("单目录超过 100000 项，未完成扫描".into());
                        }
                        let e = e.map_err(|e| e.to_string())?;
                        let name = e
                            .file_name()
                            .into_string()
                            .map_err(|_| "目录包含非 UTF-8 文件名，无法完整比较")?;
                        let m = std::fs::symlink_metadata(e.path()).map_err(|e| e.to_string())?;
                        let t = m.file_type();
                        let kind = if t.is_symlink() {
                            "link"
                        } else if t.is_dir() {
                            "dir"
                        } else if t.is_file() {
                            "file"
                        } else {
                            "special"
                        };
                        rows.push((
                            name,
                            Item {
                                kind: kind.into(),
                                size: m.len(),
                                modified: Some(m.mtime()),
                                mode: Some(m.mode() & 0o7777),
                                digest: None,
                                link: if t.is_symlink() {
                                    std::fs::read_link(e.path())
                                        .ok()
                                        .map(|p| p.to_string_lossy().into_owned())
                                } else {
                                    None
                                },
                                error: None,
                            },
                        ));
                    }
                    Ok(rows)
                })
                .await
                .map_err(|e| e.to_string())?
            }
            Self::Remote(s) => {
                if s.canonicalize(path).await.map_err(|e| e.to_string())? != path {
                    return Err("目录路径发生变化".into());
                }
                if s.lstat(path).await.map_err(|e| e.to_string())?.kind
                    != shellmate_sftp::EntryKind::Dir
                {
                    return Err("扫描目标不再是目录".into());
                }
                let mut rows = vec![];
                for e in s
                    .list_bounded(path, MAX_ENTRIES)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    lease.check()?;
                    if e.name == "." || e.name == ".." {
                        continue;
                    }
                    let kind = match e.kind {
                        shellmate_sftp::EntryKind::File => "file",
                        shellmate_sftp::EntryKind::Dir => "dir",
                        shellmate_sftp::EntryKind::Symlink => "link",
                        _ => "special",
                    };
                    let link = if kind == "link" {
                        s.read_link(&e.path).await.ok()
                    } else {
                        None
                    };
                    rows.push((
                        e.name,
                        Item {
                            kind: kind.into(),
                            size: e.size,
                            modified: e.modified,
                            mode: e.mode,
                            digest: None,
                            link,
                            error: None,
                        },
                    ));
                }
                Ok(rows)
            }
        }
    }
    async fn hash(&self, path: &str, expected: &Item, lease: &ScanLease) -> Result<String, String> {
        match self {
            Self::Local => {
                let p = path.to_owned();
                let size = expected.size;
                let modified = expected.modified;
                let lease = lease.clone();
                tokio::task::spawn_blocking(move || {
                    use std::io::Read;
                    use std::os::unix::fs::MetadataExt;
                    if std::fs::canonicalize(&p).map_err(|e| e.to_string())? != Path::new(&p) {
                        return Err("读取期间文件路径发生变化".into());
                    }
                    let before = std::fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
                    if !before.is_file() || before.len() != size || Some(before.mtime()) != modified
                    {
                        return Err("读取前文件发生变化".into());
                    }
                    use std::os::unix::fs::OpenOptionsExt;
                    let mut f = std::fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&p)
                        .map_err(|e| e.to_string())?;
                    let opened = f.metadata().map_err(|e| e.to_string())?;
                    if opened.ino() != before.ino() || opened.dev() != before.dev() {
                        return Err("读取目标发生变化".into());
                    }
                    let mut h = Sha256::new();
                    let mut buf = [0u8; 65536];
                    let mut read = 0;
                    loop {
                        lease.check()?;
                        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
                        if n == 0 {
                            break;
                        }
                        read += n as u64;
                        if read > size {
                            return Err("读取期间文件增长".into());
                        }
                        h.update(&buf[..n]);
                    }
                    let after = f.metadata().map_err(|e| e.to_string())?;
                    if read != size
                        || after.mtime_nsec() != before.mtime_nsec()
                        || after.mtime() != before.mtime()
                    {
                        return Err("读取期间文件发生变化".into());
                    }
                    Ok(hex::encode(h.finalize()))
                })
                .await
                .map_err(|e| e.to_string())?
            }
            Self::Remote(s) => {
                if s.canonicalize(path).await.map_err(|e| e.to_string())? != path {
                    return Err("文件路径发生变化".into());
                }
                let before = s.lstat(path).await.map_err(|e| e.to_string())?;
                if before.kind != shellmate_sftp::EntryKind::File
                    || before.size != expected.size
                    || before.modified != expected.modified
                {
                    return Err("读取前文件发生变化".into());
                }
                let (hash, _) = s
                    .file_digest(path, BATCH_BYTES)
                    .await
                    .map_err(|e| e.to_string())?;
                let after = s.lstat(path).await.map_err(|e| e.to_string())?;
                if before.size != after.size || before.modified != after.modified {
                    return Err("读取期间文件发生变化".into());
                }
                Ok(hex::encode(hash))
            }
        }
    }
}
fn root(side: &Side, e: &Endpoint) -> Result<String, String> {
    let path = if side.path.starts_with('/') {
        side.path.clone()
    } else {
        format!(
            "{}/{}",
            e.target.cwd.as_deref().ok_or("相对路径需要已知工作目录")?,
            side.path
        )
    };
    if path.contains('\0') || path.len() > 8192 {
        return Err("目录路径无效".into());
    }
    Ok(path)
}
fn file(root: &Path, id: &str) -> Result<PathBuf, String> {
    if !id.starts_with("compare-") || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return Err("比较记录引用无效".into());
    }
    Ok(root.join(format!("{id}.json")))
}
async fn scan(
    source: &Source,
    base: &str,
    queue: &mut VecDeque<String>,
    items: &mut BTreeMap<String, Item>,
    args: &Compare,
    lease: &ScanLease,
) -> bool {
    let mut count = 0;
    let mut bytes = 0u64;
    while let Some(relative) = queue.pop_front() {
        if let Err(e) = lease.check() {
            items.insert(String::new(), failure(e));
            queue.push_front(relative);
            return false;
        }
        let path = if relative.is_empty() {
            base.to_owned()
        } else {
            format!("{base}/{relative}")
        };
        if let Some(item) = items.get_mut(&relative) {
            if item.kind == "file" && !args.metadata_only {
                if item.size > BATCH_BYTES {
                    item.error = Some("文件超过单批读取上限 1 GiB，未核实内容".into());
                    continue;
                }
                if bytes + item.size > BATCH_BYTES {
                    queue.push_front(relative);
                    return false;
                }
                bytes += item.size;
                let hashed = tokio::select! {
                    result=source.hash(&path,item,lease)=>result,
                    _=super::exec::disconnected(&lease.endpoint.lease)=>Err("目标已断开".into()),
                    _=lease.control.urgent()=>Err("比较已暂停".into()),
                };
                if lease.check().is_err() {
                    queue.push_front(relative);
                    return false;
                }
                match hashed {
                    Ok(hash) => {
                        item.digest = Some(hash);
                        item.error = None;
                    }
                    Err(e) => item.error = Some(e),
                }
            }
            if item.kind != "dir" && item.kind != "error" {
                count += 1;
                if count >= BATCH_ENTRIES {
                    return queue.is_empty();
                }
                continue;
            }
        }
        let listed = tokio::select! {
            result=source.list(&path,lease)=>result,
            _=super::exec::disconnected(&lease.endpoint.lease)=>Err("目标已断开".into()),
            _=lease.control.urgent()=>Err("比较已暂停".into()),
        };
        if let Err(e) = lease.check() {
            items.insert(String::new(), failure(e));
            queue.push_front(relative);
            return false;
        }
        match listed {
            Ok(rows) => {
                if relative.is_empty() {
                    items.remove("");
                } else if let Some(item) = items.get_mut(&relative) {
                    item.error = None;
                }
                if items.len().saturating_add(rows.len()) > MAX_ENTRIES {
                    items.insert(
                        relative.clone(),
                        failure("清单超过 100000 项，请通过 exclude 缩小范围".into()),
                    );
                    queue.push_front(relative);
                    return false;
                }
                for (name, item) in rows {
                    let rel = if relative.is_empty() {
                        name
                    } else {
                        format!("{relative}/{name}")
                    };
                    if args.exclude.iter().any(|p| {
                        rel == *p || rel.starts_with(&format!("{}/", p.trim_end_matches('/')))
                    }) {
                        continue;
                    }
                    queue.push_back(rel.clone());
                    items.insert(rel, item);
                }
            }
            Err(e) => {
                if let Some(item) = items.get_mut(&relative) {
                    item.error = Some(e);
                } else {
                    items.insert(relative.clone(), failure(e));
                }
                queue.push_front(relative);
                return false;
            }
        }
        count += 1;
        if count >= BATCH_ENTRIES {
            return queue.is_empty();
        }
    }
    true
}
fn unread_path(path: &str, items: &BTreeMap<String, Item>) -> bool {
    items.iter().any(|(p, i)| {
        (i.kind == "error" || (i.kind == "dir" && i.error.is_some()))
            && (p.is_empty() || path == p || path.starts_with(&format!("{p}/")))
    })
}
fn differences(saved: &Saved) -> Vec<Difference> {
    let mut keys = saved
        .left
        .keys()
        .chain(saved.right.keys())
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .map(|path| {
            let l = saved.left.get(path);
            let r = saved.right.get(path);
            let status = match (l, r) {
                (Some(l), _) if l.error.is_some() => "读取失败或未核实",
                (_, Some(r)) if r.error.is_some() => "读取失败或未核实",
                (Some(l), Some(r)) if l.kind != r.kind => "类型不同",
                (Some(l), Some(_)) if l.kind == "special" => "特殊文件已跳过",
                (Some(l), Some(r)) if l.kind == "link" => {
                    if l.link.is_none() || r.link.is_none() {
                        "链接未核实"
                    } else if l.link != r.link {
                        "链接不同"
                    } else {
                        "链接相同"
                    }
                }
                (Some(l), Some(r)) if l.kind == "file" && !saved.request.metadata_only => {
                    if l.digest.is_none() || r.digest.is_none() {
                        "内容待核实"
                    } else if l.digest != r.digest {
                        "内容不同"
                    } else if l.mode != r.mode {
                        "仅权限不同"
                    } else {
                        "内容相同"
                    }
                }
                (Some(l), Some(r)) => {
                    if (l.kind != "dir" && l.size != r.size)
                        || l.modified != r.modified
                        || l.mode != r.mode
                    {
                        "元数据不同"
                    } else {
                        "元数据相同"
                    }
                }
                (Some(_), None) => {
                    if saved.right_done && !unread_path(path, &saved.right) {
                        "仅左侧存在"
                    } else {
                        "右侧尚未扫描完成"
                    }
                }
                (None, Some(_)) => {
                    if saved.left_done && !unread_path(path, &saved.left) {
                        "仅右侧存在"
                    } else {
                        "左侧尚未扫描完成"
                    }
                }
                _ => "未知",
            };
            Difference {
                path: path.clone(),
                status,
                left: l.cloned(),
                right: r.cloned(),
            }
        })
        .collect()
}
pub async fn compare(router: &Router, args: Compare, tasks: &Tasks) -> LocalResult {
    match run(router, args, tasks).await {
        Ok(r) => r,
        Err(e) => LocalResult::error(e),
    }
}
fn load(path: &Path) -> Result<Saved, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| "比较记录不存在或已清理")?;
    if file.metadata().map_err(|e| e.to_string())?.len() > MAX_SAVED_BYTES {
        return Err("比较记录超过读取上限".into());
    }
    let mut data = Vec::new();
    file.take(MAX_SAVED_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() as u64 > MAX_SAVED_BYTES {
        return Err("比较记录超过读取上限".into());
    }
    serde_json::from_slice(&data).map_err(|_| "比较记录损坏".into())
}
async fn scan_side(
    endpoint: Endpoint,
    base: &mut String,
    queue: &mut VecDeque<String>,
    items: &mut BTreeMap<String, Item>,
    args: &Compare,
    cancelled: Arc<AtomicBool>,
    control: shellmate_ai::interaction::Control,
) -> bool {
    if queue.is_empty() {
        return true;
    }
    let lease = ScanLease {
        cancelled,
        control,
        endpoint,
    };
    let _permit = tokio::select! {permit=lease.endpoint.slots.clone().acquire_owned()=>permit.ok(),_=lease.control.urgent()=>return false};
    let opened = tokio::select! {result=Source::new(&lease.endpoint)=>result,_=lease.control.urgent()=>return false};
    let source = match opened {
        Ok(s) => s,
        Err(e) => {
            items.insert(String::new(), failure(e));
            return false;
        }
    };
    if queue.front().is_some_and(|p| p.is_empty()) {
        let canonical = tokio::select! {result=source.canonical(base)=>result,_=lease.control.urgent()=>return false};
        match canonical {
            Ok(path) => {
                *base = path;
                items.remove("");
            }
            Err(e) => {
                items.insert(String::new(), failure(e));
                return false;
            }
        }
    }
    scan(&source, base, queue, items, args, &lease).await
}
async fn run(router: &Router, args: Compare, tasks: &Tasks) -> Result<LocalResult, String> {
    let left = router.resolve(Some(&args.left.session_id))?;
    let right = router.resolve(Some(&args.right.session_id))?;
    let conversation = tasks.conversation_id();
    let dir = shellmate_core::ai_output::directory(&router.root, conversation)
        .map_err(|e| e.to_string())?;
    let id = args
        .cursor
        .clone()
        .unwrap_or_else(|| format!("compare-{}", shellmate_core::HostId::generate()));
    let path = file(&dir, &id)?;
    let mut saved = if args.cursor.is_some() {
        let saved = load(&path)?;
        if saved.request.left.session_id != args.left.session_id
            || saved.request.right.session_id != args.right.session_id
            || saved.request.left.path != args.left.path
            || saved.request.right.path != args.right.path
            || saved.request.metadata_only != args.metadata_only
            || saved.request.exclude != args.exclude
        {
            return Err("继续比较时目标和比较条件必须一致".into());
        }
        saved
    } else {
        Saved {
            request: args.clone(),
            left_root: root(&args.left, &left)?,
            right_root: root(&args.right, &right)?,
            left: BTreeMap::new(),
            right: BTreeMap::new(),
            left_queue: VecDeque::from([String::new()]),
            right_queue: VecDeque::from([String::new()]),
            left_done: false,
            right_done: false,
            started: super::now(),
        }
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(cancelled.clone());
    let (a, b) = tokio::join!(
        scan_side(
            left.clone(),
            &mut saved.left_root,
            &mut saved.left_queue,
            &mut saved.left,
            &args,
            cancelled.clone(),
            tasks.control()
        ),
        scan_side(
            right.clone(),
            &mut saved.right_root,
            &mut saved.right_queue,
            &mut saved.right,
            &args,
            cancelled,
            tasks.control()
        )
    );
    saved.left_done = a;
    saved.right_done = b;
    let rows = differences(&saved);
    let mut variants: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in &rows {
        variants
            .entry(row.path.nfc().flat_map(char::to_lowercase).collect())
            .or_default()
            .push(row.path.clone());
    }
    let path_collisions = variants
        .into_values()
        .filter(|v| v.len() > 1)
        .collect::<Vec<_>>();
    let verified_bytes = serde_json::json!({"left":saved.left.values().filter(|i|i.digest.is_some()).map(|i|i.size).sum::<u64>(),"right":saved.right.values().filter(|i|i.digest.is_some()).map(|i|i.size).sum::<u64>()});
    let mut stats = BTreeMap::new();
    for row in &rows {
        *stats.entry(row.status).or_insert(0usize) += 1;
    }
    let json = serde_json::to_vec(&saved).map_err(|e| e.to_string())?;
    if json.len() as u64 > MAX_SAVED_BYTES {
        return Err("比较记录超过 64 MiB，请缩小目录范围".into());
    }
    let output_id = format!("comparison-{}", shellmate_core::HostId::generate());
    let report = rows
        .iter()
        .map(|r| format!("{}\t{}", r.path, r.status))
        .collect::<Vec<_>>()
        .join("\n");
    tasks
        .retained()
        .while_retained(|| -> Result<(), String> {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let mut temp = tempfile::NamedTempFile::new_in(&dir).map_err(|e| e.to_string())?;
            use std::io::Write;
            temp.write_all(&json).map_err(|e| e.to_string())?;
            temp.as_file().sync_all().map_err(|e| e.to_string())?;
            temp.persist(&path).map_err(|e| e.to_string())?;
            let spool =
                shellmate_core::ai_output::Spool::create(&router.root, conversation, &output_id)
                    .map_err(|e| e.to_string())?;
            spool
                .write(report.as_bytes(), false)
                .map_err(|e| e.to_string())?;
            spool.finish().map_err(|e| e.to_string())?;
            Ok(())
        })
        .ok_or("对话已删除，结果不会重新创建")??;
    let total = rows.len();
    let result = serde_json::json!({"left":left.target,"left_path":saved.left_root,"right":right.target,"right_path":saved.right_root,"started_at":saved.started,"observed_at":super::now(),"scan_complete":a&&b,"all_content_verified":a&&b&&!args.metadata_only&&saved.left.values().chain(saved.right.values()).all(|i|i.error.is_none()&&match i.kind.as_str(){"file"=>i.digest.is_some(),"link"=>i.link.is_some(),"dir"=>true,_=>false}),"path_collisions":path_collisions,"verified_bytes":verified_bytes,"limits":{"batch_entries":BATCH_ENTRIES,"batch_bytes_per_side":BATCH_BYTES,"manifest_entries_per_side":MAX_ENTRIES},"summary":stats,"cursor":id,"next_offset":(args.offset+100<total).then_some(args.offset+100),"total_entries":total,"differences":rows.into_iter().skip(args.offset).take(100).collect::<Vec<_>>()});
    let mut result = LocalResult::text(result.to_string());
    result.output_id = Some(output_id);
    Ok(result)
}

/// User-initiated preview: read only the file already present in this comparison,
/// and verify both versions against the manifest before showing a diff.
pub async fn text_diff(
    router: &Router,
    tasks: &Tasks,
    cursor: &str,
    relative: &str,
) -> Result<String, String> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.split('/').any(|p| p == ".." || p == ".")
    {
        return Err("差异路径无效".into());
    }
    let dir = shellmate_core::ai_output::directory(&router.root, tasks.conversation_id())
        .map_err(|e| e.to_string())?;
    let saved = load(&file(&dir, cursor)?)?;
    let left = router.resolve(Some(&saved.request.left.session_id))?;
    let right = router.resolve(Some(&saved.request.right.session_id))?;
    let read =
        async |endpoint: &Endpoint, base: &str, item: Option<&Item>| -> Result<String, String> {
            let item = item.ok_or("此路径只在一侧存在")?;
            if item.kind != "file" || item.size > 256 * 1024 {
                return Err("预览支持不超过 256 KiB 的普通文本文件；其他文件请查看摘要".into());
            }
            let expected = item
                .digest
                .as_ref()
                .ok_or("该文件内容尚未核实，请完成内容比较后预览")?;
            let _permit = endpoint.slots.clone().acquire_owned().await.ok();
            let source = Source::new(endpoint).await?;
            let path = format!("{base}/{relative}");
            if source.canonical(&path).await? != path {
                return Err("文件路径已变化，请重新比较".into());
            }
            let bytes = match source {
                Source::Local => tokio::task::spawn_blocking(move || {
                    use std::io::Read;
                    use std::os::unix::fs::OpenOptionsExt;
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(path)
                        .map_err(|e| e.to_string())?;
                    let mut bytes = Vec::new();
                    file.take(256 * 1024 + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    Ok::<_, String>(bytes)
                })
                .await
                .map_err(|e| e.to_string())??,
                Source::Remote(s) => {
                    s.read_file(&path, 256 * 1024)
                        .await
                        .map_err(|e| e.to_string())?
                        .0
                }
            };
            if bytes.len() > 256 * 1024 || &hex::encode(Sha256::digest(&bytes)) != expected {
                return Err("文件已变化，请重新比较后查看新差异".into());
            }
            if bytes.contains(&0) {
                return Err("二进制文件仅展示摘要与大小差异".into());
            }
            String::from_utf8(bytes).map_err(|_| "文件不是 UTF-8 文本，仅展示摘要".into())
        };
    let (l, r) = tokio::try_join!(
        read(&left, &saved.left_root, saved.left.get(relative)),
        read(&right, &saved.right_root, saved.right.get(relative))
    )?;
    router.resolve(Some(&saved.request.left.session_id))?;
    router.resolve(Some(&saved.request.right.session_id))?;
    let a = format!("{}:{}/{}", left.target.label, saved.left_root, relative);
    let b = format!("{}:{}/{}", right.target.label, saved.right_root, relative);
    tokio::task::spawn_blocking(move || {
        let diff = similar::TextDiff::configure()
            .timeout(std::time::Duration::from_secs(2))
            .diff_lines(&l, &r);
        let mut text = diff.unified_diff().header(&a, &b).to_string();
        if text.is_empty() {
            text = "两侧内容相同".into();
        }
        if text.len() > 128 * 1024 {
            text = text.chars().take(64 * 1024).collect::<String>()
                + "\n…预览已截断，完整比较结果仍可查看";
        }
        text
    })
    .await
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{exec::Binding, session_catalog::Catalog};
    use std::sync::atomic::AtomicBool;
    #[tokio::test]
    async fn local_pair_reports_content_changes_links_and_paginates_without_writes() {
        let root = tempfile::tempdir().unwrap();
        let left = root.path().join("left");
        let right = root.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("same"), "same").unwrap();
        std::fs::write(right.join("same"), "same").unwrap();
        std::fs::write(left.join("different"), "abc").unwrap();
        std::fs::write(right.join("different"), "xyz").unwrap();
        std::fs::write(left.join("only-left"), "data").unwrap();
        std::os::unix::fs::symlink("/tmp", left.join("link")).unwrap();
        std::os::unix::fs::symlink("/var", right.join("link")).unwrap();
        let (tx, _) = async_channel::unbounded();
        let c = Catalog::new(tx.clone());
        let entries = [&left, &right]
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    Binding {
                        session: i,
                        connection: None,
                        local: true,
                        alive: Arc::new(AtomicBool::new(true)),
                        capabilities: shellmate_ai::Capabilities {
                            background: true,
                            cwd: Some(p.to_string_lossy().into()),
                            ..Default::default()
                        },
                    },
                    format!("local-{i}"),
                    "本机终端".into(),
                    None,
                    "window".into(),
                )
            })
            .collect();
        c.update(entries);
        let db = shellmate_store::SharedStore::new(
            shellmate_store::HostStore::open_in_memory().unwrap(),
        );
        let router = Router::new(c.clone(), db, root.path().into(), tx);
        let l = c.session(0).unwrap().target.session_id;
        let r = c.session(1).unwrap().target.session_id;
        router.add(&l, true).unwrap();
        router.add(&r, false).unwrap();
        let args = Compare {
            left: Side {
                session_id: l,
                path: ".".into(),
            },
            right: Side {
                session_id: r,
                path: ".".into(),
            },
            metadata_only: false,
            exclude: vec![],
            cursor: None,
            offset: 0,
        };
        let tasks = Tasks::new(root.path().into(), "test".into());
        let result = run(&router, args.clone(), &tasks).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&result.inline).unwrap();
        assert_eq!(v["summary"]["内容不同"], 1);
        assert_eq!(v["summary"]["内容相同"], 1);
        assert_eq!(v["summary"]["链接不同"], 1);
        assert_eq!(v["summary"]["仅左侧存在"], 1);
        assert_eq!(v["scan_complete"], true);
        let mut next = args;
        next.cursor = v["cursor"].as_str().map(str::to_owned);
        next.offset = 2;
        let second = run(&router, next, &tasks).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&second.inline).unwrap();
        assert_eq!(v["differences"].as_array().unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(right.join("different")).unwrap(),
            "xyz"
        );
    }
    #[tokio::test]
    async fn mixed_local_ssh_comparison_preserves_partial_results_and_verifies_preview() {
        let remote = crate::sftp_fixture::Fixture::default().await;
        remote.file("/project/same", "same", 0o644);
        remote.file("/project/changed", "new content\n", 0o644);
        let root = tempfile::tempdir().unwrap();
        let local = root.path().join("project");
        std::fs::create_dir(&local).unwrap();
        std::fs::write(local.join("same"), "same").unwrap();
        std::fs::write(local.join("changed"), "old content\n").unwrap();
        let (tx, _) = async_channel::unbounded();
        let catalog = Catalog::new(tx.clone());
        catalog.update(vec![
            (
                Binding {
                    session: 1,
                    connection: None,
                    local: true,
                    alive: Arc::new(AtomicBool::new(true)),
                    capabilities: shellmate_ai::Capabilities {
                        background: true,
                        cwd: Some(local.to_string_lossy().into()),
                        ..Default::default()
                    },
                },
                "local".into(),
                "本机终端".into(),
                None,
                "window".into(),
            ),
            (
                Binding {
                    session: 2,
                    connection: Some(remote.connection.clone()),
                    local: false,
                    alive: Arc::new(AtomicBool::new(true)),
                    capabilities: shellmate_ai::Capabilities {
                        background: true,
                        cwd: Some("/project".into()),
                        ..Default::default()
                    },
                },
                "remote".into(),
                "SSH".into(),
                Some("remote-host".into()),
                "window".into(),
            ),
        ]);
        let router = Router::new(
            catalog.clone(),
            shellmate_store::SharedStore::new(
                shellmate_store::HostStore::open_in_memory().unwrap(),
            ),
            root.path().into(),
            tx,
        );
        let left = catalog.session(1).unwrap().target.session_id;
        let right = catalog.session(2).unwrap().target.session_id;
        router.add(&left, true).unwrap();
        router.add(&right, false).unwrap();
        let tasks = Tasks::new(root.path().into(), "mixed".into());
        let args = Compare {
            left: Side {
                session_id: left,
                path: ".".into(),
            },
            right: Side {
                session_id: right,
                path: ".".into(),
            },
            metadata_only: false,
            exclude: vec![],
            cursor: None,
            offset: 0,
        };
        let result = run(&router, args.clone(), &tasks).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&result.inline).unwrap();
        assert_eq!(value["summary"]["内容不同"], 1);
        assert_eq!(value["summary"]["内容相同"], 1);
        assert_eq!(value["scan_complete"], true);
        assert_eq!(value["all_content_verified"], true);
        let cursor = value["cursor"].as_str().unwrap();
        let diff = text_diff(&router, &tasks, cursor, "changed").await.unwrap();
        assert!(diff.contains("-old content"));
        assert!(diff.contains("+new content"));
        assert!(diff.contains("remote:/project"));
        remote.file("/project/changed", "later change", 0o644);
        assert!(
            text_diff(&router, &tasks, cursor, "changed")
                .await
                .unwrap_err()
                .contains("变化")
        );
        tasks
            .control()
            .request(shellmate_ai::interaction::Intent::StopReply);
        let paused = run(&router, args.clone(), &tasks).await.unwrap();
        let paused: serde_json::Value = serde_json::from_str(&paused.inline).unwrap();
        assert_eq!(paused["scan_complete"], false);
        tasks.control().take_intent();
        let mut resumed = args.clone();
        resumed.cursor = paused["cursor"].as_str().map(str::to_owned);
        let completed = run(&router, resumed, &tasks).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&completed.inline).unwrap()["scan_complete"],
            true
        );
        let mut missing = args;
        missing.right.path = "/missing".into();
        let partial = run(&router, missing, &tasks).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&partial.inline).unwrap();
        assert_eq!(value["scan_complete"], false);
        assert_eq!(value["all_content_verified"], false);
        assert!(
            value["differences"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["path"] == "same")
        );
        assert!(value["summary"].get("仅左侧存在").is_none());
        assert!(remote.logs.writes.lock().unwrap().is_empty());
        tasks.discard();
        let dir = shellmate_core::ai_output::directory(root.path(), "mixed").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            run(
                &router,
                Compare {
                    left: Side {
                        session_id: catalog.session(1).unwrap().target.session_id,
                        path: ".".into()
                    },
                    right: Side {
                        session_id: catalog.session(2).unwrap().target.session_id,
                        path: "/project".into()
                    },
                    metadata_only: true,
                    exclude: vec![],
                    cursor: None,
                    offset: 0
                },
                &tasks
            )
            .await
            .is_err()
        );
        assert!(!dir.exists());
    }
}
