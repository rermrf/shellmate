//! Deterministic prototype fixtures. Compiled only in debug builds, rendered by the real views.
mod data;
pub mod prompt_plain;
pub mod replay;
pub mod ai;
pub mod slash;
pub mod memory;
pub mod mode;
pub mod subagents;
pub mod stress;
pub use data::DATA;
use std::ops::Index;
#[derive(Clone, Copy, Debug)]
pub enum V {
    Null,
    Bool(bool),
    Num(f64),
    Str(&'static str),
    List(&'static [V]),
    Map(&'static [(&'static str, V)]),
}
impl V {
    pub fn s(&self) -> &'static str {
        if let Self::Str(s) = self { s } else { "" }
    }
    pub fn n(&self) -> f64 {
        if let Self::Num(n) = self { *n } else { 0. }
    }
    pub fn b(&self) -> bool {
        matches!(self, Self::Bool(true))
    }
    pub fn a(&self) -> &'static [V] {
        if let Self::List(a) = self { a } else { &[] }
    }
}
impl Index<&str> for V {
    type Output = V;
    fn index(&self, key: &str) -> &V {
        if let Self::Map(m) = self {
            m.iter().find(|(k, _)| *k == key).map_or(&V::Null, |(_, v)| v)
        } else {
            &V::Null
        }
    }
}
impl Index<usize> for V {
    type Output = V;
    fn index(&self, i: usize) -> &V {
        self.a().get(i).unwrap_or(&V::Null)
    }
}
pub const PAGE_SCROLL_SCENES: &[(&str, (u32, u32))] = &[
    ("mon-ov-scrolled", (1000, 640)),
    ("mon-dk-scrolled", (1000, 640)),
    ("mon-svc-scrolled", (980, 640)),
    ("fwd-scrolled", (1000, 640)),
    ("side-scrolled", (1000, 640)),
    ("cmds-scrolled", (1000, 640)),
    ("ai-scrolled", (1000, 640)),
    ("mon-proc-scrolled", (1100, 640)),
    ("files-scrolled", (1440, 640)),
];

pub const SCENES: &[&str] = &[
    "quit-confirm", "shortcuts", "tab-attention", "tab-attention-menu", "tab-list-scroll", "settings-window", "close-window-dialog", "tabmenu-move", "tabmenu-single",
    "update-available", "update-downloading", "update-ready", "update-uptodate",
    "update-error", "update-fallback", "settings-update", "main-update-pill",
    "mon-ov-scrolled", "mon-dk-scrolled", "mon-svc-scrolled", "fwd-scrolled",
    "side-scrolled", "cmds-scrolled", "ai-scrolled", "mon-proc-scrolled", "files-scrolled",
    "ai-agent", "ai-agent-run", "ai-new-confirm", "ai-agent-redline", "ai-agent-full",
    "ai-tasks", "ai-output-large", "ai-input-multiline", "ai-context-popover", "ai-context-amber", "ai-code-copy", "ai-code-copied", "ai-code-select",
    "memory-manager-empty", "memory-manager-edit", "memory-manager-agents", "memory-manager-new",
    "set-ai-memory", "memory-manager", "host-editor-ai-note", "ai-memory-chip",
    "ai-subagent-approval", "ai-tasks-subagents",
    "ai-mode-popover",
    "ai-slash", "ai-slash-filter", "ai-slash-context", "ai-slash-tasks", "set-ai-advanced",
    "ai-compacted", "ai-compacted-open", "ai-compacting", "ai-compacted-tier1",
    "ai", "ai-chat", "ai-markdown", "ai-markdown-wide", "ai-history", "ai-error", "ai-unconfigured", "set-ai", "set-ai-anthropic", "ai-explain",
    "main", "prompt-plain", "host-mosh", "mosh", "serial", "serial-hex", "telnet", "host-serial", "host-telnet",
    "files",
    "fwd",
    "fwd-new",
    "mon-ov",
    "mon-proc",
    "mon-svc",
    "mon-dk",
    "cmds",
    "bc",
    "pal",
    "pal-q",
    "hist",
    "split",
    "rv",
    "rv-log",
    "rv-end",
    "rv-wide",
    "rv-recording",
    "set-cred",
    "set-cred-master", "set-cred-focus", "set-cred-typed", "set-ai-replace",
    "vault-unlock",
    "bundle-export",
    "bundle-preview",
    "set-sync",
    "set-term",
    "set-term-scrolled",
    "set-local",
    "set-conn",
    "set-conn-close-window",
    "host-new", "host-new-focus",
    "rail", "rail-mid", "rail-mid-expand", "drawer-mid", "drawer-swap-mid", "mon-ov-reflow-mid",
    "local",
    "files-local", "files-local-trash",
    "mon-local", "mon-local-proc", "mon-local-disks",
    "qc",
    "tabmenu",
    "reconnect",
    "editor",
    "hostkey-changed",
    "hostkey-new",
    "stress-main", "stress-mon-ov", "stress-mon-proc", "stress-mon-svc", "stress-mon-dk", "stress-files", "stress-fwd",
];
pub fn requested() -> bool {
    std::env::var("SHELLMATE_DEMO").is_ok_and(|s| s.starts_with("proto:") || matches!(s.as_str(), "menu-empty" | "about"))
}
/// Optional point-size suffix, used only by the debug fixture.
pub fn request(value: &str) -> (&str, Option<(u32,u32)>) {
    let (scene,dimensions)=value.split_once('@').map_or((value,None),|(s,d)|(s,Some(d)));
    let size=dimensions.and_then(|d|d.split_once('x')).and_then(|(w,h)|Some((w.parse::<u32>().ok()?,h.parse::<u32>().ok()?)))
        .filter(|(w,h)| (980..=4096).contains(w) && (620..=2160).contains(h));
    (scene,size)
}
pub fn window_size() -> (u32,u32) {
    let value=std::env::var("SHELLMATE_DEMO").unwrap_or_default();
    let (scene,size)=request(value.strip_prefix("proto:").unwrap_or(&value));
    size.unwrap_or_else(|| default_window_size(scene))
}

pub fn default_window_size(scene: &str) -> (u32, u32) {
    if scene == "mon-ov-reflow-mid" { return (1295, 900); }
    PAGE_SCROLL_SCENES.iter().find(|(name, _)| *name == scene).map(|(_, size)| *size)
        .unwrap_or(if scene.starts_with("stress-") { (1776, 901) } else { (1440, 900) })
}

pub fn focus_after_layout(focus: gpui::FocusHandle, window: &mut gpui::Window) {
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
    });
}

/// Wait for layout, the responsive-width update, and conditional page padding.
/// No wall-clock sleeps: offsets derive from the laid-out handle on the third frame.
pub fn scroll_after_layout(
    handle: gpui::ScrollHandle,
    offset: impl FnOnce(&gpui::ScrollHandle) -> f32 + 'static,
    window: &mut gpui::Window,
) {
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, _| {
                let y = offset(&handle).clamp(0., f32::from(handle.max_offset().y));
                handle.set_offset(gpui::point(gpui::px(0.), gpui::px(-y)));
                window.refresh();
            });
            window.refresh();
        });
        window.refresh();
    });
}

/// Fake wall time: 2026-11-12 09:46 local, independent of the date the review is run.
pub fn now() -> i64 {
    timestamp("11-12 09:46")
}
pub fn timestamp(text: &str) -> i64 {
    let (date, time) = text.split_once(' ').unwrap_or(("11-12", text));
    let (month, day) = date.split_once('-').unwrap_or(("11", "12"));
    let (hour, min) = time.split_once(':').unwrap_or(("09", "41"));
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = 126;
    tm.tm_mon = month.parse::<i32>().unwrap_or(11) - 1;
    tm.tm_mday = day.parse().unwrap_or(12);
    tm.tm_hour = hour.parse().unwrap_or(9);
    tm.tm_min = min.parse().unwrap_or(41);
    tm.tm_isdst = -1;
    // SAFETY: tm is a valid initialized local-time structure.
    unsafe { libc::mktime(&mut tm) as i64 }
}
pub fn bytes(value: &str) -> u64 {
    let mut p = value.split_whitespace();
    let n = p.next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.);
    let scale = match p.next() {
        Some("TB") => 1u64 << 40,
        Some("GB") => 1 << 30,
        Some("MB") => 1 << 20,
        Some("KB") => 1 << 10,
        _ => 1,
    };
    (n * scale as f64) as u64
}
pub fn ansi(lines: &V, dark: bool) -> String {
    let colors = &DATA["TERM"][if dark { "dark" } else { "light" }];
    let mut text = String::new();
    for (i, line) in lines.a().iter().enumerate() {
        if i > 0 {
            text.push_str("\r\n");
        }
        for segment in line.a() {
            let key = segment["c"].s().trim_start_matches("var(").trim_end_matches(')');
            let hex = colors[key].s().trim_start_matches('#');
            let color = u32::from_str_radix(hex, 16).unwrap_or(0x24272e);
            text.push_str(&format!(
                "\x1b[0;{};38;2;{};{};{}m",
                if segment["w"].n() == 700. { 1 } else { 22 },
                color >> 16,
                (color >> 8) & 255,
                color & 255
            ));
            if !segment["bg"].s().is_empty() {
                text.push_str(&format!(
                    "\x1b[48;2;{};{};{}m",
                    color >> 16,
                    (color >> 8) & 255,
                    color & 255
                ));
            }
            text.push_str(segment["t"].s());
        }
    }
    text.push_str("\x1b[0m\x1b[?25l");
    text
}
mod readings;
pub fn reading(scene: &str, dark: bool) -> &'static readings::Reading {
    let scene = if PAGE_SCROLL_SCENES.iter().any(|(name, _)| *name == scene) {
        match scene.strip_suffix("-scrolled").unwrap_or(scene) {
            "side" => "main",
            base => base,
        }
    } else {
        match scene {
            _ if monitor::PORT_SCENES.contains(&scene) || monitor::SERVICE_SCENES.contains(&scene) => "local",
            s if s.starts_with("update-") || matches!(s, "settings-update" | "main-update-pill" | "quit-confirm" | "shortcuts" | "tab-attention" | "tab-attention-menu" | "close-window-dialog" | "tabmenu-move" | "tabmenu-single" | "tab-list-scroll") => "main",
            "settings-window" | "set-conn-close-window" => "set-conn",
            "rail-mid" | "rail-mid-expand" => "main",
            "drawer-mid" => "ai",
            "drawer-swap-mid" => "cmds",
            "mon-ov-reflow-mid" => "mon-ov",
            "set-cred-focus" | "set-cred-typed" => "set-cred-master",
            "set-ai-replace" => "set-ai",
            "host-new-focus" => "host-new",
            "files-local" | "files-local-trash" | "mon-local" | "mon-local-proc" | "mon-local-disks" => "local",
            _ => scene,
        }
    };
    readings::READINGS
        .iter()
        .find(|r| r.scene == scene && r.dark == dark)
        .unwrap_or(&readings::READINGS[0])
}
pub mod monitor;
pub fn ansi_line(segments: &V, dark: bool) -> String {
    let colors = &DATA["TERM"][if dark { "dark" } else { "light" }];
    let mut output = String::new();
    for s in segments.a() {
        // The HTML fixture's background-filled space is its drawn cursor,
        // not recorded output. Replay paints the emulator's own block cursor.
        if !s["bg"].s().is_empty() || s["t"].s().is_empty() {
            continue;
        }
        let key = s["c"].s().trim_start_matches("var(").trim_end_matches(')');
        let color = u32::from_str_radix(colors[key].s().trim_start_matches('#'), 16).unwrap_or(0x24272e);
        output.push_str(&format!(
            "\x1b[0;{};38;2;{};{};{}m{}",
            if s["w"].n() == 700. { 1 } else { 22 },
            color >> 16,
            (color >> 8) & 255,
            color & 255,
            s["t"].s()
        ));
    }
    output.push_str("\x1b[0m");
    output
}
#[cfg(test)]
mod tests {
    #[test]
    fn page_scroll_scenes_are_registered_and_have_bounded_sizes() {
        for &(scene, size) in super::PAGE_SCROLL_SCENES {
            assert!(super::SCENES.contains(&scene));
            assert_eq!(super::default_window_size(scene), size);
            assert_eq!(super::request(&format!("{scene}@1440x900")), (scene, Some((1440, 900))));
            assert!(size.0 >= 980 && size.1 >= 620);
        }
        assert_eq!(super::default_window_size("main"), (1440, 900));
        assert_eq!(super::default_window_size("set-term-scrolled"), (1440, 900));
        assert_eq!(super::default_window_size("stress-mon-ov"), (1776, 901));
    }
    use super::*;
    #[test]
    fn fixture_sizes_are_bounded_and_optional() {
        assert_eq!(request("stress-mon-ov@1100x700"),("stress-mon-ov",Some((1100,700))));
        assert_eq!(request("stress-files@1776x901"),("stress-files",Some((1776,901))));
        assert_eq!(request("main"),("main",None));
        assert_eq!(request("main@0x0"),("main",None));
        assert_eq!(request("main@bad"),("main",None));
    }
    #[test]
    fn fixtures_are_complete_and_deterministic() {
        assert_eq!(DATA["HOSTS"].a().len(), 11);
        assert_eq!(SCENES.len(), 128 + PAGE_SCROLL_SCENES.len());
        for scene in ["drawer-mid", "drawer-swap-mid", "mon-ov-reflow-mid"] {
            assert!(SCENES.contains(&scene));
        }
        assert_eq!(DATA["HOSTS"][0]["name"].s(), "prod-web-01");
        assert!(ansi(&DATA["S1"], true).contains("日志归档"));
        assert!(ansi(&DATA["S2"], false).contains("商品快照"));
        assert_eq!(DATA["CMDS"].a().len(), 3);
        for scene in SCENES.iter().filter(|scene|!scene.starts_with("stress-")) {
            for dark in [false, true] {
                let expected = if PAGE_SCROLL_SCENES.iter().any(|(name, _)| name == scene) {
                    if *scene == "side-scrolled" { "main" } else { scene.strip_suffix("-scrolled").unwrap() }
                } else { match *scene {
                    s if s.starts_with("update-") || matches!(s, "settings-update" | "main-update-pill" | "quit-confirm" | "shortcuts" | "tab-attention" | "tab-attention-menu" | "close-window-dialog" | "tabmenu-move" | "tabmenu-single" | "tab-list-scroll") => "main",
                    "settings-window" | "set-conn-close-window" => "set-conn",
                    "rail-mid" | "rail-mid-expand" => "main",
                    "drawer-mid" => "ai",
                    "drawer-swap-mid" => "cmds",
                    "mon-ov-reflow-mid" => "mon-ov",
                    "set-cred-focus" | "set-cred-typed" => "set-cred-master",
                    "set-ai-replace" => "set-ai",
                    "host-new-focus" => "host-new",
                    "files-local" | "files-local-trash" | "mon-local" | "mon-local-proc" | "mon-local-disks" => "local",
                    _ => scene,
                } };
                assert_eq!(reading(scene, dark).scene, expected);
                assert_eq!(reading(scene, dark).dark, dark);
            }
        }
        assert_eq!(DATA["FILES"].a().len(), 8);
        assert_eq!(DATA["TRANSFERS"].a().len(), 4);
        assert_eq!(DATA["REC"].a().len(), 3);
    }
}
