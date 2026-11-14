//! The few lines of difference the conflict dialog shows: your text against the server's, as
//! `@@ 第 n 行 @@` hunks with two lines of context. No gpui, so it is tested as text.

use similar::{ChangeTag, TextDiff};

/// Lines of context around each change.
const CONTEXT: usize = 2;
/// The most lines of text (changed and context, headers not counted) the snippet shows.
pub const MAX_LINES: usize = 12;
/// A longer line is cut: the box is for a glance, not for reading a minified file.
const MAX_WIDTH: usize = 160;

/// One line of the snippet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffLine {
    /// `@@ 第 12 行 @@`: where the hunk starts, as a line number of your text (1-based; for text
    /// the server has and you have not, the line before which it would stand).
    Hunk(usize),
    /// A line both have.
    Context(String),
    /// A line only you have (red, `-`).
    Removed(String),
    /// A line only the server has (green, `+`).
    Added(String),
    /// The snippet stops here: this many lines of difference are not shown.
    More(usize),
}

impl DiffLine {
    /// The line as the box shows it.
    pub fn text(&self) -> String {
        match self {
            Self::Hunk(n) => format!("@@ 第 {n} 行 @@"),
            Self::Context(line) => format!(" {line}"),
            Self::Removed(line) => format!("-{line}"),
            Self::Added(line) => format!("+{line}"),
            Self::More(n) => format!("… 还有 {n} 行差异未显示"),
        }
    }
}

/// `\r\n` and lone `\r` as `\n`: a file stored with CRLF and the same text with LF do not differ.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_WIDTH {
        return line.to_string();
    }
    let mut clipped: String = line.chars().take(MAX_WIDTH).collect();
    clipped.push('…');
    clipped
}

/// The difference of `local` (what is in the editor) and `remote` (what the server has), empty if the
/// texts are the same. At most [`MAX_LINES`] lines of text; if there are more, the last line is
/// [`DiffLine::More`].
pub fn snippet(local: &str, remote: &str) -> Vec<DiffLine> {
    let (local, remote) = (normalize(local), normalize(remote));
    let diff = TextDiff::from_lines(&local, &remote);
    let mut lines = Vec::new();
    let mut shown = 0;
    // Everything that is not shown, for the closing line.
    let mut hidden = 0;
    for group in diff.grouped_ops(CONTEXT) {
        let changes: Vec<_> = group.iter().flat_map(|op| diff.iter_changes(op)).collect();
        // The line of your text the hunk starts at: the first changed one; for text the server added,
        // the line it stands before (the next line you have, or one past your last).
        let header_line = changes.iter().position(|change| change.tag() != ChangeTag::Equal).map_or(1, |first| {
            match changes[first].tag() {
                ChangeTag::Insert => changes[first..]
                    .iter()
                    .find_map(|change| change.old_index())
                    .map_or(local.lines().count() + 1, |old| old + 1),
                _ => changes[first].old_index().map_or(1, |old| old + 1),
            }
        });
        let group_lines: Vec<DiffLine> = changes
            .iter()
            .map(|change| {
                let text = clip(change.value().trim_end_matches('\n'));
                match change.tag() {
                    ChangeTag::Equal => DiffLine::Context(text),
                    ChangeTag::Delete => DiffLine::Removed(text),
                    ChangeTag::Insert => DiffLine::Added(text),
                }
            })
            .collect();
        if hidden > 0 || shown >= MAX_LINES {
            hidden += group_lines.len();
            continue;
        }
        lines.push(DiffLine::Hunk(header_line));
        for (at, line) in group_lines.iter().enumerate() {
            if shown >= MAX_LINES {
                hidden += group_lines.len() - at;
                break;
            }
            lines.push(line.clone());
            shown += 1;
        }
    }
    if hidden > 0 {
        lines.push(DiffLine::More(hidden));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(local: &str, remote: &str) -> Vec<String> {
        snippet(local, remote).iter().map(DiffLine::text).collect()
    }

    fn numbered(count: usize) -> String {
        (1..=count).map(|n| format!("line {n}\n")).collect()
    }

    #[test]
    fn identical_texts_have_no_difference_even_with_other_line_endings() {
        assert!(snippet("a\nb\n", "a\nb\n").is_empty());
        assert!(snippet("", "").is_empty());
        assert!(snippet("a\r\nb\r\n", "a\nb\n").is_empty(), "CRLF and LF are the same text");
        assert!(snippet("a\nb\n", "a\r\nb\r\n").is_empty());
    }

    #[test]
    fn a_change_is_a_removed_line_and_an_added_line_after_the_context() {
        let local = "a\nb\nproxy_read_timeout 60s;\nc\nd\n";
        let remote = "a\nb\nproxy_read_timeout 120s;\nc\nd\n";
        assert_eq!(
            rendered(local, remote),
            ["@@ 第 3 行 @@", " a", " b", "-proxy_read_timeout 60s;", "+proxy_read_timeout 120s;", " c", " d"],
            "two lines of context on both sides"
        );
    }

    #[test]
    fn the_context_stops_at_the_ends_of_the_text() {
        assert_eq!(rendered("x\ny\nz\n", "X\ny\nz\n"), ["@@ 第 1 行 @@", "-x", "+X", " y", " z"]);
        assert_eq!(rendered("x\ny\nz\n", "x\ny\nZ\n"), ["@@ 第 3 行 @@", " x", " y", "-z", "+Z"]);
    }

    #[test]
    fn a_line_the_server_added_is_labelled_with_the_line_it_stands_before() {
        let local = numbered(6);
        let remote = local.replace("line 4\n", "line 4\nnew\n");
        assert_eq!(
            rendered(&local, &remote),
            ["@@ 第 5 行 @@", " line 3", " line 4", "+new", " line 5", " line 6"],
            "your line 5 is the one after it"
        );
        // At the end of the text it stands after your last line.
        assert_eq!(rendered("a\nb\n", "a\nb\nc\n"), ["@@ 第 3 行 @@", " a", " b", "+c"]);
        // Into an empty file.
        assert_eq!(rendered("", "hello\n"), ["@@ 第 1 行 @@", "+hello"]);
    }

    #[test]
    fn a_line_the_server_removed_is_shown_as_yours() {
        let local = numbered(6);
        let remote = local.replace("line 4\n", "");
        assert_eq!(rendered(&local, &remote), ["@@ 第 4 行 @@", " line 2", " line 3", "-line 4", " line 5", " line 6"]);
        assert_eq!(rendered("only\n", ""), ["@@ 第 1 行 @@", "-only"]);
    }

    #[test]
    fn changes_far_apart_are_separate_hunks_each_with_its_line_number() {
        let local = numbered(30);
        let remote = local.replace("line 5\n", "FIVE\n").replace("line 25\n", "TWENTY-FIVE\n");
        let lines = rendered(&local, &remote);
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("@@")).collect::<Vec<_>>(),
            ["@@ 第 5 行 @@", "@@ 第 25 行 @@"]
        );
        assert!(lines.contains(&"-line 5".to_string()) && lines.contains(&"+FIVE".to_string()));
        assert!(lines.contains(&"-line 25".to_string()) && lines.contains(&"+TWENTY-FIVE".to_string()));
        assert!(!lines.iter().any(|l| l == " line 12"), "the unchanged middle is left out");
        // Changes close together share one hunk.
        let remote = local.replace("line 5\n", "FIVE\n").replace("line 8\n", "EIGHT\n");
        assert_eq!(rendered(&local, &remote).iter().filter(|l| l.starts_with("@@")).count(), 1);
    }

    #[test]
    fn a_long_difference_is_cut_at_twelve_lines_and_says_how_many_are_left() {
        let local = numbered(40);
        let remote: String = (1..=40).map(|n| format!("other {n}\n")).collect();
        let lines = snippet(&local, &remote);
        let shown = lines.iter().filter(|l| !matches!(l, DiffLine::Hunk(_) | DiffLine::More(_))).count();
        assert_eq!(shown, MAX_LINES);
        assert!(matches!(lines.last(), Some(DiffLine::More(n)) if *n == 80 - MAX_LINES), "{:?}", lines.last());
        assert_eq!(lines.last().unwrap().text(), "… 还有 68 行差异未显示");
        assert!(matches!(lines.first(), Some(DiffLine::Hunk(1))));
    }

    #[test]
    fn later_hunks_that_do_not_fit_are_counted_not_shown() {
        // Two hunks of 10 lines each (3 changed lines as 3 removed + 3 added, 2 lines of context each side).
        let local = numbered(60);
        let remote = local
            .replace("line 10\nline 11\nline 12\n", "A\nB\nC\n")
            .replace("line 50\nline 51\nline 52\n", "X\nY\nZ\n");
        let lines = snippet(&local, &remote);
        let shown = lines.iter().filter(|l| !matches!(l, DiffLine::Hunk(_) | DiffLine::More(_))).count();
        assert_eq!(shown, MAX_LINES, "the first hunk whole, the second cut after two lines");
        assert_eq!(lines.iter().filter(|l| matches!(l, DiffLine::Hunk(_))).count(), 2);
        assert_eq!(lines.last(), Some(&DiffLine::More(8)), "{lines:?}");
    }

    #[test]
    fn a_short_difference_is_not_cut() {
        let lines = snippet(&numbered(10), &numbered(10).replace("line 3\n", "THREE\n"));
        assert!(!lines.iter().any(|l| matches!(l, DiffLine::More(_))));
    }

    #[test]
    fn a_very_long_line_is_clipped() {
        let long = "x".repeat(500);
        let lines = snippet(&format!("a\n{long}\n"), "a\nshort\n");
        let removed =
            lines.iter().find_map(|l| if let DiffLine::Removed(t) = l { Some(t.clone()) } else { None }).unwrap();
        assert_eq!(removed.chars().count(), MAX_WIDTH + 1);
        assert!(removed.ends_with('…'));
    }

    #[test]
    fn only_a_changed_line_ending_style_is_not_a_difference_but_a_changed_line_is() {
        let lines = rendered("a\r\nb\r\nc\r\n", "a\nB\nc\n");
        assert_eq!(lines, ["@@ 第 2 行 @@", " a", "-b", "+B", " c"], "compared as normalized text");
    }
}
