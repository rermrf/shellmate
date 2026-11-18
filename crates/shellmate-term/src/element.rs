//! Paints a terminal grid with gpui.
//!
//! Cells are batched into runs of identical style and shaped once per run.
//! Runs only contain ASCII so the monospace font guarantees the grid stays
//! aligned; anything else (CJK, box drawing, emoji) is shaped per character and
//! placed at its own cell origin, centered when it is narrower than its cells.

use alacritty_terminal::{grid::Dimensions, index::{Column, Line, Point as GridPoint}};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{TermMode, point_to_viewport};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use gpui::{
    App, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity, FocusHandle, Font,
    FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId,
    Pixels, ShapedLine, SharedString, StrikethroughStyle, Style, TextAlign, TextRun,
    UnderlineStyle, Window, fill, outline, point, px, relative, size,
};

use crate::colors::{TerminalPalette, to_hsla};
use crate::terminal::{SearchMatch, Terminal, TerminalSize};
use crate::view::{Geometry, TerminalView};

fn rendered_cursor_shape(
    mode: TermMode,
    shape: CursorShape,
    focused: bool,
    replay: bool,
) -> CursorShape {
    if !mode.contains(TermMode::SHOW_CURSOR) || shape == CursorShape::Hidden {
        CursorShape::Hidden
    } else if replay {
        CursorShape::Block
    } else if focused {
        shape
    } else {
        CursorShape::HollowBlock
    }
}

/// Font and colors used to draw the grid.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalStyle {
    pub font_family: SharedString,
    pub fallbacks: Vec<String>,
    pub font_size: Pixels,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    pub palette: TerminalPalette,
}

impl TerminalStyle {
    pub fn for_replay(mut self) -> Self {
        self.font_size = px(crate::replay::FONT_SIZE);
        self.line_height = crate::replay::LINE_HEIGHT / crate::replay::FONT_SIZE;
        self
    }

    pub fn font(&self, bold: bool, italic: bool) -> Font {
        let mut font = gpui::font(self.font_family.clone());
        font.fallbacks = Some(gpui::FontFallbacks::from_fonts(self.fallbacks.clone()));
        if bold {
            font.weight = FontWeight::BOLD;
        }
        if italic {
            font.style = FontStyle::Italic;
        }
        font
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CellStyle {
    fg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    undercurl: bool,
    strike: bool,
}

fn paint_cell(cell: &Cell, palette: &TerminalPalette, overrides: &Colors, prompt: Option<NamedColor>) -> (CellStyle, Rgb) {
    let mut fg = palette.rgb(&prompt.map_or(cell.fg, Color::Named), overrides);
    let mut bg = palette.rgb(&cell.bg, overrides);
    if cell.flags.contains(Flags::INVERSE) { std::mem::swap(&mut fg, &mut bg); }
    let mut fg = to_hsla(fg);
    if cell.flags.contains(Flags::DIM) { fg.a *= 0.66; }
    (CellStyle {
        fg,
        bold: prompt.is_some() || cell.flags.contains(Flags::BOLD),
        italic: cell.flags.contains(Flags::ITALIC),
        underline: cell.flags.intersects(Flags::ALL_UNDERLINES) && !cell.flags.contains(Flags::UNDERCURL),
        undercurl: cell.flags.contains(Flags::UNDERCURL),
        strike: cell.flags.contains(Flags::STRIKEOUT),
    }, bg)
}

struct Batch {
    line: usize,
    col: usize,
    /// Number of cells the batch covers.
    cells: usize,
    text: String,
    style: CellStyle,
    ascii: bool,
}

struct ShapedText {
    origin: gpui::Point<Pixels>,
    line: ShapedLine,
}

struct CursorLayout {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    color: Hsla,
    /// Character under a block cursor, re-drawn in the background color.
    text: Option<ShapedText>,
}

pub struct LayoutState {
    background: Hsla,
    rects: Vec<(Bounds<Pixels>, Hsla)>,
    highlights: Vec<(Bounds<Pixels>, Hsla)>,
    texts: Vec<ShapedText>,
    cursor: Option<CursorLayout>,
    marked: Option<(ShapedText, Bounds<Pixels>)>,
    line_height: Pixels,
}

pub struct TerminalElement {
    view: Entity<TerminalView>,
    terminal: Entity<Terminal>,
    focus: FocusHandle,
    focused: bool,
    style: TerminalStyle,
    marked_text: Option<String>,
    matches: Vec<SearchMatch>,
    active_match: Option<usize>,
}

impl TerminalElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        view: Entity<TerminalView>,
        terminal: Entity<Terminal>,
        focus: FocusHandle,
        focused: bool,
        style: TerminalStyle,
        marked_text: Option<String>,
        matches: Vec<SearchMatch>,
        active_match: Option<usize>,
    ) -> Self {
        Self { view, terminal, focus, focused, style, marked_text, matches, active_match }
    }

    fn cell_metrics(&self, window: &mut Window) -> (Pixels, Pixels) {
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&self.style.font(false, false));
        let cell_width = text_system
            .advance(font_id, self.style.font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(px(8.));
        let line_height = (self.style.font_size * self.style.line_height).round();
        (cell_width, line_height)
    }

    fn shape(&self, text: String, style: CellStyle, window: &mut Window) -> ShapedLine {
        let run = TextRun {
            len: text.len(),
            font: self.style.font(style.bold, style.italic),
            color: style.fg,
            background_color: None,
            underline: (style.underline || style.undercurl).then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(style.fg),
                wavy: style.undercurl,
            }),
            strikethrough: style
                .strike
                .then_some(StrikethroughStyle { thickness: px(1.), color: Some(style.fg) }),
        };
        window.text_system().shape_line(text.into(), self.style.font_size, &[run], None)
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = LayoutState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        if self.terminal.read(cx).is_replay() {
            self.style = self.style.clone().for_replay();
            let (cell_width, _) = self.cell_metrics(window);
            let terminal = self.terminal.read(cx);
            let term = terminal.term().lock();
            let extent = crate::replay::content_size(&*term, crate::replay::grid_size(terminal.size(), cell_width));
            style.size.width = extent.width.into();
            style.size.height = extent.height.into();
            style.flex_shrink = 0.;
        } else {
            style.size.width = relative(1.).into();
            style.size.height = relative(1.).into();
        }
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (cell_width, line_height) = self.cell_metrics(window);
        let replay=self.terminal.read(cx).is_replay();
        let current = self.terminal.read(cx).size();
        let width = f32::from(bounds.size.width);
        #[cfg(debug_assertions)]
        let width = width + self.view.read(cx).fixture_grid_width_offset;
        let measured = if replay {
            crate::replay::grid_size(current, cell_width)
        } else {
            TerminalSize::from_geometry(width, f32::from(bounds.size.height), cell_width, line_height)
        };
        let resize = crate::resize::layout_resize(
            current, measured, self.view.read(cx).hold_grid_size, self.terminal.read(cx).resize_pending(),
        );
        let grid = resize.unwrap_or(current);
        let (cols, rows) = (grid.cols, grid.rows);
        if let Some(grid) = resize {
            self.terminal.update(cx, |t, cx| {
                t.set_size(grid);
                if grid != current {
                    let terminal=cx.entity();
                    cx.defer(move |cx|terminal.update(cx,|_,cx|cx.emit(crate::TerminalEvent::SizeChanged)));
                }
            });
            if self.terminal.read(cx).resize_pending() { window.request_animation_frame(); }
        }
        self.view.update(cx, |v, _| v.geometry = Some(Geometry { origin: bounds.origin, size: grid }));

        let palette = &self.style.palette;
        let origin = bounds.origin;
        let cell_origin =
            |line: usize, col: usize| point(origin.x + cell_width * col as f32, origin.y + line_height * line as f32);
        let cell_bounds = |line: usize, col: usize, cells: usize| {
            Bounds::new(cell_origin(line, col), size(cell_width * cells as f32, line_height))
        };

        let terminal = self.terminal.read(cx);
        let term_arc = terminal.term().clone();
        let term = term_arc.lock();
        let content = term.renderable_content();
        let display_offset = if replay { term.history_size() } else { content.display_offset };
        let overrides = content.colors;
        let mode = content.mode;
        let default_bg: Rgb = palette.rgb(&alacritty_terminal::vte::ansi::Color::Named(NamedColor::Background), overrides);
        let background = to_hsla(default_bg);

        let mut rects: Vec<(Bounds<Pixels>, Hsla)> = Vec::new();
        let mut open_rect: Option<(usize, usize, usize, Hsla)> = None; // line, col, cells, color
        let mut batches: Vec<Batch> = Vec::new();
        let mut current: Option<Batch> = None;
        let mut prompt = None;

        let clip = window.content_mask().bounds;
        let first_visible = ((f32::from(clip.top() - origin.y) / f32::from(line_height)).floor() as i32).max(0);
        let last_visible = (f32::from(clip.bottom() - origin.y) / f32::from(line_height)).ceil() as i32;
        let top = if replay { first_visible - display_offset as i32 } else { -(display_offset as i32) };
        let bottom = if replay { rows as i32 } else { rows as i32 - display_offset as i32 };
        let bottom = if replay { bottom.min(last_visible - display_offset as i32) } else { bottom };
        for (gline, col) in (top..bottom).flat_map(|line| (0..cols as usize).map(move |col| (Line(line), col))) {
            let line = (gline.0 + display_offset as i32) as usize;
            if col == 0 { prompt = terminal.prompt_highlighter.row(&term, gline); }
            let cell = &term.grid()[gline][Column(col)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let cells = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };

            let prompt_color = prompt.as_ref().and_then(|p| p.color(col, cell));
            let (style, bg) = paint_cell(cell, palette, overrides, prompt_color);

            // Background runs.
            if bg != default_bg {
                let color = to_hsla(bg);
                match &mut open_rect {
                    Some((l, c, n, col_color)) if *l == line && *c + *n == col && *col_color == color => *n += cells,
                    _ => {
                        if let Some((l, c, n, color)) = open_rect.take() {
                            rects.push((cell_bounds(l, c, n), color));
                        }
                        open_rect = Some((line, col, cells, color));
                    }
                }
            } else if let Some((l, c, n, color)) = open_rect.take() {
                rects.push((cell_bounds(l, c, n), color));
            }

            // Text runs.
            let ch = cell.c;
            if ch == ' ' || ch == '\t' || ch == '\0' || cell.flags.contains(Flags::HIDDEN) {
                if let Some(b) = current.take() {
                    batches.push(b);
                }
                continue;
            }
            let ascii = ch.is_ascii() && cells == 1 && cell.zerowidth().is_none();
            let extends = matches!(&current, Some(b) if b.ascii && ascii && b.line == line
                && b.col + b.cells == col && b.style == style);
            if extends {
                let b = current.as_mut().expect("checked above");
                b.text.push(ch);
                b.cells += 1;
            } else {
                if let Some(b) = current.take() {
                    batches.push(b);
                }
                let mut text = String::from(ch);
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra.iter());
                }
                current = Some(Batch { line, col, cells, text, style, ascii });
            }
        }
        if let Some(b) = current.take() {
            batches.push(b);
        }
        if let Some((l, c, n, color)) = open_rect.take() {
            rects.push((cell_bounds(l, c, n), color));
        }

        // Selection and search highlights.
        let rows_usize = if replay { term.total_lines() } else { rows as usize };
        let last_col = cols as usize - 1;
        let mut highlights = Vec::new();
        let mut highlight_range = |start: GridPoint, end: GridPoint, block: bool, color: Hsla| {
            for vline in 0..rows_usize {
                let gline = Line(vline as i32 - display_offset as i32);
                if gline < start.line || gline > end.line {
                    continue;
                }
                let c0 = if block || gline == start.line { start.column.0 } else { 0 };
                let c1 = if block || gline == end.line { end.column.0 } else { last_col };
                if c1 >= c0 {
                    highlights.push((cell_bounds(vline, c0, c1.min(last_col) - c0 + 1), color));
                }
            }
        };
        for (i, m) in self.matches.iter().enumerate() {
            let color = if Some(i) == self.active_match { palette.search_active } else { palette.search_match };
            highlight_range(*m.start(), *m.end(), false, color);
        }
        if let Some(SelectionRange { start, end, is_block }) = content.selection {
            highlight_range(start, end, is_block, palette.selection);
        }

        // Cursor.
        let cursor_rgb = overrides[NamedColor::Cursor as usize].unwrap_or(palette.cursor);
        let cursor_point = content.cursor.point;
        let cursor_vp = point_to_viewport(display_offset, cursor_point);
        let cursor_cell = &term.grid()[cursor_point];
        let cursor_wide = cursor_cell.flags.contains(Flags::WIDE_CHAR);
        let cursor_char = cursor_cell.c;
        let cursor_shape = rendered_cursor_shape(mode, content.cursor.shape, self.focused, replay);
        let show_cursor = cursor_shape != CursorShape::Hidden;
        drop(term);

        let texts = batches
            .into_iter()
            .map(|b| {
                let (line, col, cells) = (b.line, b.col, b.cells);
                let shaped = self.shape(b.text, b.style, window);
                let mut origin = cell_origin(line, col);
                let slot = cell_width * cells as f32;
                if !b.ascii && shaped.width < slot {
                    origin.x += (slot - shaped.width) / 2.;
                }
                ShapedText { origin, line: shaped }
            })
            .collect();

        let marked = self.marked_text.as_ref().filter(|t| !t.is_empty()).and_then(|text| {
            let vp = cursor_vp?;
            let style = CellStyle {
                fg: to_hsla(palette.foreground),
                bold: false,
                italic: false,
                underline: true,
                undercurl: false,
                strike: false,
            };
            let shaped = self.shape(text.clone(), style, window);
            let origin = cell_origin(vp.line, vp.column.0);
            let bg = Bounds::new(origin, size(shaped.width, line_height));
            Some((ShapedText { origin, line: shaped }, bg))
        });

        let cursor = cursor_vp.filter(|_| show_cursor && marked.is_none()).map(|vp| {
            let cells = if cursor_wide { 2 } else { 1 };
            let full = cell_bounds(vp.line, vp.column.0, cells);
            let color = to_hsla(cursor_rgb);
            let bounds = match cursor_shape {
                CursorShape::Beam => Bounds::new(full.origin, size(px(2.), line_height)),
                CursorShape::Underline => Bounds::new(
                    point(full.origin.x, full.origin.y + line_height - px(2.)),
                    size(full.size.width, px(2.)),
                ),
                _ => full,
            };
            let text = (cursor_shape == CursorShape::Block && !cursor_char.is_whitespace()).then(|| {
                let style = CellStyle {
                    fg: background,
                    bold: false,
                    italic: false,
                    underline: false,
                    undercurl: false,
                    strike: false,
                };
                let shaped = self.shape(cursor_char.to_string(), style, window);
                let mut origin = full.origin;
                if shaped.width < full.size.width {
                    origin.x += (full.size.width - shaped.width) / 2.;
                }
                ShapedText { origin, line: shaped }
            });
            CursorLayout { bounds, shape: cursor_shape, color, text }
        });

        LayoutState { background, rects, highlights, texts, cursor, marked, line_height }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.handle_input(&self.focus, ElementInputHandler::new(bounds, self.view.clone()), cx);

        // Let wide glyphs and the cursor bleed into the padding instead of clipping them.
        let mask = ContentMask { bounds: bounds.dilate(px(8.)) };
        window.with_content_mask(Some(mask), |window| {
            for (rect, color) in &layout.rects {
                window.paint_quad(fill(*rect, *color));
            }
            for (rect, color) in &layout.highlights {
                window.paint_quad(fill(*rect, *color));
            }
            for text in &layout.texts {
                text.line.paint(text.origin, layout.line_height, TextAlign::Left, None, window, cx).ok();
            }
            if let Some(cursor) = &layout.cursor {
                if cursor.shape == CursorShape::HollowBlock {
                    window.paint_quad(outline(cursor.bounds, cursor.color, gpui::BorderStyle::Solid));
                } else {
                    window.paint_quad(fill(cursor.bounds, cursor.color));
                }
                if let Some(text) = &cursor.text {
                    text.line.paint(text.origin, layout.line_height, TextAlign::Left, None, window, cx).ok();
                }
            }
            if let Some((text, bg)) = &layout.marked {
                window.paint_quad(fill(*bg, layout.background));
                text.line.paint(text.origin, layout.line_height, TextAlign::Left, None, window, cx).ok();
            }
        });
    }
}

#[cfg(test)]
mod replay_cursor_tests {
    use super::*;
    use crate::session_log::{Silent, Size};
    use alacritty_terminal::{
        term::{Config, Term},
        vte::ansi::Processor,
    };

    #[test]
    fn replay_cursor_stays_block_without_focus_and_respects_dectcem() {
        let mut term = Term::new(
            Config::default(),
            &Size { cols: 100, rows: 26 },
            Silent,
        );
        let mut parser: Processor = Processor::new();
        let prompt = "deploy@prod-web-01:/var/www/app$ ";
        parser.advance(&mut term, prompt.as_bytes());
        assert_eq!(term.grid().cursor.point.column.0, prompt.len());
        assert_eq!(term.grid().cursor.point.line.0, 0);
        let shape = |term: &Term<Silent>, focused, replay| {
            rendered_cursor_shape(
                *term.mode(),
                term.renderable_content().cursor.shape,
                focused,
                replay,
            )
        };
        for focused in [false, true] {
            assert_eq!(shape(&term, focused, true), CursorShape::Block);
        }
        parser.advance(&mut term, b"\x1b[?25l");
        assert_eq!(shape(&term, false, true), CursorShape::Hidden);
        parser.advance(&mut term, b"\x1b[?25h\x1b[5 q");
        assert_eq!(shape(&term, false, true), CursorShape::Block);
        assert_eq!(shape(&term, true, false), CursorShape::Beam);
        assert_eq!(shape(&term, false, false), CursorShape::HollowBlock);
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use crate::{prompt::{PromptHighlighter, QUIET}, session_log::{Silent, Size}};
    use alacritty_terminal::{grid::Dimensions, index::Column, term::{Config, Term}, vte::ansi::Processor};
    use std::time::Instant;

    #[test]
    fn prompt_paint_exact_segments_and_weights_in_both_themes() {
        for palette in [TerminalPalette::light(), TerminalPalette::dark()] {
            for (prompt, green, blue) in [
                ("root@host:~/app# ", 0..9, 10..15),
                ("(venv) deploy@web:~/app$ ", 7..17, 18..23),
                ("emoji@mac ~ % ", 0..9, 10..11),
                ("[root@centos7 ~]# ", 1..13, 14..15),
            ] {
                let mut term = Term::new(Config::default(), &Size { cols: 80, rows: 4 }, Silent);
                let mut parser: Processor = Processor::new();
                parser.advance(&mut term, prompt.as_bytes());
                let mut h = PromptHighlighter::default();
                let now = Instant::now();
                assert!(h.learn(&term, Some(now - QUIET), now));
                parser.advance(&mut term, b"cd /var/www/app");
                let row = h.row(&term, Line(0)).unwrap();
                let terminal_style = TerminalStyle {
                    font_family: "monospace".into(), fallbacks: Vec::new(), font_size: px(13.),
                    line_height: 1.5, palette: palette.clone(),
                };
                for column in 0..prompt.len() + 15 {
                    let cell = &term.grid()[Line(0)][Column(column)];
                    let (style, bg) = paint_cell(cell, &palette, term.colors(), row.color(column, cell));
                    let expected = if green.contains(&column) { Some(palette.ansi[2]) }
                        else if blue.contains(&column) { Some(palette.ansi[4]) } else { None };
                    assert_eq!(style.fg, to_hsla(expected.unwrap_or(palette.foreground)), "{prompt:?} column {column}");
                    assert_eq!(style.bold, expected.is_some(), "{prompt:?} column {column}");
                    assert_eq!(terminal_style.font(style.bold, style.italic).weight,
                        if expected.is_some() { FontWeight::BOLD } else { FontWeight::NORMAL });
                    assert_eq!(bg, palette.background);
                }
                assert_eq!(term.columns(), 80);
            }
        }
    }

    #[test]
    fn prompt_paint_uses_terminal_palette_overrides() {
        let mut colors = Colors::default();
        let green = Rgb { r: 12, g: 34, b: 56 };
        let blue = Rgb { r: 78, g: 90, b: 12 };
        colors[NamedColor::Green] = Some(green);
        colors[NamedColor::Blue] = Some(blue);
        for (named, rgb) in [(NamedColor::Green, green), (NamedColor::Blue, blue)] {
            let (style, _) = paint_cell(&Cell::default(), &TerminalPalette::dark(), &colors, Some(named));
            assert_eq!(style.fg, to_hsla(rgb));
            assert!(style.bold);
        }
    }
}

