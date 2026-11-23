//! What a launch does with the record: the saved windows and tabs, the hosts that exist now and
//! the screens that are there now become a list of windows to open and tabs to put in them.
use super::lazy::Wait;
use super::snapshot::{Frame, Located, Snapshot, Target, current_after};
use crate::productivity::model::quick_connect;
use crate::workspace::Panel;
use shellmate_core::{Host, HostBook, HostId, Protocol};

/// The smallest window (`window_min_size` of the workspace window).
pub(crate) const MIN_SIZE: (i32, i32) = (980, 620);
/// The size of a window the record has no frame for.
pub(crate) const DEFAULT_SIZE: (i32, i32) = (1440, 900);
/// How much of a window must be on its screen to count as being there: enough to grab its title bar.
const GRAB: (i32, i32) = (120, 60);

/// A screen that exists now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Screen {
    /// The display's stable UUID.
    pub id: String,
    /// The area windows can use (no menu bar, no Dock), in the same coordinates as a window frame.
    pub area: Frame,
    pub primary: bool,
}

fn overlap(a: Frame, b: Frame) -> Option<(i32, i32)> {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    (w > 0 && h > 0).then_some((w, h))
}

fn centered(w: i32, h: i32, area: Frame) -> Frame {
    Frame {
        x: area.x + ((area.w - w) / 2).max(0),
        y: area.y + ((area.h - h) / 2).max(0),
        w,
        h,
    }
}

/// Where a window opens. Its own place, on its own screen, when enough of it is there; otherwise
/// (the screen was unplugged, the frame is outside what the screen shows now) centered on the
/// primary screen. A frame that names no screen is on the primary one. Never smaller than the
/// smallest window.
pub(crate) fn place(saved: Option<&Located>, screens: &[Screen]) -> Located {
    let (w, h) = saved.map_or(DEFAULT_SIZE, |p| {
        (p.frame.w.max(MIN_SIZE.0), p.frame.h.max(MIN_SIZE.1))
    });
    let primary = screens.iter().find(|s| s.primary).or(screens.first());
    let home = saved.and_then(|p| {
        let screen = match &p.screen {
            Some(id) => screens.iter().find(|s| &s.id == id)?,
            None => primary?,
        };
        Some((p, screen))
    });
    if let Some((saved, screen)) = home {
        let frame = Frame {
            w,
            h,
            ..saved.frame
        };
        if overlap(frame, screen.area).is_some_and(|(ow, oh)| ow >= GRAB.0 && oh >= GRAB.1) {
            return Located {
                frame,
                screen: Some(screen.id.clone()),
            };
        }
    }
    match primary {
        Some(screen) => Located {
            frame: centered(w, h, screen.area),
            screen: Some(screen.id.clone()),
        },
        None => Located {
            frame: Frame { x: 0, y: 0, w, h },
            screen: None,
        },
    }
}

/// How a tab is opened.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Open {
    /// A new shell in `cwd`; `None` is the home directory (the directory is gone, or was never known).
    Local { cwd: Option<String> },
    /// A host of the library, or (`quick`) one made from `user@host:port`.
    Remote {
        host: Box<Host>,
        quick: bool,
        dir: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TabPlan {
    pub open: Open,
    /// The panel to show; the terminal where the panel does not exist for the tab any more.
    pub panel: Panel,
    /// `None`: opens right away (a local shell). Otherwise what brings the connection up.
    pub wait: Option<Wait>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WindowPlan {
    pub place: Located,
    pub sidebar: bool,
    /// Index into `tabs`.
    pub current: usize,
    pub tabs: Vec<TabPlan>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    pub windows: Vec<WindowPlan>,
    /// Names of the saved hosts that are gone, each once, in the order they were met.
    pub missing_hosts: Vec<String>,
    /// One line for each tab that was left out or changed, for the log.
    pub notes: Vec<String>,
}

impl Plan {
    pub(crate) fn tab_count(&self) -> usize {
        self.windows.iter().map(|w| w.tabs.len()).sum()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tab_count() == 0
    }

    /// `2 windows, 4 tabs: 2 local, 2 remote (1 connects at once, 1 waits), 0 serial`
    pub(crate) fn summary(&self) -> String {
        let (mut local, mut remote, mut serial, mut at_once) = (0, 0, 0, 0);
        for window in &self.windows {
            for (ix, tab) in window.tabs.iter().enumerate() {
                match (&tab.open, tab.wait) {
                    (Open::Local { .. }, _) => local += 1,
                    (_, Some(Wait::Enter)) => serial += 1,
                    _ => {
                        remote += 1;
                        at_once += usize::from(ix == window.current);
                    }
                }
            }
        }
        format!(
            "{} windows, {} tabs: {local} local, {remote} remote ({at_once} connects at once, {} waits), {serial} serial",
            self.windows.len(),
            self.tab_count(),
            remote - at_once,
        )
    }
}

fn panel_for(panel: Panel, protocol: Option<Protocol>) -> Panel {
    if panel.supported(protocol) {
        panel
    } else {
        Panel::Terminal
    }
}

/// Windows and tabs to open for `snapshot`. `dir_exists` says whether a local directory is there.
pub(crate) fn plan(
    snapshot: &Snapshot,
    book: &HostBook,
    screens: &[Screen],
    dir_exists: &dyn Fn(&str) -> bool,
) -> Plan {
    let mut out = Plan {
        windows: Vec::new(),
        missing_hosts: Vec::new(),
        notes: Vec::new(),
    };
    let mut missing_ids: Vec<String> = Vec::new();
    for (wi, window) in snapshot.windows.iter().enumerate() {
        let mut keep = Vec::with_capacity(window.tabs.len());
        let mut tabs = Vec::with_capacity(window.tabs.len());
        for (ti, tab) in window.tabs.iter().enumerate() {
            let at = format!("window {} tab {}", wi + 1, ti + 1);
            let planned = match &tab.target {
                Target::Local { cwd } => {
                    let cwd = match cwd {
                        Some(dir) if dir_exists(dir) => Some(dir.clone()),
                        Some(dir) => {
                            out.notes.push(format!(
                                "{at}: directory {dir} is gone, opening in the home directory"
                            ));
                            None
                        }
                        None => None,
                    };
                    Some(TabPlan {
                        open: Open::Local { cwd },
                        panel: panel_for(tab.panel, None),
                        wait: None,
                    })
                }
                Target::Host { id, name } => match book.host(&HostId::from_raw(id.as_str())) {
                    Some(host) => Some(remote(host.clone(), false, tab.dir.clone(), tab.panel)),
                    None => {
                        out.notes
                            .push(format!("{at}: host {name:?} is deleted, left out"));
                        if !missing_ids.contains(id) {
                            missing_ids.push(id.clone());
                            out.missing_hosts.push(name.clone());
                        }
                        None
                    }
                },
                Target::Quick { spec } => match quick_connect(spec) {
                    Some(host) => Some(remote(host, true, tab.dir.clone(), tab.panel)),
                    None => {
                        out.notes.push(format!(
                            "{at}: {spec:?} is not a connection address, left out"
                        ));
                        None
                    }
                },
            };
            keep.push(planned.is_some());
            tabs.extend(planned);
        }
        if tabs.is_empty() {
            out.notes
                .push(format!("window {}: nothing left to open, left out", wi + 1));
            continue;
        }
        let current = current_after(window.current, &keep).min(tabs.len() - 1);
        out.windows.push(WindowPlan {
            place: place(window.place.as_ref(), screens),
            sidebar: window.sidebar,
            current,
            tabs,
        });
    }
    out
}

fn remote(host: Host, quick: bool, dir: Option<String>, panel: Panel) -> TabPlan {
    let protocol = host.protocol;
    TabPlan {
        panel: panel_for(panel, Some(protocol)),
        wait: Some(Wait::of(protocol)),
        // A serial port has no shell to tell a directory.
        open: Open::Remote {
            dir: dir.filter(|_| protocol != Protocol::Serial),
            host: Box::new(host),
            quick,
        },
    }
}

/// The one message about the hosts that were left out: `2 个主机已删除，未恢复：web-1、db`.
pub(crate) fn missing_message(names: &[String]) -> Option<String> {
    const SHOWN: usize = 5;
    if names.is_empty() {
        return None;
    }
    let shown = names
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join("、");
    let more = if names.len() > SHOWN { "等" } else { "" };
    Some(format!(
        "{} 个主机已删除，未恢复：{shown}{more}",
        names.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restore::snapshot::{Tab, Window};

    fn host(name: &str, protocol: Protocol) -> Host {
        let mut host = Host::new(name, format!("{name}.example.test"));
        host.protocol = protocol;
        host
    }

    fn book(hosts: &[&Host]) -> HostBook {
        HostBook::new(vec![], hosts.iter().map(|h| (*h).clone()).collect())
    }

    fn local(cwd: Option<&str>) -> Tab {
        Tab {
            target: Target::Local {
                cwd: cwd.map(Into::into),
            },
            dir: None,
            panel: Panel::Terminal,
        }
    }

    fn saved(host: &Host, dir: Option<&str>) -> Tab {
        Tab {
            target: Target::Host {
                id: host.id.to_string(),
                name: host.name.clone(),
            },
            dir: dir.map(Into::into),
            panel: Panel::Terminal,
        }
    }

    fn quick(spec: &str) -> Tab {
        Tab {
            target: Target::Quick { spec: spec.into() },
            dir: None,
            panel: Panel::Terminal,
        }
    }

    fn window(current: usize, tabs: Vec<Tab>) -> Window {
        Window {
            place: None,
            sidebar: true,
            current,
            tabs,
        }
    }

    fn screen(id: &str, w: i32, h: i32, primary: bool) -> Screen {
        Screen {
            id: id.into(),
            area: Frame {
                x: 0,
                y: 25,
                w,
                h: h - 25,
            },
            primary,
        }
    }

    fn here(_: &str) -> bool {
        true
    }

    fn only(windows: Vec<Window>) -> Snapshot {
        Snapshot { windows }
    }

    #[test]
    fn local_tabs_open_where_they_were_and_in_the_home_directory_when_that_is_gone() {
        let snapshot = only(vec![window(
            0,
            vec![local(Some("/work/a")), local(Some("/gone")), local(None)],
        )]);
        let plan = plan(&snapshot, &book(&[]), &[], &|dir| dir == "/work/a");
        let cwds: Vec<_> = plan.windows[0]
            .tabs
            .iter()
            .map(|t| t.open.clone())
            .collect();
        assert_eq!(
            cwds,
            vec![
                Open::Local {
                    cwd: Some("/work/a".into())
                },
                Open::Local { cwd: None },
                Open::Local { cwd: None },
            ]
        );
        assert!(
            plan.windows[0].tabs.iter().all(|t| t.wait.is_none()),
            "a local shell opens at once"
        );
        assert_eq!(plan.notes.len(), 1);
        assert!(plan.notes[0].contains("/gone") && plan.notes[0].contains("window 1 tab 2"));
        assert!(plan.missing_hosts.is_empty());
    }

    #[test]
    fn a_deleted_host_is_left_out_and_named_once_however_many_tabs_it_had() {
        let (kept, gone, also_gone) = (
            host("kept", Protocol::Ssh),
            host("web-1", Protocol::Ssh),
            host("db", Protocol::Ssh),
        );
        let snapshot = only(vec![
            window(
                1,
                vec![
                    saved(&gone, None),
                    saved(&kept, Some("/srv")),
                    saved(&gone, None),
                    local(None),
                ],
            ),
            window(0, vec![saved(&also_gone, None), saved(&gone, None)]),
            window(0, vec![local(Some("/x"))]),
        ]);
        let plan = plan(&snapshot, &book(&[&kept]), &[], &here);
        assert_eq!(plan.missing_hosts, ["web-1", "db"]);
        assert_eq!(
            plan.windows.len(),
            2,
            "the window whose only tabs were deleted hosts is not opened"
        );
        assert_eq!(plan.windows[0].tabs.len(), 2);
        assert_eq!(
            plan.windows[0].current, 0,
            "the current tab is the kept host, now first"
        );
        assert!(
            matches!(&plan.windows[0].tabs[0].open, Open::Remote { host, quick: false, dir } if host.id == kept.id && dir.as_deref() == Some("/srv"))
        );
        assert_eq!(plan.windows[1].tabs.len(), 1);
        assert_eq!(
            missing_message(&plan.missing_hosts).as_deref(),
            Some("2 个主机已删除，未恢复：web-1、db")
        );
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("window 2: nothing left"))
        );
    }

    #[test]
    fn when_the_current_tab_is_gone_the_one_before_it_is_current() {
        let (a, gone) = (host("a", Protocol::Ssh), host("gone", Protocol::Ssh));
        let snapshot = only(vec![window(
            2,
            vec![
                local(None),
                saved(&a, None),
                saved(&gone, None),
                local(Some("/z")),
            ],
        )]);
        let plan = plan(&snapshot, &book(&[&a]), &[], &here);
        assert_eq!(plan.windows[0].current, 1);
        let first_gone = only(vec![window(0, vec![saved(&gone, None), local(None)])]);
        assert_eq!(
            super::plan(&first_gone, &book(&[]), &[], &here).windows[0].current,
            0
        );
    }

    #[test]
    fn a_snapshot_whose_hosts_are_all_gone_has_nothing_to_restore() {
        let gone = host("gone", Protocol::Ssh);
        let plan = plan(
            &only(vec![window(0, vec![saved(&gone, None)])]),
            &book(&[]),
            &[],
            &here,
        );
        assert!(plan.is_empty() && plan.windows.is_empty());
        assert_eq!(plan.missing_hosts, ["gone"]);
        assert!(super::plan(&only(vec![]), &book(&[]), &[], &here).is_empty());
    }

    #[test]
    fn a_quick_connection_is_rebuilt_from_its_address_and_a_broken_one_is_left_out() {
        let snapshot = only(vec![window(
            0,
            vec![
                quick("tester@127.0.0.1:2222"),
                quick("not an address"),
                quick("@x"),
            ],
        )]);
        let plan = plan(&snapshot, &book(&[]), &[], &here);
        assert_eq!(plan.windows[0].tabs.len(), 1);
        let tab = &plan.windows[0].tabs[0];
        let Open::Remote { host, quick, .. } = &tab.open else {
            panic!("remote")
        };
        assert!(*quick);
        assert_eq!(
            (
                host.user.as_deref(),
                host.addr.as_str(),
                host.port,
                host.protocol
            ),
            (Some("tester"), "127.0.0.1", 2222, Protocol::Ssh)
        );
        assert_eq!(tab.wait, Some(Wait::Show));
        assert_eq!(plan.notes.len(), 2);
        assert!(
            plan.missing_hosts.is_empty(),
            "a bad address is not a deleted host"
        );
    }

    #[test]
    fn remote_tabs_wait_to_be_shown_and_a_serial_port_waits_for_enter() {
        let (ssh, mosh, telnet, serial) = (
            host("s", Protocol::Ssh),
            host("m", Protocol::Mosh),
            host("t", Protocol::Telnet),
            host("p", Protocol::Serial),
        );
        let snapshot = only(vec![window(
            3,
            vec![
                saved(&ssh, None),
                saved(&mosh, None),
                saved(&telnet, None),
                saved(&serial, Some("/ignored")),
            ],
        )]);
        let plan = plan(
            &snapshot,
            &book(&[&ssh, &mosh, &telnet, &serial]),
            &[],
            &here,
        );
        let waits: Vec<_> = plan.windows[0].tabs.iter().map(|t| t.wait).collect();
        assert_eq!(
            waits,
            [
                Some(Wait::Show),
                Some(Wait::Show),
                Some(Wait::Show),
                Some(Wait::Enter)
            ]
        );
        let Open::Remote { dir, .. } = &plan.windows[0].tabs[3].open else {
            panic!("remote")
        };
        assert_eq!(*dir, None, "a serial port has no directory to go to");
        assert_eq!(
            plan.windows[0].current, 3,
            "a serial tab can be the current one, and still waits for Enter"
        );
    }

    #[test]
    fn a_panel_the_tab_does_not_have_any_more_shows_the_terminal() {
        let (ssh, telnet) = (host("s", Protocol::Ssh), host("t", Protocol::Telnet));
        let with = |panel, tab: Tab| Tab { panel, ..tab };
        let snapshot = only(vec![window(
            0,
            vec![
                with(Panel::Forward, saved(&ssh, None)),
                with(Panel::Files, saved(&telnet, None)),
                with(Panel::Forward, local(None)),
                with(Panel::Files, local(None)),
            ],
        )]);
        let plan = plan(&snapshot, &book(&[&ssh, &telnet]), &[], &here);
        let panels: Vec<_> = plan.windows[0].tabs.iter().map(|t| t.panel).collect();
        assert_eq!(
            panels,
            [
                Panel::Forward,
                Panel::Terminal,
                Panel::Terminal,
                Panel::Files
            ]
        );
    }

    #[test]
    fn windows_come_back_in_the_order_they_were_saved() {
        let snapshot = only(vec![
            window(0, vec![local(Some("/1"))]),
            window(0, vec![local(Some("/2"))]),
            window(0, vec![local(Some("/3"))]),
        ]);
        let plan = plan(&snapshot, &book(&[]), &[], &here);
        let first: Vec<_> = plan
            .windows
            .iter()
            .map(|w| w.tabs[0].open.clone())
            .collect();
        assert_eq!(
            first,
            [
                Open::Local {
                    cwd: Some("/1".into())
                },
                Open::Local {
                    cwd: Some("/2".into())
                },
                Open::Local {
                    cwd: Some("/3".into())
                }
            ]
        );
    }

    #[test]
    fn the_summary_counts_what_connects_at_once() {
        let (ssh, serial) = (host("s", Protocol::Ssh), host("p", Protocol::Serial));
        let snapshot = only(vec![
            window(
                1,
                vec![
                    local(None),
                    saved(&ssh, None),
                    saved(&ssh, None),
                    saved(&serial, None),
                ],
            ),
            window(0, vec![local(None)]),
        ]);
        let plan = plan(&snapshot, &book(&[&ssh, &serial]), &[], &here);
        assert_eq!(
            plan.summary(),
            "2 windows, 5 tabs: 2 local, 2 remote (1 connects at once, 1 waits), 1 serial"
        );
    }

    fn located(x: i32, y: i32, w: i32, h: i32, screen: Option<&str>) -> Located {
        Located {
            frame: Frame { x, y, w, h },
            screen: screen.map(Into::into),
        }
    }

    #[test]
    fn a_window_that_is_on_its_screen_stays_where_it_was() {
        let screens = [
            screen("main", 1920, 1080, true),
            screen("side", 2560, 1440, false),
        ];
        let saved = located(100, 80, 1200, 800, Some("side"));
        assert_eq!(place(Some(&saved), &screens), saved);
        // A window may hang over the edge as long as enough of it is on the screen.
        let partly = located(1000, 300, 1200, 800, Some("main"));
        assert_eq!(place(Some(&partly), &screens), partly);
    }

    #[test]
    fn a_window_whose_screen_is_gone_is_centered_on_the_primary_screen() {
        let screens = [
            screen("side", 2560, 1440, false),
            screen("main", 1920, 1080, true),
        ];
        let saved = located(100, 80, 1200, 800, Some("unplugged"));
        let placed = place(Some(&saved), &screens);
        assert_eq!(placed.screen.as_deref(), Some("main"));
        assert_eq!(
            placed.frame,
            Frame {
                x: 360,
                y: 25 + (1055 - 800) / 2,
                w: 1200,
                h: 800
            }
        );
    }

    #[test]
    fn a_frame_that_names_no_screen_is_on_the_primary_screen() {
        let screens = [
            screen("side", 2560, 1440, false),
            screen("main", 1920, 1080, true),
        ];
        let placed = place(Some(&located(300, 200, 1200, 800, None)), &screens);
        assert_eq!(
            placed,
            located(300, 200, 1200, 800, Some("main")),
            "kept where it is, and now it names its screen"
        );
        let off = place(Some(&located(9000, 200, 1200, 800, None)), &screens);
        assert_eq!(
            off.frame,
            Frame {
                x: 360,
                y: 25 + (1055 - 800) / 2,
                w: 1200,
                h: 800
            }
        );
    }

    #[test]
    fn a_window_outside_what_its_screen_shows_now_is_brought_back() {
        let screens = [screen("main", 1920, 1080, true)];
        for (x, y) in [
            (5000, 100),
            (-3000, 100),
            (100, 4000),
            (1900, 100),
            (100, 1050),
            (-1100, 100),
        ] {
            let placed = place(Some(&located(x, y, 1200, 800, Some("main"))), &screens);
            assert_eq!(
                placed.frame,
                Frame {
                    x: 360,
                    y: 25 + (1055 - 800) / 2,
                    w: 1200,
                    h: 800
                },
                "at {x},{y}"
            );
        }
    }

    #[test]
    fn a_window_is_never_smaller_than_the_smallest_window() {
        let screens = [screen("main", 1920, 1080, true)];
        let placed = place(Some(&located(50, 60, 300, 200, Some("main"))), &screens);
        assert_eq!(
            placed.frame,
            Frame {
                x: 50,
                y: 60,
                w: MIN_SIZE.0,
                h: MIN_SIZE.1
            }
        );
        let gone = place(Some(&located(50, 60, 10, 10, Some("elsewhere"))), &screens);
        assert_eq!((gone.frame.w, gone.frame.h), MIN_SIZE);
    }

    #[test]
    fn a_window_without_a_frame_opens_at_the_default_size_in_the_middle() {
        let screens = [screen("main", 1920, 1080, true)];
        let placed = place(None, &screens);
        assert_eq!((placed.frame.w, placed.frame.h), DEFAULT_SIZE);
        assert_eq!(placed.frame.x, (1920 - DEFAULT_SIZE.0) / 2);
        assert_eq!(placed.screen.as_deref(), Some("main"));
    }

    #[test]
    fn a_window_bigger_than_the_screen_starts_at_its_corner_and_no_screens_means_the_origin() {
        let small = [screen("main", 1000, 700, true)];
        let placed = place(Some(&located(0, 0, 1440, 900, Some("elsewhere"))), &small);
        assert_eq!((placed.frame.x, placed.frame.y), (0, 25));
        let none = place(Some(&located(500, 500, 1200, 800, Some("x"))), &[]);
        assert_eq!(none, located(0, 0, 1200, 800, None));
    }

    #[test]
    fn windows_of_a_plan_take_their_places_from_the_screens() {
        let screens = [screen("main", 1920, 1080, true)];
        let mut on_screen = window(0, vec![local(None)]);
        on_screen.place = Some(located(40, 60, 1100, 700, Some("main")));
        let mut off_screen = window(0, vec![local(None)]);
        off_screen.place = Some(located(40, 60, 1100, 700, Some("gone")));
        let plan = plan(
            &only(vec![on_screen, off_screen]),
            &book(&[]),
            &screens,
            &here,
        );
        assert_eq!(
            plan.windows[0].place,
            located(40, 60, 1100, 700, Some("main"))
        );
        assert_eq!(plan.windows[1].place.screen.as_deref(), Some("main"));
        assert_ne!(plan.windows[1].place.frame.x, 40);
    }

    #[test]
    fn the_message_about_deleted_hosts_names_a_few() {
        assert_eq!(missing_message(&[]), None);
        assert_eq!(
            missing_message(&["a".into()]).as_deref(),
            Some("1 个主机已删除，未恢复：a")
        );
        let many: Vec<String> = (1..=7).map(|n| format!("h{n}")).collect();
        assert_eq!(
            missing_message(&many).as_deref(),
            Some("7 个主机已删除，未恢复：h1、h2、h3、h4、h5等")
        );
    }
    /// The hand-written snapshot for `SHELLMATE_DEBUG_RESTORE_FROM` (two windows, one quick connection that waits).
    const EXAMPLE: &str = r#"{
  "v": 1,
  "windows": [
    {
      "frame": { "x": 60, "y": 60, "w": 1180, "h": 760 },
      "sidebar": true,
      "current": 0,
      "tabs": [
        { "kind": "local", "cwd": "/tmp", "panel": "terminal" },
        { "kind": "local", "cwd": "/Users/emoji", "panel": "terminal" },
        { "kind": "quick", "spec": "tester@127.0.0.1:2222", "dir": "/etc", "panel": "terminal" }
      ]
    },
    {
      "frame": { "x": 300, "y": 200, "w": 1100, "h": 700 },
      "sidebar": false,
      "current": 0,
      "tabs": [
        { "kind": "local", "cwd": "/tmp", "panel": "terminal" }
      ]
    }
  ]
}
"#;

    #[test]
    fn the_example_snapshot_restores_two_windows_and_the_quick_connection_waits() {
        let snapshot = Snapshot::from_json(EXAMPLE).unwrap();
        let screens = [screen("main", 1920, 1080, true)];
        let plan = plan(&snapshot, &book(&[]), &screens, &here);
        assert_eq!(
            plan.summary(),
            "2 windows, 4 tabs: 3 local, 1 remote (0 connects at once, 1 waits), 0 serial"
        );
        assert!(plan.missing_hosts.is_empty() && plan.notes.is_empty());
        let first = &plan.windows[0];
        assert_eq!(first.place, located(60, 60, 1180, 760, Some("main")));
        assert_eq!(first.current, 0);
        assert!(first.sidebar && !plan.windows[1].sidebar);
        assert_eq!(
            first.tabs[0].open,
            Open::Local {
                cwd: Some("/tmp".into())
            }
        );
        assert_eq!(
            first.tabs[1].open,
            Open::Local {
                cwd: Some("/Users/emoji".into())
            }
        );
        let Open::Remote { host, quick, dir } = &first.tabs[2].open else {
            panic!("remote")
        };
        assert!(*quick);
        assert_eq!(
            (host.user.as_deref(), host.addr.as_str(), host.port),
            (Some("tester"), "127.0.0.1", 2222)
        );
        assert_eq!(dir.as_deref(), Some("/etc"));
        assert_eq!(
            first.tabs[2].wait,
            Some(Wait::Show),
            "it is not the current tab: it connects when it is shown"
        );
        assert_eq!(plan.windows[1].tabs.len(), 1);
        assert_eq!(
            plan.windows[1].place,
            located(300, 200, 1100, 700, Some("main"))
        );
    }
}
