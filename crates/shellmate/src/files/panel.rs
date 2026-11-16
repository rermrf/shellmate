//! The 文件 panel of one tab: its state, what it asks of the server, and what the user can do with
//! it. Drawing is in [`super::draw`] and [`super::queue_card`]; what the lists and rows say is in
//! [`super::model`] and [`super::transfers`], which need no window.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gpui::component::WindowExt as _;
use gpui::component::input::{Escape, InputEvent, InputState};
use gpui::component::notification::Notification;
use gpui::prelude::*;
use gpui::{
    App, ClipboardItem, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, KeyBinding, PathPromptOptions,
    Pixels, Point, ScrollHandle, ScrollStrategy, Subscription, UniformListScrollHandle, Window, actions,
};
use shellmate_sftp::transfer::{ItemId, TransferQueue};
use shellmate_sftp::{Revision, SaveFormat, SaveOutcome, SftpError, SftpSession, TextFile, path};
use shellmate_ssh::SshRuntime;
use shellmate_term::{Terminal, TerminalEvent};
use tokio::task::AbortHandle;

use super::model::{Applied, DeleteRequest, FilesModel, LinkStatus, Listed, Request, Row};
use super::transfers::{Reveal, RowAction, TransferList, ensure_downloads_dir, reveal};
use crate::dialogs::confirm::ConfirmDialog;
use crate::editor::refusal::is_refusal;

/// The key context of the panel: its shortcuts only work while the list has focus, so none of them
/// can reach the terminal.
pub const CONTEXT: &str = "FilesPanel";

/// The keystrokes of the panel (each is bound to the action of the same row in [`init`]).
pub const KEYS: [&str; 6] = ["up", "down", "enter", "cmd-up", "cmd-backspace", "escape"];

actions!(files, [FilesPrevious, FilesNext, FilesOpen, FilesParent, FilesDelete, FilesEscape]);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(KEYS[0], FilesPrevious, Some(CONTEXT)),
        KeyBinding::new(KEYS[1], FilesNext, Some(CONTEXT)),
        KeyBinding::new(KEYS[2], FilesOpen, Some(CONTEXT)),
        KeyBinding::new(KEYS[3], FilesParent, Some(CONTEXT)),
        KeyBinding::new(KEYS[4], FilesDelete, Some(CONTEXT)),
        KeyBinding::new(KEYS[5], FilesEscape, Some(CONTEXT)),
    ]);
}

/// What the panel asks of its owner.
pub enum FilesEvent {
    /// The session could not be opened and the user asked again (刷新).
    RetrySession,
    /// An operation found this session unusable (the connection is gone, or the server stopped
    /// answering): the owner has to tell the link, which opens a new one.
    SessionLost(Arc<SftpSession>),
    /// 处理 on a queue row: the question about names that exist is to be asked (again).
    HandleClash(ItemId),
    /// A text file was read for the built-in editor.
    OpenEditor(Box<TextFile>),
    /// The editor will not open this file: the dialog says why (and offers a download where that makes sense).
    EditorRefused { path: String, error: SftpError },
}

impl EventEmitter<FilesEvent> for FilesPanel {}

/// The snippets that make a shell report its directory.
pub const BASH_SNIPPET: &str = r#"PROMPT_COMMAND='printf "\e]7;file://%s%s\a" "$HOSTNAME" "$PWD"'"#;
pub const ZSH_SNIPPET: &str = r#"precmd() { printf '\e]7;file://%s%s\a' "$HOST" "$PWD" }"#;

/// The right-click menu of a row, as it was opened.
#[derive(Clone, Debug)]
pub struct RowMenu {
    pub name: String,
    pub path: String,
    /// A directory (or a link to one): 打开 enters it.
    pub opens_dir: bool,
    /// A file (or a link to one) the editor can be offered for.
    pub editable: bool,
    /// Where it is drawn (window coordinates).
    pub position: Point<Pixels>,
}

/// What a menu item does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Open,
    Download,
    Rename,
    CopyPath,
    Permissions,
    Delete,
}

/// The text field of a rename in progress.
pub(super) struct RenameInput {
    pub state: Entity<InputState>,
    _subscription: Subscription,
}

/// What the review hooks have set (debug builds only).
#[cfg(debug_assertions)]
#[derive(Default)]
struct Demo {
    /// The entry to select as soon as it is listed.
    select: Option<String>,
    /// The text the rename editor starts with (instead of the entry's name).
    rename_text: Option<String>,
    /// Draw the highlight of a drag over the list, as if something were being dragged there.
    drag: bool,
}

pub struct FilesPanel {
    pub(super) model: FilesModel,
    session: Option<Arc<SftpSession>>,
    runtime: SshRuntime,
    pub(super) focus: FocusHandle,
    /// The permission grid takes focus for 修改权限.
    pub(super) perm_focus: FocusHandle,
    pub(super) scroll: UniformListScrollHandle,
    /// The scroll position of the queue card's list.
    pub(super) queue_scroll: ScrollHandle,
    pub(super) hint_open: bool,
    /// The connection's transfer queue and what it has in it (shared by all its tabs).
    pub(super) queue: Option<TransferQueue>,
    pub(super) transfers: Option<Entity<TransferList>>,
    watching: Option<(EntityId, Subscription)>,
    pub(super) menu: Option<RowMenu>,
    pub(super) rename: Option<RenameInput>,
    #[cfg(debug_assertions)]
    demo: Demo,
    /// The listing in flight (superseded or dropped with the panel it is aborted: a closed tab
    /// does not keep its connection busy).
    listing_abort: Option<AbortHandle>,
    /// Other work on the SSH runtime (rename, delete, new folder, chmod), by number.
    ops: HashMap<u64, AbortHandle>,
    next_op: u64,
    _subscriptions: Vec<Subscription>,
}

impl Drop for FilesPanel {
    fn drop(&mut self) {
        for handle in self.listing_abort.take().into_iter().chain(self.ops.drain().map(|(_, handle)| handle)) {
            handle.abort();
        }
    }
}

impl Focusable for FilesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FilesPanel {
    pub fn new(terminal: &Entity<Terminal>, runtime: SshRuntime, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut model = FilesModel::new();
        if let Some(dir) = terminal.read(cx).cwd() {
            model.on_terminal_cwd(dir);
        }
        let subscription = cx.subscribe_in(terminal, window, |this, _, event: &TerminalEvent, _, cx| {
            if let TerminalEvent::CwdChanged(dir) = event {
                let request = this.model.on_terminal_cwd(dir);
                this.start(request, cx);
                cx.notify();
            }
        });
        Self {
            model,
            session: None,
            runtime,
            focus: cx.focus_handle(),
            perm_focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            queue_scroll: ScrollHandle::new(),
            hint_open: false,
            queue: None,
            transfers: None,
            watching: None,
            menu: None,
            rename: None,
            #[cfg(debug_assertions)]
            demo: Demo::default(),
            listing_abort: None,
            ops: HashMap::new(),
            next_op: 1,
            _subscriptions: vec![subscription],
        }
    }

    /// The state of the session behind the panel changed.
    pub fn set_link(&mut self, status: LinkStatus, session: Option<Arc<SftpSession>>, cx: &mut Context<Self>) {
        let session_changed = match (&self.session, &session) {
            (Some(old), Some(new)) => !Arc::ptr_eq(old, new),
            (None, None) => false,
            _ => true,
        };
        if !session_changed && self.model.link() == &status {
            return;
        }
        self.session = session;
        if !matches!(status, LinkStatus::Ready) {
            self.menu = None;
        }
        let request = self.model.set_link(status);
        self.start(request, cx);
        cx.notify();
    }

    /// The transfer queue of the connection the tab is on, and the list that follows it. The panel
    /// redraws when the list changes.
    pub fn set_transfers(
        &mut self,
        queue: Option<TransferQueue>,
        transfers: Option<Entity<TransferList>>,
        cx: &mut Context<Self>,
    ) {
        self.queue = queue;
        let id = transfers.as_ref().map(Entity::entity_id);
        if self.watching.as_ref().map(|(watched, _)| *watched) != id {
            self.watching = transfers.as_ref().map(|list| {
                (
                    list.entity_id(),
                    cx.observe(list, |this, _, cx| {
                        this.sync_hidden(cx);
                        cx.notify();
                    }),
                )
            });
            cx.notify();
        }
        self.transfers = transfers;
        self.sync_hidden(cx);
    }

    /// Leaves the temporary files of this connection's unfinished uploads out of the listing.
    fn sync_hidden(&mut self, cx: &mut Context<Self>) {
        let hidden = self.transfers.as_ref().map(|list| list.read(cx).hidden_temps()).unwrap_or_default();
        self.model.set_hidden(hidden);
    }

    /// Something changed in `dir` on the server (an upload finished there, say): a panel that shows
    /// it lists it again.
    pub fn on_remote_changed(&mut self, dir: &str, cx: &mut Context<Self>) {
        if self.model.cwd() == dir && self.model.link() == &LinkStatus::Ready {
            self.refresh(cx);
        }
    }

    // ---- loading ------------------------------------------------------------------------------------

    /// Carries out a listing request on the SSH runtime.
    fn start(&mut self, request: Option<Request>, cx: &mut Context<Self>) {
        let (Some(request), Some(session)) = (request, self.session.clone()) else { return };
        let id = request.id;
        let began = Instant::now();
        if let Some(previous) = self.listing_abort.take() {
            previous.abort();
        }
        let used = session.clone();
        let join = self.runtime.spawn(async move {
            // `.` is the home directory: ask the server where that is.
            let dir = if request.dir == "." {
                match session.home().await {
                    Ok(home) => home,
                    Err(err) => return Listed::failed(err),
                }
            } else {
                request.dir.clone()
            };
            // The directory's own attributes come even when listing it is refused (`/root`).
            let (entries, stat) = tokio::join!(session.list(&dir), session.stat_named(&dir));
            Listed { dir: Some(dir), entries, stat: stat.ok() }
        });
        self.listing_abort = Some(join.abort_handle());
        cx.spawn(async move |this, cx| {
            let result =
                join.await.unwrap_or_else(|err| Listed::failed(SftpError::Protocol(format!("任务异常终止：{err}"))));
            this.update(cx, |this, cx| {
                let listed = began.elapsed();
                let summary =
                    result.entries.as_ref().ok().zip(result.dir.clone()).map(|(entries, dir)| (dir, entries.len()));
                let before = this.model.cwd().to_string();
                let applying = Instant::now();
                if this.model.apply_listed(id, result) == Applied::SessionLost {
                    cx.emit(FilesEvent::SessionLost(used));
                }
                if let Some((dir, count)) = summary {
                    log::info!(
                        "文件面板：{dir} 共 {count} 项，列目录 {} ms，整理 {} ms",
                        listed.as_millis(),
                        applying.elapsed().as_millis()
                    );
                }
                #[cfg(debug_assertions)]
                if let Some(name) = this.demo.select.take() {
                    this.model.select_name(&name);
                    if let Some(index) = this.model.selected_index() {
                        this.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
                    }
                }
                if this.model.cwd() != before {
                    this.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
                }
                if let Some(index) = this.model.take_reveal() {
                    this.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Runs `job` on the SSH runtime with the current session and hands the outcome to `done`. A
    /// session that turns out to be unusable is reported instead (and `done` does not run): the
    /// panel waits for a new one.
    fn run_op<T, Fut>(
        &mut self,
        job: impl FnOnce(Arc<SftpSession>) -> Fut,
        done: impl FnOnce(&mut Self, Result<T, SftpError>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) where
        T: Send + 'static,
        Fut: Future<Output = Result<T, SftpError>> + Send + 'static,
    {
        self.run_job(
            job,
            |this, result, cx| {
                if !result.as_ref().is_err_and(SftpError::is_session_lost) {
                    done(this, result, cx);
                }
            },
            cx,
        );
    }

    /// Like [`run_op`](Self::run_op), but `done` always hears the outcome: the editor has a buffer to
    /// keep and a status to show when the session is lost (the loss is reported first, so that the
    /// link reopens the session by the time the user tries again). With no session at all the
    /// outcome is "disconnected".
    fn run_job<T, Fut>(
        &mut self,
        job: impl FnOnce(Arc<SftpSession>) -> Fut,
        done: impl FnOnce(&mut Self, Result<T, SftpError>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) where
        T: Send + 'static,
        Fut: Future<Output = Result<T, SftpError>> + Send + 'static,
    {
        let Some(session) = self.session.clone() else {
            done(self, Err(SftpError::Disconnected(String::new())), cx);
            return;
        };
        let used = session.clone();
        let id = self.next_op;
        self.next_op += 1;
        let join = self.runtime.spawn(job(session));
        self.ops.insert(id, join.abort_handle());
        cx.spawn(async move |this, cx| {
            let result = join.await.unwrap_or_else(|err| Err(SftpError::Protocol(format!("任务异常终止：{err}"))));
            this.update(cx, |this, cx| {
                this.ops.remove(&id);
                if result.as_ref().is_err_and(SftpError::is_session_lost) {
                    this.lose_session(used, cx);
                }
                done(this, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// An operation found `session` unusable: the panel waits for a new one, and the owner is asked
    /// to open it.
    fn lose_session(&mut self, session: Arc<SftpSession>, cx: &mut Context<Self>) {
        self.model.session_lost();
        self.menu = None;
        cx.emit(FilesEvent::SessionLost(session));
        cx.notify();
    }

    // ---- navigation ------------------------------------------------------------------------------------

    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        if matches!(self.model.link(), LinkStatus::Failed(_)) {
            cx.emit(FilesEvent::RetrySession);
            return;
        }
        let request = self.model.refresh();
        self.start(request, cx);
        cx.notify();
    }

    pub(super) fn go_to(&mut self, dir: &str, cx: &mut Context<Self>) {
        let request = self.model.go_to(dir);
        self.start(request, cx);
        cx.notify();
    }

    pub(super) fn go_parent(&mut self, cx: &mut Context<Self>) {
        let request = self.model.go_parent();
        self.start(request, cx);
        cx.notify();
    }

    pub(super) fn open_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let editable = match self.model.row(index) {
            Some(Row::Entry(entry)) if !entry.opens_as_dir() && entry.edits_as_text() => Some(entry.path.clone()),
            _ => None,
        };
        if let Some(path) = editable {
            return self.open_in_editor(path, cx);
        }
        let request = self.model.open_row(index);
        self.start(request, cx);
        cx.notify();
    }

    pub(super) fn select_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.model.select_row(index);
        if self.rename.is_none() {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    pub(crate) fn set_follow(&mut self, on: bool, cx: &mut Context<Self>) {
        let request = self.model.set_follow(on);
        self.start(request, cx);
        cx.notify();
    }

    // ---- permissions -----------------------------------------------------------------------------------

    pub(super) fn apply_chmod(&mut self, cx: &mut Context<Self>) {
        let Some((path, mode)) = self.model.chmod_request() else { return };
        self.run_op(
            move |session| async move { session.chmod(&path, mode).await },
            |this, result, cx| {
                let request = this.model.chmod_done(result);
                this.start(request, cx);
            },
            cx,
        );
    }

    // ---- renaming, new folders, deleting -----------------------------------------------------------

    /// Enter in the name editor.
    fn commit_rename(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(request) = self.model.commit_edit(text) else {
            cx.notify();
            return;
        };
        let (from, to) = (request.from.clone(), request.to.clone());
        self.run_op(
            move |session| async move { session.rename(&from, &to).await },
            move |this, result, cx| {
                let request = this.model.rename_done(&request, result);
                this.start(request, cx);
            },
            cx,
        );
        cx.notify();
    }

    /// Esc in the name editor.
    pub(super) fn on_cancel_rename(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.edit().is_some() {
            self.model.cancel_edit();
            window.focus(&self.focus, cx);
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    /// 新建文件夹: makes "新建文件夹" (or "新建文件夹 (1)" …) here, selects it and starts naming it.
    pub(super) fn new_folder(&mut self, cx: &mut Context<Self>) {
        if !self.model.ops_enabled() || self.model.cwd().is_empty() {
            return;
        }
        let dir = self.model.cwd().to_string();
        self.run_op(
            move |session| async move { session.create_dir_unique(&dir, "新建文件夹").await },
            |this, result, cx| match result {
                Ok(path) => {
                    let request = this.model.new_folder_done(&path);
                    this.start(request, cx);
                }
                Err(err) => this.model.set_banner(format!("无法新建文件夹：{err}")),
            },
            cx,
        );
    }

    /// 删除 / ⌘⌫: asks first, and for a directory says that everything inside goes.
    pub(super) fn ask_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let Some(request) = self.model.delete_request() else { return };
        let panel = cx.entity().downgrade();
        let confirmed = request.clone();
        ConfirmDialog::open(
            request.title(),
            request.body(),
            "删除",
            true,
            move |_, cx| {
                let request = confirmed.clone();
                panel.update(cx, |this, cx| this.begin_delete(request, cx)).ok();
                Ok(())
            },
            window,
            cx,
        );
    }

    fn begin_delete(&mut self, request: DeleteRequest, cx: &mut Context<Self>) {
        let path = request.path.clone();
        self.run_op(
            move |session| async move { session.remove(&path).await },
            move |this, result, cx| {
                let list = this.model.delete_done(&request, result);
                this.start(list, cx);
            },
            cx,
        );
    }

    // ---- the editor --------------------------------------------------------------------------------------

    /// Reads the file at `path` (a link is followed) for the built-in editor. What the editor will
    /// not open (binary, not UTF-8, too big, not a regular file) is reported with its reason; any
    /// other failure goes above the list.
    pub fn open_in_editor(&mut self, path: String, cx: &mut Context<Self>) {
        self.menu = None;
        if !self.model.ops_enabled() {
            return;
        }
        let name = path::file_name(&path).unwrap_or(&path).to_string();
        let (target, reported) = (path.clone(), path);
        let began = Instant::now();
        self.run_job(
            move |session| async move { session.open_text(&target).await },
            move |this, result, cx| match result {
                Ok(file) => {
                    log::info!(
                        "编辑器：读取 {} ({} 字节) 用时 {} ms",
                        file.path,
                        file.text.len(),
                        began.elapsed().as_millis()
                    );
                    cx.emit(FilesEvent::OpenEditor(Box::new(file)));
                }
                // The session is being replaced; the user opens the file again.
                Err(err) if err.is_session_lost() => {}
                Err(error) if is_refusal(&error) => cx.emit(FilesEvent::EditorRefused { path: reported, error }),
                Err(err) => this.model.set_banner(format!("无法打开 {name}：{err}")),
            },
            cx,
        );
    }

    /// Uploads the editor's `text` to `path`, expecting the server's file to still be at `expected`
    /// (see `SftpSession::save_text`). The answer comes on the returned channel, whatever happens
    /// (a lost session included, which is also reported so that it gets reopened).
    pub fn save_text(
        &mut self,
        path: String,
        text: String,
        format: SaveFormat,
        expected: Revision,
        cx: &mut Context<Self>,
    ) -> async_channel::Receiver<Result<SaveOutcome, SftpError>> {
        let (answer, receiver) = async_channel::bounded(1);
        self.run_job(
            move |session| async move { session.save_text(&path, &text, format, Some(expected)).await },
            move |_, result, _| {
                let _ = answer.try_send(result);
            },
            cx,
        );
        receiver
    }

    // ---- transfers -------------------------------------------------------------------------------------

    /// 下载 / the menu: the selected file or directory goes to the downloads folder.
    pub(super) fn begin_download(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if let Some(path) = self.model.download_path() {
            self.download(path, cx);
        }
    }

    /// Queues the download of `path` (a file or a directory of the server) into the downloads folder.
    pub fn download(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(queue) = self.queue.clone() else { return };
        match ensure_downloads_dir() {
            Ok(dir) => {
                queue.enqueue_download(vec![path], dir);
            }
            Err(message) => self.model.set_banner(message),
        }
        cx.notify();
    }

    /// 上传: asks macOS for files and folders and sends them to this directory.
    pub(super) fn pick_upload(&mut self, cx: &mut Context<Self>) {
        if !self.model.ops_enabled() || self.queue.is_none() {
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("上传".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                this.update(cx, |this, cx| this.upload(paths, cx)).ok();
            }
        })
        .detach();
    }

    /// Sends `paths` (picked, or dropped from Finder) to the directory shown.
    pub(super) fn upload(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        match self.queue.clone().filter(|_| self.model.ops_enabled() && !self.model.cwd().is_empty()) {
            Some(queue) => {
                queue.enqueue_upload(paths, self.model.cwd());
            }
            None => self.model.set_banner("连接就绪后才能上传"),
        }
        cx.notify();
    }

    /// The main button of a queue row.
    pub(super) fn row_action(&mut self, id: ItemId, action: RowAction, cx: &mut Context<Self>) {
        let Some(queue) = self.queue.clone() else { return };
        match action {
            RowAction::Pause => queue.pause(id),
            RowAction::Resume => queue.resume(id),
            RowAction::Handle => cx.emit(FilesEvent::HandleClash(id)),
            RowAction::Open => {
                let view = self.transfers.as_ref().and_then(|list| list.read(cx).get(id).cloned());
                match view.as_ref().and_then(reveal) {
                    Some(Reveal::Finder(path)) => cx.reveal_path(&path),
                    Some(Reveal::Remote(dir)) => self.go_to(&dir, cx),
                    None => {}
                }
            }
        }
    }

    /// The small 取消 of a queue row.
    pub(super) fn cancel_item(&mut self, id: ItemId) {
        if let Some(queue) = &self.queue {
            queue.cancel(id);
        }
    }

    /// 清除已完成.
    pub(super) fn clear_finished(&mut self) {
        if let Some(queue) = &self.queue {
            queue.clear_finished();
        }
    }

    // ---- the row menu ------------------------------------------------------------------------------------

    /// A right-click on row `index`: the menu opens there (never for `..`: nothing to do to it).
    pub(super) fn open_menu(&mut self, index: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.menu = match self.model.row(index) {
            Some(Row::Entry(entry)) => Some(RowMenu {
                name: entry.name.clone(),
                path: entry.path.clone(),
                opens_dir: entry.opens_as_dir(),
                editable: !entry.opens_as_dir() && entry.edits_as_text(),
                position,
            }),
            _ => None,
        };
        cx.notify();
    }

    pub(super) fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// An item of the row menu.
    pub(super) fn menu_action(&mut self, action: MenuAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.menu.take() else { return };
        self.model.select_name(&menu.name);
        match action {
            MenuAction::Open if menu.opens_dir => self.go_to(&menu.path, cx),
            MenuAction::Open if menu.editable => self.open_in_editor(menu.path.clone(), cx),
            MenuAction::Open => {}
            MenuAction::Download => self.begin_download(cx),
            MenuAction::Rename => {
                self.model.start_rename();
            }
            MenuAction::CopyPath => {
                cx.write_to_clipboard(ClipboardItem::new_string(menu.path.clone()));
                window.push_notification(Notification::success(menu.path.clone()).title("已复制路径"), cx);
            }
            MenuAction::Permissions => window.focus(&self.perm_focus, cx),
            MenuAction::Delete => self.ask_delete(window, cx),
        }
        cx.notify();
    }

    // ---- the name editor ---------------------------------------------------------------------------------

    /// Keeps the text field in step with the model: one while a name is being edited.
    pub(super) fn sync_rename_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match (self.model.edit().map(|edit| edit.name.clone()), self.rename.is_some()) {
            (Some(name), false) => {
                #[cfg(debug_assertions)]
                let name = self.demo.rename_text.take().unwrap_or(name);
                let state = cx.new(|cx| InputState::new(window, cx).default_value(name));
                let subscription = cx.subscribe_in(&state, window, |this, state, event: &InputEvent, _, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        let text = state.read(cx).value().to_string();
                        this.commit_rename(&text, cx);
                    }
                });
                state.update(cx, |state, cx| {
                    state.focus(window, cx);
                    state.select_all(window, cx);
                });
                self.rename = Some(RenameInput { state, _subscription: subscription });
            }
            (None, true) => {
                self.rename = None;
                window.focus(&self.focus, cx);
            }
            _ => {}
        }
    }

    // ---- actions -----------------------------------------------------------------------------------------

    pub(super) fn on_previous(&mut self, _: &FilesPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        if let Some(index) = self.model.move_selection(-1) {
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    pub(super) fn on_next(&mut self, _: &FilesNext, _: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        if let Some(index) = self.model.move_selection(1) {
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    pub(super) fn on_open(&mut self, _: &FilesOpen, _: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        if let Some(index) = self.model.selected_index() {
            return self.open_row(index, cx);
        }
        let request = self.model.open_selected();
        self.start(request, cx);
        cx.notify();
    }

    pub(super) fn on_parent(&mut self, _: &FilesParent, _: &mut Window, cx: &mut Context<Self>) {
        self.go_parent(cx);
    }

    /// ⌘⌫: the same confirmation as 删除.
    pub(super) fn on_delete(&mut self, _: &FilesDelete, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_delete(window, cx);
    }

    /// Esc: closes the menu (a name being edited has its own Esc, [`Self::on_cancel_rename`]).
    pub(super) fn on_escape(&mut self, _: &FilesEscape, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            self.close_menu(cx);
        } else {
            cx.propagate();
        }
    }

    /// Whether the highlight of a drag over the list is forced (the review hook).
    pub(super) fn demo_dragging(&self) -> bool {
        #[cfg(debug_assertions)]
        {
            self.demo.drag
        }
        #[cfg(not(debug_assertions))]
        {
            false
        }
    }
}

#[cfg(debug_assertions)]
impl FilesPanel {
    /// Debug review hook: goes to `dir` as if the user had.
    pub fn demo_go(&mut self, dir: &str, cx: &mut Context<Self>) {
        let request = self.model.go_to(dir);
        self.start(request, cx);
        cx.notify();
    }

    /// Debug review hook: selects the entry called `name` once it is listed (now, or when it is).
    pub fn demo_select(&mut self, name: &str) {
        self.demo.select = Some(name.to_string());
        self.model.select_name(name);
    }

    /// Debug review hook: a click on 刷新.
    pub fn demo_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
    }

    /// Debug review hook: opens the OSC 7 hint.
    pub fn demo_hint(&mut self, cx: &mut Context<Self>) {
        self.hint_open = true;
        cx.notify();
    }

    /// Debug review hook: a right-click on the row of `name`.
    pub fn demo_menu(&mut self, name: &str, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.model.select_name(name);
        if let Some(index) = self.model.selected_index() {
            self.open_menu(index, position, cx);
        }
    }

    /// Debug review hook: 重命名 on `name`, with `text` typed in and Enter pressed (to show an error).
    pub fn demo_rename(&mut self, name: &str, text: &str, cx: &mut Context<Self>) {
        self.model.select_name(name);
        if self.model.start_rename() {
            self.demo.rename_text = Some(text.to_string());
            self.commit_rename(text, cx);
        }
        cx.notify();
    }

    /// Debug review hook: 修改权限 on `name`: the row is selected and the grid takes focus.
    pub fn demo_permissions(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.model.select_name(name);
        window.focus(&self.perm_focus, cx);
        cx.notify();
    }

    /// Debug review hook: 删除 on `name`.
    pub fn demo_delete(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.model.select_name(name);
        self.ask_delete(window, cx);
    }

    /// Debug review hook: the drag highlight on or off.
    pub fn demo_drag(&mut self, on: bool, cx: &mut Context<Self>) {
        self.demo.drag = on;
        cx.notify();
    }

    /// Debug review hook: 新建文件夹.
    pub fn demo_new_folder(&mut self, cx: &mut Context<Self>) {
        self.new_folder(cx);
    }

    /// Debug review hook: uploads `paths` as a drop from Finder would.
    pub fn demo_upload(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.upload(paths, cx);
    }

    /// Debug review hook: 下载 on the selected entry.
    pub fn demo_download(&mut self, cx: &mut Context<Self>) {
        self.begin_download(cx);
    }

    /// Debug review hook: pauses the queue item called `name`.
    pub fn demo_pause_named(&mut self, name: &str) {
        if let Some(queue) = &self.queue
            && let Some(view) = queue.snapshot().into_iter().find(|view| view.name == name)
        {
            queue.pause(view.id);
        }
    }

    /// Debug review hook: scrolls the queue card's list down by `rows` rows.
    pub fn demo_scroll_queue(&self, rows: usize) {
        self.queue_scroll.set_offset(gpui::point(gpui::px(0.), gpui::px(-44. * rows as f32)));
    }

    /// Debug review hook: 处理 on the first queue row that waits for an answer.
    pub fn demo_handle_clash(&mut self, cx: &mut Context<Self>) {
        let id = self.transfers.as_ref().and_then(|list| list.read(cx).awaiting_clash().next().map(|view| view.id));
        if let Some(id) = id {
            self.row_action(id, RowAction::Handle, cx);
        }
    }

    /// Debug review hook: the transfer queue.
    pub fn demo_queue(&self) -> Option<TransferQueue> {
        self.queue.clone()
    }

    /// Debug review hook: the directory the panel is in.
    pub fn demo_cwd(&self) -> String {
        self.model.cwd().to_string()
    }
}

#[cfg(debug_assertions)]
impl FilesPanel {
    pub fn proto_fixture(&mut self,scene:&str,cx:&mut Context<Self>){
        use shellmate_sftp::{Entry,EntryKind};
        use crate::theme::ActivePalette;
        use shellmate_sftp::transfer::{TransferView,TransferEvent,TransferState,Direction,PauseReason,ItemId};
        let data=&crate::proto::DATA;
        self.model.on_terminal_cwd("/var/www/app");
        let request=self.model.set_link(LinkStatus::Ready).expect("first fixture listing");
        let entries=data["FILES"].a().iter().map(|f|Entry{name:f["n"].s().into(),path:format!("/var/www/app/{}",f["n"].s()),kind:if f["t"].s()=="d"{EntryKind::Dir}else{EntryKind::File},size:if f["n"].s()=="nginx.conf"{2316}else{crate::proto::bytes(f["s"].s())},modified:Some(crate::proto::timestamp(f["m"].s())),mode:u32::from_str_radix(f["p"].s(),8).ok(),uid:Some(1000),gid:Some(33),owner:"deploy".into(),group:"www".into(),link:None}).collect();
        self.model.apply_listed(request.id,super::model::Listed{dir:Some("/var/www/app".into()),entries:Ok(entries),stat:None});self.model.select_name("nginx.conf");
        let mut list=super::transfers::TransferList::new();let measured=crate::proto::reading(scene,cx.is_dark());
        for(i,t)in data["TRANSFERS"].a().iter().enumerate(){let total=(t["size"].n()*1_048_576.)as u64;let done=if i==0{(measured.transfer*1_048_576.) as u64}else{(total as f64*t["pct"].n()/100.)as u64};list.apply(&TransferEvent::Added(TransferView{id:ItemId(i as u64),direction:if t["dir"].s()=="up"{Direction::Upload}else{Direction::Download},name:t["n"].s().into(),destination:"/var/www/app".into(),state:match t["st"].s(){"run"=>TransferState::Running,"pause"=>TransferState::Paused(PauseReason::User),"done"=>TransferState::Done,_=>TransferState::Queued},bytes_done:done,bytes_total:total,files_done:0,files_total:1,scanning:false,speed:if i==0{measured.transfer_speed*1_048_576.}else{0.},current:None,skipped:Default::default(),in_place:0,failures:vec![],failure_total:0,clash:None,result:None,temp:None}));}
        self.transfers=Some(cx.new(|_|list));cx.notify();
    }
}


#[cfg(test)]
mod tests {
    use gpui::Keystroke;

    use super::*;

    #[test]
    fn the_panels_shortcuts_are_valid_keystrokes_in_its_own_context() {
        for key in KEYS {
            assert!(Keystroke::parse(key).is_ok(), "{key}");
        }
        assert_eq!(KEYS, ["up", "down", "enter", "cmd-up", "cmd-backspace", "escape"]);
        assert_eq!(CONTEXT, "FilesPanel");
    }

    #[test]
    fn the_snippets_are_the_documented_ones() {
        assert_eq!(BASH_SNIPPET, r#"PROMPT_COMMAND='printf "\e]7;file://%s%s\a" "$HOSTNAME" "$PWD"'"#);
        assert_eq!(ZSH_SNIPPET, r#"precmd() { printf '\e]7;file://%s%s\a' "$HOST" "$PWD" }"#);
    }
}
