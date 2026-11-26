//! One native output reader for command cards, child steps, tasks and history.
use super::{Drawer, agent_ui, markdown};
use crate::{
    ai::{
        output_document::{self as document, Cancel, Document, Segment},
        scroll::{ScrollArea, State as ScrollState},
    },
    theme::{ActivePalette, MONO_FONT},
};
use gpui::{prelude::*, *};
use shellmate_core::ai_output::Completeness;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Range,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[path = "output_ui/drawer.rs"]
mod drawer;
#[path = "output_ui/render.rs"]
mod render;
#[path = "output_ui/selection.rs"]
mod selection;

actions!(
    output_reader,
    [
        CopySelection,
        SelectAll,
        Top,
        Bottom,
        LeftEdge,
        RightEdge,
        PageUp,
        PageDown
    ]
);
pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-c", CopySelection, Some("AiOutput")),
        KeyBinding::new("ctrl-c", CopySelection, Some("AiOutput")),
        KeyBinding::new("cmd-a", SelectAll, Some("AiOutput")),
        KeyBinding::new("cmd-up", Top, Some("AiOutput")),
        KeyBinding::new("cmd-down", Bottom, Some("AiOutput")),
        KeyBinding::new("ctrl-home", Top, Some("AiOutput")),
        KeyBinding::new("ctrl-end", Bottom, Some("AiOutput")),
        KeyBinding::new("cmd-left", LeftEdge, Some("AiOutput")),
        KeyBinding::new("cmd-right", RightEdge, Some("AiOutput")),
        KeyBinding::new("pageup", PageUp, Some("AiOutput")),
        KeyBinding::new("pagedown", PageDown, Some("AiOutput")),
    ]);
}
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Source {
    pub key: String,
    pub output: Option<String>,
    pub preview: String,
    pub running: bool,
    pub incomplete: bool,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadStatus {
    Unloaded,
    Loading,
    Ready,
    Missing,
    Failed,
}
#[derive(Default)]
struct Geometry {
    origin: Point<Pixels>,
    viewport: Bounds<Pixels>,
    hits: Vec<(usize, Vec<(Pixels, u64)>)>,
}
type PageKey = (bool, usize, usize, usize, usize);
pub(super) struct Reading;
pub(super) struct OutputPane {
    source: Source,
    root: PathBuf,
    conversation: String,
    doc: Option<Arc<Document>>,
    status: ReadStatus,
    expanded: bool,
    follow: bool,
    loading: bool,
    revision: u64,
    load_generation: u64,
    lifetime: Cancel,
    load_cancel: Cancel,
    copying: bool,
    copied: bool,
    copy_error: bool,
    pages: BTreeMap<PageKey, Arc<Vec<Segment>>>,
    page_order: VecDeque<PageKey>,
    requested: BTreeSet<PageKey>,
    viewport: Entity<ScrollState>,
    selection_scroll: Entity<crate::ai::selection_scroll::SelectionScroll>,
    geometry: Rc<RefCell<Geometry>>,
    anchor: Rc<RefCell<Bounds<Pixels>>>,
    focus: FocusHandle,
    selection: Option<(u64, u64)>,
    dragging: bool,
    pointer: Point<Pixels>,
    last_visible: Rc<RefCell<Instant>>,
}
impl EventEmitter<Reading> for OutputPane {}
impl Drop for OutputPane {
    fn drop(&mut self) {
        self.load_cancel.store(true, Ordering::Relaxed);
    }
}
impl OutputPane {
    pub fn new(
        source: Source,
        root: PathBuf,
        conversation: String,
        lifetime: Cancel,
        selection_scroll: Entity<crate::ai::selection_scroll::SelectionScroll>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            source,
            root,
            conversation,
            doc: None,
            status: ReadStatus::Unloaded,
            expanded: false,
            follow: true,
            loading: false,
            revision: 0,
            load_generation: 0,
            lifetime,
            load_cancel: Arc::new(AtomicBool::new(false)),
            copying: false,
            copied: false,
            copy_error: false,
            pages: Default::default(),
            page_order: Default::default(),
            requested: Default::default(),
            viewport: cx.new(|_| ScrollState::default()),
            selection_scroll,
            geometry: Default::default(),
            anchor: Default::default(),
            focus: cx.focus_handle(),
            selection: None,
            dragging: false,
            pointer: point(px(0.), px(0.)),
            last_visible: Rc::new(RefCell::new(Instant::now())),
        }
    }
    pub(super) fn tick(&mut self, cx: &mut Context<Self>) {
        if self.source.running
            && self.expanded
            && !self.dragging
            && self.selection.is_none_or(|(a, b)| a == b)
            && self.last_visible.borrow().elapsed() < Duration::from_secs(2)
        {
            self.load(cx);
        }
    }
    fn update_source(&mut self, source: Source, cx: &mut Context<Self>) {
        if self.source == source {
            return;
        }
        let changed = self.source.output != source.output
            || self.source.preview != source.preview
            || self.source.running != source.running;
        self.source = source;
        if changed
            && (self.expanded || self.doc.is_some())
            && !self.dragging
            && self.selection.is_none_or(|(a, b)| a == b)
        {
            self.load_cancel.store(true, Ordering::Relaxed);
            self.loading = false;
            self.load(cx);
        }
        cx.notify();
    }
    fn load(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        if self.doc.is_none() {
            self.status = ReadStatus::Loading;
        }
        self.load_cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.load_cancel.clone();
        let lifetime = self.lifetime.clone();
        let source = self.source.clone();
        let root = self.root.clone();
        let conversation = self.conversation.clone();
        self.load_generation += 1;
        let generation = self.load_generation;
        let previous = self.doc.clone();
        let work = cx.background_executor().spawn(async move {
            document::check(&lifetime)?;
            let result = match source.output.as_deref() {
                Some(id) => Document::refresh(&root, &conversation, id, previous.as_ref(), &cancel),
                None => Document::inline(&source.preview, !source.incomplete, &cancel),
            };
            let (doc, status) = match result {
                Ok(doc) => (doc, ReadStatus::Ready),
                Err(e) if e.kind() != std::io::ErrorKind::Interrupted => {
                    let status = if e.kind() == std::io::ErrorKind::NotFound {
                        ReadStatus::Missing
                    } else {
                        ReadStatus::Failed
                    };
                    (Document::inline(&source.preview, false, &cancel)?, status)
                }
                Err(e) => return Err(e),
            };
            document::check(&lifetime)?;
            document::check(&cancel)?;
            Ok::<_, std::io::Error>((doc, status))
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |s, cx| {
                if generation != s.load_generation {
                    return;
                }
                s.loading = false;
                if let Ok((doc, status)) = result {
                    if s.doc.as_ref().is_none_or(|old| !Arc::ptr_eq(old, &doc)) {
                        s.revision += 1;
                        s.doc = Some(doc);
                        s.pages.clear();
                        s.page_order.clear();
                        s.requested.clear();
                    }
                    s.status = status;
                } else if s.doc.is_none() {
                    s.status = ReadStatus::Failed;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
    pub(super) fn open(&mut self, cx: &mut Context<Self>) {
        if self.expanded {
            return;
        }
        self.expanded = true;
        self.follow = self.source.running;
        self.load(cx);
        cx.notify();
    }
    pub(super) fn copy_all(&mut self, cx: &mut Context<Self>) {
        self.copy(None, cx);
    }
    fn toggle(&mut self, cx: &mut Context<Self>) {
        self.expanded = !self.expanded;
        if self.expanded {
            self.follow = self.source.running;
            self.load(cx);
        } else {
            self.dragging = false;
            self.selection = None;
            self.viewport
                .read(cx)
                .handle
                .set_offset(point(px(0.), px(0.)));
        }
        cx.notify();
    }
    fn copy(&mut self, range: Option<Range<u64>>, cx: &mut Context<Self>) {
        if self.copying {
            return;
        }
        self.copying = true;
        self.copied = false;
        self.copy_error = false;
        let known = self.doc.clone();
        let source = self.source.clone();
        let root = self.root.clone();
        let conversation = self.conversation.clone();
        let cancel = self.lifetime.clone();
        let fallback = matches!(self.status, ReadStatus::Missing | ReadStatus::Failed);
        let work = cx.background_executor().spawn(async move {
            let doc = if range.is_some() || fallback {
                known.ok_or_else(|| std::io::Error::other("no output snapshot"))?
            } else if let Some(id) = source.output.as_deref() {
                Document::refresh(&root, &conversation, id, known.as_ref(), &cancel)?
            } else {
                Document::inline(&source.preview, !source.incomplete, &cancel)?
            };
            let text = doc.copy(range.unwrap_or(0..doc.bytes), &cancel)?;
            Ok::<_, std::io::Error>((text, doc))
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |s, cx| {
                if s.lifetime.load(Ordering::Relaxed) {
                    return;
                }
                s.copying = false;
                match result {
                    Ok((text, doc)) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        s.copied = true;
                        if s.doc.is_none() {
                            s.doc = Some(doc);
                            s.status = ReadStatus::Ready;
                        }
                    }
                    Err(_) => {
                        s.copy_error = true;
                        s.load(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
    fn request_page(&mut self, key: PageKey, cx: &mut Context<Self>) {
        if self.pages.contains_key(&key) || !self.requested.insert(key) {
            return;
        }
        let Some(doc) = self.doc.clone() else {
            return;
        };
        let revision = self.revision;
        let cancel = self.load_cancel.clone();
        let work = cx.background_executor().spawn(async move {
            let rows = (key.1..key.2)
                .filter_map(|row| doc.visible_row(row, key.0))
                .collect::<Vec<_>>();
            doc.region(&rows, key.3, key.4, &cancel).map(Arc::new)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |s, cx| {
                if revision != s.revision {
                    return;
                }
                s.requested.remove(&key);
                if let Ok(page) = result {
                    s.pages.insert(key, page);
                    s.page_order.push_back(key);
                    while s.page_order.len() > 8 {
                        if let Some(old) = s.page_order.pop_front() {
                            s.pages.remove(&old);
                        }
                    }
                    if s.dragging {
                        s.select_at(s.pointer, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    fn copy_label(&self) -> &'static str {
        if self.copying {
            "复制中…"
        } else if self.copied {
            "已复制"
        } else if matches!(self.status, ReadStatus::Missing | ReadStatus::Failed) {
            "复制预览"
        } else if self
            .doc
            .as_ref()
            .is_some_and(|d| d.info.completeness == Completeness::HeadTail)
        {
            "复制已保留内容"
        } else if self.source.running {
            "复制当前输出"
        } else if (self.source.incomplete && self.source.output.is_none())
            || self
                .doc
                .as_ref()
                .is_some_and(|d| d.info.completeness == Completeness::Unknown)
        {
            "复制已有输出"
        } else if self.doc.as_ref().is_some_and(|d| d.rows > 8)
            || self.source.incomplete
            || self.source.preview.lines().count() > 8
        {
            "复制全部"
        } else {
            "复制输出"
        }
    }
}

fn compact(text: &str, incomplete: bool) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() <= 8 {
        text.to_owned()
    } else {
        let gap = if incomplete {
            "… 展开查看完整输出".into()
        } else {
            format!("… 省略 {} 行", lines.len() - 7)
        };
        format!(
            "{}\n{gap}\n{}",
            lines[..3].join("\n"),
            lines[lines.len() - 4..].join("\n")
        )
    }
}

fn source(key: String, output: Option<String>, text: &str, running: bool) -> Source {
    let view = crate::ai::output_view::display(text);
    let output = output.or_else(|| view.output_id.map(str::to_owned));
    // This is a structured command's persisted result, never arbitrary log-file contents.
    let preview = if output.is_some()
        && (text.starts_with("命令已在后台启动") || text.starts_with("命令在 "))
    {
        String::new()
    } else if output.is_none()
        && let Some((stdout, stderr)) = text
            .strip_prefix("标准输出：\n")
            .and_then(|s| s.split_once("\n标准错误：\n"))
    {
        let mut body = stdout.to_owned();
        if !stderr.is_empty() {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            for line in stderr.split_inclusive('\n') {
                body.push_str("[stderr] ");
                body.push_str(line);
            }
        }
        body
    } else {
        view.preview.to_owned()
    };
    let incomplete = (output.is_some() && text.is_empty())
        || view.truncated
        || view
            .total_bytes
            .is_some_and(|n| n > shellmate_ai::output::STORED_LIMIT as u64)
        || preview.contains("[省略")
        || preview.contains("[... omitted ...]");
    Source {
        key,
        output,
        preview,
        running,
        incomplete,
    }
}

pub(super) fn notice_label(text: &str, step: Option<&shellmate_ai::Step>) -> String {
    if let Some(step) = step
        && matches!(step.call.name.as_str(), "read_output" | "stop_task")
    {
        let action = if step.call.name == "read_output" {
            "读取命令输出"
        } else {
            "停止任务"
        };
        return format!(
            "{action}{}",
            if step.result.is_error {
                " · 未完成"
            } else {
                " · 已完成"
            }
        );
    }
    if text.starts_with("后台任务 #")
        && let Some((label, _)) = text.split_once(" · 用 read_output")
    {
        return label.into();
    }
    text.into()
}

#[cfg(test)]
mod tests {
    use super::{compact, notice_label, source};
    #[test]
    fn short_preview_of_large_output_still_has_full_output_source() {
        let text = "输出 id abc · 总计 1000000 字节 · 退出码 0\nshort\n完整输出已保存，用 read_output 查看或搜索";
        let source = source("step".into(), Some("structured-id".into()), text, false);
        assert_eq!(source.output.as_deref(), Some("structured-id"));
        assert_eq!(source.preview, "short");
        assert!(source.incomplete);
        assert!(!compact(&source.preview, source.incomplete).contains("structured-id"));
    }
    #[test]
    fn collapsed_view_preserves_head_tail_and_full_source() {
        let text = (0..30).map(|i| format!("line {i}\n")).collect::<String>();
        let preview = compact(&text, false);
        assert_eq!(preview.lines().count(), 8);
        assert!(preview.starts_with("line 0\nline 1\nline 2\n"));
        assert!(preview.ends_with("line 29"));
        assert_eq!(text.lines().count(), 30);
    }
    #[test]
    fn raw_content_that_mentions_output_ids_is_not_removed() {
        let text = "输出 id user-text\nUUID=abc\n/full/path.log";
        let source = source("step".into(), None, text, false);
        assert_eq!(source.preview, text);
        assert!(source.output.is_none());
        assert_eq!(notice_label("输出 id user-text", None), "输出 id user-text");
    }
    #[test]
    fn legacy_sections_are_unwrapped_but_log_body_that_prints_them_is_kept() {
        let legacy = "标准输出：\nok\n标准错误：\nfailed\n";
        assert_eq!(
            source("old".into(), None, legacy, false).preview,
            "ok\n[stderr] failed\n"
        );
        let wire = format!("输出 id out · 总计 100 字节 · 退出码 0\n{legacy}");
        assert_eq!(
            source("new".into(), Some("out".into()), &wire, false).preview,
            legacy
        );
    }
}
