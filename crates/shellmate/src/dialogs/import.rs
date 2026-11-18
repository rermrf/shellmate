//! Importing hosts from `~/.ssh/config`.

use std::path::Path;

use gpui::component::Disableable as _;
use gpui::component::checkbox::Checkbox;
use gpui::component::input::{InputEvent, InputState};
use crate::ui::{Surface, field_input};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FontWeight, IntoElement, Render, SharedString, Subscription, WeakEntity, Window, div, px,
};
use shellmate_core::ImportCandidate;

use super::{DialogBody, Notice, Tone, actions, button, field, hint, notice, palette, stack};
use crate::workspace::Workspace;

/// Default name of the group imported hosts go to (the prototype's wording).
pub const DEFAULT_GROUP: &str = "导入自 ssh config";

/// What the importer knows about the file besides its hosts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportNotes {
    /// Arguments of `Include` lines (not followed).
    pub includes: Vec<String>,
    /// Wildcard / negated host patterns that were skipped.
    pub skipped_patterns: Vec<String>,
    /// Number of `Match` blocks that were skipped.
    pub match_blocks: usize,
    /// Port forwardings the importable hosts have (they become rules).
    pub forwards: usize,
    /// Forwardings of those hosts that cannot be imported.
    pub skipped_forwards: usize,
}

impl ImportNotes {
    /// The remarks to show above the list.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.includes.is_empty() {
            lines.push(format!("Include {} 未展开，这些文件里的主机需要另外导入。", self.includes.join("、")));
        }
        if !self.skipped_patterns.is_empty() {
            lines.push(format!(
                "已跳过通配符 / 否定的 Host 条目：{}（它们的设置仍会作用于匹配的主机）。",
                self.skipped_patterns.join("、")
            ));
        }
        if self.match_blocks > 0 {
            lines.push(format!("已忽略 {} 个 Match 块。", self.match_blocks));
        }
        if self.forwards > 0 {
            lines.push(format!("这些主机带有 {} 条端口转发（LocalForward 等），导入时一并生成规则。", self.forwards));
        }
        if self.skipped_forwards > 0 {
            lines.push(format!(
                "另有 {} 条端口转发无法导入（Unix 套接字、远程 SOCKS 或无法识别的写法）。",
                self.skipped_forwards
            ));
        }
        lines
    }
}

struct Row {
    candidate: ImportCandidate,
    selected: bool,
}

/// Lists the hosts of an ssh config and imports the selected ones.
pub struct ImportDialog {
    workspace: WeakEntity<Workspace>,
    rows: Vec<Row>,
    all: Vec<ImportCandidate>,
    notes: ImportNotes,
    /// Set when there was nothing to read (e.g. no config file).
    unavailable: Option<SharedString>,
    group: Entity<InputState>,
    groups: Vec<String>,
    home: Option<std::path::PathBuf>,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl ImportDialog {
    /// Opens the importer for `candidates`. `unavailable` replaces the list with a message.
    pub fn open(
        workspace: WeakEntity<Workspace>,
        candidates: Vec<ImportCandidate>,
        notes: ImportNotes,
        unavailable: Option<String>,
        groups: Vec<String>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let body = cx.new(|cx| {
            let group = cx.new(|cx| InputState::new(window, cx).default_value(DEFAULT_GROUP).placeholder("不分组"));
            let subscription =
                cx.subscribe_in(&group, window, |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.submit(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                });
            Self {
                workspace,
                rows: candidates.iter().map(|c| Row { selected: c.existing.is_none(), candidate: c.clone() }).collect(),
                all: candidates,
                notes,
                unavailable: unavailable.map(Into::into),
                group,
                groups,
                home: std::env::home_dir(),
                error: None,
                _subscription: subscription,
            }
        });
        super::open(window, cx, "导入 ~/.ssh/config", 780., body);
    }

    fn selected_count(&self) -> usize {
        self.rows.iter().filter(|r| r.selected).count()
    }

    fn abbreviate(&self, path: &Path) -> String {
        match self.home.as_deref().and_then(|home| path.strip_prefix(home).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        }
    }
}

impl DialogBody for ImportDialog {
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.unavailable.is_some() {
            return true;
        }
        let selected: Vec<ImportCandidate> = self
            .rows
            .iter()
            .filter(|r| r.selected && r.candidate.existing.is_none())
            .map(|r| r.candidate.clone())
            .collect();
        if selected.is_empty() {
            self.error = Some("请至少选择一台主机".into());
            cx.notify();
            return false;
        }
        let group = self.group.read(cx).value().to_string();
        let all = self.all.clone();
        match self.workspace.update(cx, |workspace, cx| workspace.import_rows(&selected, &all, &group, window, cx)) {
            Ok(Ok(())) | Err(_) => true,
            Ok(Err(message)) => {
                self.error = Some(message.into());
                cx.notify();
                false
            }
        }
    }

    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn focus(&self, _: &mut Window, _: &mut Context<Self>) {}
}

impl Render for ImportDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let close = || {
            button("import-close", "关闭", Tone::Plain, true)
                .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx)))
        };
        if let Some(message) = &self.unavailable {
            return stack().child(hint(message.clone(), &p)).child(actions(vec![close()]));
        }

        let cell = |text: String, width: f32, color| {
            div().w(px(width)).flex_none().truncate().text_size(px(12.)).text_color(color).child(text)
        };
        let header = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .py(px(6.))
            .child(div().w(px(22.)).flex_none())
            .child(cell("Host".into(), 130., p.tx3))
            .child(cell("HostName".into(), 150., p.tx3))
            .child(cell("User".into(), 80., p.tx3))
            .child(cell("Port".into(), 44., p.tx3))
            .child(cell("IdentityFile".into(), 130., p.tx3))
            .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(p.tx3).child("ProxyJump"));

        let rows = self.rows.iter().enumerate().map(|(ix, row)| {
            let candidate = &row.candidate;
            let existing = candidate.existing.is_some();
            let tone = if existing { p.tx3 } else { p.tx };
            div()
                .id(("import-row", ix))
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(9.))
                .when(!existing, |d| d.hover(|d| d.bg(p.card2)).cursor_pointer())
                .child(div().w(px(22.)).flex_none().child(
                    Checkbox::new(("import-check", ix)).checked(row.selected && !existing).disabled(existing).on_click(
                        cx.listener(move |this, checked: &bool, _, cx| {
                            this.rows[ix].selected = *checked;
                            this.error = None;
                            cx.notify();
                        }),
                    ),
                ))
                .child(
                    div()
                        .w(px(130.))
                        .flex_none()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(12.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(tone)
                                .child(candidate.alias.clone()),
                        )
                        .children(existing.then(|| div().text_size(px(10.5)).text_color(p.tx3).child("已存在"))),
                )
                .child(cell(candidate.hostname.clone(), 150., tone))
                .child(cell(candidate.user.clone().unwrap_or_else(|| "—".into()), 80., tone))
                .child(cell(candidate.port.to_string(), 44., tone))
                .child(cell(
                    candidate.identity_file.as_deref().map_or_else(|| "—".to_string(), |path| self.abbreviate(path)),
                    130.,
                    tone,
                ))
                .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(tone).child(
                    if candidate.proxy_jump.is_empty() {
                        "—".to_string()
                    } else {
                        candidate.proxy_jump.join(" → ")
                    },
                ))
                .when(!existing, |d| {
                    d.on_click(cx.listener(move |this, _, _, cx| {
                        this.rows[ix].selected = !this.rows[ix].selected;
                        this.error = None;
                        cx.notify();
                    }))
                })
        });

        let current_group = self.group.read(cx).value().to_string();
        let group_chips = (!self.groups.is_empty()).then(|| {
            div().flex().flex_wrap().gap(px(6.)).children(self.groups.iter().enumerate().map(|(ix, name)| {
                let picked = name.clone();
                let active = *name == current_group;
                div()
                    .id(("import-group", ix))
                    .px(px(10.))
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .rounded(px(8.))
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .when(active, |d| d.bg(p.accs).text_color(p.acc).font_weight(FontWeight::SEMIBOLD))
                    .when(!active, |d| d.bg(p.card2).text_color(p.tx2).hover(|d| d.bg(p.card3)))
                    .child(name.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let picked = picked.clone();
                        this.group.update(cx, |input, cx| input.set_value(picked, window, cx));
                    }))
            }))
        });

        let count = self.selected_count();
        let summary = format!("解析到 {} 个 Host", self.rows.len());
        let remarks = self.notes.lines();
        stack()
            .child(hint(summary, &p))
            .children(remarks.into_iter().map(|line| notice(Notice::Info, line, &p)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .rounded(px(12.))
                    .bg(p.card)
                    .border_1()
                    .border_color(p.line)
                    .child(header)
                    .child(super::scroll_body("import-rows", div().children(rows))
                        .flex_none().max_h(px(300.)).surface(p.card)),
            )
            .child(field(
                "导入到分组",
                &p,
                stack().gap(px(8.)).child(field_input(&self.group, Surface::Card, false)).children(group_chips),
            ))
            .children(self.error.clone().map(|message| notice(Notice::Error, message, &p)))
            .child(actions(vec![
                button("import-cancel", "取消", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                button("import-ok", format!("导入 {count} 台主机"), Tone::Primary, count > 0)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_describe_what_was_not_imported() {
        assert!(ImportNotes::default().lines().is_empty());
        let notes = ImportNotes {
            includes: vec!["~/.ssh/config.d/*".into(), "extra.conf".into()],
            skipped_patterns: vec!["*.corp.io".into(), "!bastion".into()],
            match_blocks: 2,
            forwards: 3,
            skipped_forwards: 1,
        };
        let lines = notes.lines();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with("Include ~/.ssh/config.d/*、extra.conf 未展开"));
        assert!(lines[1].contains("*.corp.io、!bastion"));
        assert_eq!(lines[2], "已忽略 2 个 Match 块。");
        assert_eq!(lines[3], "这些主机带有 3 条端口转发（LocalForward 等），导入时一并生成规则。");
        assert!(lines[4].starts_with("另有 1 条端口转发无法导入"));
    }
}
