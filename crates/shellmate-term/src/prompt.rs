//! Render-only prompt recognition. Signatures contain identity and shape, never grid positions.
use std::{
    ops::Range,
    time::{Duration, Instant},
};

use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line},
    term::{
        Term, TermMode,
        cell::{Cell, Flags},
    },
    vte::ansi::{Color, NamedColor},
};

pub(crate) const QUIET: Duration = Duration::from_millis(100);
const MAX_SIGNATURES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Colon,
    Space,
    Bracket,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Signature {
    identity: String,
    shape: Shape,
}

#[derive(Debug)]
struct Parsed {
    shape: Shape,
    identity: Range<usize>,
    path: Range<usize>,
    end: usize,
}

/// Byte ranges in an anchored prompt prefix; a command may follow `end`.
/// USER/HOST use the ASCII limits in the prompt contract. Paths cannot contain
/// whitespace/control characters. Colon requires ~, ~/… or /…; space requires
/// one component (or ~ or /); bracket accepts either, as RHEL uses a basename.
fn parse(text: &str) -> Option<Parsed> {
    let bytes = text.as_bytes();
    let mut start = 0;
    if text.starts_with('(') {
        let close = text.find(')')?;
        let prefix = &text[1..close];
        if !(1..=40).contains(&prefix.chars().count())
            || prefix.chars().any(char::is_control)
            || bytes.get(close + 1) != Some(&b' ')
        {
            return None;
        }
        start = close + 2;
    }
    let bracket = bytes.get(start) == Some(&b'[');
    start += usize::from(bracket);
    let user_len = bytes
        .get(start..)?
        .iter()
        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(c))
        .count();
    if !(1..=32).contains(&user_len)
        || !(bytes[start].is_ascii_lowercase() || bytes[start] == b'_')
        || bytes.get(start + user_len) != Some(&b'@')
    {
        return None;
    }
    let host = start + user_len + 1;
    let host_len = bytes
        .get(host..)?
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || b".-".contains(c))
        .count();
    if !(1..=253).contains(&host_len) || !bytes[host].is_ascii_alphanumeric() {
        return None;
    }
    let identity_end = host + host_len;
    let shape = match (bracket, bytes.get(identity_end)?) {
        (false, b':') => Shape::Colon,
        (false, b' ') => Shape::Space,
        (true, b' ') => Shape::Bracket,
        _ => return None,
    };
    let path_start = identity_end + 1;
    // The first space terminates the path/marker token, so missing trailing
    // spaces and whitespace inside paths cannot be mistaken for prompts.
    let space = path_start + text.get(path_start..)?.find(' ')?;
    let (path_end, end) = match shape {
        Shape::Colon if space > path_start && matches!(bytes[space - 1], b'$' | b'#') => {
            (space - 1, space + 1)
        }
        Shape::Bracket
            if space >= path_start + 2
                && bytes[space - 2] == b']'
                && matches!(bytes[space - 1], b'$' | b'#') =>
        {
            (space - 2, space + 1)
        }
        Shape::Space
            if matches!(bytes.get(space + 1), Some(b'%' | b'#' | b'$'))
                && bytes.get(space + 2) == Some(&b' ') =>
        {
            (space, space + 3)
        }
        _ => return None,
    };
    let path = &text[path_start..path_end];
    if path.is_empty() || path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let absolute = path == "~" || path.starts_with("~/") || path.starts_with('/');
    let component = !path.contains('/') || path == "/";
    if !match shape {
        Shape::Colon => absolute,
        Shape::Space => component,
        Shape::Bracket => absolute || component,
    } {
        return None;
    }
    Some(Parsed {
        shape,
        identity: start..identity_end,
        path: path_start..path_end,
        end,
    })
}

/// Preserve all server SGR attributes, including background/underline colours.
fn plain(cell: &Cell) -> bool {
    cell.fg == Color::Named(NamedColor::Foreground)
        && cell.bg == Color::Named(NamedColor::Background)
        && cell.underline_color().is_none()
        && (cell.flags
            - (Flags::WRAPLINE
                | Flags::WIDE_CHAR
                | Flags::WIDE_CHAR_SPACER
                | Flags::LEADING_WIDE_CHAR_SPACER))
            .is_empty()
}

fn continuation<T>(term: &Term<T>, line: Line) -> bool {
    line > term.topmost_line()
        && term.grid()[Line(line.0 - 1)][term.last_column()]
            .flags
            .contains(Flags::WRAPLINE)
}

/// Keep byte-to-grid-column mapping so wide paths and combining characters do
/// not move the colour boundary into the delimiter or command.
fn row_text<T>(term: &Term<T>, line: Line, end: usize) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut columns = Vec::new();
    for col in 0..end {
        let cell = &term.grid()[line][Column(col)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let before = text.len();
        text.push(cell.c);
        if let Some(extra) = cell.zerowidth() {
            text.extend(extra);
        }
        columns.resize(columns.len() + text.len() - before, col);
    }
    columns.push(end);
    (text, columns)
}

#[derive(Debug)]
pub(crate) struct RowPrompt {
    identity: Range<usize>,
    path: Range<usize>,
}

impl RowPrompt {
    /// Only the paint attributes are returned; the caller keeps the grid intact.
    pub(crate) fn color(&self, column: usize, cell: &Cell) -> Option<NamedColor> {
        if !plain(cell) {
            return None;
        }
        if self.identity.contains(&column) {
            Some(NamedColor::Green)
        } else if self.path.contains(&column) {
            Some(NamedColor::Blue)
        } else {
            None
        }
    }
}

pub(crate) struct PromptHighlighter {
    pub(crate) enabled: bool,
    signatures: Vec<Signature>,
}

impl Default for PromptHighlighter {
    fn default() -> Self {
        Self {
            enabled: true,
            signatures: Vec::new(),
        }
    }
}

impl PromptHighlighter {
    pub(crate) fn reset(&mut self) {
        self.signatures.clear();
    }

    /// Replay has no idle shell. Count complete, unstyled, column-zero prompt
    /// prefixes in the current primary screen and history. Repeated scans do
    /// not inflate counts; ties use identity order, independent of playback speed.
    /// Require two occurrences per identity to avoid learning one-off quoted output.
    pub(crate) fn learn_replay<T>(&mut self, term: &Term<T>) {
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let mut counts = std::collections::BTreeMap::<String, (usize, Vec<Shape>)>::new();
        for line in term.topmost_line().0..=term.bottommost_line().0 {
            let line = Line(line);
            if continuation(term, line) {
                continue;
            }
            let (text, columns) = row_text(term, line, term.columns());
            let Some(parsed) = parse(&text) else {
                continue;
            };
            // A prefix that ends at the screen edge may have wrapped; do not
            // infer its missing delimiter/space from terminal padding.
            if parsed.end >= columns.len()
                || !(0..columns[parsed.end]).all(|c| plain(&term.grid()[line][Column(c)]))
            {
                continue;
            }
            let entry = counts.entry(text[parsed.identity].to_owned()).or_default();
            entry.0 += 1;
            if !entry.1.contains(&parsed.shape) {
                entry.1.push(parsed.shape);
            }
        }
        let mut counts = counts.into_iter().collect::<Vec<_>>();
        counts.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
        self.signatures = counts
            .into_iter()
            .filter(|(_, (count, _))| *count >= 2)
            .take(MAX_SIGNATURES)
            .flat_map(|(identity, (_, shapes))| {
                shapes.into_iter().map(move |shape| Signature {
                    identity: identity.clone(),
                    shape,
                })
            })
            .collect();
    }

    /// Called after the output idle timer. Learning continues while disabled so
    /// enabling the setting can immediately paint already observed identities.
    pub(crate) fn learn<T>(
        &mut self,
        term: &Term<T>,
        last_output: Option<Instant>,
        now: Instant,
    ) -> bool {
        if term.mode().contains(TermMode::ALT_SCREEN)
            || !last_output.is_some_and(|at| now.saturating_duration_since(at) >= QUIET)
        {
            return false;
        }
        let cursor = term.grid().cursor.point;
        if continuation(term, cursor.line) {
            return false;
        }
        let (text, _) = row_text(term, cursor.line, cursor.column.0);
        let Some(parsed) = parse(&text).filter(|p| p.end == text.len()) else {
            return false;
        };
        if !(0..cursor.column.0).all(|col| plain(&term.grid()[cursor.line][Column(col)])) {
            return false;
        }
        let signature = Signature {
            identity: text[parsed.identity].into(),
            shape: parsed.shape,
        };
        if self.signatures.first() == Some(&signature) {
            return false;
        }
        self.signatures.retain(|s| s != &signature);
        self.signatures.insert(0, signature);
        self.signatures.truncate(MAX_SIGNATURES);
        true
    }

    /// Resolve fresh ranges from each visible row, including scrollback after reflow.
    pub(crate) fn row<T>(&self, term: &Term<T>, line: Line) -> Option<RowPrompt> {
        if !self.enabled
            || self.signatures.is_empty()
            || term.mode().contains(TermMode::ALT_SCREEN)
            || continuation(term, line)
        {
            return None;
        }
        let (text, columns) = row_text(term, line, term.columns());
        let parsed = parse(&text)?;
        // A partially server-styled prompt belongs to the server too. Commands
        // after the delimiter may have any attributes without affecting this.
        if !(0..columns[parsed.end]).all(|col| plain(&term.grid()[line][Column(col)])) {
            return None;
        }
        if !self
            .signatures
            .iter()
            .any(|s| s.shape == parsed.shape && s.identity == text[parsed.identity.clone()])
        {
            return None;
        }
        Some(RowPrompt {
            identity: columns[parsed.identity.start]..columns[parsed.identity.end],
            path: columns[parsed.path.start]..columns[parsed.path.end],
        })
    }
}

#[cfg(test)]
mod tests;
