//! The collapsed sidebar: a 76 px rail with the expand button, three shortcuts, the favourite and
//! the connected hosts as tiles, and the settings button.

use std::collections::HashMap;

use gpui::assets::IconName as Lucide;
use gpui::component::Icon;
use gpui::component::tooltip::Tooltip;
use gpui::prelude::*;
use gpui::{ClickEvent, Context, SharedString, Stateful, div, px};
use shellmate_core::{GroupTone, Host, HostBook, HostFilter, sections};

use super::{Active, Avatar, LiveStatus, SYNC_STATUS, Sidebar, SidebarEvent, host_avatar};
use crate::theme::{ActivePalette, Palette};
use crate::ui::{card_shadow, window_drag_area};

/// The width of the rail.
pub const RAIL_WIDTH: f32 = 76.;

/// The hosts the rail shows tiles for: favourites and every host that has a tab, in the order the
/// sidebar lists them (its groups in order, the hosts of each as the book has them), each with the
/// tint of its group.
pub fn rail_hosts<'a>(book: &'a HostBook, live: &HashMap<String, LiveStatus>) -> Vec<(&'a Host, GroupTone)> {
    sections(book, HostFilter::All, "")
        .into_iter()
        .flat_map(|section| {
            let tone = section.tone;
            section.hosts.into_iter().map(move |host| (host, tone))
        })
        .filter(|(host, _)| host.favorite || live.contains_key(host.id.as_str()))
        .collect()
}

/// A 40 × 34 button of the rail's shortcut column.
fn key(id: &'static str, icon: Lucide, size: f32, on: bool, tip: &'static str, p: &Palette) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(40.))
        .h(px(34.))
        .rounded(px(11.))
        .cursor_pointer()
        .when(on, |d| d.bg(p.card).shadow(card_shadow(p)))
        .hover(|d| d.bg(p.card3))
        .child(Icon::new(icon).size(px(size)).text_color(p.tx2))
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
}

impl Sidebar {
    fn render_rail_host(&self, host: &Host, tone: GroupTone, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let id = host.id.clone();
        let live = self.live.get(host.id.as_str()).copied();
        let on = self.selected.as_deref() == Some(host.id.as_str()) || self.active == Active::Host(host.id.to_string());
        let tip: SharedString =
            if live.is_some() { host.name.clone().into() } else { format!("{} · 双击连接", host.name).into() };
        div()
            .id(SharedString::from(format!("rail-host-{}", host.id)))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(44.))
            .rounded(px(14.))
            .cursor_pointer()
            .when(on, |d| d.bg(p.card).shadow(card_shadow(p)))
            .when(!on, |d| d.hover(|d| d.bg(p.card)))
            .child(host_avatar(host, tone, live, Avatar { size: 34., radius: 11., dot: 8. }, p))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            // A click shows the host's tab (or just selects the host if it has none); a double click connects.
            .on_click(cx.listener(move |_, event: &ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    cx.emit(SidebarEvent::Connect(id.clone()));
                } else {
                    cx.emit(SidebarEvent::ActivateHost(id.clone()));
                }
            }))
    }

    pub(super) fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let local_on = self.active == Active::Local;
        let local = key("rail-local", Lucide::SquareTerminal, 17., local_on, "本机终端 ⌘T", &p)
            .children(
                self.local_alive
                    .then(|| div().absolute().right(px(7.)).bottom(px(6.)).size(px(6.)).rounded_full().bg(p.ok)),
            )
            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenLocal)));
        let hosts: Vec<_> = rail_hosts(&self.book, &self.live)
            .into_iter()
            .map(|(host, tone)| self.render_rail_host(host, tone, &p, cx).into_any_element())
            .collect();

        div()
            .w(px(RAIL_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.))
            .pt(px(14.))
            .pb(px(12.))
            // The traffic lights are the window's own, at the same place as with the sidebar open.
            .child(window_drag_area(div().id("rail-drag")).w_full().h(px(26.)))
            .child(
                key("rail-expand", Lucide::PanelLeft, 16., true, "展开侧边栏 ⌃⌘S", &p)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ToggleSidebar))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        key("rail-search", Lucide::Search, 15., false, "搜索主机", &p)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::SearchHosts))),
                    )
                    .child(
                        key("rail-new-host", Lucide::Plus, 16., false, "新建连接", &p)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::NewHost))),
                    )
                    .child(local),
            )
            .child(div().flex_none().w(px(28.)).h(px(1.)).bg(p.line))
            .child(
                div()
                    .id("rail-hosts")
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .py(px(2.))
                    .overflow_y_scroll()
                    .children(hosts),
            )
            .child(
                div()
                    .id("rail-settings")
                    .relative()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(40.))
                    .rounded(px(13.))
                    .bg(p.card)
                    .shadow(card_shadow(&p))
                    // Settings come with a later phase: the button is there, dimmed like the one in the sidebar.
                    .child(Icon::new(Lucide::Settings).size(px(16.)).text_color(p.tx2).opacity(0.6))
                    .child(
                        // The sync state: none yet, so grey.
                        div()
                            .absolute()
                            .top(px(5.))
                            .right(px(5.))
                            .size(px(7.))
                            .rounded_full()
                            .border_1()
                            .border_color(p.card)
                            .bg(p.tx3),
                    )
                    .tooltip(|window, cx| Tooltip::new(format!("设置 · {SYNC_STATUS}")).build(window, cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use shellmate_core::sample::sample_book;

    use super::*;

    fn names(book: &HostBook, live: &[(&str, LiveStatus)]) -> Vec<String> {
        let live = live.iter().map(|(id, status)| (id.to_string(), *status)).collect();
        rail_hosts(book, &live).into_iter().map(|(host, _)| host.name.clone()).collect()
    }

    #[test]
    fn with_nothing_connected_the_rail_has_the_favourites_in_the_sidebars_order() {
        let book = sample_book();
        assert_eq!(names(&book, &[]), ["prod-web-01", "prod-db-01", "vps-tokyo"]);
    }

    #[test]
    fn hosts_with_a_tab_join_the_favourites_where_the_sidebar_lists_them() {
        let book = sample_book();
        // staging-api and prod-web-02 are not favourites; they have tabs. A favourite with a tab is shown once.
        let live = [("h5", LiveStatus::Ok), ("h2", LiveStatus::Connecting), ("h1", LiveStatus::Failed)];
        assert_eq!(names(&book, &live), ["prod-web-01", "prod-web-02", "prod-db-01", "staging-api", "vps-tokyo"]);
    }

    #[test]
    fn the_order_is_the_sidebars_whatever_order_the_tabs_were_opened_in() {
        let book = sample_book();
        let sidebar: Vec<String> =
            sections(&book, HostFilter::All, "").iter().flat_map(|s| s.hosts.iter().map(|h| h.name.clone())).collect();
        let live = [("h11", LiveStatus::Ok), ("h5", LiveStatus::Ok), ("h9", LiveStatus::Ok), ("h2", LiveStatus::Ok)];
        let rail = names(&book, &live);
        let mut expected: Vec<String> = sidebar.into_iter().filter(|name| rail.contains(name)).collect();
        assert_eq!(rail, expected, "the same relative order as the sidebar");
        // Stable: asking again gives the same list.
        assert_eq!(names(&book, &live), rail);
        expected.sort();
        let mut sorted = rail.clone();
        sorted.sort();
        assert_eq!(sorted, expected, "and the same hosts");
    }

    #[test]
    fn each_tile_has_the_tint_of_its_group() {
        let book = sample_book();
        let live = HashMap::from([("h5".to_string(), LiveStatus::Ok), ("h11".to_string(), LiveStatus::Ok)]);
        let tones: Vec<(String, GroupTone)> =
            rail_hosts(&book, &live).into_iter().map(|(h, tone)| (h.name.clone(), tone)).collect();
        assert!(tones.contains(&("prod-web-01".to_string(), GroupTone::Accent)));
        assert!(tones.contains(&("staging-api".to_string(), GroupTone::Warn)));
        assert!(tones.contains(&("vps-tokyo".to_string(), GroupTone::Ok)));
        assert!(tones.contains(&("office-router".to_string(), GroupTone::Neutral)));
    }

    #[test]
    fn a_host_without_a_group_or_with_an_unknown_one_still_shows_when_it_qualifies() {
        let mut book = sample_book();
        for host in &mut book.hosts {
            host.favorite = false;
        }
        book.hosts[0].group = None;
        book.hosts[0].favorite = true;
        book.hosts[1].group = Some(shellmate_core::GroupId::from_raw("gone"));
        let live = HashMap::from([("h2".to_string(), LiveStatus::Ok)]);
        let found: Vec<(String, GroupTone)> =
            rail_hosts(&book, &live).into_iter().map(|(h, tone)| (h.name.clone(), tone)).collect();
        assert_eq!(
            found,
            [("prod-web-01".to_string(), GroupTone::Neutral), ("prod-web-02".to_string(), GroupTone::Neutral)]
        );
    }

    #[test]
    fn an_empty_book_or_no_favourites_and_no_tabs_leave_the_list_empty() {
        assert!(names(&HostBook::default(), &[]).is_empty());
        let mut book = sample_book();
        for host in &mut book.hosts {
            host.favorite = false;
        }
        assert!(names(&book, &[]).is_empty());
    }
}
