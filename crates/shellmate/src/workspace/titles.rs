//! Program titles are presentation only: session names and connection identities stay stable.
use super::*;
use std::collections::HashSet;

impl Session {
    pub(super) fn display_title(&self, cx: &App) -> SharedString {
        select_title(
            self.terminal.read(cx).title(),
            self.local_title.as_ref(),
            &self.title,
        )
    }

    pub(super) fn title_tooltip(&self, cx: &App) -> SharedString {
        let title = self.display_title(cx);
        let identity = match self.host() {
            Some(host) => format!("{} · {}", self.title, host.address_line()),
            None => self.title.to_string(),
        };
        if title == identity {
            title
        } else {
            format!("{title}\n{identity}").into()
        }
    }

    /// Polled on the existing one-second workspace tick, never from tab layout.
    /// Fixtures have no local launch options, so keep their original labels.
    pub(super) fn refresh_local_title(&mut self, cx: &mut App) -> bool {
        if !matches!(self.kind, SessionKind::Local) {
            return false;
        }
        let title = self.terminal.update(cx, |terminal, _| {
            let options = terminal.options()?.clone();
            if terminal.is_exited() {
                return None;
            }
            let process = terminal.foreground_process();
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let cwd = process
                .as_ref()
                .and_then(|p| p.cwd.as_deref())
                .or_else(|| terminal.cwd().map(Path::new))
                .or(options.cwd.as_deref())
                .or(home.as_deref());
            let shell = shell_name(&options.shell_path());
            let program = process.as_ref().map_or(shell.as_str(), |p| p.name.as_str());
            local_title(cwd, program).map(Into::into)
        });
        if self.local_title == title {
            return false;
        }
        self.local_title = title;
        true
    }
}

impl Workspace {
    pub(super) fn refresh_titles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        for session in &mut self.sessions {
            changed |= session.refresh_local_title(cx);
        }
        if changed {
            // The title follows the directory and the program: the record of the session follows too.
            self.hub.saver.borrow_mut().mark();
            self.update_window_title(window, cx);
            self.sync_productivity(cx);
            cx.notify();
        }
    }
}

impl Workspace {
    /// The names of every session in every window (sessions keep their name when they move).
    fn taken_session_names(&self, cx: &App) -> Vec<SharedString> {
        let mut names: Vec<_> = self.sessions.iter().map(|s| s.title.clone()).collect();
        // The caller holds this window; only peers are read.
        for entry in self.peers(Route::All, cx) {
            if let Some(w) = entry.workspace.upgrade() {
                names.extend(w.read(cx).sessions.iter().map(|s| s.title.clone()));
            }
        }
        names
    }

    /// The name for a new session of `base` that no session in any window has.
    pub(super) fn unique_session_name(&self, base: &str, cx: &App) -> String {
        unique_name(base, &self.taken_session_names(cx))
    }
}

/// The first of `base`, `base · 2`, `base · 3`, … that is not in `taken`: the smallest free number wins
/// and the first name has none. Only exact matches count, so `本机终端` or `本机 · x` do not block `本机`.
pub(super) fn unique_name(base: &str, taken: &[impl AsRef<str>]) -> String {
    let taken: HashSet<&str> = taken.iter().map(AsRef::as_ref).collect();
    let mut name = base.to_owned();
    let mut number = 1;
    while taken.contains(name.as_str()) {
        number += 1;
        name = format!("{base} · {number}");
    }
    name
}

fn select_title(
    osc: Option<&SharedString>,
    local: Option<&SharedString>,
    original: &SharedString,
) -> SharedString {
    osc.filter(|title| !title.trim().is_empty())
        .or(local)
        .unwrap_or(original)
        .clone()
}

fn local_title(cwd: Option<&Path>, program: &str) -> Option<String> {
    let directory = cwd.filter(|path| !path.as_os_str().is_empty()).map(|path| {
        path.file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
    });
    match (directory, program.trim()) {
        (Some(directory), "") => Some(directory.into_owned()),
        (Some(directory), program) => Some(format!("{directory} — {program}")),
        (None, "") => None,
        (None, program) => Some(program.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens a session called `base` the way the workspace does and returns its name.
    fn open(base: &str, taken: &mut Vec<String>) -> String {
        let name = unique_name(base, taken);
        taken.push(name.clone());
        name
    }

    fn close(name: &str, taken: &mut Vec<String>) {
        taken.retain(|taken| taken != name);
    }

    #[test]
    fn the_first_name_has_no_number() {
        assert_eq!(unique_name("本机", &[] as &[&str]), "本机");
        assert_eq!(unique_name("db", &["本机"]), "db");
    }

    #[test]
    fn numbers_are_handed_out_in_order() {
        let mut taken = Vec::new();
        let names: Vec<_> = (0..4).map(|_| open("本机", &mut taken)).collect();
        assert_eq!(names, ["本机", "本机 · 2", "本机 · 3", "本机 · 4"]);
    }

    #[test]
    fn closing_a_middle_one_reuses_its_number() {
        let mut taken = Vec::new();
        for _ in 0..3 {
            open("本机", &mut taken);
        }
        close("本机 · 2", &mut taken);
        assert_eq!(open("本机", &mut taken), "本机 · 2");
        assert_eq!(open("本机", &mut taken), "本机 · 4");
    }

    #[test]
    fn closing_the_first_reuses_the_bare_base() {
        let mut taken = Vec::new();
        for _ in 0..3 {
            open("本机", &mut taken);
        }
        close("本机", &mut taken);
        assert_eq!(open("本机", &mut taken), "本机");
        assert_eq!(open("本机", &mut taken), "本机 · 4");
    }

    #[test]
    fn a_gap_is_filled_before_the_end_is_extended() {
        assert_eq!(unique_name("本机", &["本机", "本机 · 3"]), "本机 · 2");
        assert_eq!(unique_name("本机", &["本机 · 3"]), "本机");
        assert_eq!(unique_name("本机", &["本机 · 2"]), "本机");
        assert_eq!(unique_name("本机", &["本机", "本机 · 2", "本机 · 4"]), "本机 · 3");
        assert_eq!(unique_name("本机", &["本机", "本机 · 2", "本机 · 3"]), "本机 · 4");
    }

    #[test]
    fn similar_names_do_not_block() {
        let taken = ["本机终端", "本机 · x", "本机 · ", "本机 · 02", "本机·2", "本机  · 2", "x本机", "本机 · 2 ", " 本机"];
        assert_eq!(unique_name("本机", &taken), "本机");
        let taken = ["本机", "本机终端", "本机 · x", "本机 · 02", "本机 · 3"];
        assert_eq!(unique_name("本机", &taken), "本机 · 2");
    }

    #[test]
    fn series_of_different_bases_are_independent() {
        let mut taken = Vec::new();
        assert_eq!(open("web", &mut taken), "web");
        assert_eq!(open("本机", &mut taken), "本机");
        assert_eq!(open("web", &mut taken), "web · 2");
        assert_eq!(open("本机", &mut taken), "本机 · 2");
        // A base that already looks numbered starts its own series.
        assert_eq!(open("web · 2", &mut taken), "web · 2 · 2");
        close("web", &mut taken);
        assert_eq!(open("web", &mut taken), "web");
    }

    #[test]
    fn names_from_all_windows_are_taken() {
        let first: Vec<String> = vec!["本机".into(), "web".into()];
        let second: Vec<String> = vec!["本机 · 2".into(), "web · 2".into(), "本机 · 4".into()];
        let all: Vec<&String> = first.iter().chain(&second).collect();
        assert_eq!(unique_name("本机", &all), "本机 · 3");
        assert_eq!(unique_name("web", &all), "web · 3");
        // Each window alone would have handed out a name that the other one already has.
        assert_eq!(unique_name("本机", &first), "本机 · 2");
        assert_eq!(unique_name("本机", &second), "本机");
        assert_eq!(unique_name("db", &all), "db");
    }

    #[test]
    fn program_title_wins_without_losing_symbols_or_original_identity() {
        let original: SharedString = "production · 2".into();
        let local: SharedString = "shellmate-agent — zsh".into();
        for text in ["✳ Claude Code", "⠋ 正在处理 — 项目 α", "work", "a;b;c"] {
            let osc: SharedString = text.to_string().into();
            assert_eq!(select_title(Some(&osc), Some(&local), &original), text);
        }
        assert_eq!(original, "production · 2");
        assert_eq!(select_title(None, Some(&local), &original), local);
        assert_eq!(select_title(None, None, &original), original);
        assert_eq!(select_title(Some(&" \t".into()), None, &original), original);
    }

    #[test]
    fn local_fallback_handles_directories_programs_and_missing_metadata() {
        for (path, program, expected) in [
            (
                Some("/work/shellmate-agent"),
                "zsh",
                Some("shellmate-agent — zsh"),
            ),
            (Some("/work/项目 α/"), "python3", Some("项目 α — python3")),
            (Some("/"), "zsh", Some("/ — zsh")),
            (None, "zsh", Some("zsh")),
            (Some("/work/app"), "", Some("app")),
            (Some(""), "", None),
            (None, " ", None),
        ] {
            assert_eq!(
                local_title(path.map(Path::new), program).as_deref(),
                expected
            );
        }
    }
}
