use super::{Hit, public_url};
use html5ever::{parse_document, tendril::TendrilSink};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
fn name(node: &Handle) -> &str {
    if let NodeData::Element { name, .. } = &node.data {
        name.local.as_ref()
    } else {
        ""
    }
}
fn attr(node: &Handle, key: &str) -> Option<String> {
    if let NodeData::Element { attrs, .. } = &node.data {
        attrs
            .borrow()
            .iter()
            .find(|a| a.name.local.as_ref() == key)
            .map(|a| a.value.to_string())
    } else {
        None
    }
}
fn walk(node: &Handle, depth: usize, visit: &mut impl FnMut(&Handle)) {
    if depth > 128 {
        return;
    }
    visit(node);
    for child in node.children.borrow().iter() {
        walk(child, depth + 1, visit);
    }
}
fn text(node: &Handle) -> String {
    let mut out = String::new();
    walk(node, 0, &mut |node| {
        if let NodeData::Text { contents } = &node.data {
            out.push_str(&contents.borrow());
        }
    });
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
pub fn challenge(body: &str) -> bool {
    let b = body.to_lowercase();
    [
        "anomaly.js",
        "bots use duckduckgo too",
        "verify you are human",
        "cf-chl-",
        "just a moment...",
    ]
    .iter()
    .any(|s| b.contains(s))
}
pub fn duck(body: &str) -> Result<Vec<Hit>, String> {
    let doc = parse_document(RcDom::default(), Default::default()).one(body);
    let mut hits: Vec<Hit> = vec![];
    walk(&doc.document, 0, &mut |node| {
        let class = attr(node, "class").unwrap_or_default();
        if name(node) == "a"
            && class
                .split_whitespace()
                .any(|c| c == "result-link" || c == "result__a")
        {
            let Some(href) = attr(node, "href") else {
                return;
            };
            let base = reqwest::Url::parse("https://lite.duckduckgo.com/").unwrap();
            let Ok(u) = base.join(&href) else {
                return;
            };
            let destination = u
                .query_pairs()
                .find(|(k, _)| k == "uddg")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_else(|| u.to_string());
            let Ok(url) = public_url(&destination) else {
                return;
            };
            if hits.iter().any(|h| h.url == url.as_str()) {
                return;
            }
            hits.push(Hit {
                title: text(node),
                url: url.into(),
                snippet: String::new(),
            });
        } else if class
            .split_whitespace()
            .any(|c| c == "result-snippet" || c == "result__snippet")
            && let Some(hit) = hits.last_mut()
        {
            hit.snippet = super::trim(&text(node), 2000);
        }
    });
    if hits.is_empty() && !body.contains("No results") && !body.contains("no results") {
        return Err("DuckDuckGo 返回格式变化或验证页面，未获得有效搜索结果".into());
    }
    Ok(hits)
}
pub fn extract(body: &str, url: &str) -> Result<(String, String), String> {
    let doc = parse_document(RcDom::default(), Default::default()).one(body);
    let mut title = String::new();
    let mut main = None;
    let mut article = None;
    let mut content = None;
    walk(&doc.document, 0, &mut |node| match name(node) {
        "title" => title = text(node),
        "main" if main.is_none() => main = Some(node.clone()),
        "article" if article.is_none() => article = Some(node.clone()),
        "body" => content = Some(node.clone()),
        _ => {}
    });
    // RcDom's root drop clears descendant children even if an individual node
    // still has an Rc owner. Keep the document alive while extracting its subtree.
    let root = main
        .or(article)
        .or(content)
        .unwrap_or_else(|| doc.document.clone());
    let base = public_url(url)?;
    let mut out = String::new();
    render(&root, 0, &base, &mut out, false);
    if out.len() > super::MAX_BYTES {
        return Err("提取后的正文超过处理上限，请读取更具体的文档页面".into());
    }
    let mut clean = String::new();
    let mut blank = false;
    for line in out.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            if !blank {
                clean.push('\n');
            }
            blank = true;
        } else {
            clean.push_str(line);
            clean.push('\n');
            blank = false;
        }
    }
    Ok((
        if title.is_empty() {
            url.into()
        } else {
            super::trim(&title, 500)
        },
        clean.trim().to_owned(),
    ))
}
fn render(node: &Handle, depth: usize, base: &reqwest::Url, out: &mut String, pre: bool) {
    if depth > 128 || out.len() > super::MAX_BYTES {
        return;
    }
    if let NodeData::Text { contents } = &node.data {
        let text = contents.borrow();
        if pre {
            out.push_str(&text);
        } else {
            for part in text.split_whitespace() {
                if !out.ends_with([' ', '\n']) {
                    out.push(' ');
                }
                out.push_str(part);
            }
        }
        return;
    }
    let tag = name(node);
    if matches!(
        tag,
        "script"
            | "style"
            | "nav"
            | "header"
            | "footer"
            | "aside"
            | "form"
            | "noscript"
            | "svg"
            | "iframe"
    ) {
        return;
    }
    if attr(node, "hidden").is_some() || attr(node, "aria-hidden").as_deref() == Some("true") {
        return;
    }
    let block = matches!(
        tag,
        "p" | "div"
            | "section"
            | "article"
            | "main"
            | "blockquote"
            | "ul"
            | "ol"
            | "table"
            | "tr"
            | "li"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "pre"
    );
    if block {
        out.push('\n');
    }
    match tag {
        "h1" => out.push_str("# "),
        "h2" => out.push_str("## "),
        "h3" | "h4" | "h5" | "h6" => out.push_str("### "),
        "li" => out.push_str("- "),
        "br" => out.push('\n'),
        "pre" => out.push_str("```\n"),
        "code" if !pre => out.push('`'),
        _ => {}
    }
    let link = if tag == "a" {
        attr(node, "href")
            .and_then(|h| base.join(&h).ok())
            .filter(|u| {
                matches!(u.scheme(), "https" | "http")
                    && u.username().is_empty()
                    && u.password().is_none()
            })
    } else {
        None
    };
    if link.is_some() {
        out.push('[');
    }
    for child in node.children.borrow().iter() {
        render(child, depth + 1, base, out, pre || tag == "pre");
    }
    if let Some(link) = link {
        out.push_str("](");
        out.push_str(link.as_str());
        out.push(')');
    }
    match tag {
        "pre" => out.push_str("\n```\n"),
        "code" if !pre => out.push('`'),
        "td" | "th" => out.push_str(" | "),
        _ => {}
    }
    if block {
        out.push('\n');
    }
}
