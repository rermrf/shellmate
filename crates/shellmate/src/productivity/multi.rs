//! Broadcast, split, and tab-list behavior, independent of gpui.
use super::model::risky;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Connected,
    Connecting,
    Reconnecting,
    Failed,
    Offline,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub id: usize,
    pub title: String,
    pub local: bool,
    pub protocol: shellmate_core::Protocol,
    pub status: Status,
    /// Something happened in the session since it was last looked at (a dot beside its title in the tab menu).
    pub attention: bool,
}
impl SessionRow {
    pub fn connected(&self) -> bool {
        self.status == Status::Connected
    }
    pub fn subtitle(&self) -> &'static str {
        match self.status {
            Status::Connected if self.local => "本机",
            Status::Connected => self.protocol.label(),
            Status::Connecting => "连接中",
            Status::Reconnecting => "重连中",
            Status::Failed => "连接失败",
            Status::Offline => "离线",
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Broadcast {
    pub enabled: bool,
    pub checked: HashSet<usize>,
}
impl Broadcast {
    pub fn enable(&mut self, enabled: bool, current: Option<usize>, sessions: &[SessionRow]) {
        self.enabled = enabled;
        if enabled
            && self.checked.is_empty()
            && let Some(id) = current.filter(|id| sessions.iter().any(|s| s.id == *id && s.connected()))
        {
            self.checked.insert(id);
        }
    }
    pub fn toggle(&mut self, id: usize, sessions: &[SessionRow]) {
        if sessions.iter().any(|s| s.id == id && s.connected()) && !self.checked.remove(&id) {
            self.checked.insert(id);
        }
    }
    /// Resolve in tab order at send time, retaining disconnected checkmarks in the model.
    pub fn targets(&self, sessions: &[SessionRow]) -> Vec<usize> {
        connected_targets(&self.checked, sessions)
    }
    pub fn chip(&self, sessions: &[SessionRow]) -> String {
        format!("广播 → {} 个会话", self.targets(sessions).len())
    }
}

pub fn connected_targets(checked: &HashSet<usize>, sessions: &[SessionRow]) -> Vec<usize> {
    let mut seen = HashSet::new();
    sessions
        .iter()
        .filter(|s| checked.contains(&s.id) && s.connected() && seen.insert(s.id))
        .map(|s| s.id)
        .collect()
}

/// Execute the resolved set exactly once in tab order. Failures do not retry successful targets.
pub fn fan_out<E>(
    ids: &[usize],
    mut send: impl FnMut(usize) -> Result<(), E>,
) -> Vec<(usize, Result<(), E>)> {
    let mut seen = HashSet::new();
    ids.iter()
        .copied()
        .filter(|id| seen.insert(*id))
        .map(|id| (id, send(id)))
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendPlan {
    Disabled,
    Confirm(Vec<usize>),
    Direct(Vec<usize>),
}
pub fn broadcast_plan(state: &Broadcast, sessions: &[SessionRow], command: &str) -> SendPlan {
    let ids = state.targets(sessions);
    if !state.enabled || command.trim().is_empty() || ids.is_empty() {
        SendPlan::Disabled
    } else if risky(command) {
        SendPlan::Confirm(ids)
    } else {
        SendPlan::Direct(ids)
    }
}

/// History is newest first; quick commands follow in their configured order.
pub fn suggestions<'a>(
    query: &str,
    history: impl IntoIterator<Item = &'a str>,
    snippets: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return vec![];
    }
    let mut seen = HashSet::new();
    history
        .into_iter()
        .chain(snippets)
        .filter(|s| {
            let lower = s.to_lowercase();
            lower != query && lower.contains(&query) && seen.insert(*s)
        })
        .take(5)
        .map(str::to_string)
        .collect()
}
#[derive(Clone, Copy)]
pub enum SuggestKey {
    Up,
    Down,
    Tab,
    Enter,
}
#[derive(Debug, PartialEq, Eq)]
pub enum SuggestAction {
    None,
    Accept(String),
    Send,
}
#[derive(Default)]
pub struct SuggestionSelection {
    pub selected: Option<usize>,
}
impl SuggestionSelection {
    pub fn key(&mut self, key: SuggestKey, items: &[String]) -> SuggestAction {
        match key {
            SuggestKey::Up => self.selected = self.selected.and_then(|i| i.checked_sub(1)),
            SuggestKey::Down if !items.is_empty() => {
                self.selected = Some(self.selected.map_or(0, |i| (i + 1).min(items.len() - 1)))
            }
            SuggestKey::Tab | SuggestKey::Enter => {
                let index = self
                    .selected
                    .or_else(|| matches!(key, SuggestKey::Tab).then_some(0));
                if let Some(value) = index.and_then(|i| items.get(i)) {
                    self.selected = None;
                    return SuggestAction::Accept(value.clone());
                }
                if matches!(key, SuggestKey::Enter) {
                    return SuggestAction::Send;
                }
            }
            _ => {}
        }
        SuggestAction::None
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Axis {
    #[default]
    Horizontal,
    Vertical,
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Pane {
    #[default]
    Main,
    Other,
}
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Split {
    pub other: Option<usize>,
    pub axis: Axis,
    pub focus: Pane,
}
impl Split {
    pub fn candidate(main: usize, sessions: &[SessionRow], recent: &[usize]) -> Option<usize> {
        recent.iter().copied().chain(sessions.iter().map(|session| session.id))
            .find(|id| *id != main && sessions.iter().any(|s| s.id == *id && (s.connected() || s.local)))
    }

    pub fn toggle(&mut self, main: usize, sessions: &[SessionRow], recent: &[usize]) -> bool {
        if self.other.is_some() {
            self.close();
            return true;
        }
        self.other = Self::candidate(main, sessions, recent);
        self.focus = Pane::Main;
        self.other.is_some()
    }
    pub fn close(&mut self) {
        self.other = None;
        self.focus = Pane::Main;
    }
    pub fn flip(&mut self) {
        self.axis = if self.axis == Axis::Horizontal {
            Axis::Vertical
        } else {
            Axis::Horizontal
        };
    }
    pub fn focused(&self, main: usize, terminal: bool) -> usize {
        if terminal && self.focus == Pane::Other {
            self.other.filter(|id| *id != main).unwrap_or(main)
        } else {
            main
        }
    }
    pub fn choose(&mut self, main: usize, other: usize, sessions: &[SessionRow]) -> bool {
        if main == other || !sessions.iter().any(|s| s.id == other) {
            return false;
        }
        self.other = Some(other);
        self.focus = Pane::Other;
        true
    }
    pub fn select_tab(&mut self, old_main: Option<usize>, new_main: usize) {
        if self.other == Some(new_main) {
            self.other = old_main.filter(|id| *id != new_main);
        }
        self.focus = Pane::Main;
    }
    pub fn close_session(&mut self, main: Option<usize>, closed: usize) {
        if main == Some(closed) || self.other == Some(closed) {
            self.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(id: usize, status: Status) -> SessionRow {
        SessionRow {
            id,
            title: format!("tab {id}"),
            local: false,
            protocol:shellmate_core::Protocol::Ssh,
            status,
            attention: false,
        }
    }
    #[test]
    fn targets_skip_disconnected_without_losing_checkmarks_and_keep_tab_order() {
        let mut sessions = vec![
            row(3, Status::Connected),
            row(1, Status::Reconnecting),
            row(2, Status::Connected),
            row(3, Status::Connected),
        ];
        let state = Broadcast {
            enabled: true,
            checked: [1, 2, 3].into(),
        };
        assert_eq!(state.targets(&sessions), [3, 2]);
        assert_eq!(state.chip(&sessions), "广播 → 2 个会话");
        sessions[1].status = Status::Connected;
        assert_eq!(state.targets(&sessions), [3, 1, 2]);
        assert!(state.checked.contains(&1));
    }
    #[test]
    fn enabling_defaults_only_to_a_connected_current_session() {
        let sessions = vec![row(1, Status::Connected), row(2, Status::Offline)];
        let mut state = Broadcast::default();
        state.enable(true, Some(2), &sessions);
        assert!(state.checked.is_empty());
        state.enable(true, Some(1), &sessions);
        assert_eq!(state.targets(&sessions), [1]);
        state.toggle(2, &sessions);
        assert!(!state.checked.contains(&2));
        state.enable(false, Some(1), &sessions);
        assert_eq!(state.checked, [1].into());
    }
    #[test]
    fn fanout_is_once_per_target_and_risk_is_confirmed_first() {
        let sessions = vec![
            row(2, Status::Connected),
            row(1, Status::Connected),
            row(2, Status::Connected),
        ];
        let state = Broadcast {
            enabled: true,
            checked: [1, 2].into(),
        };
        assert_eq!(
            broadcast_plan(&state, &sessions, "uptime"),
            SendPlan::Direct(vec![2, 1])
        );
        assert_eq!(
            broadcast_plan(&state, &sessions, "sudo reboot"),
            SendPlan::Confirm(vec![2, 1])
        );
        assert_eq!(broadcast_plan(&state, &[], "uptime"), SendPlan::Disabled);
        assert_eq!(broadcast_plan(&state, &sessions, "  "), SendPlan::Disabled);
        assert_eq!(Broadcast::default().chip(&sessions), "广播 → 0 个会话");
    }
    #[test]
    fn dispatch_calls_each_target_once_even_when_one_fails() {
        let mut calls = vec![];
        let result = fan_out(&[3, 1, 3, 2], |id| {
            calls.push(id);
            if id == 1 { Err("disconnected") } else { Ok(()) }
        });
        assert_eq!(calls, [3, 1, 2]);
        assert_eq!(result, [(3, Ok(())), (1, Err("disconnected")), (2, Ok(()))]);
    }

    #[test]
    fn suggestions_merge_sources_exclude_exact_matches_and_limit_five() {
        let rows = suggestions(
            "DOCKER",
            ["docker", "Docker ps", "docker logs", "docker ps", "other"],
            ["Docker ps", "docker images", "docker inspect", "docker info"],
        );
        assert_eq!(
            rows,
            [
                "Docker ps",
                "docker logs",
                "docker ps",
                "docker images",
                "docker inspect"
            ]
        );
        assert!(suggestions("", ["pwd"], []).is_empty());
        assert_eq!(suggestions("UP", ["UP"], ["uptime"]), ["uptime"]);
    }
    #[test]
    fn suggestion_keys_follow_prototype_including_unselected_enter() {
        let items = vec!["one".into(), "two".into()];
        let mut s = SuggestionSelection::default();
        assert_eq!(s.key(SuggestKey::Enter, &items), SuggestAction::Send);
        assert_eq!(
            s.key(SuggestKey::Tab, &items),
            SuggestAction::Accept("one".into())
        );
        s.key(SuggestKey::Down, &items);
        s.key(SuggestKey::Down, &items);
        s.key(SuggestKey::Down, &items);
        assert_eq!(s.selected, Some(1));
        assert_eq!(
            s.key(SuggestKey::Enter, &items),
            SuggestAction::Accept("two".into())
        );
        s.key(SuggestKey::Down, &items);
        s.key(SuggestKey::Up, &items);
        assert_eq!(s.selected, None);
        assert_eq!(s.key(SuggestKey::Tab, &[]), SuggestAction::None);
    }
    #[test]
    fn split_lifecycle_and_hidden_view_preserve_focus_without_duplicate_panes() {
        let rows = vec![
            row(1, Status::Connected),
            row(2, Status::Connected),
            row(3, Status::Connected),
        ];
        let mut s = Split::default();
        assert!(s.toggle(1, &rows, &[1, 3, 2]));
        assert_eq!(s.other, Some(3));
        s.focus = Pane::Other;
        assert_eq!(s.focused(1, true), 3);
        assert_eq!(s.focused(1, false), 1);
        assert_eq!(s.focused(1, true), 3);
        s.flip();
        assert_eq!(s.axis, Axis::Vertical);
        assert!(!s.choose(1, 1, &rows));
        assert!(s.choose(1, 2, &rows));
        s.select_tab(Some(1), 2);
        assert_eq!(s.other, Some(1));
        assert_eq!(s.focus, Pane::Main);
        s.close_session(Some(2), 3);
        assert_eq!(s.other, Some(1));
        s.close_session(Some(2), 1);
        assert_eq!(s.other, None);
        s.toggle(2, &rows, &[1]);
        s.close_session(Some(2), 2);
        assert_eq!(s.other, None);
    }
    #[test]
    fn split_requires_another_eligible_session_and_toggle_closes() {
        let mut s = Split::default();
        let mut rows = vec![row(1, Status::Connected), row(2, Status::Failed)];
        assert!(!s.toggle(1, &rows, &[2]));
        rows[1].local = true;
        assert!(s.toggle(1, &rows, &[2]));
        assert!(s.toggle(1, &rows, &[]));
        assert_eq!(s.other, None);
    }
    #[test]
    fn session_menu_subtitles() {
        for (status, text) in [
            (Status::Connected, "SSH"),
            (Status::Reconnecting, "重连中"),
            (Status::Failed, "连接失败"),
            (Status::Offline, "离线"),
        ] {
            assert_eq!(row(1, status).subtitle(), text);
        }
        let mut local = row(1, Status::Connected);
        local.local = true;
        assert_eq!(local.subtitle(), "本机");
    }
}

#[derive(Clone)]
pub struct Target {
    pub id: usize,
    pub title: String,
    pub production: bool,
}
#[derive(Clone)]
pub struct SendRequest {
    pub targets: Vec<Target>,
    pub broadcast: bool,
    /// A successful send of the broadcast input clears that input; quick commands do not.
    pub clear_input: bool,
}
