//! Own the scroll viewport while retaining TextView for selectable rich cells
//! and syntax-highlighted code. No registry patch or dependency fork is needed.
use super::*;
use crate::ai::scroll::ScrollArea;
use gpui::component::text::{MarkdownNode, MarkdownParseContext};
use gpui::{AnyElement, Font, FontWeight, TextAlign, TextRun, div};
use markdown::mdast::AlignKind;

use super::cells::{Cell, table_rows};

enum Block {
    Table {
        rows: Vec<Vec<Cell>>,
        aligns: Vec<AlignKind>,
    },
    Code(String),
}

pub(super) fn parse(node: &Node, context: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
    let (block, text) = match node {
        Node::Table(table) => {
            let rows = table_rows(table, context.source());
            let text = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|c| c.plain.as_ref())
                        .collect::<Vec<_>>()
                        .join("\t")
                })
                .collect::<Vec<_>>()
                .join("\n");
            (
                Block::Table {
                    rows,
                    aligns: table.align.clone(),
                },
                text,
            )
        }
        Node::Code(code) => (Block::Code(code.value.clone()), code.value.clone()),
        _ => return None,
    };
    Some(
        MarkdownNode::new("ai-scroll-block", block)
            .text(text)
            .markdown(context.node_source(node).unwrap_or("").to_owned()),
    )
}

fn text_width(text: &str, font: Font, size: f32, window: &Window) -> f32 {
    let run = TextRun {
        len: text.len(),
        font,
        color: window.text_style().color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    f32::from(
        window
            .text_system()
            .shape_line(text.to_owned().into(), px(size), &[run], None)
            .width(),
    )
}

// Match gpui-base's scrolling table policy: at most roughly two lines, with
// floors between 160 and 480 px, never exceeding a column's intrinsic width.
fn column_floor(width: f32) -> f32 {
    (width / 2.).clamp(160., 480.).min(width)
}

fn text_alignment(align: Option<&AlignKind>) -> TextAlign {
    match align {
        Some(AlignKind::Center) => TextAlign::Center,
        Some(AlignKind::Right) => TextAlign::Right,
        _ => TextAlign::Left,
    }
}

pub(super) fn render(
    scope: &str,
    node: &MarkdownNode,
    selection: Option<gpui::Entity<crate::ai::selection_scroll::SelectionScroll>>,
    fixture: CopyFixture,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let id: SharedString = format!(
        "{scope}:block:{}",
        node.source_range().map_or(0, |r| r.start)
    )
    .into();
    let p = cx.palette().clone();
    match node.data::<Block>().expect("our Markdown parser") {
        Block::Code(code) => {
            let font = gpui::font(MONO_FONT);
            let width = code
                .lines()
                .map(|line| text_width(line, font.clone(), 12.5, window))
                .fold(0., f32::max)
                .ceil()
                + 24.;
            let bg = shellmate_term::colors::to_hsla(cx.terminal_style().palette.background);
            let text = base_view(
                SharedString::from(format!("{id}:text")).into(),
                node.as_markdown().to_owned().into(),
                window,
                cx,
            )
            .selectable(false);
            let selection_layer =
                super::code_selection::overlay(id.clone(), code.clone(), window, cx);
            let block = div()
                .w_full()
                .rounded(px(12.))
                .border_1()
                .border_color(p.line)
                .bg(bg)
                .overflow_hidden()
                .child(
                    ScrollArea::new(
                        id.clone(),
                        div()
                            .relative()
                            .w(px(width))
                            .min_w_full()
                            .child(text)
                            .child(selection_layer),
                        bg,
                    )
                    .when_some(selection.clone(), |area, selection| {
                        area.selection(selection)
                    })
                    .height_limit(320., true),
                );
            #[cfg(debug_assertions)]
            let block = if selection.is_some() {
                crate::debug_input::log_code_bounds(block, id.clone(), window, cx)
            } else {
                block
            };
            if let Some(selection) = selection {
                super::copy::overlay(block, id, code.clone(), selection, fixture, window, cx)
            } else {
                block.into_any_element()
            }
        }
        Block::Table { rows, aligns } => {
            let count = rows.iter().map(Vec::len).max().unwrap_or(0);
            let mut columns = vec![0_f32; count];
            for row in rows {
                for (col, cell) in row.iter().enumerate() {
                    // Measure plain rendered content, not Markdown delimiters.
                    // Bold is a conservative bound for inline emphasis; mono runs
                    // also participate so inline code never loses its last glyph.
                    let mut font = window.text_style().font();
                    font.weight = FontWeight::BOLD;
                    let width = text_width(&cell.plain, font, 13., window).max(text_width(
                        &cell.plain,
                        gpui::font(MONO_FONT),
                        12.5,
                        window,
                    ));
                    columns[col] = columns[col].max((width + 20.).ceil());
                }
            }
            let min_width = columns.iter().map(|w| column_floor(*w)).sum::<f32>()
                + count.saturating_sub(1) as f32;
            let total = columns.iter().sum::<f32>().max(1.);
            let mut track = div().w(px(min_width)).min_w_full();
            for (r, row) in rows.iter().enumerate() {
                let mut line = div()
                    .id(("row", r))
                    .flex()
                    .w_full()
                    .border_color(p.line)
                    .when(r + 1 < rows.len(), |d| d.border_b_1())
                    .when(r == 0, |d| d.bg(p.card2));
                for (c, width) in columns.iter().enumerate() {
                    // As in Base, equal high flex-grow factors shrink only the
                    // widest columns. Tiny slack factors distribute spare room
                    // proportionally once every cell reaches its intrinsic width.
                    let grow = *width / total;
                    let (lead, trail) = match aligns.get(c) {
                        Some(AlignKind::Center) => (grow / 2., grow / 2.),
                        Some(AlignKind::Right) => (grow, 0.),
                        _ => (0., grow),
                    };
                    if lead > 0. {
                        line = line.child(div().flex_basis(px(0.)).flex_grow(lead).flex_shrink(0.));
                    }
                    let mut cell = div()
                        .flex_basis(px(0.))
                        .flex_grow(1e6)
                        .min_w(px(column_floor(*width)))
                        .max_w(px(*width))
                        .overflow_hidden()
                        .px(px(10.))
                        .py(px(6.))
                        .min_h(px(21.45 + 12.));
                    if let Some(value) = row.get(c) {
                        cell = cell.child(
                            base_view(
                                SharedString::from(format!("{id}:{r}:{c}")).into(),
                                value.markdown.clone(),
                                window,
                                cx,
                            )
                            .text_align(text_alignment(aligns.get(c)))
                            .when(r == 0, |v| v.font_weight(FontWeight::MEDIUM))
                            .text_color(if r == 0 {
                                p.tx2
                            } else {
                                p.tx
                            }),
                        );
                    }
                    line = line.child(cell).child(
                        div()
                            .flex_basis(px(0.))
                            .flex_grow(trail)
                            .flex_shrink(0.)
                            .when(c + 1 < count, |d| d.border_r_1().border_color(p.line)),
                    );
                }
                track = track.child(line);
            }
            div()
                .w_full()
                .min_w_0()
                .border_1()
                .border_color(p.line)
                .rounded(px(12.))
                .bg(p.card)
                .overflow_hidden()
                .child(
                    ScrollArea::new(id, track, p.card)
                        .when_some(selection, |area, selection| area.selection(selection)),
                )
                .into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn code_copy_value_is_the_entire_parsed_block_without_fences_or_viewport_clipping() {
        let text = "docker compose -f deploy/docker-compose.production.yml --project-name 服务 --profile workers --env-file deploy/production.env up -d api worker\nprintf '%s' '<not-markdown> & literal'";
        let source = format!("```sh\n{text}\n```");
        let root = markdown::to_mdast(&source, &ParseOptions::gfm()).unwrap();
        let Node::Code(code) = &root.children().unwrap()[0] else {
            panic!("code block");
        };
        let block = Block::Code(code.value.clone());
        let Block::Code(copied) = block else {
            unreachable!()
        };
        assert_eq!(copied, text);
        assert!(copied.len() > 120);
        assert!(copied.ends_with("& literal'"));
    }
    #[test]
    fn adaptive_columns_keep_short_columns_and_bound_wrapping() {
        assert_eq!(column_floor(80.), 80.);
        assert_eq!(column_floor(240.), 160.);
        assert_eq!(column_floor(640.), 320.);
        assert_eq!(column_floor(2000.), 480.);
    }

    #[test]
    fn alignment_applies_to_wrapped_text_inside_each_cell() {
        assert_eq!(text_alignment(Some(&AlignKind::Left)), TextAlign::Left);
        assert_eq!(text_alignment(Some(&AlignKind::Center)), TextAlign::Center);
        assert_eq!(text_alignment(Some(&AlignKind::Right)), TextAlign::Right);
        assert_eq!(text_alignment(Some(&AlignKind::None)), TextAlign::Left);
        assert_eq!(text_alignment(None), TextAlign::Left);
    }
}
