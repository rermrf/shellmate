//! Productivity rules and palette ordering, independent of gpui.
use shellmate_core::{AuthMethod, Host, HostBook, HostId};
use shellmate_store::{Snippet, SnippetGroup};

/// Equivalent to the prototype's case-insensitive RISK regex, without a new dependency.
/// The first alternative checks both boundaries; the other alternatives deliberately
/// retain the prototype's unanchored prefix matching (for example, `systemctl stopX`).
/// ```text
/// /(^|\s|;|&&)(sudo|rm|reboot|shutdown|kill|pkill|killall|mkfs|dd|truncate|chmod|chown|iptables)(\s|$)|systemctl\s+(stop|restart|reload|disable)|docker\s+(rm|stop|kill|image\s+prune|system\s+prune)|drop\s+(table|database)/i
/// ```
pub fn risky(command: &str) -> bool {
    let s = command.to_lowercase();
    let verbs = [
        "sudo", "rm", "reboot", "shutdown", "kill", "pkill", "killall", "mkfs", "dd", "truncate", "chmod",
        "chown", "iptables",
    ];
    for (at, _) in s.char_indices() {
        let before = &s[..at];
        if (at == 0
            || before.ends_with(char::is_whitespace)
            || before.ends_with(';')
            || before.ends_with("&&"))
            && verbs.iter().any(|verb| {
                s[at..]
                    .strip_prefix(verb)
                    .is_some_and(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
            })
        {
            return true;
        }
    }
    fn sequence(s: &str, first: &str, tails: &[&str]) -> bool {
        s.match_indices(first).any(|(at, _)| {
            let rest = &s[at + first.len()..];
            rest.starts_with(char::is_whitespace)
                && tails.iter().any(|tail| rest.trim_start().starts_with(tail))
        })
    }
    sequence(&s, "systemctl", &["stop", "restart", "reload", "disable"])
        || sequence(&s, "docker", &["rm", "stop", "kill"])
        || s.match_indices("docker").any(|(at, _)| {
            let rest = &s[at + 6..];
            rest.starts_with(char::is_whitespace)
                && ["image", "system"].iter().any(|word| {
                    rest.trim_start().strip_prefix(word).is_some_and(|tail| {
                        tail.starts_with(char::is_whitespace) && tail.trim_start().starts_with("prune")
                    })
                })
        })
        || sequence(&s, "drop", &["table", "database"])
}

pub fn production(group: &str, tags: &[String]) -> bool {
    std::iter::once(group)
        .chain(tags.iter().map(String::as_str))
        .any(|s| s.contains("生产") || s.to_lowercase().contains("prod"))
}

pub fn quick_connect(query: &str) -> Option<Host> {
    let query = query.trim();
    let (user, address) = query.split_once('@')?;
    if user.is_empty()
        || !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
    {
        return None;
    }
    let (addr, port) = if let Some(rest) = address.strip_prefix('[') {
        let (addr, tail) = rest.split_once(']')?;
        addr.parse::<std::net::Ipv6Addr>().ok()?;
        let port = if tail.is_empty() {
            22
        } else {
            tail.strip_prefix(':')?.parse().ok()?
        };
        (addr, port)
    } else {
        let (addr, port) = match address.split_once(':') {
            Some((a, p)) => (a, p.parse().ok()?),
            None => (address, 22),
        };
        if addr.is_empty()
            || addr.starts_with('-')
            || !addr
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        {
            return None;
        }
        (addr, port)
    };
    if port == 0 {
        return None;
    }
    let mut host = Host::new(addr, addr);
    host.user = Some(user.into());
    host.port = port;
    host.auth = AuthMethod::Agent;
    Some(host)
}

/// A subsequence match; contiguous matches rank ahead of scattered ones within a group.
pub fn fuzzy(query: &str, text: &str) -> Option<usize> {
    let q = query.trim().to_lowercase();
    let t = text.to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    if let Some(at) = t.find(&q) {
        return Some(at);
    }
    let mut chars = t.chars().enumerate();
    let mut score = 1000;
    for wanted in q.chars().filter(|c| !c.is_whitespace()) {
        let (at, _) = chars.find(|(_, c)| *c == wanted)?;
        score += at;
    }
    Some(score)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Local,
    NewHost,
    Duplicate,
    Split,
    Files,
    Monitor,
    Forward,
    Theme,
    Sidebar,
    Import,
    History,
    Settings,
    LockVault,
    AiAssistant,
    ExplainError,
    Records,
}
impl Action {
    pub const ALL: [Self; 16] = [
        Self::Local,
        Self::NewHost,
        Self::Duplicate,
        Self::Split,
        Self::Files,
        Self::Monitor,
        Self::Forward,
        Self::Theme,
        Self::Sidebar,
        Self::Import,
        Self::History,
        Self::Settings,
        Self::LockVault,
        Self::AiAssistant,
        Self::ExplainError,
        Self::Records,
    ];
    pub fn label(self, sidebar_open: bool, split: bool) -> &'static str {
        match self {
            Self::Local => "打开本地终端",
            Self::NewHost => "新建连接",
            Self::Duplicate => "复制当前会话",
            Self::Split if split => "取消分屏",
            Self::Split => "分屏",
            Self::Files => "打开文件面板",
            Self::Monitor => "打开监控",
            Self::Forward => "打开端口转发",
            Self::Theme => "切换明暗主题",
            Self::Sidebar if sidebar_open => "收起侧边栏",
            Self::Sidebar => "展开侧边栏",
            Self::Import => "导入 ~/.ssh/config",
            Self::History => "搜索命令历史",
            Self::Settings => "打开设置",
            Self::AiAssistant => "打开 AI 助手",
            Self::ExplainError => "解释最近的报错",
            Self::LockVault => "立即锁定凭据库",
            Self::Records => "会话记录与回放",
        }
    }
    pub fn shortcut(self) -> &'static str {
        match self {
            Self::Local => "⌘T",
            Self::Duplicate => "⇧⌘T",
            Self::Sidebar => "⌃⌘S",
            Self::History => "⌃R",
            _ => "",
        }
    }
}
#[derive(Clone)]
pub enum Choice {
    Quick(Box<Host>),
    Host(HostId),
    Session(usize),
    Command(Snippet),
    Action(Action),
}
#[derive(Clone)]
pub struct Item {
    pub group: &'static str,
    pub title: String,
    pub detail: String,
    pub hint: String,
    pub choice: Choice,
}
#[derive(Clone)]
pub struct SessionItem {
    pub id: usize,
    pub title: String,
    pub detail: String,
    pub host: Option<HostId>,
}

pub fn palette(
    query: &str,
    book: &HostBook,
    sessions: &[SessionItem],
    active: Option<usize>,
    snippets: &[SnippetGroup],
    sidebar_open: bool,
    split: bool,
) -> Vec<Item> {
    let empty = query.trim().is_empty();
    let mut items = Vec::new();
    if let Some(host) = quick_connect(query) {
        items.push(Item {
            group: "快速连接",
            title: format!("连接 {}", query.trim()),
            detail: "临时会话，不写入主机列表".into(),
            hint: "↵".into(),
            choice: Choice::Quick(Box::new(host)),
        });
    }
    let mut hosts: Vec<_> = book
        .hosts
        .iter()
        .filter(|h| !empty || h.last_connected_at.is_some())
        .collect();
    if empty {
        hosts.sort_by_key(|h| std::cmp::Reverse(h.last_connected_at));
        hosts.truncate(4);
    }
    let hosts: Vec<_> = hosts
        .into_iter()
        .map(|h| Item {
            group: if empty { "最近主机" } else { "主机" },
            title: h.name.clone(),
            detail: h.address_line(),
            hint: if sessions.iter().any(|s| s.host.as_ref() == Some(&h.id)) {
                "已连接".into()
            } else {
                String::new()
            },
            choice: Choice::Host(h.id.clone()),
        })
        .collect();
    let sessions: Vec<_> = sessions
        .iter()
        .filter(|s| Some(s.id) != active)
        .map(|s| Item {
            group: "切换会话",
            title: s.title.clone(),
            detail: s.detail.clone(),
            hint: String::new(),
            choice: Choice::Session(s.id),
        })
        .collect();
    let commands: Vec<_> = snippets
        .iter()
        .flat_map(|g| &g.items)
        .map(|s| Item {
            group: "快捷命令",
            title: s.name.clone(),
            detail: s.command.clone(),
            hint: String::new(),
            choice: Choice::Command(s.clone()),
        })
        .collect();
    let actions: Vec<_> = Action::ALL
        .into_iter()
        .take(if empty { 5 } else { 13 })
        .map(|a| Item {
            group: "操作",
            title: a.label(sidebar_open, split).into(),
            detail: String::new(),
            hint: a.shortcut().into(),
            choice: Choice::Action(a),
        })
        .collect();
    let groups = if empty {
        vec![sessions, hosts, actions]
    } else {
        vec![hosts, sessions, commands, actions]
    };
    for group in groups {
        let mut matched: Vec<_> = group
            .into_iter()
            .filter_map(|i| fuzzy(query, &format!("{} {}", i.title, i.detail)).map(|score| (score, i)))
            .collect();
        matched.sort_by_key(|(score, _)| *score);
        items.extend(matched.into_iter().map(|(_, item)| item));
    }
    items.truncate(12);
    items
}

pub fn samples() -> Vec<SnippetGroup> {
    [
        (
            "系统",
            vec![("负载", "uptime"), ("磁盘占用", "df -h"), ("内存", "free -h")],
        ),
        (
            "服务",
            vec![
                ("重载 Nginx", "sudo systemctl reload nginx"),
                ("Nginx 错误日志", "tail -n 3 /var/log/nginx/error.log"),
            ],
        ),
        (
            "Docker",
            vec![
                ("容器列表", "docker ps"),
                ("清理悬空镜像", "docker image prune -f"),
            ],
        ),
    ]
    .into_iter()
    .map(|(name, items)| SnippetGroup { id: String::new(),
        name: name.into(),
        items: items
            .into_iter()
            .map(|(name, command)| Snippet { id: String::new(),
                name: name.into(),
                command: command.into(),
            })
            .collect(),
    })
    .collect()
}

pub fn validate(groups: &[SnippetGroup]) -> Result<(), &'static str> {
    for (i, group) in groups.iter().enumerate() {
        if group.name.trim().is_empty() {
            return Err("请填写分组名称");
        }
        if groups[..i].iter().any(|g| g.name.trim() == group.name.trim()) {
            return Err("分组名称不能重复");
        }
        if group
            .items
            .iter()
            .any(|s| s.name.trim().is_empty() || s.command.trim().is_empty())
        {
            return Err("请填写命令名称和命令内容");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn risk_table_matches_prototype_boundaries() {
        for s in [
            "sudo uptime",
            "ls;rm x",
            "true&&reboot",
            "SHUTDOWN now",
            "kill -9 1",
            "pkill foo",
            "killall foo",
            "mkfs /dev/sda",
            "dd if=x",
            "truncate x",
            "chmod 777 x",
            "chown root x",
            "iptables -F",
            "systemctl\trestart nginx",
            "systemctl stop x",
            "systemctl reload x",
            "systemctl disable x",
            "docker rm x",
            "docker stop x",
            "docker kill x",
            "docker image  prune",
            "docker system prune",
            "DROP TABLE a",
            "drop database a",
            "echo rm x",
            "nosystemctl stopX",
        ] {
            assert!(risky(s), "{s}");
        }
        for s in [
            "uptime",
            "free -h",
            "docker ps",
            "systemctl status nginx",
            "rmfile",
            "farm x",
            "echo|rm x",
            "mkfs.ext4 x",
        ] {
            assert!(!risky(s), "{s}");
        }
    }
    #[test]
    fn production_table() {
        for (g, tags, expected) in [
            ("生产环境", vec![], true),
            ("prePROD", vec![], true),
            ("开发", vec!["production".into()], true),
            ("", vec!["生产-db".into()], true),
            ("开发", vec!["test".into()], false),
        ] {
            assert_eq!(production(g, &tags), expected);
        }
    }
    #[test]
    fn quick_connect_table() {
        for s in [
            "root@host",
            "deploy@10.0.1.21:2222",
            "ops@[::1]:2200",
            "ops@web.example.test",
        ] {
            assert!(quick_connect(s).is_some(), "{s}");
        }
        for s in [
            "host",
            "@host",
            "user@",
            "a@b:0",
            "a@b:65536",
            "a@b:-1",
            "a@b:abc",
            "a@b c",
            "a@b@c",
            "a@-oProxyCommand",
        ] {
            assert!(quick_connect(s).is_none(), "{s}");
        }
        let h = quick_connect("user@host:42").unwrap();
        assert_eq!(h.port, 42);
        assert_eq!(h.user.as_deref(), Some("user"));
    }
    #[test]
    fn palette_fuzzy_order_groups_and_cap() {
        let mut hosts: Vec<_> = (0..20)
            .map(|i| Host::new(format!("prod-{i}"), "10.0.0.1"))
            .collect();
        for (i, h) in hosts.iter_mut().enumerate() {
            h.last_connected_at = Some(i as i64);
        }
        let book = HostBook::new(vec![], hosts);
        let rows = palette("", &book, &[], None, &samples(), true, false);
        assert_eq!(rows[0].title, "prod-19");
        assert_eq!(rows.iter().filter(|i| i.group == "最近主机").count(), 4);
        assert_eq!(palette("prd", &book, &[], None, &[], true, false).len(), 12);
        assert_eq!(
            palette("root@new:2222", &book, &[], None, &[], true, false)[0].group,
            "快速连接"
        );
        assert_eq!(
            palette("upt", &book, &[], None, &samples(), true, false)[0].group,
            "快捷命令"
        );
        assert!(fuzzy("xx", "prod").is_none());
        assert!(fuzzy("prod", "prod").unwrap() < fuzzy("prod", "p-r-o-d").unwrap());
    }
    #[test]
    fn grouping_and_management_validation() {
        let mut groups = samples();
        assert_eq!(
            groups.iter().map(|g| g.items.len()).collect::<Vec<_>>(),
            [3, 2, 2]
        );
        assert!(validate(&groups).is_ok());
        groups[1].name = groups[0].name.clone();
        assert!(validate(&groups).is_err());
        groups.pop();
        groups[1].name = "".into();
        assert!(validate(&groups).is_err());
    }
}
