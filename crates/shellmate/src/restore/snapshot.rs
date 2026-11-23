//! The saved windows and tabs: the record kept in the settings table under [`KEY`], its JSON form
//! and how the live windows become one.
//!
//! The record never holds screen contents, splits, AI conversations, drafts, the temporary font
//! zoom or a credential: for a remote tab only which host it was (or `user@host:port` of a quick
//! connection) and the directory its shell last reported.
use crate::productivity::model::quick_connect;
use crate::workspace::Panel;
use serde_json::{Map, Value, json};
use shellmate_core::{Host, Protocol};

/// Where the record lives (settings table).
pub(crate) const KEY: &str = "session_restore";
/// The version of the record this build writes and reads.
pub(crate) const VERSION: u64 = 1;
/// A record with more windows, or more tabs in a window, is damaged rather than a session.
const MAX_WINDOWS: usize = 16;
const MAX_TABS: usize = 64;
/// Longest directory, host name or quick-connect text taken from a record.
const MAX_TEXT: usize = 4096;

/// A window's position and size in the coordinates gpui reports them in: points, relative to the
/// display the window is on, y counted from the top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A window's frame and the display it was on (the display's stable UUID).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Located {
    pub frame: Frame,
    pub screen: Option<String>,
}

/// What a tab was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// A local shell, started in `cwd` (the directory of its foreground process or shell).
    Local { cwd: Option<String> },
    /// A host of the host library. The name is only for the message about a host that is gone.
    Host { id: String, name: String },
    /// A connection made from the quick-connect box, written as `user@host:port`.
    Quick { spec: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tab {
    pub target: Target,
    /// The directory the remote shell reported (OSC 7), if it ever did.
    pub dir: Option<String>,
    pub panel: Panel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub place: Option<Located>,
    pub sidebar: bool,
    /// Index into `tabs` of the current tab.
    pub current: usize,
    pub tabs: Vec<Tab>,
}

/// The windows of the last run, the most recently used first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub windows: Vec<Window>,
}

/// Why a record cannot be used.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// Not JSON, or not shaped like a record.
    Corrupt(String),
    /// Written by another version of the format (a newer build, most likely).
    Version(u64),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt(why) => write!(f, "unreadable ({why})"),
            Self::Version(v) => write!(f, "format version {v} (this build reads {VERSION})"),
        }
    }
}

fn panel_name(panel: Panel) -> &'static str {
    match panel {
        Panel::Terminal => "terminal",
        Panel::Files => "files",
        Panel::Forward => "forward",
        Panel::Monitor => "monitor",
    }
}

/// Unknown names show the terminal.
fn panel_from(name: &str) -> Panel {
    match name {
        "files" => Panel::Files,
        "forward" => Panel::Forward,
        "monitor" => Panel::Monitor,
        _ => Panel::Terminal,
    }
}

impl Snapshot {
    pub(crate) fn tab_count(&self) -> usize {
        self.windows.iter().map(|w| w.tabs.len()).sum()
    }

    pub(crate) fn to_value(&self) -> Value {
        let windows: Vec<Value> = self.windows.iter().map(window_json).collect();
        json!({ "v": VERSION, "windows": windows })
    }

    /// The compact form kept in the store.
    pub(crate) fn to_json(&self) -> String {
        self.to_value().to_string()
    }

    /// The form of the debug copy (`SHELLMATE_DEBUG_SNAPSHOT_TO`), also a good snapshot to write by hand.
    #[cfg(debug_assertions)]
    pub(crate) fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(&self.to_value()).unwrap_or_default()
    }

    /// Reads a record. What cannot be understood in it is left out: a tab of an unknown kind, a
    /// window without tabs; the record as a whole is refused only when it is not a record at all
    /// or has a version this build does not know.
    pub(crate) fn from_json(text: &str) -> Result<Self, Error> {
        let root: Value = serde_json::from_str(text).map_err(|e| Error::Corrupt(e.to_string()))?;
        let version = root
            .get("v")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Corrupt("no version".into()))?;
        if version != VERSION {
            return Err(Error::Version(version));
        }
        let windows = root
            .get("windows")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Corrupt("no window list".into()))?;
        let windows = windows
            .iter()
            .take(MAX_WINDOWS)
            .filter_map(window_from)
            .collect();
        Ok(Self { windows })
    }
}

fn frame_json(place: &Located) -> (Value, Option<Value>) {
    let Frame { x, y, w, h } = place.frame;
    (
        json!({ "x": x, "y": y, "w": w, "h": h }),
        place.screen.as_ref().map(|s| json!(s)),
    )
}

fn window_json(window: &Window) -> Value {
    let mut out = Map::new();
    if let Some(place) = &window.place {
        let (frame, screen) = frame_json(place);
        out.insert("frame".into(), frame);
        if let Some(screen) = screen {
            out.insert("screen".into(), screen);
        }
    }
    out.insert("sidebar".into(), json!(window.sidebar));
    out.insert("current".into(), json!(window.current));
    out.insert(
        "tabs".into(),
        Value::Array(window.tabs.iter().map(tab_json).collect()),
    );
    Value::Object(out)
}

fn tab_json(tab: &Tab) -> Value {
    let mut out = Map::new();
    match &tab.target {
        Target::Local { cwd } => {
            out.insert("kind".into(), json!("local"));
            if let Some(cwd) = cwd {
                out.insert("cwd".into(), json!(cwd));
            }
        }
        Target::Host { id, name } => {
            out.insert("kind".into(), json!("host"));
            out.insert("id".into(), json!(id));
            out.insert("name".into(), json!(name));
        }
        Target::Quick { spec } => {
            out.insert("kind".into(), json!("quick"));
            out.insert("spec".into(), json!(spec));
        }
    }
    if let Some(dir) = &tab.dir {
        out.insert("dir".into(), json!(dir));
    }
    out.insert("panel".into(), json!(panel_name(tab.panel)));
    Value::Object(out)
}

/// A non-empty string field of reasonable length.
fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)?
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= MAX_TEXT)
        .map(str::to_owned)
}

fn number(value: &Value, key: &str) -> Option<i32> {
    let n = value.get(key)?.as_f64()?;
    (n.is_finite() && n.abs() < 1.0e6).then_some(n.round() as i32)
}

fn place_from(window: &Value) -> Option<Located> {
    let frame = window.get("frame")?;
    let frame = Frame {
        x: number(frame, "x")?,
        y: number(frame, "y")?,
        w: number(frame, "w")?,
        h: number(frame, "h")?,
    };
    (frame.w > 0 && frame.h > 0).then(|| Located {
        frame,
        screen: text(window, "screen"),
    })
}

fn tab_from(value: &Value) -> Option<Tab> {
    let target = match value.get("kind")?.as_str()? {
        "local" => Target::Local {
            cwd: text(value, "cwd"),
        },
        "host" => {
            let id = text(value, "id")?;
            let name = text(value, "name").unwrap_or_else(|| id.clone());
            Target::Host { id, name }
        }
        "quick" => Target::Quick {
            spec: text(value, "spec")?,
        },
        _ => return None,
    };
    Some(Tab {
        target,
        dir: text(value, "dir"),
        panel: value
            .get("panel")
            .and_then(Value::as_str)
            .map_or(Panel::Terminal, panel_from),
    })
}

fn window_from(value: &Value) -> Option<Window> {
    let raw = value.get("tabs")?.as_array()?;
    let parsed: Vec<Option<Tab>> = raw.iter().take(MAX_TABS).map(tab_from).collect();
    let keep: Vec<bool> = parsed.iter().map(Option::is_some).collect();
    let tabs: Vec<Tab> = parsed.into_iter().flatten().collect();
    if tabs.is_empty() {
        return None;
    }
    let current = value
        .get("current")
        .and_then(Value::as_u64)
        .map_or(0, |n| n as usize);
    Some(Window {
        place: place_from(value),
        sidebar: value
            .get("sidebar")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        current: current_after(current, &keep).min(tabs.len() - 1),
        tabs,
    })
}

/// Where the current tab ends up once the tabs flagged `false` in `keep` are left out: its own new
/// place, or when it is gone the tab before it (the first one if there is none).
pub(crate) fn current_after(current: usize, keep: &[bool]) -> usize {
    let kept_before = |upto: usize| keep[..upto.min(keep.len())].iter().filter(|k| **k).count();
    if keep.get(current).copied().unwrap_or(false) {
        kept_before(current)
    } else {
        kept_before(current).saturating_sub(1)
    }
}

// ---- from the windows on screen ------------------------------------------------------------

/// What a tab is now.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    Local {
        cwd: Option<String>,
    },
    /// `saved`: the host is in the host library; otherwise it came from the quick-connect box.
    Remote {
        host: Box<Host>,
        saved: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct TabState {
    pub source: Source,
    pub panel: Panel,
    /// The directory the remote shell reported (OSC 7) in the connection it has now.
    pub dir: Option<String>,
    /// The directory the tab was restored with, as long as its `cd` line has not been typed into a
    /// connected shell yet (the first connection may fail, or the tab may not have been shown).
    pub restore_dir: Option<String>,
}

/// What a window is now.
#[derive(Clone, Debug)]
pub(crate) struct WindowState {
    pub place: Option<Located>,
    pub sidebar: bool,
    pub active: Option<usize>,
    pub tabs: Vec<TabState>,
}

/// `user@host:port` for a connection that came from the quick-connect box, if it can be written
/// that way and read back by [`quick_connect`] as the same connection.
pub(crate) fn quick_spec(host: &Host) -> Option<String> {
    if host.protocol != Protocol::Ssh || !host.jump.is_empty() {
        return None;
    }
    let user = host.user.as_deref()?;
    let addr = if host.addr.contains(':') {
        format!("[{}]", host.addr)
    } else {
        host.addr.clone()
    };
    let spec = format!("{user}@{addr}:{}", host.port);
    let back = quick_connect(&spec)?;
    (back.addr == host.addr
        && back.port == host.port
        && back.user == host.user
        && back.auth == host.auth)
        .then_some(spec)
}

/// The record of the windows given, most recently used first. A tab that cannot be written down
/// (a connection that is neither saved nor a quick one) is left out; so is a window with no tab left.
pub(crate) fn build(windows: &[WindowState]) -> Snapshot {
    let windows = windows
        .iter()
        .filter_map(|window| {
            let converted: Vec<Option<Tab>> = window.tabs.iter().map(tab_of).collect();
            let keep: Vec<bool> = converted.iter().map(Option::is_some).collect();
            let tabs: Vec<Tab> = converted.into_iter().flatten().collect();
            if tabs.is_empty() {
                return None;
            }
            let current = window
                .active
                .map_or(0, |active| current_after(active, &keep));
            Some(Window {
                place: window.place.clone(),
                sidebar: window.sidebar,
                current: current.min(tabs.len() - 1),
                tabs,
            })
        })
        .collect();
    Snapshot { windows }
}

/// The directory a remote tab is saved with: what its shell reported, else the one it was restored
/// with and has not gone to yet, else none.
pub(crate) fn remote_dir(reported: Option<&str>, restore: Option<&str>) -> Option<String> {
    [reported, restore]
        .into_iter()
        .flatten()
        .find(|dir| !dir.is_empty())
        .map(str::to_owned)
}

fn tab_of(tab: &TabState) -> Option<Tab> {
    let target = match &tab.source {
        Source::Local { cwd } => Target::Local { cwd: cwd.clone() },
        Source::Remote { host, saved: true } => Target::Host {
            id: host.id.to_string(),
            name: host.name.clone(),
        },
        Source::Remote { host, saved: false } => Target::Quick {
            spec: quick_spec(host)?,
        },
    };
    let dir = match &target {
        Target::Local { .. } => None,
        _ => remote_dir(tab.dir.as_deref(), tab.restore_dir.as_deref()),
    };
    Some(Tab {
        target,
        dir,
        panel: tab.panel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_core::AuthMethod;

    fn local(cwd: &str) -> Tab {
        Tab {
            target: Target::Local {
                cwd: Some(cwd.into()),
            },
            dir: None,
            panel: Panel::Terminal,
        }
    }

    fn host_tab(id: &str, name: &str, dir: Option<&str>, panel: Panel) -> Tab {
        Tab {
            target: Target::Host {
                id: id.into(),
                name: name.into(),
            },
            dir: dir.map(Into::into),
            panel,
        }
    }

    fn sample() -> Snapshot {
        Snapshot {
            windows: vec![
                Window {
                    place: Some(Located {
                        frame: Frame {
                            x: 120,
                            y: 80,
                            w: 1440,
                            h: 900,
                        },
                        screen: Some("37D8832A-2D66-02CA-B9F7-8F30A301B230".into()),
                    }),
                    sidebar: true,
                    current: 2,
                    tabs: vec![
                        local("/tmp"),
                        host_tab("9f2c", "生产 web-1", Some("/srv/app"), Panel::Files),
                        Tab {
                            target: Target::Quick {
                                spec: "tester@127.0.0.1:2222".into(),
                            },
                            dir: None,
                            panel: Panel::Terminal,
                        },
                    ],
                },
                Window {
                    place: None,
                    sidebar: false,
                    current: 0,
                    tabs: vec![local("/Users/me")],
                },
            ],
        }
    }

    #[test]
    fn a_snapshot_survives_a_round_trip() {
        let snapshot = sample();
        assert_eq!(
            Snapshot::from_json(&snapshot.to_json()),
            Ok(snapshot.clone())
        );
        assert_eq!(snapshot.tab_count(), 4);
        assert_eq!(snapshot.to_value()["v"], 1);
        assert_eq!(
            Snapshot::from_json(&Snapshot::default().to_json()),
            Ok(Snapshot::default())
        );
    }

    #[test]
    fn the_record_has_the_documented_shape() {
        let value = sample().to_value();
        assert_eq!(
            value["windows"][0]["frame"],
            json!({ "x": 120, "y": 80, "w": 1440, "h": 900 })
        );
        assert_eq!(
            value["windows"][0]["screen"],
            "37D8832A-2D66-02CA-B9F7-8F30A301B230"
        );
        assert_eq!(
            value["windows"][0]["tabs"][0],
            json!({ "kind": "local", "cwd": "/tmp", "panel": "terminal" })
        );
        assert_eq!(
            value["windows"][0]["tabs"][1],
            json!({ "kind": "host", "id": "9f2c", "name": "生产 web-1", "dir": "/srv/app", "panel": "files" })
        );
        assert_eq!(
            value["windows"][0]["tabs"][2],
            json!({ "kind": "quick", "spec": "tester@127.0.0.1:2222", "panel": "terminal" })
        );
        assert!(
            value["windows"][1].get("frame").is_none(),
            "a window with no known frame saves none"
        );
    }

    #[test]
    fn unreadable_or_foreign_records_are_refused() {
        for bad in [
            "",
            "not json",
            "[]",
            "null",
            "{}",
            r#"{"v":"1","windows":[]}"#,
            r#"{"v":1}"#,
            r#"{"v":1,"windows":{}}"#,
        ] {
            assert!(
                matches!(Snapshot::from_json(bad), Err(Error::Corrupt(_))),
                "{bad:?}"
            );
        }
        for version in [0, 2, 99] {
            let record = format!(r#"{{"v":{version},"windows":[]}}"#);
            assert_eq!(Snapshot::from_json(&record), Err(Error::Version(version)));
        }
    }

    #[test]
    fn what_cannot_be_understood_is_left_out_and_the_rest_kept() {
        let record = json!({
            "v": 1,
            "future": true,
            "windows": [
                { "tabs": [ { "kind": "hologram" }, { "kind": "local", "cwd": "/a", "extra": 1 }, { "kind": "host" }, { "kind": "local", "cwd": "/b" } ],
                  "current": 3, "sidebar": false, "frame": { "x": 1, "y": 2, "w": 1000, "h": 700 } },
                { "tabs": [ { "kind": "hologram" } ] },
                { "tabs": [] },
                { "no": "tabs" },
                "nonsense",
            ]
        });
        let snapshot = Snapshot::from_json(&record.to_string()).unwrap();
        assert_eq!(snapshot.windows.len(), 1);
        let window = &snapshot.windows[0];
        assert_eq!(window.tabs, vec![local("/a"), local("/b")]);
        assert_eq!(
            window.current, 1,
            "the current tab is the same tab as before the others were dropped"
        );
        assert!(!window.sidebar);
        assert_eq!(
            window.place,
            Some(Located {
                frame: Frame {
                    x: 1,
                    y: 2,
                    w: 1000,
                    h: 700
                },
                screen: None
            })
        );
    }

    #[test]
    fn missing_or_silly_fields_get_safe_values() {
        let record = json!({ "v": 1, "windows": [ { "tabs": [ { "kind": "local" }, { "kind": "host", "id": "h1", "panel": "gibberish" } ],
            "current": 99, "frame": { "x": 1, "y": 2, "w": 0, "h": 700 } } ] });
        let window = &Snapshot::from_json(&record.to_string()).unwrap().windows[0];
        assert_eq!(
            window.current, 1,
            "an index past the end selects the last tab"
        );
        assert!(
            window.sidebar,
            "the sidebar is open unless the record says otherwise"
        );
        assert_eq!(window.place, None, "a frame without a size is no frame");
        assert_eq!(window.tabs[0].target, Target::Local { cwd: None });
        assert_eq!(
            window.tabs[1].target,
            Target::Host {
                id: "h1".into(),
                name: "h1".into()
            }
        );
        assert_eq!(window.tabs[1].panel, Panel::Terminal);
    }

    #[test]
    fn a_damaged_record_cannot_open_hundreds_of_windows_or_tabs() {
        let tab = json!({ "kind": "local" });
        let windows: Vec<Value> = (0..40)
            .map(|_| json!({ "tabs": vec![tab.clone(); 200] }))
            .collect();
        let snapshot =
            Snapshot::from_json(&json!({ "v": 1, "windows": windows }).to_string()).unwrap();
        assert_eq!(snapshot.windows.len(), MAX_WINDOWS);
        assert!(snapshot.windows.iter().all(|w| w.tabs.len() == MAX_TABS));
        let long = "x".repeat(MAX_TEXT + 1);
        let record =
            json!({ "v": 1, "windows": [ { "tabs": [ { "kind": "local", "cwd": long } ] } ] });
        assert_eq!(
            Snapshot::from_json(&record.to_string()).unwrap().windows[0].tabs[0].target,
            Target::Local { cwd: None }
        );
    }

    #[test]
    fn the_current_tab_follows_the_tabs_that_are_left_out() {
        for (current, keep, expected) in [
            (0, vec![true, true, true], 0),
            (2, vec![true, true, true], 2),
            (2, vec![false, true, true], 1),
            (1, vec![true, false, true], 0),
            (0, vec![false, true], 0),
            (2, vec![true, true, false], 1),
            (9, vec![true, false, true], 1),
            (1, vec![false, false], 0),
        ] {
            assert_eq!(
                current_after(current, &keep),
                expected,
                "current {current} of {keep:?}"
            );
        }
    }

    fn state(tabs: Vec<TabState>, active: Option<usize>) -> WindowState {
        WindowState {
            place: None,
            sidebar: true,
            active,
            tabs,
        }
    }

    fn local_state(cwd: &str) -> TabState {
        TabState {
            source: Source::Local {
                cwd: Some(cwd.into()),
            },
            panel: Panel::Terminal,
            dir: None,
            restore_dir: None,
        }
    }

    fn remote_state(host: Host, saved: bool, dir: Option<&str>) -> TabState {
        TabState {
            source: Source::Remote {
                host: Box::new(host),
                saved,
            },
            panel: Panel::Terminal,
            dir: dir.map(Into::into),
            restore_dir: None,
        }
    }

    fn quick_host() -> Host {
        let mut host = quick_connect("tester@127.0.0.1:2222").unwrap();
        host.auth = AuthMethod::Agent;
        host
    }

    #[test]
    fn building_a_snapshot_names_saved_hosts_by_id_and_quick_ones_by_address() {
        let saved = Host::new("web-1", "10.0.0.1");
        let windows = [
            state(
                vec![
                    local_state("/tmp"),
                    remote_state(saved.clone(), true, Some("/srv")),
                    remote_state(quick_host(), false, Some("/var")),
                ],
                Some(2),
            ),
            state(vec![local_state("/Users/me")], Some(0)),
        ];
        let snapshot = build(&windows);
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].current, 2);
        assert_eq!(snapshot.windows[0].tabs[0], local("/tmp"));
        assert_eq!(
            snapshot.windows[0].tabs[1],
            host_tab(saved.id.as_str(), "web-1", Some("/srv"), Panel::Terminal)
        );
        assert_eq!(
            snapshot.windows[0].tabs[2],
            Tab {
                target: Target::Quick {
                    spec: "tester@127.0.0.1:2222".into()
                },
                dir: Some("/var".into()),
                panel: Panel::Terminal
            }
        );
        assert_eq!(snapshot.windows[1].tabs, vec![local("/Users/me")]);
    }

    #[test]
    fn a_local_tab_keeps_its_directory_only_in_its_target() {
        let mut tab = local_state("/tmp");
        tab.dir = Some("/stale".into());
        tab.restore_dir = Some("/stale".into());
        assert_eq!(
            build(&[state(vec![tab], Some(0))]).windows[0].tabs[0].dir,
            None
        );
    }

    /// What a snapshot of one saved-host tab holds as its directory.
    fn saved_dir(reported: Option<&str>, restore: Option<&str>) -> Option<String> {
        let mut tab = remote_state(Host::new("web-1", "10.0.0.1"), true, reported);
        tab.restore_dir = restore.map(Into::into);
        build(&[state(vec![tab], Some(0))]).windows[0].tabs[0]
            .dir
            .clone()
    }

    #[test]
    fn a_remote_directory_is_what_the_shell_reported_then_what_the_tab_was_restored_with_then_nothing()
     {
        for (reported, restore, expected) in [
            (Some("/srv/now"), Some("/etc"), Some("/srv/now")),
            (None, Some("/etc"), Some("/etc")),
            (Some("/srv/now"), None, Some("/srv/now")),
            (None, None, None),
            // An empty report says nothing.
            (Some(""), Some("/etc"), Some("/etc")),
            (Some(""), None, None),
            (None, Some(""), None),
        ] {
            assert_eq!(
                saved_dir(reported, restore).as_deref(),
                expected,
                "reported {reported:?}, restored with {restore:?}"
            );
            assert_eq!(remote_dir(reported, restore).as_deref(), expected);
        }
    }

    #[test]
    fn a_tab_that_has_not_connected_yet_keeps_its_directory_in_every_snapshot_until_it_is_typed() {
        // Restored with /etc; the first connection failed, so nothing was typed and the shell told nothing.
        let mut quick = remote_state(quick_host(), false, None);
        quick.restore_dir = Some("/etc".into());
        for _ in 0..3 {
            let snapshot = build(&[state(vec![quick.clone()], Some(0))]);
            assert_eq!(snapshot.windows[0].tabs[0].dir.as_deref(), Some("/etc"));
        }
        // The line was typed: the owner clears its copy, and the shell has not reported a directory.
        quick.restore_dir = None;
        assert_eq!(
            build(&[state(vec![quick.clone()], Some(0))]).windows[0].tabs[0].dir,
            None
        );
        // A shell that reports one is what is saved from then on.
        quick.dir = Some("/var/log".into());
        assert_eq!(
            build(&[state(vec![quick], Some(0))]).windows[0].tabs[0]
                .dir
                .as_deref(),
            Some("/var/log")
        );
    }

    #[test]
    fn connections_that_cannot_be_written_down_are_left_out() {
        let mut jumped = quick_host();
        jumped.jump = vec![shellmate_core::HostId::from_raw("bastion")];
        let mut password = quick_host();
        password.auth = AuthMethod::Password;
        let mut anonymous = quick_host();
        anonymous.user = None;
        let mut serial = quick_host();
        serial.protocol = Protocol::Telnet;
        let tabs = vec![
            remote_state(jumped, false, None),
            local_state("/a"),
            remote_state(password, false, None),
            remote_state(anonymous, false, None),
            remote_state(serial, false, None),
            local_state("/b"),
        ];
        // The current tab is one of those left out: the tab before it takes its place.
        let snapshot = build(&[state(tabs.clone(), Some(2))]);
        assert_eq!(snapshot.windows[0].tabs, vec![local("/a"), local("/b")]);
        assert_eq!(snapshot.windows[0].current, 0);
        assert_eq!(build(&[state(tabs, Some(5))]).windows[0].current, 1);
        // A window that is left with nothing is not saved at all.
        assert!(
            build(&[state(
                vec![remote_state(quick_host_with_jump(), false, None)],
                Some(0)
            )])
            .windows
            .is_empty()
        );
        assert!(build(&[state(vec![], None)]).windows.is_empty());
    }

    fn quick_host_with_jump() -> Host {
        let mut host = quick_host();
        host.jump = vec![shellmate_core::HostId::from_raw("bastion")];
        host
    }

    #[test]
    fn quick_specs_round_trip_including_ipv6_and_odd_ports() {
        for spec in [
            "root@host:22",
            "deploy@10.0.1.21:2222",
            "ops@[::1]:2200",
            "ops@web.example.test:2",
        ] {
            let host = quick_connect(spec).unwrap();
            assert_eq!(quick_spec(&host).as_deref(), Some(spec));
        }
        // The address of a quick connection comes back as the same connection.
        let host = quick_connect("tester@127.0.0.1:2222").unwrap();
        let again = quick_connect(&quick_spec(&host).unwrap()).unwrap();
        assert_eq!(
            (again.addr, again.port, again.user),
            (host.addr, host.port, host.user)
        );
    }
}
