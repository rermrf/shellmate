//! Sidebar host list: filtering by tab and search query, then grouping.

use crate::host::{GroupId, GroupTone, Host, HostBook};

/// The segmented filter above the host list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HostFilter {
    /// Every host, grouped.
    #[default]
    All,
    /// Favorite hosts, grouped.
    Favorites,
    /// Hosts that were connected to, most recent first.
    Recent,
}

impl HostFilter {
    /// The filters in tab order.
    pub const ALL: [HostFilter; 3] = [HostFilter::All, HostFilter::Favorites, HostFilter::Recent];

    /// Tab label.
    pub fn label(self) -> &'static str {
        match self {
            HostFilter::All => "全部",
            HostFilter::Favorites => "收藏",
            HostFilter::Recent => "最近",
        }
    }
}

/// One collapsible section of the host list.
#[derive(Debug)]
pub struct HostSection<'a> {
    /// The group shown in this section; `None` for the recent and ungrouped sections.
    pub group: Option<GroupId>,
    /// Section title.
    pub title: String,
    /// Avatar tint of the group.
    pub tone: GroupTone,
    /// The section's hosts in display order.
    pub hosts: Vec<&'a Host>,
}

impl HostSection<'_> {
    /// Key identifying the section across re-renders (collapse state).
    pub fn key(&self) -> String {
        match &self.group {
            Some(group) => format!("group:{group}"),
            None => format!("title:{}", self.title),
        }
    }
}

/// Title of the section with the most recently used hosts.
pub const RECENT_SECTION: &str = "最近连接";

/// Title of the section with hosts that belong to no group.
pub const UNGROUPED_SECTION: &str = "未分组";

/// Filters the book and returns non-empty sections in display order.
///
/// `Recent` yields a single section sorted by most recent connection; the other
/// filters group hosts in the book's group order, followed by the hosts that
/// belong to no (or an unknown) group.
pub fn sections<'a>(book: &'a HostBook, filter: HostFilter, query: &str) -> Vec<HostSection<'a>> {
    let visible = book.hosts.iter().filter(|h| h.matches(query));

    if filter == HostFilter::Recent {
        let mut hosts: Vec<&Host> = visible.filter(|h| h.last_connected_at.is_some()).collect();
        hosts.sort_by_key(|h| std::cmp::Reverse(h.last_connected_at));
        return non_empty(vec![HostSection {
            group: None,
            title: RECENT_SECTION.to_string(),
            tone: GroupTone::Neutral,
            hosts,
        }]);
    }

    let hosts: Vec<&Host> = visible.filter(|h| filter != HostFilter::Favorites || h.favorite).collect();
    let in_group = |group: &GroupId| hosts.iter().copied().filter(|h| h.group.as_ref() == Some(group)).collect();

    let mut out: Vec<HostSection<'a>> = book
        .groups
        .iter()
        .map(|g| HostSection { group: Some(g.id.clone()), title: g.name.clone(), tone: g.tone, hosts: in_group(&g.id) })
        .collect();
    out.push(HostSection {
        group: None,
        title: UNGROUPED_SECTION.to_string(),
        tone: GroupTone::Neutral,
        hosts: hosts.iter().copied().filter(|h| h.group.as_ref().is_none_or(|g| book.group(g).is_none())).collect(),
    });
    non_empty(out)
}

fn non_empty(sections: Vec<HostSection<'_>>) -> Vec<HostSection<'_>> {
    sections.into_iter().filter(|s| !s.hosts.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::GroupId;
    use crate::sample::sample_book;

    fn names(section: &HostSection) -> Vec<String> {
        section.hosts.iter().map(|h| h.name.clone()).collect()
    }

    #[test]
    fn all_groups_follow_book_order() {
        let book = sample_book();
        let s = sections(&book, HostFilter::All, "");
        let titles: Vec<_> = s.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["生产环境", "测试环境", "个人", "设备"]);
        assert_eq!(s[0].tone, GroupTone::Accent);
        assert_eq!(s[0].group, Some(book.groups[0].id.clone()));
        assert_eq!(s.iter().map(|s| s.hosts.len()).sum::<usize>(), book.hosts.len());
    }

    #[test]
    fn favorites_drop_empty_groups() {
        let book = sample_book();
        let s = sections(&book, HostFilter::Favorites, "");
        let titles: Vec<_> = s.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["生产环境", "个人"]);
        assert_eq!(names(&s[0]), ["prod-web-01", "prod-db-01"]);
    }

    #[test]
    fn recent_is_one_section_with_the_latest_first() {
        let book = sample_book();
        let s = sections(&book, HostFilter::Recent, "");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].title, RECENT_SECTION);
        assert_eq!(s[0].group, None);
        let times: Vec<_> = s[0].hosts.iter().map(|h| h.last_connected_at.unwrap()).collect();
        let mut sorted = times.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(times, sorted);
        assert_eq!(s[0].hosts[0].name, "prod-web-01");
        assert!(s[0].hosts.iter().all(|h| h.last_connected_at.is_some()), "never-connected hosts are not recent");
    }

    #[test]
    fn query_filters_by_name_but_not_tags() {
        let book = sample_book();
        let s = sections(&book, HostFilter::All, "redis");
        assert_eq!(s.len(), 1);
        assert_eq!(names(&s[0]), ["staging-redis"]);
        for filter in HostFilter::ALL {
            assert!(sections(&book, filter, "nginx").is_empty());
        }
    }

    #[test]
    fn no_match_yields_no_sections() {
        let book = sample_book();
        assert!(sections(&book, HostFilter::All, "zzz-nothing").is_empty());
        assert!(sections(&book, HostFilter::Recent, "zzz-nothing").is_empty());
    }

    #[test]
    fn empty_book_has_no_sections() {
        let book = HostBook::default();
        for filter in HostFilter::ALL {
            assert!(sections(&book, filter, "").is_empty());
        }
    }

    #[test]
    fn hosts_without_a_known_group_go_to_the_ungrouped_section() {
        let mut book = sample_book();
        let mut loose = book.hosts[0].clone();
        loose.id = crate::host::HostId::from_raw("loose");
        loose.name = "loose".into();
        loose.group = None;
        let mut dangling = loose.clone();
        dangling.id = crate::host::HostId::from_raw("dangling");
        dangling.name = "dangling".into();
        dangling.group = Some(GroupId::from_raw("deleted-group"));
        book.hosts.extend([loose, dangling]);

        let s = sections(&book, HostFilter::All, "");
        let last = s.last().unwrap();
        assert_eq!(last.title, UNGROUPED_SECTION);
        assert_eq!(last.tone, GroupTone::Neutral);
        assert_eq!(names(last), ["loose", "dangling"]);
        assert_eq!(last.key(), "title:未分组");
        assert!(s[0].key().starts_with("group:"));
    }
}
