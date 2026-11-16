//! The log panes of the 服务 and Docker pages: a command that is followed (`journalctl -f`, `docker logs -f`),
//! the lines it has produced so far, and how they are drawn.

use std::collections::VecDeque;

use gpui::prelude::*;
use gpui::{Context, Hsla, Task, div, px};
use shellmate_monitor::services::{LogLevel, LogLine, is_journal_hint};
use shellmate_monitor::{Follow, FollowEvent, Monitor};

use super::MonitorPanel;
use crate::monitor::services::{JOURNAL_DENIED, LogSource};
use crate::theme::{MONO_FONT, Palette};

/// How many lines a pane keeps.
pub(super) const KEEP: usize = 300;

/// Where the lines come from, and so how they are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LogKind {
    /// `journalctl -o short-iso`.
    Journal,
    /// `docker logs -t`.
    Docker,
}

impl LogKind {
    fn parse(self, line: &str) -> LogLine {
        match self {
            LogKind::Journal => shellmate_monitor::services::parse_journal_line(line),
            LogKind::Docker => shellmate_monitor::docker::parse_log_line(line),
        }
    }
}

fn unique_key(subject: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("{subject}#{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// A pane: what it follows (`key`), what it has, and what to say instead when there is nothing.
pub(super) struct LogPane {
    /// What is followed: `service:nginx.service`, `container:web:true`. A pane for another subject is another pane.
    pub subject: String,
    /// Tells this pane from an older one with the same subject (events of the older one that were already on
    /// their way must not land in this one).
    pub key: String,
    kind: LogKind,
    /// The lines, the oldest first.
    pub lines: VecDeque<LogLine>,
    /// A sentence instead of lines (the journal cannot be read, the command ended).
    pub note: Option<String>,
    /// The command is over (the lines stay).
    pub ended: bool,
    /// The channel on the server: dropped with the pane, which closes it.
    _follow: Option<Follow>,
    _task: Option<Task<()>>,
}

impl LogPane {
    /// A pane that follows `source` (through `monitor`) and tells the panel about every batch of lines.
    pub(super) fn start(
        subject: String,
        kind: LogKind,
        source: LogSource,
        monitor: Option<&Monitor>,
        cx: &mut Context<MonitorPanel>,
    ) -> Self {
        let key = unique_key(&subject);
        let mut pane = Self {
            subject,
            key: key.clone(),
            kind,
            lines: VecDeque::new(),
            note: None,
            ended: false,
            _follow: None,
            _task: None,
        };
        match (source, monitor) {
            (LogSource::Denied(why), _) => pane.note = Some(why.to_string()),
            (LogSource::Follow(_), None) => pane.note = Some("没有可用的监控连接".into()),
            (LogSource::Follow(line), Some(monitor)) => {
                let follow = monitor.follow(line);
                let events = follow.events.clone();
                pane._task = Some(cx.spawn(async move |this, cx| {
                    while let Ok(first) = events.recv().await {
                        // Everything that is there already goes in one go: one redraw for a burst of lines.
                        let mut batch = vec![first];
                        while batch.len() < 500 {
                            match events.try_recv() {
                                Ok(next) => batch.push(next),
                                Err(_) => break,
                            }
                        }
                        let key = key.clone();
                        if this.update(cx, |this, cx| this.on_log_events(&key, batch, cx)).is_err() {
                            break;
                        }
                    }
                }));
                pane._follow = Some(follow);
            }
        }
        pane
    }

    /// A pane that follows nothing: the lines it was given (the review data), or none (a container whose page is not
    /// in front).
    pub(super) fn fixed(subject: String, kind: LogKind, lines: Vec<LogLine>) -> Self {
        let key = unique_key(&subject);
        Self { subject, key, kind, lines: lines.into(), note: None, ended: false, _follow: None, _task: None }
    }

    /// Takes what the command said.
    pub(super) fn push(&mut self, events: Vec<FollowEvent>) {
        for event in events {
            match event {
                FollowEvent::Line(line) => {
                    // A user who may not read the journal gets a hint instead of an error: say what it means.
                    if self.kind == LogKind::Journal && is_journal_hint(&line) {
                        self.note = Some(JOURNAL_DENIED.to_string());
                        continue;
                    }
                    if self.lines.len() == KEEP {
                        self.lines.pop_front();
                    }
                    self.lines.push_back(self.kind.parse(&line));
                }
                FollowEvent::Ended(why) => {
                    self.ended = true;
                    if self.lines.is_empty() && self.note.is_none() {
                        self.note = Some(if why.is_empty() { "没有日志".to_string() } else { why });
                    }
                }
            }
        }
    }
}

fn level_color(p: &Palette, level: LogLevel) -> Hsla {
    match level {
        LogLevel::Error => p.err,
        LogLevel::Warning => p.warn,
        LogLevel::Good => p.ok,
        LogLevel::Plain => p.tx,
    }
}

/// The lines of a pane, newest at the bottom: a box that keeps the end of the log in view and clips the rest.
/// `max` is how many lines are drawn at the most.
pub(super) fn log_box(pane: Option<&LogPane>, empty: &str, max: usize, p: &Palette) -> gpui::Div {
    let body = div().flex_1().min_h_0().overflow_hidden().flex().flex_col().justify_end().gap(px(3.));
    let body = match pane {
        Some(pane) if !pane.lines.is_empty() => {
            let skip = pane.lines.len().saturating_sub(max);
            body.children(pane.lines.iter().skip(skip).map(|line| {
                div()
                    .flex()
                    .gap(px(10.))
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .font_family(MONO_FONT)
                    .when(!line.time.is_empty(), |d| {
                        d.child(div().flex_none().text_color(p.tx3).child(line.time.clone()))
                    })
                    .child(div().flex_1().min_w_0().text_color(level_color(p, line.level)).child(line.text.clone()))
            }))
        }
        Some(LogPane { note: Some(note), .. }) => body
            .justify_start()
            .child(div().text_size(px(12.)).line_height(px(18.)).text_color(p.tx2).child(note.clone())),
        _ => body.justify_start().child(div().text_size(px(12.)).text_color(p.tx3).child(empty.to_string())),
    };
    div().flex().flex_col().p(px(12.)).rounded(px(12.)).bg(p.card2).child(body)
}
