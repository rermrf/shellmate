//! Sidebar host list: filtering by tab and search query, then grouping.

use crate::host::{GroupTone, Host, HostBook};

/// The segmented filter above the host list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HostFilter {
    #[default]
    All,
    Favorites,
    Recent,
}

impl HostFilter {
    pub const ALL: [HostFilter; 3] = [HostFilter::All, HostFilter::Favorites, HostFilter::Recent];

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
    pub title: String,
    pub tone: GroupTone,
    pub hosts: Vec<&'a Host>,
}

pub const RECENT_SECTION: &str = "最近连接";

/// Filters the book and returns non-empty sections in display order.
///
/// `Recent` yields a single section sorted by most recent connection; the other
/// filters group hosts by the book's group order, then by first appearance for
/// groups the book doesn't list.
pub fn sections<'a>(book: &'a HostBook, filter: HostFilter, query: &str) -> Vec<HostSection<'a>> {
    let visible = book.hosts.iter().filter(|h| h.matches(query));

    if filter == HostFilter::Recent {
        let mut hosts: Vec<&Host> = visible.filter(|h| h.last_connected_mins.is_some()).collect();
        hosts.sort_by_key(|h| h.last_connected_mins);
        return non_empty(vec![HostSection {
            title: RECENT_SECTION.to_string(),
            tone: GroupTone::Neutral,
            hosts,
        }]);
    }

    let hosts: Vec<&Host> = visible
        .filter(|h| filter != HostFilter::Favorites || h.favorite)
        .collect();

    let mut order: Vec<&str> = book.groups.iter().map(|g| g.name.as_str()).collect();
    for h in &hosts {
        if !order.contains(&h.group.as_str()) {
            order.push(&h.group);
        }
    }

    non_empty(
        order
            .into_iter()
            .map(|name| HostSection {
                title: name.to_string(),
                tone: book.tone_of(name),
                hosts: hosts.iter().copied().filter(|h| h.group == name).collect(),
            })
            .collect(),
    )
}

fn non_empty(sections: Vec<HostSection<'_>>) -> Vec<HostSection<'_>> {
    sections.into_iter().filter(|s| !s.hosts.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn recent_is_one_section_sorted_by_time() {
        let book = sample_book();
        let s = sections(&book, HostFilter::Recent, "");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].title, RECENT_SECTION);
        let mins: Vec<_> = s[0].hosts.iter().map(|h| h.last_connected_mins.unwrap()).collect();
        let mut sorted = mins.clone();
        sorted.sort();
        assert_eq!(mins, sorted);
        assert_eq!(s[0].hosts[0].name, "prod-web-01");
    }

    #[test]
    fn query_filters_by_tag() {
        let book = sample_book();
        let s = sections(&book, HostFilter::All, "redis");
        assert_eq!(s.len(), 1);
        assert_eq!(names(&s[0]), ["staging-redis"]);
    }

    #[test]
    fn no_match_yields_no_sections() {
        let book = sample_book();
        assert!(sections(&book, HostFilter::All, "zzz-nothing").is_empty());
        assert!(sections(&book, HostFilter::Recent, "zzz-nothing").is_empty());
    }

    #[test]
    fn unknown_groups_are_appended() {
        let mut book = sample_book();
        let mut extra = book.hosts[0].clone();
        extra.id = "x".into();
        extra.group = "临时".into();
        book.hosts.push(extra);
        let s = sections(&book, HostFilter::All, "");
        assert_eq!(s.last().unwrap().title, "临时");
        assert_eq!(s.last().unwrap().tone, GroupTone::Neutral);
    }
}
