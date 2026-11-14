//! Drawing the panel: top bar, list, details.

use std::ops::Range;
use std::time::Instant;

use gpui::assets::IconName as Lucide;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::Input;
use gpui::component::popover::Popover;
use gpui::component::tooltip::Tooltip;
use gpui::component::{Icon, Sizable as _};
use gpui::prelude::*;
use gpui::{
    Anchor, AnyElement, BoxShadow, ClipboardItem, Context, ExternalPaths, FontWeight, Hsla, MouseButton,
    MouseDownEvent, SharedString, Stateful, Window, anchored, black, deferred, div, point, px, transparent_black,
    uniform_list, white,
};
use shellmate_sftp::{Entry, EntryKind, format_size, mode_string, path};

use super::format::mtime_text;
use super::model::{LinkStatus, Row, Subject, View};
use super::panel::{BASH_SNIPPET, CONTEXT, FilesPanel, MenuAction, ZSH_SNIPPET};
use crate::theme::{ActivePalette, MONO_FONT, Palette};
use crate::ui::card_shadow;

impl FilesPanel {
    // ---- drawing ---------------------------------------------------------------------------------------------

    fn button(id: &'static str, label: &'static str, primary: bool, p: &Palette) -> Stateful<gpui::Div> {
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(32.))
            .px(px(if primary { 14. } else { 12. }))
            .rounded(px(10.))
            .text_size(px(12.5))
            .font_weight(FontWeight::SEMIBOLD)
            .bg(if primary { p.acc } else { p.card2 })
            .text_color(if primary { white() } else { p.tx })
            .child(label)
    }

    /// A toolbar button. Disabled, it says why in a tooltip; the caller adds the click handler only
    /// when it is enabled.
    fn tool(
        id: &'static str,
        label: &'static str,
        primary: bool,
        why_not: Option<&'static str>,
        p: &Palette,
    ) -> Stateful<gpui::Div> {
        let button = Self::button(id, label, primary, p);
        match why_not {
            None => button.cursor_pointer().hover(|d| d.opacity(0.88)),
            Some(why) => {
                button.opacity(0.45).cursor_not_allowed().tooltip(move |window, cx| Tooltip::new(why).build(window, cx))
            }
        }
    }

    fn render_breadcrumbs(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let crumbs = self.model.crumbs();
        let last = crumbs.len().saturating_sub(1);
        let piece = |id: (&'static str, usize), label: String, dir: String, strong: bool, cx: &mut Context<Self>| {
            div()
                .id(id)
                .px(px(4.))
                .py(px(2.))
                .rounded(px(6.))
                .cursor_pointer()
                .font_weight(if strong { FontWeight::BOLD } else { FontWeight::MEDIUM })
                .text_color(if strong { p.tx } else { p.tx2 })
                .hover(|d| d.bg(p.card2))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.go_to(&dir, cx)))
        };
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(4.))
            .overflow_hidden()
            .whitespace_nowrap()
            .font_family(MONO_FONT)
            .text_size(px(13.))
            .child(piece(("crumb", 0), "/".into(), "/".into(), crumbs.is_empty(), cx))
            .children(crumbs.into_iter().enumerate().map(|(i, (name, dir))| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(piece(("crumb", i + 1), name, dir, i == last, cx))
                    .child(div().text_color(p.tx3).child("/"))
            }))
    }

    fn render_follow(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.model.follow();
        let panel = cx.entity();
        let hint = (!self.model.osc7_seen()).then(|| {
            let panel_for_open = panel.clone();
            let (tx2, card2) = (p.tx2, p.card2);
            Popover::new("osc7-hint")
                .anchor(Anchor::TopRight)
                .open(self.hint_open)
                .on_open_change(move |open, _, cx| {
                    panel_for_open.update(cx, |this, cx| {
                        this.hint_open = *open;
                        cx.notify();
                    });
                })
                .trigger(Button::new("osc7-hint-trigger").ghost().small().label("终端未上报目录"))
                .content(move |_, _, _| {
                    let snippet = |id: &'static str, shell: &'static str, text: &'static str| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(div().text_size(px(12.)).text_color(tx2).child(shell))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .px(px(10.))
                                            .py(px(7.))
                                            .rounded(px(8.))
                                            .bg(card2)
                                            .font_family(MONO_FONT)
                                            .text_size(px(11.5))
                                            .child(text),
                                    )
                                    .child(Button::new(id).small().label("复制").on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                                    })),
                            )
                    };
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .w(px(560.))
                        .p(px(4.))
                        .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child("让终端报告当前目录"))
                        .child(div().text_size(px(12.)).text_color(tx2).child(
                            "远程 shell 默认不上报目录。把下面一行加进 ~/.bashrc 或 ~/.zshrc，面板就能跟随终端所在目录。",
                        ))
                        .child(snippet("copy-bash", "bash", BASH_SNIPPET))
                        .child(snippet("copy-zsh", "zsh", ZSH_SNIPPET))
                })
        });
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(12.))
            .text_color(p.tx2)
            .children(hint)
            .child("跟随终端目录 · OSC 7")
            .child(
                div()
                    .id("follow-switch")
                    .flex()
                    .flex_none()
                    .items_center()
                    .w(px(42.))
                    .h(px(24.))
                    .p(px(2.))
                    .rounded(px(12.))
                    .cursor_pointer()
                    .bg(if on { p.ok } else { p.card3 })
                    .when(on, |d| d.justify_end())
                    .child(div().size(px(20.)).rounded_full().bg(white()).shadow(vec![gpui::BoxShadow {
                        color: gpui::black().opacity(0.2),
                        offset: gpui::point(px(0.), px(1.)),
                        blur_radius: px(3.),
                        spread_radius: px(0.),
                        inset: false,
                    }]))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_follow(!on, cx))),
            )
    }

    fn render_top_bar(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = self.model.ops_enabled() && !self.model.cwd().is_empty();
        let offline = (!ready).then_some("连接就绪后可用");
        let no_selection = if ready { Some("先选中要下载的文件或目录") } else { offline };
        let can_download = self.model.download_path().is_some();
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .pl(px(18.))
            .pr(px(14.))
            .pt(px(12.))
            .pb(px(10.))
            .child(self.render_breadcrumbs(p, cx))
            .child(self.render_follow(p, cx))
            .child(
                Self::tool("new-folder", "新建文件夹", false, offline, p)
                    .when(ready, |d| d.on_click(cx.listener(|this, _, _, cx| this.new_folder(cx)))),
            )
            .child(
                Self::tool("download", "下载", false, if can_download { None } else { no_selection }, p)
                    .when(can_download, |d| d.on_click(cx.listener(|this, _, _, cx| this.begin_download(cx)))),
            )
            .child(
                Self::tool("upload", "上传", true, offline, p)
                    .when(ready, |d| d.on_click(cx.listener(|this, _, _, cx| this.pick_upload(cx)))),
            )
            .child(
                div()
                    .id("refresh")
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(32.))
                    .rounded(px(10.))
                    .bg(p.card2)
                    .cursor_pointer()
                    .hover(|d| d.bg(p.card3))
                    .child(Icon::new(Lucide::RefreshCw).size(px(15.)).text_color(p.tx2))
                    .tooltip(|window, cx| Tooltip::new("刷新").build(window, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    fn header_cell(label: &'static str, width: Option<f32>, right: bool) -> gpui::Div {
        let cell = div().child(label);
        let cell = match width {
            Some(w) => cell.w(px(w)).flex_none(),
            None => cell.flex_1().min_w_0(),
        };
        if right { cell.text_right() } else { cell }
    }

    fn render_header(&self, p: &Palette) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(18.))
            .py(px(6.))
            .text_size(px(11.5))
            .text_color(p.tx2)
            .border_b_1()
            .border_color(p.line)
            .child(Self::header_cell("名称", None, false))
            .child(Self::header_cell("大小", Some(84.), true))
            .child(Self::header_cell("修改时间", Some(110.), false))
            .child(Self::header_cell("权限", Some(104.), false))
    }

    fn entry_icon(entry: &Entry, p: &Palette) -> Icon {
        match entry.kind {
            EntryKind::Dir => Icon::new(Lucide::Folder).text_color(p.acc),
            EntryKind::Symlink if entry.opens_as_dir() => Icon::new(Lucide::Link).text_color(p.acc),
            EntryKind::Symlink => Icon::new(Lucide::Link).text_color(p.tx2),
            _ => Icon::new(Lucide::File).text_color(p.tx3),
        }
        .size(px(17.))
    }

    fn render_row(&self, index: usize, p: &Palette, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let row = self.model.row(index)?;
        let selected = self.model.selected_index() == Some(index);
        let (icon, name, note, size, time, perms) = match row {
            Row::Parent => (
                Icon::new(Lucide::Folder).size(px(17.)).text_color(p.acc),
                "..".to_string(),
                None,
                String::new(),
                String::new(),
                String::new(),
            ),
            Row::Entry(entry) => {
                let note = entry.link.as_ref().map(|link| {
                    if link.resolved.is_none() {
                        (format!("→ {}（目标不存在）", link.target), p.err)
                    } else {
                        (format!("→ {}", link.target), p.tx3)
                    }
                });
                let size = if entry.kind == EntryKind::File { format_size(entry.size) } else { "—".to_string() };
                let perms = entry.mode.map_or_else(String::new, mode_string);
                (Self::entry_icon(entry, p), entry.name.clone(), note, size, mtime_text(entry.modified), perms)
            }
        };
        let dangling = matches!(row, Row::Entry(e) if e.is_dangling());
        // The name cell: an input while the name is being edited, the name (and where a link points) otherwise.
        let editing = match row {
            Row::Entry(entry) => self.model.edit().filter(|edit| edit.name == entry.name).zip(self.rename.as_ref()),
            Row::Parent => None,
        };
        let name_cell =
            match editing {
                Some((edit, input)) => div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(10.))
                    .child(icon)
                    .child(div().w(px(260.)).flex_none().child(Input::new(&input.state).small()))
                    .children(
                        edit.error
                            .clone()
                            .map(|error| div().min_w_0().truncate().text_size(px(11.5)).text_color(p.err).child(error)),
                    ),
                None => div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(10.))
                    .child(icon)
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if selected {
                                p.acc
                            } else if dangling {
                                p.err
                            } else {
                                p.tx
                            })
                            .child(name),
                    )
                    .children(note.map(|(text, color)| {
                        div().min_w_0().truncate().text_size(px(11.5)).text_color(color).child(text)
                    })),
            };
        let is_entry = matches!(row, Row::Entry(_));
        Some(
            div()
                .id(("file-row", index))
                .flex()
                .w_full()
                .items_center()
                .gap(px(12.))
                .h(px(38.))
                .px(px(10.))
                .rounded(px(10.))
                .text_size(px(13.))
                .cursor_default()
                .when(selected, |d| d.bg(p.accs))
                .when(!selected, |d| d.hover(|d| d.bg(p.card2)))
                .child(name_cell)
                .child(div().w(px(84.)).flex_none().text_right().text_size(px(12.)).text_color(p.tx2).child(size))
                .child(div().w(px(110.)).flex_none().text_size(px(12.)).text_color(p.tx2).child(time))
                .child(
                    div()
                        .w(px(104.))
                        .flex_none()
                        .text_size(px(12.))
                        .font_family(MONO_FONT)
                        .text_color(p.tx2)
                        .child(perms),
                )
                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    this.select_row(index, window, cx);
                    if event.click_count() >= 2 {
                        this.open_row(index, cx);
                    }
                }))
                .when(is_entry, |row| {
                    row.on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.select_row(index, window, cx);
                            this.open_menu(index, event.position, cx);
                        }),
                    )
                })
                .into_any_element(),
        )
    }

    /// A centered message in the list area, optionally with a button.
    fn message(
        &self,
        text: impl Into<SharedString>,
        color: Hsla,
        button: Option<Stateful<gpui::Div>>,
        icon: Option<Lucide>,
    ) -> gpui::AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.))
            .px(px(24.))
            .py(px(40.))
            .text_size(px(13.))
            .text_color(color)
            .children(icon.map(|icon| Icon::new(icon).size(px(26.)).text_color(color)))
            .child(div().max_w(px(520.)).text_center().child(text.into()))
            .children(button)
            .into_any_element()
    }

    fn render_list_area(&self, p: &Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        match (self.model.link(), self.model.view()) {
            (LinkStatus::Disconnected, _) => {
                self.message("连接已断开，重连后自动刷新", p.warn, None, Some(Lucide::TriangleAlert))
            }
            (LinkStatus::Failed(reason), _) => {
                let retry = Self::button("retry", "刷新", false, p)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
                self.message(reason.clone(), p.err, Some(retry), Some(Lucide::CircleAlert))
            }
            (_, View::Loading) => self.message("加载中…", p.tx2, None, None),
            (_, View::Error(reason)) => {
                let back = self.model.parent_dir().is_some().then(|| {
                    Self::button("back", "返回上级", false, p)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.go_parent(cx)))
                });
                self.message(reason.clone(), p.err, back, Some(Lucide::CircleAlert))
            }
            (_, View::List) if self.model.entries().is_empty() => {
                let parent = if self.model.has_parent() { self.render_row(0, p, cx) } else { None };
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(8.))
                    .py(px(4.))
                    .children(parent)
                    .child(
                        div()
                            .py(px(40.))
                            .text_center()
                            .text_size(px(13.))
                            .text_color(p.tx2)
                            .child("空目录 · 拖入文件或新建文件夹"),
                    )
                    .into_any_element()
            }
            (_, View::List) => {
                let list = uniform_list(
                    "file-rows",
                    self.model.row_count(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        let began = Instant::now();
                        let p = cx.palette().clone();
                        let rows = range.clone().filter_map(|index| this.render_row(index, &p, cx)).collect::<Vec<_>>();
                        log::debug!("文件面板：构建第 {range:?} 行用时 {} µs", began.elapsed().as_micros());
                        rows
                    }),
                )
                .track_scroll(&self.scroll)
                .size_full();
                div().flex_1().min_h_0().px(px(8.)).py(px(4.)).child(list).into_any_element()
            }
        }
    }

    fn render_files_card(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let banner = self.model.banner().map(|text| {
            div()
                .mx(px(18.))
                .mb(px(8.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(8.))
                .bg(p.errs)
                .text_size(px(12.))
                .text_color(p.err)
                .child(text.to_string())
        });
        // Something dragged in from Finder over the card: it lights up, and a drop uploads here.
        let (hot_bg, hot_border) = (p.accs, p.acc);
        let dragging = self.demo_dragging();
        div()
            .id("files-card")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .rounded(crate::theme::CARD_RADIUS)
            .bg(p.card)
            .border_2()
            .border_color(transparent_black())
            .when(dragging, |d| d.bg(hot_bg).border_color(hot_border))
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(hot_bg).border_color(hot_border))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.upload(paths.paths().to_vec(), cx)))
            .shadow(card_shadow(p))
            .overflow_hidden()
            .child(self.render_top_bar(p, cx))
            .children(banner)
            .child(self.render_header(p))
            .child(self.render_list_area(p, cx))
            .child(
                div()
                    .px(px(18.))
                    .pt(px(10.))
                    .pb(px(12.))
                    .text_size(px(12.))
                    .text_color(p.tx2)
                    .border_t_1()
                    .border_color(p.line)
                    .child("拖拽文件到此处上传 · 双击进入目录或编辑文件 · 右键更多操作"),
            )
    }

    fn info_row(label: &'static str, value: String, p: &Palette) -> impl IntoElement {
        div()
            .flex()
            .gap(px(12.))
            .child(div().w(px(30.)).flex_none().text_color(p.tx2).child(label))
            .child(div().flex_1().min_w_0().child(value))
    }

    fn render_details(&self, window: &Window, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let subject = self.model.subject();
        let dash = || "—".to_string();
        let attributes = |entry: &Entry| {
            let owner = if entry.owner_display().is_empty() { dash() } else { entry.owner_display() };
            (mtime_text(entry.modified), owner)
        };
        let (title, kind, size, time, owner) = match subject {
            Subject::Entry(entry) => {
                let kind = match (&entry.kind, &entry.link) {
                    (EntryKind::Dir, _) => "目录".to_string(),
                    (EntryKind::File, _) => "文件".to_string(),
                    (EntryKind::Symlink, Some(link)) if link.resolved.is_none() => {
                        format!("符号链接 → {}（目标不存在）", link.target)
                    }
                    (EntryKind::Symlink, Some(link)) => format!("符号链接 → {}", link.target),
                    (EntryKind::Symlink, None) => "符号链接".to_string(),
                    (EntryKind::Other, _) => "特殊文件".to_string(),
                };
                let size = if entry.kind == EntryKind::File { format_size(entry.size) } else { dash() };
                let (time, owner) = attributes(entry);
                (entry.name.clone(), kind, size, time, owner)
            }
            // Nothing selected: the directory itself, with what `stat` said about it.
            Subject::Dir(stat) => {
                let name = path::file_name(self.model.cwd()).unwrap_or("/").to_string();
                let (time, owner) = stat.map_or_else(|| (dash(), dash()), attributes);
                (name, "目录".into(), dash(), time, owner)
            }
            Subject::Parent => ("..".into(), "上级目录".into(), dash(), dash(), dash()),
        };
        let editable_file = matches!(subject, Subject::Entry(entry) if entry.kind == EntryKind::File);
        let grid_enabled = self.model.chmod_enabled();
        let draft = self.model.draft();
        let can_apply = self.model.chmod_request().is_some();
        let rows = ["所有者", "用户组", "其他"];
        let columns = ["读", "写", "执行"];
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(268.))
            .gap(px(12.))
            .p(px(18.))
            .rounded(crate::theme::CARD_RADIUS)
            .bg(p.card)
            .shadow(card_shadow(p))
            .child(div().text_size(px(17.)).font_weight(FontWeight::BOLD).child(title))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .text_size(px(12.5))
                    .child(Self::info_row("类型", kind, p))
                    .child(Self::info_row("大小", size, p))
                    .child(Self::info_row("修改", time, p))
                    .child(Self::info_row("属主", owner, p)),
            )
            .child(div().h(px(1.)).bg(p.line))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child("权限"))
                    .child(
                        div().font_family(MONO_FONT).text_size(px(12.5)).text_color(p.tx2).child(
                            draft.map_or_else(|| "—".to_string(), |d| format!("{} · {}", d.octal(), d.symbolic())),
                        ),
                    ),
            )
            .child(
                div()
                    .id("perm-grid")
                    .track_focus(&self.perm_focus)
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .p(px(3.))
                    .m(px(-3.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(if self.perm_focus.is_focused(window) { p.acc } else { transparent_black() })
                    .opacity(if draft.is_some() && grid_enabled { 1.0 } else { 0.45 })
                    .children(rows.iter().enumerate().map(|(row, label)| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().w(px(48.)).text_size(px(12.)).text_color(p.tx2).child(*label))
                            .children(columns.iter().enumerate().map(|(column, name)| {
                                let on = draft.is_some_and(|d| d.is_set(row, column));
                                div()
                                    .id(("perm", row * 3 + column))
                                    .flex()
                                    .flex_1()
                                    .items_center()
                                    .justify_center()
                                    .h(px(30.))
                                    .rounded(px(9.))
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .bg(if on { p.acc } else { p.card2 })
                                    .text_color(if on { white() } else { p.tx2 })
                                    .when(draft.is_some() && grid_enabled, |d| {
                                        d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                                            this.model.toggle_perm(row, column);
                                            cx.notify();
                                        }))
                                    })
                                    .child(*name)
                            }))
                    })),
            )
            .child(
                Self::button("apply-chmod", "应用 chmod", false, p)
                    .h(px(34.))
                    .rounded(px(11.))
                    .when(!can_apply, |d| d.opacity(0.5))
                    .when(can_apply, |d| {
                        d.cursor_pointer().on_click(cx.listener(|this, _, _, cx| this.apply_chmod(cx)))
                    }),
            )
            .children(
                self.model
                    .chmod_error()
                    .map(|error| div().text_size(px(12.)).text_color(p.err).child(error.to_string())),
            )
            .child(div().flex_1())
            .children(editable_file.then(|| {
                Self::button("open-editor", "用内置编辑器打开", true, p)
                    .h(px(38.))
                    .rounded(px(12.))
                    .text_size(px(13.))
                    .opacity(0.45)
                    .cursor_not_allowed()
                    .tooltip(|window, cx| Tooltip::new("内置编辑器稍后提供").build(window, cx))
            }))
    }
}

impl FilesPanel {
    /// The right-click menu of a row, drawn over everything at the place it was opened.
    fn render_menu(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.clone()?;
        let ops = self.model.ops_enabled();
        let grid = ops && self.model.draft().is_some();
        let item = |id: &'static str,
                    label: &'static str,
                    action: MenuAction,
                    enabled: bool,
                    danger: bool,
                    cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(32.))
                .px(px(10.))
                .rounded(px(9.))
                .text_size(px(13.))
                .text_color(if danger { p.err } else { p.tx })
                .when(enabled, |d| {
                    d.cursor_pointer()
                        .hover(|d| d.bg(p.card2))
                        .on_click(cx.listener(move |this, _, window, cx| this.menu_action(action, window, cx)))
                })
                .when(!enabled, |d| d.opacity(0.45).cursor_not_allowed())
                .child(label)
        };
        let (open_label, open_enabled) =
            if menu.opens_dir { ("打开", ops) } else { ("用内置编辑器打开", false) };
        let shadow = vec![
            BoxShadow {
                color: black().opacity(0.22),
                offset: point(px(0.), px(12.)),
                blur_radius: px(36.),
                spread_radius: px(0.),
                inset: false,
            },
            BoxShadow {
                color: p.line,
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(1.),
                inset: false,
            },
        ];
        Some(
            deferred(
                anchored().position(menu.position).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id("row-menu")
                        .occlude()
                        .flex()
                        .flex_col()
                        .w(px(196.))
                        .p(px(6.))
                        .rounded(px(14.))
                        .bg(p.card)
                        .shadow(shadow)
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(cx)))
                        .child(item("menu-open", open_label, MenuAction::Open, open_enabled, false, cx))
                        .child(item("menu-download", "下载", MenuAction::Download, ops, false, cx))
                        .child(item("menu-rename", "重命名", MenuAction::Rename, ops, false, cx))
                        .child(item("menu-copy", "复制路径", MenuAction::CopyPath, true, false, cx))
                        .child(item("menu-perm", "修改权限", MenuAction::Permissions, grid, false, cx))
                        .child(item("menu-delete", "删除", MenuAction::Delete, ops, true, cx)),
                ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }
}

impl Render for FilesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_rename_input(window, cx);
        let p = cx.palette().clone();
        div()
            .id("files-panel")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_previous))
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_parent))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_cancel_rename))
            .size_full()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(10.))
                    .child(self.render_files_card(&p, cx))
                    .child(self.render_details(window, &p, cx)),
            )
            .children(self.render_queue(&p, cx))
            .children(self.render_menu(&p, cx))
    }
}
