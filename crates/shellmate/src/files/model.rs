//! The state of a file panel that does not need the UI: where it is, what is listed, what is
//! selected, whether it follows the terminal, and the permission draft. Pure, so it is tested
//! without a window; the panel only draws it and feeds it what happens.

use std::collections::HashSet;

use shellmate_sftp::path;
use shellmate_sftp::{Entry, EntryKind, SftpError, mode_octal, mode_string};

/// What the panel knows about the SFTP session behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkStatus {
    /// The session is being opened (or nothing has asked for it yet).
    Connecting,
    /// There is a session; directories can be listed.
    Ready,
    /// The session could not be opened.
    Failed(String),
    /// The connection is gone; the panel refreshes by itself when it is back.
    Disconnected,
}

pub use shellmate_sftp::Listed;

/// What an answer from a listing task did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The listing (or its error) is on screen.
    Shown,
    /// The answer was for a listing that is no longer the one in flight.
    Stale,
    /// The session behind it is unusable: the owner has to say so ([`FilesModel::session_lost`]
    /// was done already).
    SessionLost,
}

/// Inline editing of an entry's name (a rename, or the name of a folder just made).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// The entry being renamed (its current name).
    pub name: String,
    /// Why the last attempt did not work; the editor stays open to try again.
    pub error: Option<String>,
    /// The rename is on its way to the server.
    pub busy: bool,
}

/// A rename to carry out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameRequest {
    pub from: String,
    pub to: String,
    pub new_name: String,
}

/// What a deletion removes, for the confirmation and the sentence in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteKind {
    File,
    /// A directory: everything inside goes too.
    Dir,
    /// A symbolic link: the link goes, not what it points at.
    Link,
}

/// A deletion to confirm and carry out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteRequest {
    pub path: String,
    pub name: String,
    pub kind: DeleteKind,
    /// The entry to select afterwards: the one below the deleted, else the one above.
    next: Option<String>,
}

impl DeleteRequest {
    /// The confirmation's title.
    pub fn title(&self) -> String {
        match self.kind {
            DeleteKind::Dir => format!("删除目录 {}？", self.name),
            DeleteKind::File | DeleteKind::Link => format!("删除 {}？", self.name),
        }
    }

    /// The confirmation's text. For a directory it says that everything inside goes, recursively.
    pub fn body(&self) -> String {
        match self.kind {
            DeleteKind::File => format!("将从服务器永久删除 {}，无法撤销。", self.path),
            DeleteKind::Dir => format!("将递归删除目录 {} 及其中的全部文件和子目录，无法撤销。", self.path),
            DeleteKind::Link => format!("将删除符号链接 {}（不会删除它指向的内容），无法撤销。", self.path),
        }
    }
}

/// What to do with the next listing: select something, and maybe start renaming it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct After {
    select: Option<String>,
    rename: bool,
}

/// What the details panel is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subject<'a> {
    /// The selected entry.
    Entry(&'a Entry),
    /// Nothing is selected: the directory itself, as far as it is known.
    Dir(Option<&'a Entry>),
    /// The `..` row.
    Parent,
}

/// What the list area shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// Nothing loaded yet.
    Loading,
    /// The entries of [`FilesModel::cwd`] (an empty list is "empty directory").
    List,
    /// The directory the user went to could not be listed: the reason, and a way back.
    Error(String),
}

/// Why a directory is being loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The first listing of the panel.
    Initial,
    /// The user navigated (click, double-click, Enter, ⌘↑): following the terminal stops.
    Hand,
    /// The terminal changed directory and the panel follows.
    Follow,
    /// The same directory again.
    Refresh,
}

/// A listing to carry out. The answer goes to [`FilesModel::apply`] with the same id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: u64,
    pub dir: String,
    pub cause: Cause,
}

/// A row of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row<'a> {
    /// `..`, the parent directory.
    Parent,
    Entry(&'a Entry),
}

/// What is selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    Parent,
    /// An entry, by name (it stays selected across a refresh).
    Name(String),
}

/// The permission grid being edited: applied only by "应用 chmod".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermDraft {
    original: u32,
    bits: u32,
}

/// The mask of one cell of the grid: rows are owner, group, other; columns read, write, execute.
pub fn perm_bit(row: usize, column: usize) -> u32 {
    0o400 >> (row * 3 + column)
}

impl PermDraft {
    /// A draft of `mode` (permission bits, special bits included, kept as they are).
    pub fn new(mode: u32) -> Self {
        let mode = mode & 0o7777;
        Self { original: mode, bits: mode }
    }

    /// Whether the cell is set.
    pub fn is_set(&self, row: usize, column: usize) -> bool {
        self.bits & perm_bit(row, column) != 0
    }

    /// Flips a cell.
    pub fn toggle(&mut self, row: usize, column: usize) {
        self.bits ^= perm_bit(row, column);
    }

    /// The draft as a mode.
    pub fn mode(&self) -> u32 {
        self.bits
    }

    /// `644`
    pub fn octal(&self) -> String {
        mode_octal(self.bits)
    }

    /// `rw-r--r--`
    pub fn symbolic(&self) -> String {
        mode_string(self.bits)
    }

    /// Whether the draft differs from what the file has.
    pub fn is_dirty(&self) -> bool {
        self.bits != self.original
    }
}

/// The panel's state.
#[derive(Debug)]
pub struct FilesModel {
    link: LinkStatus,
    /// The directory the list is of (or that failed to list: see [`View::Error`]).
    cwd: String,
    entries: Vec<Entry>,
    /// `stat` of `cwd` itself (its size, time, owner and permissions for the details panel).
    dir_entry: Option<Entry>,
    view: View,
    selection: Option<Selection>,
    additional: std::collections::BTreeSet<String>,
    follow: bool,
    /// The directory the terminal last reported with OSC 7.
    terminal_cwd: Option<String>,
    /// The error of a directory that could not be listed while an older listing stays on screen.
    banner: Option<String>,
    /// The listing in flight.
    pending: Option<Request>,
    next_request: u64,
    draft: Option<PermDraft>,
    /// What went wrong applying the permissions.
    chmod_error: Option<String>,
    /// The name being edited in the list.
    edit: Option<Edit>,
    /// What the next listing of this directory selects (after a rename, a new folder, a delete).
    after: Option<After>,
    /// The listing just selected something that is to be scrolled into view.
    reveal: bool,
    /// A message that has to survive the listing that follows the operation which failed.
    banner_after: Option<String>,
    /// Paths left out of the listing: the temporary files of this connection's unfinished uploads.
    hidden: HashSet<String>,
}

impl Default for FilesModel {
    fn default() -> Self {
        Self::new()
    }
}

impl FilesModel {
    /// A panel that has not listed anything; following the terminal is on, as in the prototype.
    pub fn new() -> Self {
        Self {
            link: LinkStatus::Connecting,
            cwd: String::new(),
            entries: Vec::new(),
            dir_entry: None,
            view: View::Loading,
            selection: None,
            additional: Default::default(),
            follow: true,
            terminal_cwd: None,
            banner: None,
            pending: None,
            next_request: 1,
            draft: None,
            chmod_error: None,
            edit: None,
            after: None,
            reveal: false,
            banner_after: None,
            hidden: HashSet::new(),
        }
    }

    // ---- reading --------------------------------------------------------------------------------

    pub fn link(&self) -> &LinkStatus {
        &self.link
    }

    pub fn view(&self) -> &View {
        &self.view
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn follow(&self) -> bool {
        self.follow
    }

    /// Whether the terminal ever reported a directory (OSC 7).
    pub fn osc7_seen(&self) -> bool {
        self.terminal_cwd.is_some()
    }

    #[cfg(test)]
    pub fn terminal_cwd(&self) -> Option<&str> {
        self.terminal_cwd.as_deref()
    }

    pub fn banner(&self) -> Option<&str> {
        self.banner.as_deref()
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    #[cfg(test)]
    pub fn pending(&self) -> Option<&Request> {
        self.pending.as_ref()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Whether `..` is the first row: everywhere but at the root.
    pub fn has_parent(&self) -> bool {
        !self.cwd.is_empty() && path::parent(&self.cwd).is_some()
    }

    /// The number of rows.
    pub fn row_count(&self) -> usize {
        if matches!(self.view, View::List) { self.entries.len() + usize::from(self.has_parent()) } else { 0 }
    }

    pub fn row(&self, index: usize) -> Option<Row<'_>> {
        if !matches!(self.view, View::List) {
            return None;
        }
        let offset = usize::from(self.has_parent());
        match index.checked_sub(offset) {
            None if offset == 1 => Some(Row::Parent),
            Some(entry) => self.entries.get(entry).map(Row::Entry),
            None => None,
        }
    }

    /// The breadcrumbs of the current directory: name and full path of each part.
    pub fn crumbs(&self) -> Vec<(String, String)> {
        path::breadcrumbs(&self.cwd).into_iter().filter(|(name, _)| name != "/").collect()
    }

    #[cfg(test)]
    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    /// The row index of the selection, if it is visible.
    pub fn selected_index(&self) -> Option<usize> {
        let offset = usize::from(self.has_parent());
        match self.selection.as_ref()? {
            Selection::Parent => (offset == 1).then_some(0),
            Selection::Name(name) => self.entries.iter().position(|e| &e.name == name).map(|i| i + offset),
        }
    }

    /// Leaves `paths` out of the listing (now and in the listings to come): the temporary files an
    /// upload of this connection is writing. A file nobody here is writing stays visible, so that
    /// what a crash left behind can be deleted.
    pub fn set_hidden(&mut self, paths: HashSet<String>) {
        if self.hidden == paths {
            return;
        }
        self.hidden = paths;
        let before = self.entries.len();
        self.entries.retain(|entry| !self.hidden.contains(&entry.path));
        if self.entries.len() != before {
            if self.selection.as_ref().is_some_and(|selection| !self.selection_exists(selection)) {
                self.selection = None;
            }
            if self.edit.as_ref().is_some_and(|edit| !self.entries.iter().any(|e| e.name == edit.name)) {
                self.edit = None;
            }
            self.reset_draft();
        }
    }

    /// The selected entry (`None` for nothing, or for `..`).
    pub fn selected_entry(&self) -> Option<&Entry> {
        match self.selection.as_ref()? {
            Selection::Parent => None,
            Selection::Name(name) => self.entries.iter().find(|e| &e.name == name),
        }
    }

    /// What the details panel shows: the selected entry, else the directory itself.
    pub fn subject(&self) -> Subject<'_> {
        match &self.selection {
            Some(Selection::Parent) => Subject::Parent,
            Some(Selection::Name(_)) => {
                self.selected_entry().map_or(Subject::Dir(self.dir_entry.as_ref()), Subject::Entry)
            }
            None => Subject::Dir(self.dir_entry.as_ref()),
        }
    }

    // ---- the session -------------------------------------------------------------------------------

    /// The session changed: a ready one makes the panel load (again); losing it keeps the listing
    /// on record but the panel says so, and it refreshes when the session is back.
    pub fn set_link(&mut self, link: LinkStatus) -> Option<Request> {
        let was_ready = self.link == LinkStatus::Ready;
        self.link = link;
        match self.link {
            LinkStatus::Ready => {
                // A new session: whatever was in flight on the old one is lost. Where to look: where
                // the user was (or wanted to go), else where the terminal is, else the home directory (`.`).
                self.pending = None;
                let dir = if self.cwd.is_empty() { self.start_dir() } else { Some(self.cwd.clone()) };
                let cause = if matches!(self.view, View::Loading) { Cause::Initial } else { Cause::Refresh };
                Some(self.begin(dir.unwrap_or_else(|| ".".to_string()), cause))
            }
            LinkStatus::Disconnected | LinkStatus::Failed(_) => {
                self.pending = None;
                let _ = was_ready;
                None
            }
            LinkStatus::Connecting => {
                self.pending = None;
                None
            }
        }
    }

    /// An operation found the session unusable (the connection is gone, or the server stopped
    /// answering) while the connection may live on. The panel waits for a new session as it does
    /// while one is being opened: the owner reports the loss to the link, which reopens it, and
    /// the new session makes the panel refresh by itself. It does not say "disconnected": no
    /// reconnect is coming if the SSH connection is fine.
    pub fn session_lost(&mut self) {
        self.pending = None;
        // A rename that was on its way is not known to have happened: the user may try again.
        if let Some(edit) = &mut self.edit {
            edit.busy = false;
        }
        if self.link == LinkStatus::Ready {
            self.link = LinkStatus::Connecting;
        }
    }

    /// Where the panel starts: the directory the terminal reported, else the home directory
    /// (`None`; the caller asks the server).
    fn start_dir(&self) -> Option<String> {
        self.terminal_cwd.clone().filter(|_| self.follow)
    }

    // ---- loading ------------------------------------------------------------------------------------

    fn begin(&mut self, dir: String, cause: Cause) -> Request {
        let request = Request { id: self.next_request, dir, cause };
        self.next_request += 1;
        self.pending = Some(request.clone());
        request
    }

    /// Goes to `dir`. A hand navigation turns following the terminal off. Returns the listing to
    /// carry out (none unless the session is ready).
    pub fn navigate(&mut self, dir: &str, cause: Cause) -> Option<Request> {
        if cause == Cause::Hand {
            self.follow = false;
        }
        if cause != Cause::Refresh {
            // Somewhere else: a half-typed name and what an earlier operation wanted selected are moot.
            self.edit = None;
            self.after = None;
        }
        if self.link != LinkStatus::Ready {
            // The user's choice is remembered for when there is a session; following the terminal
            // needs nothing: the terminal's directory is known and used then.
            if matches!(cause, Cause::Hand | Cause::Initial) {
                self.cwd = dir.to_string();
            }
            return None;
        }
        Some(self.begin(path::normalize(dir), cause))
    }

    /// Lists the current directory again.
    pub fn refresh(&mut self) -> Option<Request> {
        let dir = if self.cwd.is_empty() { ".".to_string() } else { self.cwd.clone() };
        self.chmod_error = None;
        self.navigate(&dir, Cause::Refresh)
    }

    /// The answer to a [`Request`] (see [`apply_listed`](Self::apply_listed)) without the directory's own `stat`.
    #[cfg(test)]
    pub fn apply(&mut self, id: u64, result: Result<(String, Vec<Entry>), SftpError>) -> Applied {
        let listed = match result {
            Ok((dir, entries)) => Listed { dir: Some(dir), entries: Ok(entries), stat: None },
            Err(error) => Listed::failed(error),
        };
        self.apply_listed(id, listed)
    }

    /// The answer to a [`Request`]: the directory that was listed (the server's absolute form: the
    /// home directory for `.`), its entries and its own attributes. Answers to listings that are
    /// no longer the one in flight are ignored. A directory that cannot be listed leaves an older
    /// listing on screen (with the reason above it) when the panel got there by following the
    /// terminal or by refreshing, and is shown as an error with a way back when the user went there.
    /// An unusable session is not the directory's fault: see [`Applied::SessionLost`].
    pub fn apply_listed(&mut self, id: u64, listed: Listed) -> Applied {
        let Some(request) = self.pending.take_if(|pending| pending.id == id) else { return Applied::Stale };
        let Listed { dir, entries, stat } = listed;
        match entries {
            Ok(mut entries) => {
                let dir = dir.unwrap_or_else(|| request.dir.clone());
                let keep = matches!(request.cause, Cause::Refresh) && dir == self.cwd;
                let previous = self.selection.clone();
                self.cwd = dir;
                self.dir_entry = stat;
                entries.retain(|entry| !self.hidden.contains(&entry.path));
                // The server lists directories in a sorted order already; keep it as it is.
                std::mem::swap(&mut self.entries, &mut entries);
                self.view = View::List;
                self.banner = self.banner_after.take();
                self.selection = previous.filter(|_| keep).filter(|sel| self.selection_exists(sel));
                self.additional.retain(|name| keep && self.entries.iter().any(|entry| entry.name == *name));
                if self.edit.as_ref().is_some_and(|edit| !self.entries.iter().any(|e| e.name == edit.name)) {
                    self.edit = None;
                }
                if let Some(After { select, rename }) = self.after.take() {
                    self.selection =
                        select.clone().filter(|name| self.entries.iter().any(|e| &e.name == name)).map(Selection::Name);
                    if let Some(name) = select.filter(|_| self.selection.is_some()) {
                        self.reveal = true;
                        if rename {
                            self.edit = Some(Edit { name, error: None, busy: false });
                        }
                    }
                }
                self.reset_draft();
            }
            Err(err) if err.is_session_lost() => {
                self.session_lost();
                return Applied::SessionLost;
            }
            Err(err) => {
                let reason = err.to_string();
                let dir = dir.unwrap_or_else(|| request.dir.clone());
                match request.cause {
                    Cause::Hand | Cause::Initial => {
                        self.cwd = dir;
                        self.dir_entry = stat;
                        self.entries.clear();
                        self.view = View::Error(reason);
                        self.banner = None;
                        self.selection = None;
                        self.reset_draft();
                    }
                    Cause::Follow | Cause::Refresh => {
                        // Keep what is on screen: never a blank panel.
                        if matches!(self.view, View::Loading) {
                            self.cwd = dir;
                            self.dir_entry = stat;
                            self.view = View::Error(reason);
                            self.reset_draft();
                        } else {
                            self.banner = Some(match request.cause {
                                Cause::Follow => format!("无法打开终端当前目录 {}：{reason}", request.dir),
                                _ => reason,
                            });
                        }
                    }
                }
            }
        }
        Applied::Shown
    }

    pub fn show_cached(&mut self, request: &Request, listed: Listed) {
        if self.apply_listed(request.id, listed) == Applied::Shown {
            self.pending = Some(Request { cause: Cause::Refresh, ..request.clone() });
        }
    }

    pub fn describe_rows(&mut self, id: u64, mut entries: Vec<Entry>) {
        if self.link == LinkStatus::Ready && self.next_request == id + 1 && self.pending.is_none() {
            entries.retain(|entry| !self.hidden.contains(&entry.path));
            self.entries = entries;
        }
    }

    fn selection_exists(&self, selection: &Selection) -> bool {
        match selection {
            Selection::Parent => self.has_parent(),
            Selection::Name(name) => self.entries.iter().any(|e| &e.name == name),
        }
    }

    // ---- navigation ------------------------------------------------------------------------------------

    /// The parent directory, if there is one.
    pub fn parent_dir(&self) -> Option<String> {
        path::parent(&self.cwd)
    }

    /// ⌘↑, the `..` row, or "返回上级".
    pub fn go_parent(&mut self) -> Option<Request> {
        let parent = self.parent_dir()?;
        self.navigate(&parent, Cause::Hand)
    }

    /// Enter or a double-click on row `index`: enters a directory (or a link to one) and goes up
    /// from `..`. A file does nothing yet (the editor comes later).
    pub fn open_row(&mut self, index: usize) -> Option<Request> {
        match self.row(index)? {
            Row::Parent => self.go_parent(),
            Row::Entry(entry) if entry.opens_as_dir() => {
                let target = entry.path.clone();
                self.navigate(&target, Cause::Hand)
            }
            Row::Entry(_) => None,
        }
    }

    /// Enter on the selection.
    pub fn open_selected(&mut self) -> Option<Request> {
        let index = self.selected_index()?;
        self.open_row(index)
    }

    /// A click on a breadcrumb.
    pub fn go_to(&mut self, dir: &str) -> Option<Request> {
        self.navigate(dir, Cause::Hand)
    }

    // ---- selection ----------------------------------------------------------------------------------------

    /// Selects row `index` (a click).
    pub fn select_row(&mut self, index: usize) {
        let selection = match self.row(index) {
            Some(Row::Parent) => Some(Selection::Parent),
            Some(Row::Entry(entry)) => Some(Selection::Name(entry.name.clone())),
            None => None,
        };
        self.set_selection(selection);
    }

    /// Selects the entry called `name`, if it is listed (a click or a right-click on its row, as far
    /// as the operations that follow are concerned).
    pub fn select_name(&mut self, name: &str) {
        if self.entries.iter().any(|entry| entry.name == name) {
            self.set_selection(Some(Selection::Name(name.to_string())));
        }
    }

    /// ↑ (`-1`) and ↓ (`1`): moves the selection, without wrapping around; with nothing selected, ↓ takes
    /// the first row and ↑ the last. Returns the row now selected.
    pub fn move_selection(&mut self, delta: i32) -> Option<usize> {
        let count = self.row_count();
        if count == 0 {
            return None;
        }
        let next = match self.selected_index() {
            None if delta >= 0 => 0,
            None => count - 1,
            Some(current) => (current as i64 + i64::from(delta)).clamp(0, count as i64 - 1) as usize,
        };
        self.select_row(next);
        Some(next)
    }

    fn set_selection(&mut self, selection: Option<Selection>) {
        self.additional.clear();
        if self.selection != selection {
            self.selection = selection;
            // Selecting something else ends an edit of a name (unless it is being sent).
            if self.edit.as_ref().is_some_and(|edit| !edit.busy) {
                self.edit = None;
            }
            self.reset_draft();
        }
    }

    pub fn row_selected(&self, index: usize) -> bool {
        self.selected_index() == Some(index) || matches!(self.row(index), Some(Row::Entry(entry)) if self.additional.contains(&entry.name))
    }

    pub fn toggle_row(&mut self, index: usize) {
        let Some(Row::Entry(entry)) = self.row(index) else { return self.select_row(index) };
        let name = entry.name.clone();
        let mut selected = std::mem::take(&mut self.additional);
        if let Some(primary) = self.selected_entry() { selected.insert(primary.name.clone()); }
        let primary = if selected.remove(&name) { selected.pop_first() } else { Some(name) };
        self.set_selection(primary.map(Selection::Name));
        self.additional = selected;
    }

    pub fn selected_paths(&self) -> Vec<String> {
        self.entries.iter().filter(|entry| self.additional.contains(&entry.name) || matches!(&self.selection, Some(Selection::Name(name)) if *name == entry.name))
            .map(|entry| entry.path.clone()).collect()
    }

    // ---- following the terminal -----------------------------------------------------------------------------

    /// The terminal reported a directory (OSC 7). With following on, the panel goes there.
    pub fn on_terminal_cwd(&mut self, dir: &str) -> Option<Request> {
        self.terminal_cwd = Some(dir.to_string());
        if !self.follow || (self.cwd == dir && !self.is_loading() && matches!(self.view, View::List)) {
            return None;
        }
        self.navigate(dir, Cause::Follow)
    }

    /// The switch: turning it on goes to the terminal's directory, if it has told us.
    pub fn set_follow(&mut self, on: bool) -> Option<Request> {
        self.follow = on;
        if on {
            let dir = self.terminal_cwd.clone()?;
            if dir != self.cwd {
                return self.navigate(&dir, Cause::Follow);
            }
        }
        None
    }

    // ---- operations ---------------------------------------------------------------------------------------------

    /// Whether the toolbar and menus can act: there has to be a session, and no rename on its way.
    pub fn ops_enabled(&self) -> bool {
        self.link == LinkStatus::Ready && !self.edit.as_ref().is_some_and(|edit| edit.busy)
    }

    /// Shows `text` above the list until the next listing.
    pub fn set_banner(&mut self, text: impl Into<String>) {
        self.banner = Some(text.into());
    }

    /// The name being edited, if any.
    pub fn edit(&self) -> Option<&Edit> {
        self.edit.as_ref()
    }

    /// 重命名 on the selected entry. `false` if nothing that can be renamed is selected.
    pub fn start_rename(&mut self) -> bool {
        if !self.ops_enabled() {
            return false;
        }
        let Some(name) = self.selected_entry().map(|entry| entry.name.clone()) else { return false };
        self.edit = Some(Edit { name, error: None, busy: false });
        true
    }

    /// Esc: stops editing the name (nothing is renamed).
    pub fn cancel_edit(&mut self) {
        if self.edit.as_ref().is_some_and(|edit| !edit.busy) {
            self.edit = None;
        }
    }

    /// Enter: the rename to carry out for `text`, or `None` — when the name did not change (the
    /// editor closes) or cannot be used (the editor stays open and says why).
    pub fn commit_edit(&mut self, text: &str) -> Option<RenameRequest> {
        let edit = self.edit.as_mut().filter(|edit| !edit.busy)?;
        let from_name = edit.name.clone();
        let invalid = if text.trim().is_empty() {
            Some("名称不能为空".to_string())
        } else if text.contains('/') {
            Some("名称不能包含「/」".to_string())
        } else if text.contains('\0') || matches!(text, "." | "..") {
            Some("名称无效".to_string())
        } else {
            None
        };
        if let Some(error) = invalid {
            edit.error = Some(error);
            return None;
        }
        if text == from_name {
            self.edit = None;
            return None;
        }
        let to = path::join(&self.cwd, text);
        // The server would say the same; saying it here saves the trip (and holds on servers that
        // would silently replace).
        if self.entries.iter().any(|entry| entry.name == text) {
            edit.error = Some(SftpError::AlreadyExists { path: to }.to_string());
            return None;
        }
        edit.error = None;
        edit.busy = true;
        Some(RenameRequest { from: path::join(&self.cwd, &from_name), to, new_name: text.to_string() })
    }

    /// The outcome of a rename. Success selects the entry under its new name and refreshes (the
    /// returned listing); a failure is shown in the editor, which stays open for another try.
    pub fn rename_done(&mut self, request: &RenameRequest, result: Result<(), SftpError>) -> Option<Request> {
        match result {
            Ok(()) => {
                self.edit = None;
                self.after = Some(After { select: Some(request.new_name.clone()), rename: false });
                self.refresh()
            }
            Err(err) => {
                if let Some(edit) = &mut self.edit {
                    edit.busy = false;
                    edit.error = Some(err.to_string());
                }
                None
            }
        }
    }

    /// A folder was made at `path`: list again, select it and start naming it.
    pub fn new_folder_done(&mut self, path: &str) -> Option<Request> {
        let name = path::file_name(path)?.to_string();
        self.after = Some(After { select: Some(name), rename: true });
        self.refresh()
    }

    /// The scroll position (row) of what the last listing selected for the user, once.
    pub fn take_reveal(&mut self) -> Option<usize> {
        std::mem::take(&mut self.reveal).then(|| self.selected_index()).flatten()
    }

    /// 删除 on the selected entry (never `..`).
    pub fn delete_request(&self) -> Option<DeleteRequest> {
        if !self.ops_enabled() {
            return None;
        }
        let entry = self.selected_entry()?;
        let index = self.entries.iter().position(|e| e.name == entry.name)?;
        let next = self.entries.get(index + 1).or_else(|| index.checked_sub(1).and_then(|i| self.entries.get(i)));
        let kind = match entry.kind {
            EntryKind::Dir => DeleteKind::Dir,
            EntryKind::Symlink => DeleteKind::Link,
            _ => DeleteKind::File,
        };
        Some(DeleteRequest {
            path: entry.path.clone(),
            name: entry.name.clone(),
            kind,
            next: next.map(|entry| entry.name.clone()),
        })
    }

    /// The outcome of a deletion. Either way the directory is listed again (a directory that failed
    /// halfway has lost part of what was in it); a failure stays above the list.
    pub fn delete_done(&mut self, request: &DeleteRequest, result: Result<(), SftpError>) -> Option<Request> {
        match result {
            Ok(()) => self.after = Some(After { select: request.next.clone(), rename: false }),
            Err(err) => {
                let text = format!("无法删除 {}：{err}", request.name);
                self.banner = Some(text.clone());
                self.banner_after = Some(text);
            }
        }
        self.refresh()
    }

    /// 下载 on the selected entry: its path (a file, a directory, or a link, which is followed).
    pub fn download_path(&self) -> Option<String> {
        if !self.ops_enabled() {
            return None;
        }
        self.selected_entry().map(|entry| entry.path.clone())
    }

    // ---- permissions ------------------------------------------------------------------------------------------

    /// What the permission grid is about: the selected entry, or the directory itself while
    /// nothing is selected. A link's own mode means nothing, and `..` has no attributes at hand.
    fn grid_target(&self) -> Option<(String, &Entry)> {
        match &self.selection {
            None => self.dir_entry.as_ref().map(|entry| (self.cwd.clone(), entry)),
            Some(Selection::Parent) => None,
            Some(Selection::Name(_)) => self.selected_entry().map(|entry| (entry.path.clone(), entry)),
        }
    }

    fn reset_draft(&mut self) {
        self.chmod_error = None;
        self.draft = self
            .grid_target()
            .map(|(_, entry)| entry)
            .filter(|entry| matches!(entry.kind, EntryKind::File | EntryKind::Dir))
            .and_then(|entry| entry.mode)
            .map(PermDraft::new);
    }

    pub fn draft(&self) -> Option<&PermDraft> {
        self.draft.as_ref()
    }

    /// Whether the grid and "应用 chmod" can be used: there has to be a session to apply it on.
    pub fn chmod_enabled(&self) -> bool {
        self.link == LinkStatus::Ready
    }

    pub fn toggle_perm(&mut self, row: usize, column: usize) {
        if !self.chmod_enabled() {
            return;
        }
        if let Some(draft) = &mut self.draft {
            draft.toggle(row, column);
            self.chmod_error = None;
        }
    }

    /// "应用 chmod": the path and mode to set, if there is a draft that differs.
    pub fn chmod_request(&self) -> Option<(String, u32)> {
        if !self.chmod_enabled() {
            return None;
        }
        let (path, _) = self.grid_target()?;
        let draft = self.draft.as_ref().filter(|d| d.is_dirty())?;
        Some((path, draft.mode()))
    }

    pub fn chmod_error(&self) -> Option<&str> {
        self.chmod_error.as_deref()
    }

    /// The outcome of `chmod`: an error stays under the button; success refreshes the list (the
    /// caller does that with the returned request).
    pub fn chmod_done(&mut self, result: Result<(), SftpError>) -> Option<Request> {
        match result {
            Ok(()) => self.refresh(),
            Err(err) => {
                self.chmod_error = Some(err.to_string());
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind, mode: u32) -> Entry {
        let dir = "/srv/app";
        Entry {
            name: name.to_string(),
            path: format!("{dir}/{name}"),
            kind,
            size: 10,
            modified: Some(1_700_000_000),
            mode: Some(mode),
            uid: Some(1000),
            gid: Some(1000),
            owner: "deploy".into(),
            group: "www".into(),
            link: None,
        }
    }

    fn dir(name: &str) -> Entry {
        entry(name, EntryKind::Dir, 0o755)
    }

    fn file(name: &str) -> Entry {
        entry(name, EntryKind::File, 0o644)
    }

    fn listing() -> Vec<Entry> {
        vec![dir("config"), dir("dist"), file(".env"), file("nginx.conf")]
    }

    /// The successful answer to `request`: it lists the directory that was asked for.
    fn ok(request: &Request, entries: Vec<Entry>) -> Result<(String, Vec<Entry>), SftpError> {
        Ok((request.dir.clone(), entries))
    }

    /// A ready model that has listed `/srv/app` (and follows the terminal, which has said nothing yet).
    fn at_app() -> FilesModel {
        let mut model = FilesModel::new();
        let request = model.navigate("/srv/app", Cause::Hand);
        assert!(request.is_none(), "no session yet");
        let request = model.set_link(LinkStatus::Ready).expect("a listing");
        assert_eq!((request.dir.as_str(), request.cause), ("/srv/app", Cause::Initial));
        model.apply(request.id, ok(&request, listing()));
        model.set_follow(true);
        model
    }

    #[test]
    fn a_new_panel_asks_for_the_terminals_directory_or_else_the_home_directory() {
        let mut model = FilesModel::new();
        let request = model.set_link(LinkStatus::Ready).unwrap();
        assert_eq!((request.dir.as_str(), request.cause), (".", Cause::Initial), "the home directory");

        let mut model = FilesModel::new();
        model.on_terminal_cwd("/var/log");
        let request = model.set_link(LinkStatus::Ready).unwrap();
        assert_eq!((request.dir.as_str(), request.cause), ("/var/log", Cause::Initial));
    }

    #[test]
    fn two_panels_on_one_connection_keep_their_own_directories_and_selections() {
        // Both tabs are told the session is ready (it is the same one); each lists where it is.
        let mut first = at_app();
        let mut second = FilesModel::new();
        second.on_terminal_cwd("/var/log");
        let request = second.set_link(LinkStatus::Ready).unwrap();
        second.apply(request.id, ok(&request, vec![file("syslog"), file("auth.log")]));

        first.select_row(4);
        second.select_row(1);
        assert_eq!((first.cwd(), second.cwd()), ("/srv/app", "/var/log"));
        assert_eq!(first.selected_entry().map(|e| e.name.as_str()), Some("nginx.conf"));
        assert_eq!(second.selected_entry().map(|e| e.name.as_str()), Some("syslog"));

        // One of them moving on (a follow, a click) leaves the other where it was.
        let request = first.on_terminal_cwd("/etc").expect("the first one follows its terminal");
        first.apply(request.id, ok(&request, vec![file("hosts")]));
        assert_eq!((first.cwd(), second.cwd()), ("/etc", "/var/log"));
        assert_eq!(second.selected_entry().map(|e| e.name.as_str()), Some("syslog"));
    }

    #[test]
    fn cached_navigation_keeps_rows_on_failure_and_ignores_old_descriptions() {
        let mut model = at_app();
        let request = model.go_to("/cached").unwrap();
        model.show_cached(
            &request,
            Listed { dir: Some("/cached".into()), entries: Ok(vec![file("cached")]), stat: None },
        );
        assert_eq!(model.cwd(), "/cached");
        model.apply_listed(request.id, Listed::failed(SftpError::PermissionDenied { path: "/cached".into() }));
        assert!(model.banner().is_some());
        let next = model.go_to("/new").unwrap();
        model.describe_rows(request.id, vec![]);
        assert_eq!(model.entries()[0].name, "cached");
        model.apply(next.id, ok(&next, vec![file("new")]));
        model.describe_rows(request.id, vec![]);
        assert_eq!(model.entries()[0].name, "new");
    }

    #[test]
    fn the_listing_has_a_parent_row_except_at_the_root() {
        let model = at_app();
        assert!(model.has_parent());
        assert_eq!(model.row_count(), 5);
        assert_eq!(model.row(0), Some(Row::Parent));
        assert!(matches!(model.row(1), Some(Row::Entry(e)) if e.name == "config"));
        assert!(matches!(model.row(4), Some(Row::Entry(e)) if e.name == "nginx.conf"));
        assert_eq!(model.row(5), None);

        // The home directory is asked for as `.`; what the server says it is becomes the directory.
        let mut home = FilesModel::new();
        home.set_link(LinkStatus::Ready);
        let id = home.pending().unwrap().id;
        home.apply(id, Ok(("/home/me".to_string(), vec![dir("docs")])));
        assert_eq!((home.cwd(), home.has_parent()), ("/home/me", true));
    }

    #[test]
    fn the_root_has_no_parent_row() {
        let mut model = FilesModel::new();
        model.set_link(LinkStatus::Ready);
        let request = model.navigate("/", Cause::Hand).unwrap();
        model.apply(request.id, ok(&request, vec![dir("etc"), dir("var")]));
        assert_eq!((model.cwd(), model.has_parent(), model.row_count()), ("/", false, 2));
        assert!(matches!(model.row(0), Some(Row::Entry(e)) if e.name == "etc"));
        assert_eq!(model.go_parent(), None);
        assert_eq!(model.crumbs(), []);
    }

    #[test]
    fn breadcrumbs_lead_from_the_root_to_the_directory() {
        let model = at_app();
        assert_eq!(
            model.crumbs(),
            [("srv".to_string(), "/srv".to_string()), ("app".to_string(), "/srv/app".to_string())]
        );
    }

    #[test]
    fn clicking_selects_and_the_selection_survives_a_refresh() {
        let mut model = at_app();
        model.select_row(4);
        assert_eq!(model.selected_entry().map(|e| e.name.as_str()), Some("nginx.conf"));
        assert_eq!(model.selected_index(), Some(4));
        let request = model.refresh().unwrap();
        model.apply(request.id, ok(&request, vec![dir("config"), file("nginx.conf")]));
        assert_eq!(model.selected_entry().map(|e| e.name.as_str()), Some("nginx.conf"), "found by name");
        assert_eq!(model.selected_index(), Some(2), "wherever it is now");
        // If it is gone, nothing is selected.
        let request = model.refresh().unwrap();
        model.apply(request.id, ok(&request, vec![dir("config")]));
        assert_eq!(model.selection(), None);
        // A different directory starts with nothing selected.
        model.select_row(1);
        let request = model.go_to("/srv").unwrap();
        model.apply(request.id, ok(&request, vec![dir("app")]));
        assert_eq!(model.selection(), None);
    }

    #[test]
    fn the_arrow_keys_move_the_selection_without_wrapping() {
        let mut model = at_app();
        assert_eq!(model.move_selection(1), Some(0), "down from nothing takes the first row");
        assert_eq!(model.selection(), Some(&Selection::Parent));
        assert_eq!(model.move_selection(-1), Some(0), "stays at the top");
        for expected in 1..=4 {
            assert_eq!(model.move_selection(1), Some(expected));
        }
        assert_eq!(model.move_selection(1), Some(4), "stays at the bottom");
        assert_eq!(model.selected_entry().map(|e| e.name.as_str()), Some("nginx.conf"));
        model.set_selection(None);
        assert_eq!(model.move_selection(-1), Some(4), "up from nothing takes the last row");
        let mut empty = FilesModel::new();
        assert_eq!(empty.move_selection(1), None);
    }

    #[test]
    fn enter_opens_directories_links_to_directories_and_the_parent_but_not_files() {
        let mut model = at_app();
        model.select_row(1);
        let request = model.open_selected().expect("a directory is entered");
        assert_eq!((request.dir.as_str(), request.cause), ("/srv/app/config", Cause::Hand));
        model.apply(request.id, ok(&request, vec![]));

        let mut model = at_app();
        model.select_row(4);
        assert_eq!(model.open_selected(), None, "the editor is for later");
        model.select_row(0);
        let request = model.open_selected().expect("..");
        assert_eq!(request.dir, "/srv");
        assert_eq!(model.open_row(4), None);

        // A link to a directory is entered, a link to a file is not.
        let mut with_links = at_app();
        let mut to_dir = entry("shortcut", EntryKind::Symlink, 0o777);
        to_dir.link = Some(shellmate_sftp::Link { target: "dist".into(), resolved: Some(EntryKind::Dir) });
        let mut to_file = entry("alias", EntryKind::Symlink, 0o777);
        to_file.link = Some(shellmate_sftp::Link { target: "x".into(), resolved: Some(EntryKind::File) });
        let mut dangling = entry("dead", EntryKind::Symlink, 0o777);
        dangling.link = Some(shellmate_sftp::Link { target: "nowhere".into(), resolved: None });
        let request = with_links.refresh().unwrap();
        with_links.apply(request.id, ok(&request, vec![to_dir, to_file, dangling]));
        assert_eq!(with_links.open_row(1).map(|r| r.dir), Some("/srv/app/shortcut".to_string()));
        assert_eq!(with_links.open_row(2), None);
        assert_eq!(with_links.open_row(3), None);
    }

    #[test]
    fn navigating_by_hand_turns_following_off() {
        let mut model = at_app();
        assert!(model.follow());
        model.go_to("/srv");
        assert!(!model.follow());
        let mut model = at_app();
        model.go_parent();
        assert!(!model.follow());
        let mut model = at_app();
        model.select_row(1);
        model.open_selected();
        assert!(!model.follow());
        let mut model = at_app();
        model.refresh();
        assert!(model.follow(), "a refresh is not a navigation");
    }

    #[test]
    fn the_panel_follows_the_terminal_while_following_is_on() {
        let mut model = at_app();
        let request = model.on_terminal_cwd("/var/log").expect("goes there");
        assert_eq!((request.dir.as_str(), request.cause), ("/var/log", Cause::Follow));
        model.apply(request.id, ok(&request, vec![file("syslog")]));
        assert_eq!(model.cwd(), "/var/log");
        assert!(model.follow(), "following does not turn following off");
        assert!(model.osc7_seen());
        // The same directory again changes nothing.
        assert_eq!(model.on_terminal_cwd("/var/log"), None);

        // Off: the panel stays, but remembers where the terminal is.
        model.go_to("/etc");
        assert_eq!(model.on_terminal_cwd("/home/me"), None);
        assert_eq!(model.terminal_cwd(), Some("/home/me"));
        // On again: back to the terminal's directory.
        let request = model.set_follow(true).expect("goes to the terminal");
        assert_eq!((request.dir.as_str(), request.cause), ("/home/me", Cause::Follow));
        // Off does nothing, and on without a known directory does nothing.
        assert_eq!(model.set_follow(false), None);
        assert!(!model.follow());
        let mut fresh = at_app();
        assert_eq!(fresh.set_follow(true), None);
    }

    #[test]
    fn a_followed_directory_that_cannot_be_listed_keeps_the_old_listing_and_says_why() {
        let mut model = at_app();
        let request = model.on_terminal_cwd("/root").unwrap();
        model.apply(request.id, Err(SftpError::PermissionDenied { path: "/root".into() }));
        assert_eq!(model.cwd(), "/srv/app", "still where it was");
        assert_eq!(model.view(), &View::List);
        assert_eq!(model.row_count(), 5, "the listing is intact: never a blank panel");
        assert_eq!(model.banner(), Some("无法打开终端当前目录 /root：权限不足：/root"));
        // The next successful listing clears the message.
        let request = model.refresh().unwrap();
        model.apply(request.id, ok(&request, listing()));
        assert_eq!(model.banner(), None);
    }

    #[test]
    fn a_directory_the_user_went_to_that_cannot_be_listed_is_an_error_with_a_way_back() {
        let mut model = at_app();
        let request = model.go_to("/root").unwrap();
        model.apply(request.id, Err(SftpError::PermissionDenied { path: "/root".into() }));
        assert_eq!(model.view(), &View::Error("权限不足：/root".to_string()));
        assert_eq!(model.cwd(), "/root");
        assert_eq!(model.row_count(), 0);
        assert_eq!(model.crumbs(), [("root".to_string(), "/root".to_string())]);
        // 返回上级
        let request = model.go_parent().expect("the way back");
        assert_eq!(request.dir, "/");
        model.apply(request.id, ok(&request, vec![dir("root")]));
        assert_eq!(model.view(), &View::List);
    }

    #[test]
    fn a_first_listing_that_fails_is_an_error_not_a_blank() {
        let mut model = FilesModel::new();
        let request = model.set_link(LinkStatus::Ready).unwrap();
        model.apply(request.id, Err(SftpError::NotFound { path: "/gone".into() }));
        assert!(matches!(model.view(), View::Error(_)));
    }

    #[test]
    fn stale_answers_are_ignored() {
        let mut model = at_app();
        let slow = model.go_to("/slow").unwrap();
        let fast = model.go_to("/fast").unwrap();
        model.apply(fast.id, ok(&fast, vec![file("fast.txt")]));
        assert_eq!(model.cwd(), "/fast");
        model.apply(slow.id, ok(&slow, vec![file("slow.txt")]));
        assert_eq!(model.cwd(), "/fast", "the answer of the older request came too late");
        assert!(matches!(model.row(1), Some(Row::Entry(e)) if e.name == "fast.txt"));
        model.apply(slow.id, Err(SftpError::Timeout));
        assert_eq!(model.view(), &View::List);
    }

    #[test]
    fn losing_the_connection_is_shown_and_the_panel_refreshes_itself_when_it_is_back() {
        let mut model = at_app();
        model.select_row(4);
        assert_eq!(model.set_link(LinkStatus::Disconnected), None);
        assert_eq!(model.link(), &LinkStatus::Disconnected);
        assert!(!model.is_loading());
        // Navigating meanwhile only remembers where to go.
        assert_eq!(model.go_to("/var"), None);
        assert_eq!(model.cwd(), "/var");
        // The session is back: the panel loads where it was.
        let request = model.set_link(LinkStatus::Ready).expect("an automatic refresh");
        assert_eq!(request.dir, "/var");
        model.apply(request.id, ok(&request, vec![file("log")]));
        assert_eq!(model.view(), &View::List);
        assert_eq!(model.row_count(), 2);
    }

    #[test]
    fn a_listing_that_meets_a_dead_session_waits_for_a_new_one_instead_of_claiming_a_disconnect() {
        // A dead SFTP subsystem under a live SSH connection: no reconnect is coming, so the panel
        // must not say "连接已断开，重连后自动刷新".
        for error in [SftpError::Disconnected(String::new()), SftpError::Timeout] {
            let mut model = at_app();
            let request = model.refresh().unwrap();
            assert_eq!(model.apply(request.id, Err(error)), Applied::SessionLost);
            assert_eq!(model.link(), &LinkStatus::Connecting, "waiting for the session to be reopened");
            assert!(!model.is_loading());
            assert_eq!(model.row_count(), 5, "the old listing stays on screen");
            // The reopened session arrives: the panel refreshes by itself, where it was.
            let request = model.set_link(LinkStatus::Ready).expect("an automatic refresh");
            assert_eq!((request.dir.as_str(), request.cause), ("/srv/app", Cause::Refresh));
            assert_eq!(model.apply(request.id, ok(&request, listing())), Applied::Shown);
            assert_eq!(model.link(), &LinkStatus::Ready);
        }
    }

    #[test]
    fn other_errors_and_stale_answers_are_not_a_lost_session() {
        let mut model = at_app();
        let request = model.refresh().unwrap();
        let denied = SftpError::PermissionDenied { path: "/srv/app".into() };
        assert_eq!(model.apply(request.id, Err(denied)), Applied::Shown);
        assert_eq!(model.link(), &LinkStatus::Ready);
        // An answer for a listing that is not the one in flight is dropped, lost session or not.
        assert_eq!(model.apply(request.id, Err(SftpError::Timeout)), Applied::Stale);
        assert_eq!(model.link(), &LinkStatus::Ready);
    }

    #[test]
    fn an_operation_that_loses_the_session_makes_the_panel_wait_too() {
        let mut model = at_app();
        model.session_lost();
        assert_eq!(model.link(), &LinkStatus::Connecting);
        // A session that is not ready already is left as it is (the link's state wins).
        model.set_link(LinkStatus::Failed("x".into()));
        model.session_lost();
        assert_eq!(model.link(), &LinkStatus::Failed("x".into()));
    }

    #[test]
    fn a_failed_session_is_reported_by_the_link_status() {
        let mut model = FilesModel::new();
        assert_eq!(model.set_link(LinkStatus::Failed("服务器拒绝了「sftp」子系统请求".into())), None);
        assert_eq!(model.link(), &LinkStatus::Failed("服务器拒绝了「sftp」子系统请求".into()));
        assert_eq!(model.refresh(), None, "nothing to list on");
        assert!(model.set_link(LinkStatus::Ready).is_some(), "a later success loads");
    }

    // ---- permissions ----

    #[test]
    fn the_permission_grid_edits_a_draft_of_the_selected_file() {
        let mut model = at_app();
        assert!(model.draft().is_none(), "nothing selected");
        model.select_row(4);
        let draft = model.draft().unwrap();
        assert_eq!((draft.octal().as_str(), draft.symbolic().as_str(), draft.is_dirty()), ("644", "rw-r--r--", false));
        assert!(draft.is_set(0, 0) && draft.is_set(0, 1) && !draft.is_set(0, 2));
        assert_eq!(model.chmod_request(), None, "nothing to apply yet");

        model.toggle_perm(0, 2); // owner execute
        model.toggle_perm(2, 0); // other read off
        let draft = model.draft().unwrap();
        assert_eq!((draft.octal().as_str(), draft.symbolic().as_str(), draft.is_dirty()), ("740", "rwxr-----", true));
        assert_eq!(model.chmod_request(), Some(("/srv/app/nginx.conf".to_string(), 0o740)));
        // Toggling back makes it clean again.
        model.toggle_perm(0, 2);
        model.toggle_perm(2, 0);
        assert_eq!(model.chmod_request(), None);
    }

    #[test]
    fn selecting_something_else_drops_the_draft() {
        let mut model = at_app();
        model.select_row(4);
        model.toggle_perm(0, 2);
        model.select_row(3);
        assert!(!model.draft().unwrap().is_dirty(), "a new draft of the other file");
        model.select_row(0);
        assert!(model.draft().is_none(), "none for ..");
    }

    #[test]
    fn only_files_and_directories_have_a_grid() {
        let mut model = at_app();
        let mut link = entry("link", EntryKind::Symlink, 0o777);
        link.link = Some(shellmate_sftp::Link { target: "x".into(), resolved: Some(EntryKind::File) });
        let request = model.refresh().unwrap();
        model.apply(request.id, ok(&request, vec![link, dir("d"), entry("pipe", EntryKind::Other, 0o644)]));
        model.select_row(1);
        assert!(model.draft().is_none(), "a link's own mode means nothing");
        model.select_row(2);
        assert!(model.draft().is_some());
        model.select_row(3);
        assert!(model.draft().is_none());
    }

    #[test]
    fn special_bits_are_kept_by_the_draft() {
        let mut draft = PermDraft::new(0o4755);
        assert_eq!((draft.octal().as_str(), draft.symbolic().as_str()), ("4755", "rwsr-xr-x"));
        draft.toggle(1, 1);
        assert_eq!(draft.mode(), 0o4775);
        assert_eq!(PermDraft::new(0o100644).mode(), 0o644, "file type bits are not permissions");
    }

    #[test]
    fn chmod_errors_stay_under_the_button_and_success_refreshes() {
        let mut model = at_app();
        model.select_row(4);
        model.toggle_perm(0, 2);
        assert_eq!(model.chmod_done(Err(SftpError::PermissionDenied { path: "/srv/app/nginx.conf".into() })), None);
        assert_eq!(model.chmod_error(), Some("权限不足：/srv/app/nginx.conf"));
        model.toggle_perm(0, 2);
        assert_eq!(model.chmod_error(), None, "editing clears the message");
        let request = model.chmod_done(Ok(())).expect("the list is refreshed");
        assert_eq!((request.dir.as_str(), request.cause), ("/srv/app", Cause::Refresh));
        let mut changed = listing();
        changed[3].mode = Some(0o740);
        model.apply(request.id, ok(&request, changed));
        assert_eq!(model.selected_entry().and_then(|e| e.mode), Some(0o740), "the selection stayed");
        assert!(!model.draft().unwrap().is_dirty());
        assert_eq!(model.draft().unwrap().octal(), "740");
    }

    // ---- the directory itself ----

    fn stat_of(path: &str, mode: u32) -> Entry {
        let mut entry = entry("app", EntryKind::Dir, mode);
        entry.path = path.to_string();
        entry.size = 4096;
        entry.owner = "deploy".into();
        entry.group = "www".into();
        entry
    }

    #[test]
    fn with_nothing_selected_the_details_are_those_of_the_directory() {
        let mut model = FilesModel::new();
        model.navigate("/srv/app", Cause::Hand);
        let request = model.set_link(LinkStatus::Ready).unwrap();
        model.apply_listed(
            request.id,
            Listed { dir: Some("/srv/app".into()), entries: Ok(listing()), stat: Some(stat_of("/srv/app", 0o750)) },
        );
        let Subject::Dir(Some(stat)) = model.subject() else { panic!("{:?}", model.subject()) };
        assert_eq!((stat.owner.as_str(), stat.modified), ("deploy", Some(1_700_000_000)));
        assert_eq!(model.draft().map(PermDraft::octal).as_deref(), Some("750"), "its permissions are on the grid");

        // chmod works on it.
        model.toggle_perm(1, 1);
        assert_eq!(model.chmod_request(), Some(("/srv/app".to_string(), 0o770)));

        // A selection takes over, and `..` is none of the directory's business.
        model.select_row(4);
        assert!(matches!(model.subject(), Subject::Entry(e) if e.name == "nginx.conf"));
        model.select_row(0);
        assert_eq!(model.subject(), Subject::Parent);
        assert!(model.draft().is_none() && model.chmod_request().is_none());
    }

    #[test]
    fn a_directory_that_cannot_be_listed_still_shows_its_own_details() {
        let mut model = FilesModel::new();
        let request = model.set_link(LinkStatus::Ready).unwrap();
        model.apply_listed(request.id, Listed { dir: Some("/srv/app".into()), entries: Ok(listing()), stat: None });
        let request = model.navigate("/root", Cause::Hand).unwrap();
        let denied = SftpError::PermissionDenied { path: "/root".into() };
        model.apply_listed(
            request.id,
            Listed { dir: Some("/root".into()), entries: Err(denied), stat: Some(stat_of("/root", 0o700)) },
        );
        assert_eq!((model.cwd(), model.view()), ("/root", &View::Error("权限不足：/root".into())));
        assert!(matches!(model.subject(), Subject::Dir(Some(stat)) if stat.owner == "deploy"));
        assert_eq!(model.draft().map(PermDraft::octal).as_deref(), Some("700"));
    }

    #[test]
    fn a_followed_directory_that_fails_keeps_the_old_details() {
        let mut model = FilesModel::new();
        let request = model.set_link(LinkStatus::Ready).unwrap();
        model.apply_listed(
            request.id,
            Listed { dir: Some("/srv/app".into()), entries: Ok(listing()), stat: Some(stat_of("/srv/app", 0o755)) },
        );
        let request = model.on_terminal_cwd("/elsewhere").expect("follows");
        let missing = SftpError::NotFound { path: "/elsewhere".into() };
        model.apply_listed(request.id, Listed { dir: None, entries: Err(missing), stat: None });
        assert_eq!(model.cwd(), "/srv/app");
        assert_eq!(model.draft().map(PermDraft::octal).as_deref(), Some("755"), "still the old directory's");
    }

    #[test]
    fn the_grid_and_apply_are_off_while_there_is_no_ready_session() {
        let mut model = at_app();
        model.select_row(4);
        model.toggle_perm(0, 2);
        assert!(model.chmod_enabled() && model.chmod_request().is_some());
        for link in [LinkStatus::Disconnected, LinkStatus::Connecting, LinkStatus::Failed("x".into())] {
            model.set_link(link.clone());
            assert!(!model.chmod_enabled(), "{link:?}");
            assert_eq!(model.chmod_request(), None, "{link:?}: nothing can be applied");
            let before = model.draft().cloned();
            model.toggle_perm(1, 1);
            assert_eq!(model.draft().cloned(), before, "{link:?}: the grid does not move");
        }
    }

    // ---- renaming, new folders, deleting ----

    #[test]
    fn rename_opens_an_editor_on_the_selected_entry_only() {
        let mut model = at_app();
        assert!(!model.start_rename(), "nothing selected");
        model.select_row(0);
        assert!(!model.start_rename(), "`..` cannot be renamed");
        model.select_row(4);
        assert!(model.start_rename());
        assert_eq!(model.edit(), Some(&Edit { name: "nginx.conf".into(), error: None, busy: false }));
        // Esc closes it, selecting something else too.
        model.cancel_edit();
        assert_eq!(model.edit(), None);
        assert!(model.start_rename());
        model.select_row(3);
        assert_eq!(model.edit(), None, "the editor belonged to the other row");
    }

    #[test]
    fn an_unchanged_name_closes_the_editor_and_bad_names_keep_it_open_with_the_reason() {
        let mut model = at_app();
        model.select_row(4);
        model.start_rename();
        assert_eq!(model.commit_edit("nginx.conf"), None);
        assert_eq!(model.edit(), None, "nothing to do: closed");

        model.start_rename();
        for (text, why) in [
            ("", "名称不能为空"),
            ("   ", "名称不能为空"),
            ("a/b", "名称不能包含「/」"),
            ("..", "名称无效"),
            (".", "名称无效"),
            (".env", "已存在：/srv/app/.env"),
        ] {
            assert_eq!(model.commit_edit(text), None, "{text:?}");
            assert_eq!(model.edit().and_then(|e| e.error.as_deref()), Some(why), "{text:?}");
            assert_eq!(model.edit().map(|e| e.busy), Some(false), "still editable");
        }
    }

    #[test]
    fn a_rename_is_sent_once_and_selects_the_new_name_when_it_worked() {
        let mut model = at_app();
        model.select_row(4);
        model.start_rename();
        let request = model.commit_edit("nginx.old").expect("a rename");
        assert_eq!(
            request,
            RenameRequest {
                from: "/srv/app/nginx.conf".into(),
                to: "/srv/app/nginx.old".into(),
                new_name: "nginx.old".into()
            }
        );
        assert_eq!(model.edit().map(|e| e.busy), Some(true));
        assert_eq!(model.commit_edit("again"), None, "not twice while it is on its way");
        assert!(!model.ops_enabled(), "nothing else meanwhile");

        let list = model.rename_done(&request, Ok(())).expect("the directory is listed again");
        assert_eq!((list.dir.as_str(), list.cause), ("/srv/app", Cause::Refresh));
        assert_eq!(model.edit(), None);
        model.apply(list.id, ok(&list, vec![dir("config"), dir("dist"), file(".env"), file("nginx.old")]));
        assert_eq!(model.selected_entry().map(|e| e.name.as_str()), Some("nginx.old"));
        assert_eq!(model.take_reveal(), Some(4), "scrolled to it");
        assert_eq!(model.take_reveal(), None, "once");
    }

    #[test]
    fn a_refused_rename_shows_the_servers_reason_and_stays_in_edit_mode() {
        let mut model = at_app();
        model.select_row(4);
        model.start_rename();
        let request = model.commit_edit("other.conf").unwrap();
        let exists = SftpError::AlreadyExists { path: "/srv/app/other.conf".into() };
        assert_eq!(model.rename_done(&request, Err(exists)), None);
        let edit = model.edit().unwrap();
        assert_eq!((edit.error.as_deref(), edit.busy), (Some("已存在：/srv/app/other.conf"), false));
        // Another try with another name works.
        let request = model.commit_edit("third.conf").expect("try again");
        assert_eq!(model.edit().and_then(|e| e.error.clone()), None, "the old message is gone");
        assert!(model.rename_done(&request, Ok(())).is_some());
    }

    #[test]
    fn a_rename_that_loses_the_session_can_be_tried_again() {
        let mut model = at_app();
        model.select_row(4);
        model.start_rename();
        model.commit_edit("x.conf").unwrap();
        model.session_lost();
        assert_eq!(model.edit().map(|e| e.busy), Some(false));
        assert!(model.edit().is_some(), "the typed name is still there to send again");
    }

    #[test]
    fn a_new_folder_is_selected_and_named_right_away() {
        let mut model = at_app();
        let list = model.new_folder_done("/srv/app/新建文件夹").expect("a listing");
        let mut entries = listing();
        entries.insert(2, dir("新建文件夹"));
        model.apply(list.id, ok(&list, entries));
        assert_eq!(model.selected_entry().map(|e| e.name.as_str()), Some("新建文件夹"));
        assert_eq!(model.edit(), Some(&Edit { name: "新建文件夹".into(), error: None, busy: false }));
        assert_eq!(model.take_reveal(), Some(3));
        // Esc leaves it with the name it has.
        model.cancel_edit();
        assert_eq!(model.edit(), None);
        assert_eq!(model.selected_entry().map(|e| e.name.as_str()), Some("新建文件夹"));
    }

    #[test]
    fn an_edit_does_not_outlive_the_listing_it_belongs_to() {
        let mut model = at_app();
        let list = model.new_folder_done("/srv/app/新建文件夹").unwrap();
        // The user went elsewhere before the listing arrived: nothing is selected or edited there.
        model.go_to("/var");
        let mut entries = listing();
        entries.push(dir("新建文件夹"));
        model.apply(list.id, ok(&list, entries));
        assert_eq!(model.edit(), None);
        // A refresh in which the edited entry is gone closes the editor.
        model.select_row(4);
        model.start_rename();
        let list = model.refresh().expect("refresh");
        model.apply(list.id, ok(&list, vec![dir("config")]));
        assert_eq!(model.edit(), None);
    }

    #[test]
    fn deleting_asks_about_the_selected_entry_and_says_what_goes() {
        let mut model = at_app();
        assert_eq!(model.delete_request(), None, "nothing selected");
        model.select_row(0);
        assert_eq!(model.delete_request(), None, "never `..`");

        model.select_row(4);
        let file_request = model.delete_request().unwrap();
        assert_eq!(file_request.title(), "删除 nginx.conf？");
        assert_eq!(file_request.body(), "将从服务器永久删除 /srv/app/nginx.conf，无法撤销。");

        model.select_row(1);
        let dir_request = model.delete_request().unwrap();
        assert_eq!(dir_request.kind, DeleteKind::Dir);
        assert_eq!(dir_request.title(), "删除目录 config？");
        assert_eq!(dir_request.body(), "将递归删除目录 /srv/app/config 及其中的全部文件和子目录，无法撤销。");

        let mut link = entry("current", EntryKind::Symlink, 0o777);
        link.link = Some(shellmate_sftp::Link { target: "config".into(), resolved: Some(EntryKind::Dir) });
        let list = model.refresh().unwrap();
        model.apply(list.id, ok(&list, vec![link]));
        model.select_row(1);
        let link_request = model.delete_request().unwrap();
        assert_eq!(link_request.kind, DeleteKind::Link, "a link to a directory is removed as a link");
        assert_eq!(link_request.body(), "将删除符号链接 /srv/app/current（不会删除它指向的内容），无法撤销。");
    }

    #[test]
    fn after_a_delete_the_next_entry_is_selected_else_the_previous_one() {
        let delete = |model: &mut FilesModel, row: usize, remaining: Vec<Entry>| {
            model.select_row(row);
            let request = model.delete_request().unwrap();
            let list = model.delete_done(&request, Ok(())).expect("listed again");
            model.apply(list.id, ok(&list, remaining));
        };
        let selected = |model: &FilesModel| model.selected_entry().map(|e| e.name.clone());

        let mut model = at_app();
        delete(&mut model, 2, vec![dir("config"), file(".env"), file("nginx.conf")]);
        assert_eq!(selected(&model).as_deref(), Some(".env"), "the one that moved up into its place");
        assert_eq!(model.take_reveal(), Some(2));

        let mut model = at_app();
        delete(&mut model, 4, vec![dir("config"), dir("dist"), file(".env")]);
        assert_eq!(selected(&model).as_deref(), Some(".env"), "the last one was deleted: the one above");

        let mut model = at_app();
        let list = model.refresh().unwrap();
        model.apply(list.id, ok(&list, vec![file("only")]));
        delete(&mut model, 1, vec![]);
        assert_eq!(selected(&model), None, "nothing left to select");
        assert_eq!(model.row_count(), 1, "just `..`");
    }

    #[test]
    fn a_failed_delete_lists_again_and_keeps_saying_so() {
        let mut model = at_app();
        model.select_row(1);
        let request = model.delete_request().unwrap();
        let denied = SftpError::PermissionDenied { path: "/srv/app/config".into() };
        let list = model.delete_done(&request, Err(denied)).expect("part of it may be gone: list again");
        assert_eq!(model.banner(), Some("无法删除 config：权限不足：/srv/app/config"));
        model.apply(list.id, ok(&list, listing()));
        assert_eq!(model.banner(), Some("无法删除 config：权限不足：/srv/app/config"), "survives the listing");
        let list = model.refresh().unwrap();
        model.apply(list.id, ok(&list, listing()));
        assert_eq!(model.banner(), None, "gone with the next one");
    }

    #[test]
    fn download_takes_the_selection_and_only_with_a_session() {
        let mut model = at_app();
        assert_eq!(model.download_path(), None, "nothing selected");
        model.select_row(0);
        assert_eq!(model.download_path(), None, "`..` is not downloaded");
        model.select_row(2);
        assert_eq!(model.download_path().as_deref(), Some("/srv/app/dist"), "a directory too");
        model.set_link(LinkStatus::Disconnected);
        assert_eq!(model.download_path(), None);
        assert_eq!(model.delete_request(), None);
        assert!(!model.start_rename());
    }

    #[test]
    fn messages_set_by_the_panel_stay_until_the_next_listing() {
        let mut model = at_app();
        model.set_banner("无法创建下载目录");
        assert_eq!(model.banner(), Some("无法创建下载目录"));
        let list = model.refresh().unwrap();
        model.apply(list.id, ok(&list, listing()));
        assert_eq!(model.banner(), None);
    }

    // ---- temporary files of our own uploads ----

    #[test]
    fn the_temporary_files_of_unfinished_uploads_are_left_out_of_the_list_and_crash_leftovers_stay() {
        let mut model = at_app();
        let mut with_temps = listing();
        with_temps.push(file(".nginx.conf.shellmate-upload"));
        with_temps.push(file(".crashed.bin.shellmate-upload"));
        model.set_hidden(HashSet::from(["/srv/app/.nginx.conf.shellmate-upload".to_string()]));
        let list = model.refresh().unwrap();
        model.apply(list.id, ok(&list, with_temps));
        let names: Vec<_> = model.entries().iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&".nginx.conf.shellmate-upload"), "an upload of ours is writing it: {names:?}");
        assert!(names.contains(&".crashed.bin.shellmate-upload"), "nobody is writing this one: {names:?}");
        assert_eq!(model.row_count(), 1 + 5, "`..` and the entries that are shown");
    }

    #[test]
    fn a_file_that_starts_being_written_while_listed_disappears_and_the_selection_survives_or_clears() {
        let mut model = at_app();
        let list = model.refresh().unwrap();
        let mut entries = listing();
        entries.push(file(".late.bin.shellmate-upload"));
        model.apply(list.id, ok(&list, entries));
        assert_eq!(model.row_count(), 1 + 5);
        model.select_name(".late.bin.shellmate-upload");
        model.set_hidden(HashSet::from(["/srv/app/.late.bin.shellmate-upload".to_string()]));
        assert_eq!(model.row_count(), 1 + 4);
        assert_eq!(model.selected_entry(), None, "what was selected is gone");
        // The same set again changes nothing; an empty one lets later listings show everything.
        model.set_hidden(HashSet::from(["/srv/app/.late.bin.shellmate-upload".to_string()]));
        model.set_hidden(HashSet::new());
        let list = model.refresh().unwrap();
        let mut entries = listing();
        entries.push(file(".late.bin.shellmate-upload"));
        model.apply(list.id, ok(&list, entries));
        assert_eq!(model.row_count(), 1 + 5);
    }
}
