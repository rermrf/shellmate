use super::*;
use gpui::ClipboardItem;
use shellmate_monitor::Snapshot;
use shellmate_monitor::ports::{Exposure, Family, Listener, PortSource, PortSources, Protocol, exposure};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Sort {
    #[default]
    Port,
    Process,
}

struct Filters {
    tcp: bool,
    udp: bool,
    sort: Sort,
    descending: bool,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            tcp: true,
            udp: false,
            sort: Sort::Port,
            descending: false,
        }
    }
}

impl Filters {
    fn shows_user_limit(&self, sources: PortSources) -> bool {
        (self.tcp && sources.tcp == PortSource::Lsof) || (self.udp && sources.udp == PortSource::Lsof)
    }

    fn order(&mut self, key: Sort) {
        self.descending = self.sort == key && !self.descending;
        self.sort = key;
    }

    fn indices(&self, rows: &[Listener], snapshot: Option<&Snapshot>, query: &str) -> Vec<usize> {
        let query = query.trim().to_lowercase();
        let mut indices: Vec<_> = (0..rows.len())
            .filter(|&index| {
                let row = &rows[index];
                let enabled = match row.protocol {
                    Protocol::Tcp => self.tcp,
                    Protocol::Udp => self.udp,
                };
                enabled
                    && (query.is_empty()
                        || row.port.to_string().contains(&query)
                        || row.pid.to_string().contains(&query)
                        || row.name.to_lowercase().contains(&query)
                        || row.addresses.iter().any(|addr| addr.to_lowercase().contains(&query))
                        || row.owner(snapshot).is_some_and(|proc| {
                            proc.name.to_lowercase().contains(&query) || proc.command.to_lowercase().contains(&query)
                        }))
            })
            .collect();
        let name = |row: &Listener| row.owner(snapshot).map_or(&row.name, |proc| &proc.name).to_lowercase();
        indices.sort_by(|&left, &right| {
            let (left, right) = (&rows[left], &rows[right]);
            let by = match self.sort {
                Sort::Port => left.port.cmp(&right.port),
                Sort::Process => name(left).cmp(&name(right)),
            };
            (if self.descending { by.reverse() } else { by })
                .then(left.port.cmp(&right.port))
                .then(left.pid.cmp(&right.pid))
                .then(left.protocol.cmp(&right.protocol))
                .then(left.addresses.cmp(&right.addresses))
        });
        indices
    }
}

pub(super) struct PortsPage {
    query: Entity<InputState>,
    filters: Filters,
    selected: Option<Listener>,
    scroll: UniformListScrollHandle,
    _subscription: Subscription,
}

impl PortsPage {
    pub(super) fn new(window: &mut Window, cx: &mut Context<MonitorPanel>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("过滤端口、进程、PID 或地址"));
        let subscription = cx.subscribe(&query, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            query,
            filters: Filters::default(),
            selected: None,
            scroll: UniformListScrollHandle::new(),
            _subscription: subscription,
        }
    }
}

fn button(label: &'static str, bg: Hsla, fg: Hsla) -> gpui::Stateful<gpui::Div> {
    div()
        .id(label)
        .h(px(32.))
        .px(px(10.))
        .flex()
        .flex_none()
        .items_center()
        .rounded(px(9.))
        .bg(bg)
        .text_color(fg)
        .text_size(px(12.5))
        .font_weight(FontWeight::SEMIBOLD)
        .child(label)
}

impl MonitorPanel {
    #[cfg(debug_assertions)]
    pub fn demo_select_port(&mut self, port: u16) {
        if let Some(page) = &mut self.ports {
            page.selected = self
                .data
                .ports
                .as_ref()
                .and_then(|list| list.rows.as_ref())
                .and_then(|rows| rows.iter().find(|row| row.port == port))
                .cloned();
        }
    }

    fn render_listener(&self, row: &Listener, index: usize, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let owner = row.owner(self.data.snapshot.as_deref());
        let name = owner.map_or(&row.name, |proc| &proc.name);
        let user = owner.map(|proc| proc.user.as_str()).or(row.user.as_deref()).unwrap_or("—");
        let address = row.addresses.join(" / ");
        let (scope, warn) = if row.addresses.iter().any(|addr| exposure(addr) == Exposure::All) {
            ("所有网络".to_owned(), true)
        } else if row.addresses.iter().all(|addr| exposure(addr) == Exposure::Local) {
            ("仅本机".to_owned(), false)
        } else {
            (format!("仅 {address}"), false)
        };
        let family = match row.family {
            Family::V4 => "IPv4",
            Family::V6 => "IPv6",
            Family::Both => "IPv4 + IPv6",
        };
        let transport = match row.protocol {
            Protocol::Tcp => "TCP",
            Protocol::Udp => "UDP",
        };
        let selected = self.ports.as_ref().and_then(|page| page.selected.as_ref()) == Some(row);
        let listener = row.clone();
        div()
            .id(("listener", index))
            .flex()
            .items_center()
            .w_full()
            .h(px(44.))
            .px(px(12.))
            .gap(px(10.))
            .rounded(px(8.))
            .text_size(px(12.5))
            .whitespace_nowrap()
            .cursor_pointer()
            .when(selected, |el| el.bg(palette.accs))
            .when(!selected, |el| el.hover(|el| el.bg(palette.card2)))
            .child(
                div()
                    .w(px(54.))
                    .flex_none()
                    .font_family(MONO_FONT)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(row.port.to_string()),
            )
            .child(
                div()
                    .w(px(74.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .child(transport)
                    .child(div().text_size(px(10.5)).text_color(palette.tx2).child(family)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .items_start()
                    .child(
                        ellipsis(("bind-address", index), address)
                            .max_w_full()
                            .font_family(MONO_FONT),
                    )
                    .child(
                        ellipsis(("bind-scope", index), scope)
                            .max_w_full()
                            .px(px(5.))
                            .rounded(px(5.))
                            .text_size(px(10.5))
                            .bg(if warn { palette.warns } else { palette.card2 })
                            .text_color(if warn { palette.warn } else { palette.tx2 }),
                    ),
            )
            .child(ellipsis(("listener-process", index), name.clone()).flex_1())
            .child(
                div()
                    .w(px(60.))
                    .flex_none()
                    .font_family(MONO_FONT)
                    .child(row.pid.to_string()),
            )
            .child(
                ellipsis(("listener-user", index), user.to_owned())
                    .w(px(78.))
                    .flex_none()
                    .text_color(palette.tx2),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.ports.as_mut().unwrap().selected = Some(listener.clone());
                cx.notify();
            }))
            .into_any_element()
    }

    fn port_bar(&self, row: &Listener, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let pid = row.pid;
        let endpoint = row.endpoint();
        let owner = row.owner(self.data.snapshot.as_deref());
        let command = owner.map_or(&row.name, |proc| &proc.command);
        let key = owner.map(ProcKey::of);
        let end = owner
            .zip(self.data.probe.as_deref())
            .and_then(|(proc, probe)| process::plan(ProcessAction::Term, proc, probe).map(|_| ProcKey::of(proc)));
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .flex_none()
            .gap(px(8.))
            .px(px(14.))
            .py(px(10.))
            .border_t_1()
            .border_color(palette.line)
            .child(
                ellipsis("listener-command", format!("PID {pid} · {command}"))
                    .min_w(px(115.))
                    .flex_1()
                    .font_family(MONO_FONT)
                    .text_size(px(12.5)),
            )
            .children(row.browser_url().map(|url| {
                button("在浏览器打开", palette.accs, palette.acc)
                    .cursor_pointer()
                    .on_click(move |_, _, cx| cx.open_url(&url))
            }))
            .child(
                button("拷贝地址", palette.card2, palette.tx)
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(endpoint.clone()));
                    }),
            )
            .child(
                button("查看进程", palette.card2, palette.tx)
                    .when(key.is_some(), |el| {
                        el.cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| this.show_process(pid, window, cx)))
                    })
                    .when(key.is_none(), |el| el.opacity(0.4).cursor_not_allowed()),
            )
            .child(
                button("结束进程", palette.errs, palette.err)
                    .when_some(end, |el, key| {
                        el.cursor_pointer()
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(MonitorPanelEvent::EndPortProcess(key))))
                    })
                    .when(end.is_none(), |el| el.opacity(0.4).cursor_not_allowed()),
            )
            .into_any_element()
    }

    pub(super) fn render_ports(&mut self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let page = self.ports.as_mut().expect("local ports page");
        let list = self.data.ports.as_ref().expect("local port list");
        let shows_user_limit = page.filters.shows_user_limit(list.sources);
        let rows = list.rows.clone().unwrap_or_default();
        let indices = page
            .filters
            .indices(&rows, self.data.snapshot.as_deref(), &page.query.read(cx).value());
        let selected = page
            .selected
            .take()
            .filter(|sel| indices.iter().any(|&index| &rows[index] == sel));
        page.selected.clone_from(&selected);
        let mut toolbar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .flex_none()
            .gap(px(8.))
            .px(px(16.))
            .py(px(10.))
            .child(
                div()
                    .w(px(245.))
                    .h(px(34.))
                    .px(px(10.))
                    .rounded(px(10.))
                    .bg(palette.card2)
                    .flex()
                    .gap(px(8.))
                    .items_center()
                    .child(crate::icons::AppIcon::Search.at(14.).text_color(palette.tx3))
                    .child(
                        div().min_w_0().flex_1().child(
                            Input::new(&page.query)
                                .appearance(false)
                                .bordered(false)
                                .p_0()
                                .h(px(18.))
                                .text_size(px(13.)),
                        ),
                    ),
            );
        for (label, active) in [("TCP", page.filters.tcp), ("UDP", page.filters.udp)] {
            toolbar = toolbar.child(
                button(
                    label,
                    if active { palette.accs } else { palette.card2 },
                    if active { palette.acc } else { palette.tx2 },
                )
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    let filters = &mut this.ports.as_mut().unwrap().filters;
                    let enabled = if label == "TCP" {
                        &mut filters.tcp
                    } else {
                        &mut filters.udp
                    };
                    *enabled = !*enabled;
                    cx.notify();
                })),
            );
        }
        toolbar = toolbar
            .child(div().flex_1())
            .child(div().text_size(px(12.)).text_color(palette.tx2).child(format!(
                "{} / {} 个端口 · 每 5 秒刷新",
                indices.len(),
                rows.len()
            )));
        let mut headings = div()
            .flex()
            .items_center()
            .flex_none()
            .px(px(20.))
            .gap(px(10.))
            .py(px(6.))
            .border_b_1()
            .border_color(palette.line)
            .text_size(px(11.5))
            .text_color(palette.tx2);
        for (label, width, sort) in [
            ("端口", Some(54.), Some(Sort::Port)),
            ("协议", Some(74.), None),
            ("监听地址", None, None),
            ("进程", None, Some(Sort::Process)),
            ("PID", Some(60.), None),
            ("用户", Some(78.), None),
        ] {
            let arrow = if sort == Some(page.filters.sort) {
                if page.filters.descending { " ↓" } else { " ↑" }
            } else {
                ""
            };
            let cell = div().id(label).child(format!("{label}{arrow}"));
            let cell = match width {
                Some(width) => cell.w(px(width)).flex_none(),
                None => cell.min_w_0().flex_1(),
            };
            headings = headings.child(cell.when_some(sort, |el, key| {
                el.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                    this.ports.as_mut().unwrap().filters.order(key);
                    cx.notify();
                }))
            }));
        }
        let body = if indices.is_empty() {
            let message = match (&list.rows, &list.error) {
                (None, Some(_)) => "暂无端口数据",
                (None, None) => "正在读取监听端口…",
                (Some(rows), _) if rows.is_empty() => "没有监听中的端口",
                _ => "没有匹配的端口",
            };
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.))
                .text_color(palette.tx3)
                .child(message)
                .into_any_element()
        } else {
            let table = uniform_list(
                "local-port-rows",
                indices.len(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    let palette = cx.palette().clone();
                    range
                        .map(|index| this.render_listener(&rows[indices[index]], index, &palette, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&page.scroll)
            .size_full();
            div()
                .flex_1()
                .min_h_0()
                .px(px(8.))
                .py(px(4.))
                .child(table)
                .into_any_element()
        };
        self.card(palette)
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .flex_1()
            .overflow_hidden()
            .child(toolbar)
            .when(shows_user_limit, |el| {
                el.child(
                    div()
                        .flex_none()
                        .px(px(16.))
                        .pb(px(6.))
                        .text_size(px(12.))
                        .text_color(palette.tx2)
                        .child("仅显示当前用户的进程，系统和其他用户的端口需要管理员权限"),
                )
            })
            .child(headings)
            .child(body)
            .children(list.error.as_ref().map(|err| {
                ellipsis("ports-error", format!("端口刷新失败：{err}"))
                    .px(px(16.))
                    .py(px(4.))
                    .text_size(px(12.))
                    .text_color(palette.tx2)
            }))
            .children(selected.as_ref().map(|row| self.port_bar(row, palette, cx)))
            .into_any_element()
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn user_limit_notice_follows_visible_sources() {
        let mut filters = Filters::default();
        for (scene, limited) in [("mon-local-ports", false), ("mon-local-ports-lsof", true)] {
            let data = crate::proto::monitor::local_ports(scene);
            let list = data.ports.unwrap();
            assert_eq!(filters.shows_user_limit(list.sources), limited);
            if limited {
                let rows = list.rows.as_ref().unwrap();
                assert!(!rows.is_empty());
                assert!(rows.iter().all(|row| row.user.as_deref() == Some("chen")));
            }
        }
        let mixed = PortSources { tcp: PortSource::Netstat, udp: PortSource::Lsof };
        assert!(!filters.shows_user_limit(mixed));
        filters.udp = true;
        assert!(filters.shows_user_limit(mixed));
    }

    #[test]
    fn ports_tab_count_and_sort_filter_use_the_fixture() {
        let data = crate::proto::monitor::local_ports("mon-local-ports");
        let tabs = model::header(&data, Page::Ports, "本机").tabs;
        assert_eq!(
            tabs.iter().map(|tab| tab.page).collect::<Vec<_>>(),
            [Page::Overview, Page::Processes, Page::Ports, Page::Docker]
        );
        assert_eq!(tabs[2].badge, Some(("6".into(), Tone::Plain)));
        let rows = data.ports.as_ref().unwrap().rows.as_ref().unwrap();
        let snapshot = data.snapshot.as_deref();
        let mut filters = Filters::default();
        let ports = |filters: &Filters, query| {
            filters
                .indices(rows, snapshot, query)
                .iter()
                .map(|&index| rows[index].port)
                .collect::<Vec<_>>()
        };
        assert_eq!(ports(&filters, ""), [22, 3000, 3306, 6379, 9010]);
        for query in ["3000", "3550", "NODE", "server.js"] {
            assert_eq!(ports(&filters, query), [3000]);
        }
        assert_eq!(ports(&filters, "192.0.2"), [9010]);
        filters.order(Sort::Port);
        assert_eq!(ports(&filters, ""), [9010, 6379, 3306, 3000, 22]);
        filters.order(Sort::Process);
        assert_eq!(ports(&filters, ""), [22, 3306, 3000, 6379, 9010]);
        filters.udp = true;
        assert!(ports(&filters, "").contains(&5353));
        filters.tcp = false;
        assert_eq!(ports(&filters, ""), [5353]);
    }

    #[test]
    fn listener_joins_latest_process_and_rejects_a_reused_pid() {
        let data = crate::proto::monitor::local_ports("mon-local-ports");
        let mut row = data
            .ports
            .as_ref()
            .unwrap()
            .rows
            .as_ref()
            .unwrap()
            .iter()
            .find(|row| row.port == 3000)
            .unwrap()
            .clone();
        row.name = "short-name".into();
        row.owner_start = None;
        assert!(row.owner(None).is_none());
        assert_eq!(row.name, "short-name");
        let owner = row.owner(data.snapshot.as_deref()).unwrap();
        assert_eq!(
            (owner.user.as_str(), owner.command.as_str()),
            ("chen", "node dev server.js")
        );
        row.owner_start = Some(owner.start_time + 1);
        assert!(row.owner(data.snapshot.as_deref()).is_none());
    }
}
