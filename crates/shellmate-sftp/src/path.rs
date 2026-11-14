//! Remote paths. Everything on the server is a POSIX path string: `/` separates, no drive
//! letters, no `\`. Never use `std::path::Path` for these (it follows the local OS).

/// Joins `name` onto `base`. An absolute `name` replaces `base`; an empty one leaves it.
///
/// ```
/// use shellmate_sftp::path::join;
/// assert_eq!(join("/home/me", "docs"), "/home/me/docs");
/// assert_eq!(join("/", "etc"), "/etc");
/// assert_eq!(join("/a/", "b"), "/a/b");
/// assert_eq!(join("/a", "/b"), "/b");
/// ```
pub fn join(base: &str, name: &str) -> String {
    if name.starts_with('/') {
        return name.to_string();
    }
    if name.is_empty() {
        return base.to_string();
    }
    if base.is_empty() {
        return name.to_string();
    }
    if base.ends_with('/') { format!("{base}{name}") } else { format!("{base}/{name}") }
}

/// Whether the path starts at the root.
pub fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
}

/// The last component, ignoring trailing slashes. `None` for the root, an empty path, `.` and `..`.
///
/// ```
/// use shellmate_sftp::path::file_name;
/// assert_eq!(file_name("/home/me/报告.txt"), Some("报告.txt"));
/// assert_eq!(file_name("/home/me/"), Some("me"));
/// assert_eq!(file_name("/"), None);
/// assert_eq!(file_name("/a/.."), None);
/// ```
pub fn file_name(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    (!matches!(name, "" | "." | "..")).then_some(name)
}

/// Everything before the last component, without a trailing slash. `None` for the root and the
/// empty path; a single relative name has the parent `.`.
///
/// ```
/// use shellmate_sftp::path::parent;
/// assert_eq!(parent("/home/me/x.txt").as_deref(), Some("/home/me"));
/// assert_eq!(parent("/home").as_deref(), Some("/"));
/// assert_eq!(parent("/"), None);
/// assert_eq!(parent("/a/b/").as_deref(), Some("/a"));
/// assert_eq!(parent("a").as_deref(), Some("."));
/// ```
pub fn parent(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        // "" or only slashes: the root (or nothing) has no parent.
        return None;
    }
    match trimmed.rfind('/') {
        None => Some(".".to_string()),
        Some(at) => {
            let head = trimmed[..at].trim_end_matches('/');
            Some(if head.is_empty() { "/".to_string() } else { head.to_string() })
        }
    }
}

/// Resolves `.`, `..` and repeated slashes without asking the server. `..` above the root stays
/// at the root; relative paths keep leading `..`. The result has no trailing slash (except `/`).
///
/// ```
/// use shellmate_sftp::path::normalize;
/// assert_eq!(normalize("/a/./b//c/../d/"), "/a/b/d");
/// assert_eq!(normalize("/../.."), "/");
/// assert_eq!(normalize("a/../../b"), "../b");
/// assert_eq!(normalize(""), ".");
/// ```
pub fn normalize(path: &str) -> String {
    let absolute = is_absolute(path);
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ if absolute => {}
                _ => parts.push(".."),
            },
            other => parts.push(other),
        }
    }
    match (absolute, parts.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{}", parts.join("/")),
        (false, true) => ".".to_string(),
        (false, false) => parts.join("/"),
    }
}

/// The path split into breadcrumbs: each name with the full path up to it. An absolute path
/// starts with the root (`/`).
///
/// ```
/// use shellmate_sftp::path::breadcrumbs;
/// let crumbs = breadcrumbs("/home/用户");
/// assert_eq!(crumbs, [("/".to_string(), "/".to_string()), ("home".into(), "/home".into()), ("用户".into(), "/home/用户".into())]);
/// ```
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let normalized = normalize(path);
    let mut crumbs = Vec::new();
    let mut current = String::new();
    if is_absolute(&normalized) {
        crumbs.push(("/".to_string(), "/".to_string()));
        current.push('/');
    }
    for part in normalized.split('/').filter(|part| !part.is_empty() && *part != ".") {
        current = join(&current, part);
        crumbs.push((part.to_string(), current.clone()));
    }
    crumbs
}

/// Whether `path` is `ancestor` itself or lies below it (compared component-wise after
/// normalizing both, so `/ab` is not inside `/a`).
pub fn is_within(path: &str, ancestor: &str) -> bool {
    let (path, ancestor) = (normalize(path), normalize(ancestor));
    if ancestor == "/" {
        return is_absolute(&path);
    }
    path == ancestor || path.strip_prefix(&ancestor).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_handles_slashes_and_absolute_names() {
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(join("/a/", "b"), "/a/b");
        assert_eq!(join("/a//", "b"), "/a//b", "joining does not rewrite what is already there");
        assert_eq!(join("/a", "/etc"), "/etc");
        assert_eq!(join("/a", ""), "/a");
        assert_eq!(join("", "b"), "b");
        assert_eq!(join("/", ""), "/");
        assert_eq!(join("/home/用户", "文档 (1).txt"), "/home/用户/文档 (1).txt");
        assert_eq!(join("/a", ".."), "/a/..", "joining is textual; normalize resolves");
    }

    #[test]
    fn file_name_ignores_trailing_slashes() {
        assert_eq!(file_name("/a/b"), Some("b"));
        assert_eq!(file_name("/a/b/"), Some("b"));
        assert_eq!(file_name("/a/b///"), Some("b"));
        assert_eq!(file_name("b"), Some("b"));
        assert_eq!(file_name("/"), None);
        assert_eq!(file_name("//"), None);
        assert_eq!(file_name(""), None);
        assert_eq!(file_name("/a/."), None);
        assert_eq!(file_name("/a/.."), None);
        assert_eq!(file_name("/a/.hidden"), Some(".hidden"));
        assert_eq!(file_name("/a/..."), Some("..."));
        assert_eq!(file_name("/home/用户/报告.txt"), Some("报告.txt"));
        assert_eq!(file_name("/home/用户/"), Some("用户"));
    }

    #[test]
    fn parent_walks_up_to_the_root() {
        assert_eq!(parent("/a/b/c").as_deref(), Some("/a/b"));
        assert_eq!(parent("/a/b/").as_deref(), Some("/a"));
        assert_eq!(parent("/a").as_deref(), Some("/"));
        assert_eq!(parent("//a").as_deref(), Some("/"));
        assert_eq!(parent("/"), None);
        assert_eq!(parent("///"), None);
        assert_eq!(parent(""), None);
        assert_eq!(parent("a/b").as_deref(), Some("a"));
        assert_eq!(parent("a").as_deref(), Some("."));
        assert_eq!(parent("/a//b").as_deref(), Some("/a"));
        assert_eq!(parent("/home/用户/文档").as_deref(), Some("/home/用户"));
        assert_eq!(parent("/a/..").as_deref(), Some("/a"), "textual: normalize first to go further");
    }

    #[test]
    fn normalize_resolves_dots_and_slashes() {
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("//"), "/");
        assert_eq!(normalize("/a/b/"), "/a/b");
        assert_eq!(normalize("/a/./b"), "/a/b");
        assert_eq!(normalize("/a/b/../c"), "/a/c");
        assert_eq!(normalize("/a/b/../../.."), "/");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("/a/../.."), "/");
        assert_eq!(normalize("a/b/.."), "a");
        assert_eq!(normalize("a/.."), ".");
        assert_eq!(normalize("../a"), "../a");
        assert_eq!(normalize("a/../../b"), "../b");
        assert_eq!(normalize("../.."), "../..");
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("."), ".");
        assert_eq!(normalize("/home/用户/../用户/文档/"), "/home/用户/文档");
        assert_eq!(normalize("/a/.../b"), "/a/.../b", "three dots are an ordinary name");
    }

    #[test]
    fn breadcrumbs_lead_from_the_root_to_the_leaf() {
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(breadcrumbs("/"), [pair("/", "/")]);
        assert_eq!(breadcrumbs("/home"), [pair("/", "/"), pair("home", "/home")]);
        assert_eq!(
            breadcrumbs("/home/用户/文档/"),
            [pair("/", "/"), pair("home", "/home"), pair("用户", "/home/用户"), pair("文档", "/home/用户/文档")]
        );
        assert_eq!(breadcrumbs("/a/../b"), [pair("/", "/"), pair("b", "/b")]);
        assert_eq!(breadcrumbs("a/b"), [pair("a", "a"), pair("b", "a/b")]);
    }

    #[test]
    fn is_within_compares_whole_components() {
        assert!(is_within("/a/b", "/a"));
        assert!(is_within("/a", "/a"));
        assert!(is_within("/a/b/", "/a/"));
        assert!(!is_within("/ab", "/a"));
        assert!(!is_within("/", "/a"));
        assert!(is_within("/a", "/"));
        assert!(is_within("/a/../b", "/b"));
        assert!(!is_within("/a/../b", "/a"));
        assert!(is_within("/用户/文档", "/用户"));
        assert!(!is_within("a", "/"), "a relative path is not below the root");
    }
}
