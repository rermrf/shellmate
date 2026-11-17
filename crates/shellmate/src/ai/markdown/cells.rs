//! Project parsed table-cell children into standalone inline Markdown for TextView.
//! TableCell positions include separators; never use their source slices as content.
use super::*;
use markdown::mdast::{Definition, Table};

#[derive(Clone, Default)]
pub(super) struct Cell {
    pub markdown: SharedString,
    pub plain: SharedString,
}

pub(super) fn table_rows(table: &Table, source: &str) -> Vec<Vec<Cell>> {
    // A cell is rendered as its own paragraph, so resolve reference links against
    // the enclosing document before handing it to the paragraph renderer.
    let mut definitions = HashMap::new();
    if has_reference(&table.children)
        && let Ok(root) = markdown::to_mdast(source, &ParseOptions::gfm())
    {
        collect_definitions(&root, &mut definitions);
    }
    table
        .children
        .iter()
        .filter_map(|row| {
            let Node::TableRow(row) = row else {
                return None;
            };
            let mut cells: Vec<_> = row
                .children
                .iter()
                .filter_map(|cell| {
                    let Node::TableCell(cell) = cell else {
                        return None;
                    };
                    let mut rendered = String::new();
                    let mut plain = String::new();
                    for child in &cell.children {
                        inline(child, &definitions, &mut rendered, &mut plain);
                    }
                    Some(Cell {
                        markdown: rendered.into(),
                        plain: plain.into(),
                    })
                })
                .take(table.align.len())
                .collect();
            // GFM ignores excess cells and pads ragged body rows to the header width.
            cells.resize_with(table.align.len(), Cell::default);
            Some(cells)
        })
        .collect()
}

fn has_reference(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| {
        matches!(node, Node::LinkReference(_))
            || node
                .children()
                .is_some_and(|children| has_reference(children))
    })
}

fn collect_definitions(node: &Node, definitions: &mut HashMap<String, Definition>) {
    if let Node::Definition(definition) = node {
        definitions
            .entry(definition.identifier.clone())
            .or_insert_with(|| definition.clone());
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_definitions(child, definitions);
        }
    }
}

fn link_target(url: &str, title: Option<&str>) -> String {
    let mut target = format!("(<{}>", literal(url));
    if let Some(title) = title {
        target.push_str(&format!(" \"{}\"", literal(title)));
    }
    target.push(')');
    target
}

fn inline(
    node: &Node,
    definitions: &HashMap<String, Definition>,
    rendered: &mut String,
    plain: &mut String,
) {
    let (open, close) = match node {
        Node::Text(text) => {
            rendered.push_str(&literal(&text.value));
            plain.push_str(&text.value);
            return;
        }
        Node::InlineCode(code) => {
            // The parser has already decoded table-specific \| escaping. Rebuild
            // the code span from its value, using a delimiter longer than any run
            // of backticks in that value. It is now outside table syntax.
            let ticks = code
                .value
                .split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0)
                + 1;
            let fence = "`".repeat(ticks);
            let pad = if code.value.chars().all(|c| c == ' ') {
                ""
            } else {
                " "
            };
            rendered.push_str(&format!("{fence}{pad}{}{pad}{fence}", code.value));
            plain.push_str(&code.value);
            return;
        }
        Node::Strong(_) => ("**", "**".to_owned()),
        Node::Emphasis(_) => ("*", "*".to_owned()),
        Node::Delete(_) => ("~~", "~~".to_owned()),
        Node::Link(link) => ("[", link_target(&link.url, link.title.as_deref())),
        Node::LinkReference(link) => match definitions.get(&link.identifier) {
            Some(definition) => (
                "[",
                link_target(&definition.url, definition.title.as_deref()),
            ),
            None => ("", String::new()),
        },
        Node::Break(_) => {
            rendered.push_str("\\\n");
            plain.push('\n');
            return;
        }
        Node::Html(html) => {
            rendered.push_str(&literal(&html.value));
            plain.push_str(&html.value);
            return;
        }
        _ => ("", String::new()),
    };
    rendered.push_str(open);
    if let Some(children) = node.children() {
        for child in children {
            inline(child, definitions, rendered, plain);
        }
    }
    if open == "[" {
        rendered.push(']');
    }
    rendered.push_str(&close);
}

#[cfg(test)]
mod tests {
    use super::*;
    use markdown::mdast::AlignKind;

    fn table(source: &str) -> (Vec<Vec<Cell>>, Vec<AlignKind>) {
        let root = markdown::to_mdast(source, &ParseOptions::gfm()).unwrap();
        let table = root
            .children()
            .unwrap()
            .iter()
            .find_map(|node| match node {
                Node::Table(table) => Some(table),
                _ => None,
            })
            .expect("table fixture");
        (table_rows(table, source), table.align.clone())
    }

    fn text(node: &Node) -> String {
        match node {
            Node::Text(text) => text.value.clone(),
            Node::InlineCode(code) => code.value.clone(),
            Node::Break(_) => "\n".into(),
            _ => node
                .children()
                .map(|children| children.iter().map(text).collect())
                .unwrap_or_default(),
        }
    }

    // Assert the content actually supplied to TextView, reparsed by its Markdown
    // parser. Checking Cell::plain alone would miss the original delimiter bug.
    fn rendered(rows: &[Vec<Cell>]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|cell| {
                        let root =
                            markdown::to_mdast(&cell.markdown, &ParseOptions::gfm()).unwrap();
                        assert!(
                            root.children()
                                .unwrap()
                                .iter()
                                .all(|n| matches!(n, Node::Paragraph(_)))
                        );
                        let rendered = text(&root);
                        assert_eq!(rendered, cell.plain.as_ref());
                        rendered
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn leading_and_trailing_pipes_are_not_cell_text() {
        let (rows, _) = table("| Name | Status |\n| --- | --- |\n| nginx | OK |");
        assert_eq!(rendered(&rows), [["Name", "Status"], ["nginx", "OK"]]);
    }

    #[test]
    fn table_without_outer_pipes_has_the_same_cells() {
        let (rows, _) = table("Name | Status\n--- | ---\nnginx | OK");
        assert_eq!(rendered(&rows), [["Name", "Status"], ["nginx", "OK"]]);
    }

    #[test]
    fn escaped_pipe_is_content_and_not_a_separator() {
        let (rows, _) = table("| Value | Status |\n| --- | --- |\n| a\\|b | OK |");
        assert_eq!(rendered(&rows), [["Value", "Status"], ["a|b", "OK"]]);
    }

    #[test]
    fn inline_code_pipe_and_backticks_survive_cell_projection() {
        // GFM requires pipes to be escaped even inside a table's code span.
        let (rows, _) = table("| Command | Status |\n| --- | --- |\n| `a\\|b` | `` `x` `` |");
        assert_eq!(rendered(&rows), [["Command", "Status"], ["a|b", "`x`"]]);
        let root = markdown::to_mdast(&rows[1][0].markdown, &ParseOptions::gfm()).unwrap();
        assert!(
            matches!(&root.children().unwrap()[0].children().unwrap()[0], Node::InlineCode(code) if code.value == "a|b")
        );
    }

    #[test]
    fn empty_cells_preserve_column_positions() {
        let (rows, _) = table("| A | B | C |\n| --- | --- | --- |\n| | middle | |\n| | | |");
        assert_eq!(
            rendered(&rows),
            [["A", "B", "C"], ["", "middle", ""], ["", "", ""]]
        );
    }

    #[test]
    fn ragged_rows_are_padded_to_header_width() {
        let (rows, _) = table("A | B | C\n--- | --- | ---\nonly one\ntwo | cells");
        assert_eq!(
            rendered(&rows),
            [["A", "B", "C"], ["only one", "", ""], ["two", "cells", ""]]
        );
    }

    #[test]
    fn column_alignments_come_from_the_parsed_table() {
        let (rows, aligns) = table(
            "| Left | Center | Right | Default |\n| :--- | :---: | ---: | --- |\n| a | b | c | d |",
        );
        assert_eq!(
            aligns,
            [
                AlignKind::Left,
                AlignKind::Center,
                AlignKind::Right,
                AlignKind::None
            ]
        );
        assert_eq!(
            rendered(&rows),
            [["Left", "Center", "Right", "Default"], ["a", "b", "c", "d"]]
        );
    }

    #[test]
    fn cjk_fixtures_have_clean_cell_text() {
        let (rows, _) = table(crate::proto::ai::MARKDOWN);
        assert_eq!(
            rendered(&rows),
            [
                vec!["名称", "镜像版本", "端口映射", "挂载目录", "健康状态"],
                vec![
                    "nginx",
                    "registry.example.com/nginx:1.27",
                    "443/tcp",
                    "/srv/nginx",
                    "正常"
                ],
                vec![
                    "api",
                    "registry.example.com/api:2.8.1",
                    "8080/tcp",
                    "/srv/api",
                    "正常"
                ],
            ]
        );
        let (rows, _) = table(crate::proto::ai::AGENT_ANALYSIS);
        assert_eq!(rendered(&rows)[0], ["名称", "镜像", "端口"]);
        assert_eq!(
            rendered(&rows)[1],
            ["nginx", "nginx:1.27-alpine", "443/tcp"]
        );
    }

    #[test]
    fn inline_styles_and_links_survive_cell_projection() {
        let (rows, _) = table(
            "| Styles | Link |\n| --- | --- |\n| **粗体** *强调* ~~删除~~ `x` | [文档](https://example.com/?a=1&b=2) |\n| literal \\* and &amp; | [参考][docs] |\n\n[docs]: https://example.com/ref \"标题\"",
        );
        assert_eq!(
            rendered(&rows),
            [
                ["Styles", "Link"],
                ["粗体 强调 删除 x", "文档"],
                ["literal * and &", "参考"]
            ]
        );
        let root = markdown::to_mdast(&rows[1][0].markdown, &ParseOptions::gfm()).unwrap();
        let children = root.children().unwrap()[0].children().unwrap();
        assert!(matches!(children[0], Node::Strong(_)));
        assert!(matches!(children[2], Node::Emphasis(_)));
        assert!(matches!(children[4], Node::Delete(_)));
        assert!(matches!(children[6], Node::InlineCode(_)));
        for (row, url, title) in [
            (1, "https://example.com/?a=1&b=2", None),
            (2, "https://example.com/ref", Some("标题")),
        ] {
            let root = markdown::to_mdast(&rows[row][1].markdown, &ParseOptions::gfm()).unwrap();
            assert!(
                matches!(&root.children().unwrap()[0].children().unwrap()[0], Node::Link(link) if link.url == url && link.title.as_deref() == title)
            );
        }
    }
}
