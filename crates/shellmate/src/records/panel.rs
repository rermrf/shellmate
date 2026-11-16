//! Recording/replay and virtualized plain-text log browser.
use crate::{
    dialogs::{self, DialogBody},
    theme::{ActivePalette, MONO_FONT},
    ui::card_shadow,
};
use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, FontWeight, Pixels, Subscription, Task, Window, canvas,
    component::{
        Disableable as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
    },
    div,
    prelude::*,
    px, relative, uniform_list,
};
use shellmate_store::{HostStore, Recording};
use shellmate_term::{
    Terminal, TerminalView, archive,
    cast::{Cast, Data, Playback},
    replay::Timeline,
    session_log::{self, LogFile},
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};
pub struct RecordsWindow {
    store: Rc<RefCell<HostStore>>,
    log_root: PathBuf,
    recordings: Vec<Recording>,
    logs: Vec<LogFile>,
    selected: usize,
    logs_tab: bool,
    active: HashSet<i64>,
    errors: HashMap<i64, String>,
    error: Option<String>,
    loading: bool,
    generation: u64,
    terminal: Entity<Terminal>,
    view: Entity<TerminalView>,
    timeline: Option<Timeline>,
    play: Playback,
    last_tick: Instant,
    query: Entity<InputState>,
    log_lines: Vec<String>,
    filtered: Vec<usize>,
    truncated: bool,
    rename: Option<Entity<InputState>>,
    progress: Rc<Cell<Option<Bounds<Pixels>>>>,
    _subscriptions: Vec<Subscription>,
    _timer: Task<()>,
    #[cfg(debug_assertions)]
    demo_end: Option<bool>,
}
impl RecordsWindow {
    pub fn open(
        store: Rc<RefCell<HostStore>>,
        log_root: PathBuf,
        selected: Option<i64>,
        logs_tab: bool,
        active: HashSet<i64>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let body = cx.new(|cx| {
            let recordings = store.borrow().recordings().unwrap_or_default();
            let index = selected
                .and_then(|id| recordings.iter().position(|r| r.id == id))
                .unwrap_or(0);
            let logs = session_log::list(&log_root).unwrap_or_default();
            let terminal = cx.new(|cx| Terminal::new(cx.terminal_style().palette, cx));
            terminal.update(cx, |t, _| t.replay_reset(80, 24));
            let view = cx.new(|cx| {
                let mut view = TerminalView::new(terminal.clone(), cx.terminal_style(), cx);
                view.top_radius = px(18.);
                view
            });
            let query = cx.new(|cx| InputState::new(window, cx).placeholder("过滤日志"));
            let subscription = cx.subscribe(&query, |this: &mut Self, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.filter(cx);
                }
            });
            let timer = cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(33)).await;
                    if this.update(cx, |s, cx| s.tick(cx)).is_err() {
                        break;
                    }
                }
            });
            let mut this = Self {
                store,
                log_root,
                recordings,
                logs,
                selected: index,
                logs_tab,
                active,
                errors: HashMap::new(),
                error: None,
                loading: false,
                generation: 0,
                terminal,
                view,
                timeline: None,
                play: Playback {
                    speed: 1,
                    ..Default::default()
                },
                last_tick: Instant::now(),
                query,
                log_lines: vec![],
                filtered: vec![],
                truncated: false,
                rename: None,
                progress: Rc::new(Cell::new(None)),
                _subscriptions: vec![subscription],
                _timer: timer,
                #[cfg(debug_assertions)]
                demo_end: None,
            };
            this.load(cx);
            this.refresh_durations(cx);
            this
        });
        dialogs::open(
            window,
            cx,
            "会话记录",
            1140_f32.min(f32::from(window.viewport_size().width) - 40.),
            body.clone(),
        );
        body
    }
    /// Older catalog rows contain wall time. Repair them from the immutable event stream,
    /// off the UI thread, without loading all recordings into memory.
    fn refresh_durations(&mut self, cx: &mut Context<Self>) {
        let rows = self
            .recordings
            .iter()
            .filter(|r| !self.active.contains(&r.id))
            .map(|r| (r.id, PathBuf::from(&r.path)))
            .collect::<Vec<_>>();
        cx.spawn(async move |this, cx| {
            let durations = cx
                .background_executor()
                .spawn(async move {
                    rows.into_iter()
                        .map(|(id, path)| (id, Cast::playback_duration(&path)))
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |s, cx| {
                for (id, result) in durations {
                    match result {
                        Ok(duration) => s.set_duration(id, duration),
                        Err(error) => {
                            s.errors.insert(id, error);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    fn set_duration(&mut self, id: i64, duration: f64) {
        if let Some(row) = self.recordings.iter_mut().find(|r| r.id == id)
            && row.duration != duration
        {
            row.duration = duration;
            if let Err(error) = self
                .store
                .borrow_mut()
                .finish_recording(row.id, duration, row.size)
            {
                self.error = Some(format!("无法更新录制时长：{error}"));
            }
        }
    }
    fn load(&mut self, cx: &mut Context<Self>) {
        self.play.playing = false;
        self.play.position = 0.;
        self.timeline = None;
        self.error = None;
        self.rename = None;
        self.generation += 1;
        let generation = self.generation;
        if self.logs_tab {
            self.log_lines.clear();
            self.filtered.clear();
            let Some(file) = self.logs.get(self.selected) else {
                self.loading = false;
                return;
            };
            let path = file.path.clone();
            self.loading = true;
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        session_log::tail(&path).map_err(|e| format!("无法读取日志：{e}"))
                    })
                    .await;
                this.update(cx, |s, cx| {
                    if s.generation != generation {
                        return;
                    }
                    s.loading = false;
                    match result {
                        Ok((lines, truncated)) => {
                            s.log_lines = lines;
                            s.truncated = truncated;
                            s.filter(cx);
                        }
                        Err(e) => s.error = Some(e),
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        } else {
            let Some(row) = self.recordings.get(self.selected) else {
                self.loading = false;
                return;
            };
            if self.active.contains(&row.id) {
                self.error = Some("正在录制，停止后可回放".into());
                self.loading = false;
                return;
            }
            let path = PathBuf::from(&row.path);
            let id = row.id;
            self.loading = true;
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { Cast::open(&path) })
                    .await;
                this.update(cx, |s, cx| {
                    if s.generation != generation {
                        return;
                    }
                    s.loading = false;
                    match result {
                        Ok(cast) => {
                            s.set_duration(id, cast.duration);
                            s.terminal
                                .update(cx, |t, _| t.replay_reset(cast.header.cols, cast.header.rows));
                            s.timeline = Some(Timeline::new(cast));
                            let time = 0.;
                            #[cfg(debug_assertions)]
                            let time = if let Some(end) = s.demo_end.take() {
                                let duration = s.timeline.as_ref().unwrap().cast.duration;
                                s.play.speed = 2;
                                if end { duration } else { duration / 2. }
                            } else {
                                time
                            };
                            s.seek(time, true, cx);
                        }
                        Err(e) => {
                            s.errors.insert(id, e.clone());
                            s.error = Some(e);
                        }
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }
    fn duration(&self) -> f64 {
        self.timeline.as_ref().map_or(0., |t| t.cast.duration)
    }
    fn filter(&mut self, cx: &mut Context<Self>) {
        let q = self.query.read(cx).value().to_lowercase();
        self.filtered = self
            .log_lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        cx.notify();
    }
    fn seek(&mut self, time: f64, reset: bool, cx: &mut Context<Self>) {
        let Some(timeline) = &mut self.timeline else {
            return;
        };
        let (reset, range) = timeline.advance(time, reset);
        self.play.position = timeline.position;
        self.terminal.update(cx, |t, cx| {
            if reset {
                t.replay_reset(timeline.cast.header.cols, timeline.cast.header.rows);
            }
            for event in &timeline.cast.events[range] {
                match &event.data {
                    Data::Output(text) => t.replay_output(text.as_bytes(), cx),
                    Data::Resize(cols, rows) => {
                        let mut size = t.size();
                        size.cols = *cols;
                        size.rows = *rows;
                        t.set_size(size);
                    }
                }
            }
            cx.notify();
        });
        self.last_tick = Instant::now();
        cx.notify();
    }
    fn tick(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        if self.play.playing {
            self.play.tick(elapsed, self.duration());
            self.seek(self.play.position, false, cx);
        }
    }
    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.timeline.is_none() {
            return;
        }
        if self.play.position >= self.duration() {
            self.seek(0., true, cx);
        }
        self.play.playing = !self.play.playing;
        self.last_tick = Instant::now();
        cx.notify();
    }
    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = index;
        self.load(cx);
    }
    fn switch(&mut self, logs: bool, cx: &mut Context<Self>) {
        self.logs_tab = logs;
        self.selected = 0;
        self.logs = session_log::list(&self.log_root).unwrap_or_default();
        self.load(cx);
    }
    pub(crate) fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.recordings.get(self.selected).cloned() else {
            return;
        };
        if self.active.contains(&row.id) {
            return;
        }
        self.play.playing = false;
        let this = cx.entity().downgrade();
        let store = self.store.clone();
        dialogs::confirm::ConfirmDialog::open(
            "删除这条录制？",
            "将删除 .cast 文件和录制记录，无法撤销。",
            "删除",
            true,
            move |_, cx| {
                match std::fs::remove_file(&row.path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("无法删除文件：{e}")),
                }
                store
                    .borrow_mut()
                    .delete_recording(row.id)
                    .map_err(|e| e.to_string())?;
                this.update(cx, |s, cx| {
                    s.recordings = s.store.borrow().recordings().unwrap_or_default();
                    s.selected = s.selected.min(s.recordings.len().saturating_sub(1));
                    s.load(cx);
                })
                .ok();
                Ok(())
            },
            window,
            cx,
        );
    }
    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.recordings.get(self.selected) else {
            return;
        };
        let source = PathBuf::from(&row.path);
        let suggested = source
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("recording.cast");
        let picked = cx.prompt_for_new_path(
            source.parent().unwrap_or(std::path::Path::new(".")),
            Some(suggested),
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = picked.await {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        if path != source {
                            std::fs::copy(source, path).map(|_| ())
                        } else {
                            Ok(())
                        }
                    })
                    .await;
                this.update(cx, |s, cx| {
                    if let Err(e) = result {
                        s.error = Some(format!("导出失败：{e}"));
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
    fn begin_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.recordings.get(self.selected) {
            self.rename = Some(cx.new(|cx| InputState::new(window, cx).default_value(row.title.clone())));
            cx.notify();
        }
    }
    fn save_title(&mut self, cx: &mut Context<Self>) {
        let Some(input) = &self.rename else { return };
        let title = input.read(cx).value().trim().to_string();
        if title.is_empty() {
            return;
        }
        if let Some(row) = self.recordings.get_mut(self.selected) {
            match self.store.borrow_mut().rename_recording(row.id, &title) {
                Ok(()) => {
                    row.title = title;
                    self.rename = None;
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        cx.notify();
    }
    #[cfg(debug_assertions)]
    pub(crate) fn demo_position(&mut self, end: bool, cx: &mut Context<Self>) {
        if self.timeline.is_some() {
            self.play.speed = 2;
            self.seek(
                if end {
                    self.duration()
                } else {
                    self.duration() / 2.
                },
                true,
                cx,
            );
        } else {
            self.demo_end = Some(end);
        }
    }
    #[cfg(debug_assertions)]
    pub(crate) fn demo_query(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |q, cx| q.set_value(text, window, cx));
        self.filter(cx);
    }
}
impl DialogBody for RecordsWindow {
    fn confirm(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        false
    }
    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.play.playing = false;
    }
}
fn date_label(unix: i64) -> String {
    let now = archive::now();
    #[cfg(debug_assertions)]
    let now = if crate::proto::requested() {
        crate::proto::now()
    } else {
        now
    };
    if archive::local_time(unix, "%Y-%m-%d") == archive::local_time(now, "%Y-%m-%d") {
        format!("今天 {}", archive::local_time(unix, "%H:%M"))
    } else {
        archive::local_time(unix, "%m-%d %H:%M")
    }
}
impl Render for RecordsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let mut list = div().id("archive-list").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().gap(px(2.));
        if self.logs_tab {
            for (i, file) in self.logs.iter().enumerate() {
                list = list.child(
                    div()
                        .id(("log-file", i)).h(px(52.)).flex_none()
                        .px(px(12.)).py(px(10.))
                        .rounded(px(12.))
                        .cursor_pointer()
                        .when(self.selected == i, |d| d.bg(p.card).shadow(card_shadow(&p)))
                        .child(
                            div()
                                .text_size(px(13.5)).line_height(px(16.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(file.host.clone()),
                        )
                        .child(
                            div()
                                .mt(px(3.)).flex().items_center().justify_between()
                                .text_size(px(11.5)).line_height(px(13.))
                                .text_color(p.tx2)
                                .child(file.date.clone())
                                .child(shellmate_sftp::format_size(file.size).replace(".0 ", " ")),
                        )
                        .on_click(cx.listener(move |s, _, _, cx| s.select(i, cx))),
                );
            }
        } else {
            for (i, row) in self.recordings.iter().enumerate() {
                let error = self.errors.get(&row.id).cloned().or_else(|| {
                    (!crate::prototype_fixture() && !std::path::Path::new(&row.path).is_file())
                        .then(|| "文件不存在".into())
                });
                list = list.child(
                    div()
                        .id(("recording", row.id as usize))
                        .h(px(58.)).flex_none()
                        .px(px(12.))
                        .py(px(10.))
                        .rounded(px(13.))
                        .cursor_pointer()
                        .when(self.selected == i, |d| d.bg(p.card).shadow(card_shadow(&p)))
                        .child(
                            div()
                                .text_size(px(13.5))
                                .line_height(px(16.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(row.title.clone()),
                        )
                        .child(
                            div()
                                .mt(px(6.))
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .text_size(px(11.5))
                                .line_height(px(13.))
                                .text_color(p.tx2)
                                .child(div().flex_1().min_w_0().truncate().child(format!(
                                    "{} · {}",
                                    row.label,
                                    date_label(row.started_at)
                                )))
                                .child(div().flex_none().child(format!(
                                    "{} · {}",
                                    archive::mmss(row.duration),
                                    shellmate_sftp::format_size(row.size).replace(".0 "," ")
                                ))),
                        )
                        .children(error.map(|e| div().text_size(px(11.)).text_color(p.err).child(e)))
                        .on_click(cx.listener(move |s, _, _, cx| s.select(i, cx))),
                );
            }
        }
        let nav =
            div()
                .w(px(246.))
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(
                    div()
                        .h(px(30.))
                        .px(px(8.))
                        .text_size(px(22.))
                        .line_height(px(30.))
                        .font_weight(FontWeight::BOLD)
                        .child("会话记录"),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(3.))
                        .p(px(3.))
                        .rounded(px(12.))
                        .bg(p.card3)
                        .children([(false, "录制回放"), (true, "会话日志")].into_iter().map(
                            |(logs, label)| {
                                div()
                                    .id(("archive-tab", usize::from(logs)))
                                    .flex_1()
                                    .h(px(28.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(8.))
                                    .text_center()
                                    .text_size(px(12.5))
                                    .line_height(px(15.))
                                    .font_weight(if self.logs_tab == logs {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::MEDIUM
                                    })
                                    .cursor_pointer()
                                    .when(self.logs_tab == logs, |d| d.bg(p.card))
                                    .child(label)
                                    .on_click(cx.listener(move |s, _, _, cx| s.switch(logs, cx)))
                            },
                        )),
                )
                .child(list)
                .child(
                    div()
                        .p(px(8.))
                        .text_size(px(11.))
                        .line_height(px(18.))
                        .text_color(p.tx2)
                        .child("录制保存为 asciinema v2 .cast，日志为纯文本，按主机和日期归档"),
                );
        let mut body = div()
            .flex_1()
            .min_w_0()
            .pt(px(if self.logs_tab {40.} else {38.}))
            .flex()
            .flex_col()
            .gap(px(12.));
        if self.logs_tab {
            let selected = self.logs.get(self.selected);
            let heading =
                selected.map_or_else(|| "暂无会话日志".into(), |f| format!("{} · {}", f.host, f.date));
            let path = selected.map(|f| f.path.clone());
            body = body
                .child(
                    div()
                        .h(px(34.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(MONO_FONT).text_size(px(17.))
                                .line_height(px(22.))
                                .font_weight(FontWeight::BOLD)
                                .child(heading),
                        )
                        .child(
                            div().w(px(264.)).child(
                                Input::new(&self.query)
                                    .bordered(false).bg(p.card).rounded(px(11.))
                                    .prefix(gpui::component::Icon::new(gpui::assets::IconName::Search).size(px(14.)).text_color(p.tx3))
                                    .h(px(34.))
                                    .text_size(px(13.)),
                            ),
                        )
                        .child(
                            Button::new("reveal-log")
                                .h(px(34.))
                                .px(px(12.))
                                .text_size(px(12.5))
                                .rounded(px(11.)).bg(p.card)
                                .label("在 Finder 中显示")
                                .disabled(path.is_none())
                                .on_click(move |_, _, cx| {
                                    if let Some(path) = &path {
                                        cx.reveal_path(path);
                                    }
                                }),
                        ),
                )
                .when(self.truncated, |d| {
                    d.child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.warn)
                            .child("仅显示文件最后 5 MB"),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .rounded(px(18.)).shadow(card_shadow(&p)).pt(px(12.)).pb(px(10.))
                        .bg(p.card)
                        .overflow_hidden()
                        .child(
                            uniform_list(
                                "log-lines",
                                self.filtered.len(),
                                cx.processor(|s, range: std::ops::Range<usize>, _, cx| {
                                    let p = cx.palette().clone();
                                    range
                                        .map(|i| {
                                            div()
                                                .h(px(20.)).line_height(px(20.))
                                                .px(px(16.))
                                                .font_family(MONO_FONT)
                                                .text_size(px(12.5))
                                                .text_color(p.tx)
                                                .overflow_hidden()
                                                .flex().gap(px(14.)).children({
                                                    let line=&s.log_lines[s.filtered[i]];
                                                    let (time,text)=line.split_once("  ").unwrap_or(("",line));
                                                    [div().flex_none().text_color(p.tx3).child(time.to_string()), div().min_w_0().whitespace_nowrap().child(text.to_string())]
                                                })
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .size_full(),
                        ),
                );
        } else {
            let row = self.recordings.get(self.selected);
            let exists = row.is_some();
            let title = row.map_or_else(|| "暂无录制".into(), |r| r.title.clone());
            let sub = row.map_or_else(String::new, |r| {
                format!("{} · {} · asciinema v2", r.label, date_label(r.started_at))
            });
            let mut header = div().h(px(28.)).flex_none().flex().items_center().gap(px(10.));
            if let Some(input) = &self.rename {
                header = header
                    .child(div().flex_1().child(Input::new(input).small()))
                    .child(
                        Button::new("save-recording-title")
                            .small()
                            .label("保存")
                            .on_click(cx.listener(|s, _, _, cx| s.save_title(cx))),
                    );
            } else {
                header = header
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_baseline()
                            .gap(px(10.))
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .line_height(px(20.))
                                    .font_weight(FontWeight::BOLD)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .line_height(px(16.))
                                    .text_color(p.tx2)
                                    .truncate()
                                    .child(sub),
                            ),
                    )
                    .child(
                        Button::new("rename-recording")
                            .small()
                            .label("重命名")
                            .disabled(!exists)
                            .on_click(cx.listener(|s, _, w, cx| s.begin_rename(w, cx))),
                    );
            }
            let deletable = row.is_some_and(|r| !self.active.contains(&r.id));
            header = header.child(
                Button::new("delete-recording")
                    .small()
                    .label("删除")
                    .disabled(!deletable)
                    .on_click(cx.listener(|s, _, w, cx| s.delete_selected(w, cx))),
            );
            body = body.child(header).child(
                div()
                    .flex_1()
                    .min_h_0()
                    .rounded(px(18.))
                    .bg(shellmate_term::colors::to_hsla(
                        cx.terminal_style().palette.background,
                    ))
                    .overflow_hidden()
                    .shadow(crate::ui::terminal_shadow(&p))
                    .when(self.timeline.is_some(), |d| d.child(self.view.clone()))
                    .when(self.timeline.is_none(), |d| {
                        d.child(div().p(px(22.)).text_size(px(13.)).text_color(p.tx2).child(
                            if self.loading {
                                "正在读取录制…"
                            } else {
                                "选择一条录制开始回放"
                            },
                        ))
                    }),
            );
            let bounds = self.progress.clone();
            let fraction = if self.duration() > 0. {
                self.play.position / self.duration()
            } else {
                0.
            };
            let progress = div()
                .id("replay-progress")
                .relative()
                .flex_1()
                .h(px(24.))
                .cursor_pointer()
                .flex()
                .items_center()
                .child(
                    div().h(px(5.)).w_full().rounded(px(3.)).bg(p.card3).child(
                        div()
                            .h_full()
                            .w(relative(fraction as f32))
                            .rounded(px(3.))
                            .bg(p.acc),
                    ),
                )
                .child(
                    canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
                .on_click(cx.listener(|s, event: &gpui::ClickEvent, _, cx| {
                    if let Some(b) = s.progress.get() {
                        let fraction =
                            f32::from(event.position().x - b.origin.x) / f32::from(b.size.width).max(1.);
                        s.seek(fraction.clamp(0., 1.) as f64 * s.duration(), true, cx);
                    }
                }));
            body = body.child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .p(px(12.))
                    .rounded(px(16.))
                    .bg(p.card)
                    .shadow(card_shadow(&p))
                    .child(
                        Button::new("play-recording")
                            .h(px(34.))
                            .px(px(18.))
                            .rounded(px(11.))
                            .text_size(px(13.))
                            .primary()
                            .label(if self.play.playing {
                                "暂停"
                            } else if self.play.position >= self.duration() && self.duration() > 0. {
                                "重播"
                            } else {
                                "播放"
                            })
                            .disabled(self.timeline.is_none())
                            .on_click(cx.listener(|s, _, _, cx| s.toggle_play(cx))),
                    )
                    .child(progress)
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_size(px(11.))
                            .text_color(p.tx2)
                            .child(format!(
                                "{} / {}",
                                archive::mmss(self.play.position),
                                archive::mmss(self.duration())
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(2.))
                            .p(px(3.))
                            .rounded(px(9.))
                            .bg(p.card3)
                            .children([1, 2, 4].into_iter().map(|speed| {
                                div()
                                    .id(("replay-speed", speed as usize))
                                    .h(px(24.))
                                    .px(px(9.))
                                    .rounded(px(7.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_size(px(11.5))
                                    .line_height(px(13.))
                                    .text_color(p.tx2)
                                    .cursor_pointer()
                                    .when(self.play.speed == speed, |d| {
                                        d.bg(p.card).text_color(p.tx).font_weight(FontWeight::SEMIBOLD)
                                    })
                                    .child(format!("{speed}×"))
                                    .on_click(cx.listener(move |s, _, _, cx| {
                                        s.tick(cx);
                                        s.play.speed = speed;
                                        cx.notify();
                                    }))
                            })),
                    )
                    .child(
                        Button::new("copy-recording")
                            .h(px(30.))
                            .px(px(12.))
                            .rounded(px(10.))
                            .text_size(px(12.5))
                            .label("复制文本")
                            .disabled(self.timeline.is_none())
                            .on_click(cx.listener(|s, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    s.terminal.read(cx).screen_text(),
                                ))
                            })),
                    )
                    .child(
                        Button::new("export-recording")
                            .h(px(30.))
                            .px(px(12.))
                            .rounded(px(10.))
                            .text_size(px(12.5))
                            .label("导出 .cast")
                            .disabled(self.timeline.is_none())
                            .on_click(cx.listener(|s, _, w, cx| s.export(w, cx))),
                    ),
            );
        }
        body = body.children(
            self.error
                .clone()
                .map(|e| dialogs::notice(dialogs::Notice::Error, e, &p)),
        );
        div()
            .relative()
            .h((window.viewport_size().height - px(40.)).min(px(740.)))
            .pt(px(22.))
            .pb(px(20.))
            .pl(px(12.))
            .pr(px(20.))
            .flex()
            .gap(px(18.))
            .child(nav)
            .child(body)
            .child(
                div()
                    .id("records-close")
                    .absolute()
                    .right(px(20.))
                    .top(px(20.))
                    .size(px(30.))
                    .rounded(px(10.))
                    .bg(p.card)
                    .text_color(p.tx2)
                    .text_size(px(16.))
                    .line_height(px(16.))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child("×")
                    .on_click(cx.listener(|s, _, w, cx| s.dismiss(w, cx))),
            )
    }
}

#[cfg(debug_assertions)]
impl RecordsWindow {
    /// In-memory recording/log fixtures: no scratch file reads, timers, or catalog repair.
    pub fn proto_scene(&mut self, logs: bool, cx: &mut Context<Self>) {
        use crate::proto::{self, DATA};
        self.generation += 1;
        self.logs_tab = logs;
        self.error = None;
        self.loading = false;
        self.play.playing = false;
        self.play.speed = 2;
        self.recordings = DATA["REC"]
            .a()
            .iter()
            .enumerate()
            .map(|(i, r)| Recording {
                id: i as i64,
                title: r["title"].s().into(),
                host: r["host"].s().into(),
                label: r["host"].s().into(),
                path: String::new(),
                started_at: if i == 2 {
                    proto::timestamp("11-10 21:05")
                } else {
                    proto::timestamp(if i == 0 { "09:41" } else { "09:30" })
                },
                duration: r["frames"].a().last().map_or(0., |f| f["at"].n()) + 2.,
                size: proto::bytes(r["size"].s()),
                cols: 100,
                rows: 28,
            })
            .collect();
        let frames = DATA["REC"][0]["frames"].a();
        let mut header = shellmate_term::cast::Header::new(100, 28);
        header.timestamp = Some(proto::timestamp("09:41"));
        let events = frames
            .iter()
            .map(|f| {
                let segs = f["segs"];
                let line = proto::ansi_line(&segs, cx.is_dark());
                shellmate_term::cast::Event {
                    at: f["at"].n(),
                    data: Data::Output(format!("{line}\r\n")),
                }
            })
            .collect();
        self.terminal.update(cx, |t, _| t.replay_reset(100, 28));
        self.timeline = Some(Timeline::new(Cast {
            header,
            events,
            duration: self.recordings[0].duration,
        }));
        self.seek(0., true, cx);
        self.logs = DATA["LOGS"]
            .a()
            .iter()
            .map(|l| LogFile {
                host: l["host"].s().into(),
                date: format!("{}.log", l["date"].s()),
                path: PathBuf::new(),
                size: proto::bytes(l["size"].s()),
            })
            .collect();
        self.log_lines = DATA["LOGS"][0]["lines"]
            .a()
            .iter()
            .enumerate()
            .map(|(i, line)| {
                format!(
                    "09:{:02}:{:02}  {}",
                    30 + i / 6,
                    (i * 7) % 60,
                    line.a().iter().map(|s| s["t"].s()).collect::<String>()
                )
            })
            .collect();
        self.filter(cx);
        cx.notify();
    }
}
