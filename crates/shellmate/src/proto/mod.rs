//! Deterministic prototype fixtures. Compiled only in debug builds, rendered by the real views.
mod data;
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
pub const SCENES: &[&str] = &[
    "main",
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
    "set-cred",
    "set-sync",
    "set-term",
    "set-local",
    "set-conn",
    "host-new",
    "rail",
    "local",
    "qc",
    "tabmenu",
    "reconnect",
    "editor",
    "hostkey-changed",
    "hostkey-new",
    "stress-main", "stress-mon-ov", "stress-mon-proc", "stress-mon-svc", "stress-mon-dk", "stress-files", "stress-fwd",
];
pub fn requested() -> bool {
    std::env::var("SHELLMATE_DEMO").is_ok_and(|s| s.starts_with("proto:"))
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
    size.unwrap_or(if scene.starts_with("stress-"){(1776,901)}else{(1440,900)})
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
    output.push_str("\x1b[0m\x1b[?25l");
    output
}
#[cfg(test)]
mod tests {
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
        assert_eq!(SCENES.len(), 37);
        assert_eq!(DATA["HOSTS"][0]["name"].s(), "prod-web-01");
        assert!(ansi(&DATA["S1"], true).contains("日志归档"));
        assert!(ansi(&DATA["S2"], false).contains("商品快照"));
        assert_eq!(DATA["CMDS"].a().len(), 3);
        for scene in SCENES.iter().filter(|scene|!scene.starts_with("stress-")) {
            for dark in [false, true] {
                assert_eq!(reading(scene, dark).scene, *scene);
                assert_eq!(reading(scene, dark).dark, dark);
            }
        }
        assert_eq!(DATA["FILES"].a().len(), 8);
        assert_eq!(DATA["TRANSFERS"].a().len(), 4);
        assert_eq!(DATA["REC"].a().len(), 3);
    }
}
