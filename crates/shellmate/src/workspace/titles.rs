//! Program titles are presentation only: session names and connection identities stay stable.
use super::*;

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
