//! The state of a file panel that does not need the UI: where it is, what is listed, what is
//! selected, whether it follows the terminal, and the permission draft. Pure, so it is tested
//! without a window; the panel only draws it and feeds it what happens.

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
    view: View,
    selection: Option<Selection>,
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
            view: View::Loading,
            selection: None,
            follow: true,
            terminal_cwd: None,
            banner: None,
            pending: None,
            next_request: 1,
            draft: None,
            chmod_error: None,
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

    /// The selected entry (`None` for nothing, or for `..`).
    pub fn selected_entry(&self) -> Option<&Entry> {
        match self.selection.as_ref()? {
            Selection::Parent => None,
            Selection::Name(name) => self.entries.iter().find(|e| &e.name == name),
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
            LinkStatus::Connecting => None,
        }
    }

    /// Where the panel starts: the directory the terminal reported, else the home directory
    /// (`None`; the caller asks the server).
    fn start_dir(&self) -> Option<String> {
        self.terminal_cwd.clone()
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

    /// The answer to a [`Request`]: the directory that was listed (the server's absolute form: the
    /// home directory for `.`) and its entries. Answers to listings that are no longer the one in
    /// flight are ignored. A directory that cannot be listed leaves an older listing on screen (with the
    /// reason above it) when the panel got there by following the terminal or by refreshing, and
    /// is shown as an error with a way back when the user went there.
    pub fn apply(&mut self, id: u64, result: Result<(String, Vec<Entry>), SftpError>) {
        let Some(request) = self.pending.take_if(|pending| pending.id == id) else { return };
        match result {
            Ok((dir, mut entries)) => {
                let keep = matches!(request.cause, Cause::Refresh) && dir == self.cwd;
                let previous = self.selection.clone();
                self.cwd = dir;
                // The server lists directories in a sorted order already; keep it as it is.
                std::mem::swap(&mut self.entries, &mut entries);
                self.view = View::List;
                self.banner = None;
                self.selection = previous.filter(|_| keep).filter(|sel| self.selection_exists(sel));
                self.reset_draft();
            }
            Err(err) if err.is_disconnected() => {
                self.link = LinkStatus::Disconnected;
            }
            Err(err) => {
                let reason = err.to_string();
                match request.cause {
                    Cause::Hand | Cause::Initial => {
                        self.cwd = request.dir;
                        self.entries.clear();
                        self.view = View::Error(reason);
                        self.banner = None;
                        self.selection = None;
                        self.reset_draft();
                    }
                    Cause::Follow | Cause::Refresh => {
                        // Keep what is on screen: never a blank panel.
                        if matches!(self.view, View::Loading) {
                            self.cwd = request.dir;
                            self.view = View::Error(reason);
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
        if self.selection != selection {
            self.selection = selection;
            self.reset_draft();
        }
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

    // ---- permissions ------------------------------------------------------------------------------------------

    fn reset_draft(&mut self) {
        self.chmod_error = None;
        self.draft = self
            .selected_entry()
            .filter(|entry| matches!(entry.kind, EntryKind::File | EntryKind::Dir))
            .and_then(|entry| entry.mode)
            .map(PermDraft::new);
    }

    pub fn draft(&self) -> Option<&PermDraft> {
        self.draft.as_ref()
    }

    pub fn toggle_perm(&mut self, row: usize, column: usize) {
        if let Some(draft) = &mut self.draft {
            draft.toggle(row, column);
            self.chmod_error = None;
        }
    }

    /// "应用 chmod": the path and mode to set, if there is a draft that differs.
    pub fn chmod_request(&self) -> Option<(String, u32)> {
        let entry = self.selected_entry()?;
        let draft = self.draft.as_ref().filter(|d| d.is_dirty())?;
        Some((entry.path.clone(), draft.mode()))
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
    fn a_listing_that_meets_a_dead_connection_turns_into_the_disconnected_state() {
        let mut model = at_app();
        let request = model.refresh().unwrap();
        model.apply(request.id, Err(SftpError::Disconnected(String::new())));
        assert_eq!(model.link(), &LinkStatus::Disconnected);
        assert_eq!(model.row_count(), 5, "the old listing is kept for when it is back");
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
}
