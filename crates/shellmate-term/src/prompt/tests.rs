use super::*;
use crate::session_log::{Silent, Size};
use alacritty_terminal::{term::Config, vte::ansi::Processor};

fn grid(text: &str, cols: usize, rows: usize) -> Term<Silent> {
    let mut term = Term::new(Config::default(), &Size { cols, rows }, Silent);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, text.as_bytes());
    term
}

fn learn(highlighter: &mut PromptHighlighter, term: &Term<Silent>) -> bool {
    let now = Instant::now();
    highlighter.learn(term, Some(now - QUIET), now)
}

#[test]
fn shape_parser_positive_table() {
    for (text, shape, identity, path, command) in [
        (
            "root@serv2147439633:~/wen/cpa# docker ps",
            Shape::Colon,
            "root@serv2147439633",
            "~/wen/cpa",
            "docker ps",
        ),
        (
            "deploy@prod-web-01:/var/www/app$ cd /tmp",
            Shape::Colon,
            "deploy@prod-web-01",
            "/var/www/app",
            "cd /tmp",
        ),
        (
            "(venv) deploy@web:~/app$ ",
            Shape::Colon,
            "deploy@web",
            "~/app",
            "",
        ),
        (
            "(base) _a.b-c@Host-01.example:/.a-b/c.d# ",
            Shape::Colon,
            "_a.b-c@Host-01.example",
            "/.a-b/c.d",
            "",
        ),
        (
            "emoji@emojideMac-mini ~ % ls",
            Shape::Space,
            "emoji@emojideMac-mini",
            "~",
            "ls",
        ),
        (
            "emoji@Mac app.v2-dev $ ",
            Shape::Space,
            "emoji@Mac",
            "app.v2-dev",
            "",
        ),
        ("(base) root@mac / # ", Shape::Space, "root@mac", "/", ""),
        (
            "[root@centos7 ~]# uptime",
            Shape::Bracket,
            "root@centos7",
            "~",
            "uptime",
        ),
        (
            "(venv) [deploy@web app.v2-dev]$ ",
            Shape::Bracket,
            "deploy@web",
            "app.v2-dev",
            "",
        ),
        (
            "[root@centos7 /srv/app.v2-dev]# ",
            Shape::Bracket,
            "root@centos7",
            "/srv/app.v2-dev",
            "",
        ),
        (
            "(环境) deploy@web:~/项目/e\u{301}$ ",
            Shape::Colon,
            "deploy@web",
            "~/项目/e\u{301}",
            "",
        ),
    ] {
        let parsed = parse(text).unwrap_or_else(|| panic!("{text:?}"));
        assert_eq!(parsed.shape, shape, "{text:?}");
        assert_eq!(&text[parsed.identity], identity);
        assert_eq!(&text[parsed.path], path);
        assert_eq!(&text[parsed.end..], command);
    }
    let (user, host, prefix) = ("a".repeat(32), "H".repeat(253), "v".repeat(40));
    assert!(parse(&format!("({prefix}) {user}@{host}:~$ ")).is_some());
}

#[test]
fn shape_parser_negative_table() {
    for text in [
        "",
        "root@host:~#",
        "root@host:~#ls",
        "root@host ~ %",
        "[root@host ~]#",
        "prefix root@host:~# ",
        " root@host:~# ",
        "mail root@example.com",
        "user@example.com",
        "https://user@host:~/app# ",
        "ssh://user@host/path",
        "root@host:relative# ",
        "root@host:~other# ",
        "root@host:~/has space# ",
        "root@host:~% ",
        "root@host ~/app % ",
        "root@host /srv/app % ",
        "[root@host ~]% ",
        "[root@host ~# ",
        "root@host ]# ",
        "root@host:# ",
        "[root@host ]# ",
        "root@host  % ",
        "root@host:~#\t",
        "root@host:~/a\tb# ",
        "Root@host:~# ",
        "1root@host:~# ",
        "root@-host:~# ",
        "root@host_name:~# ",
        "root@:~# ",
        "@host:~# ",
        "() root@host:~# ",
        "(venv)root@host:~# ",
        "(a\nb) root@host:~# ",
        "(base) (venv) root@host:~# ",
    ] {
        assert!(parse(text).is_none(), "{text:?}");
    }
    for text in [
        format!("{}@host:~# ", "a".repeat(33)),
        format!("a@{}:~# ", "h".repeat(254)),
        format!("({}) a@h:~# ", "v".repeat(41)),
    ] {
        assert!(parse(&text).is_none(), "{text:?}");
    }
}

#[test]
fn learns_only_idle_default_primary_cursor_prompt() {
    let mut h = PromptHighlighter::default();
    let term = grid("root@host:~# ", 80, 4);
    let now = Instant::now();
    assert!(!h.learn(&term, None, now));
    assert!(!h.learn(&term, Some(now - Duration::from_millis(99)), now));
    assert!(h.learn(&term, Some(now - QUIET), now));
    for text in [
        "\x1b[?1049hother@host:~# ",
        "\x1b[31mother@host:~# ",
        "\x1b[1mother@host:~# ",
        "other@host:\x1b[34m~\x1b[0m# ",
        "\x1b[44mother@host:~# ",
        "other@host:~# ls",
    ] {
        assert!(!learn(&mut h, &grid(text, 80, 4)), "{text:?}");
    }
    assert_eq!(h.signatures.len(), 1);
    assert!(!learn(
        &mut h,
        &grid("xxxxxxxxxxxxxxxxroot@host:~# ", 16, 4)
    ));
}

#[test]
fn su_new_identity_and_shape_lru_is_bounded_to_four() {
    let mut h = PromptHighlighter::default();
    let mut term = grid("deploy@host:~$ ", 80, 10);
    assert!(learn(&mut h, &term));
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, b"su -\r\nroot@host:~# ");
    assert!(learn(&mut h, &term));
    assert_eq!(h.signatures[0].identity, "root@host");
    assert_eq!(h.signatures[1].identity, "deploy@host");
    for text in ["root@host ~ # ", "[root@host ~]# ", "new@remote:~$ "] {
        assert!(learn(&mut h, &grid(text, 80, 4)));
    }
    assert_eq!(h.signatures.len(), 4);
    assert!(!h.signatures.iter().any(|s| s.identity == "deploy@host"));
    assert!(learn(&mut h, &grid("root@host:/other# ", 80, 4)));
    assert_eq!(
        h.signatures[0],
        Signature {
            identity: "root@host".into(),
            shape: Shape::Colon
        }
    );
    assert!(!learn(&mut h, &grid("(base) root@host:~/app# ", 80, 4)));
    assert_eq!(h.signatures.len(), 4);
    h.enabled = false;
    h.reset();
    assert!(h.signatures.is_empty());
    assert!(!h.enabled);
}

#[test]
fn matching_requires_learned_identity_shape_and_column_zero() {
    let mut h = PromptHighlighter::default();
    learn(&mut h, &grid("root@host:~# ", 80, 4));
    for text in [
        "root@other:~# ",
        "other@host:~# ",
        "root@host ~ # ",
        "[root@host ~]# ",
        "output root@host:~# ",
        " root@host:~# ",
    ] {
        assert!(h.row(&grid(text, 80, 4), Line(0)).is_none(), "{text:?}");
    }
    assert!(
        h.row(&grid("(venv) root@host:/new.path-dir$ ls", 80, 4), Line(0))
            .is_some()
    );
    assert!(
        h.row(&grid("\x1b[?1049hroot@host:~# ", 80, 4), Line(0))
            .is_none()
    );
}

#[test]
fn existing_sgr_cells_and_partly_styled_prompts_are_untouched() {
    let mut h = PromptHighlighter::default();
    let term = grid("root@host:~# ", 80, 4);
    learn(&mut h, &term);
    let row = h.row(&term, Line(0)).unwrap();
    for sgr in [
        "31",
        "1",
        "2",
        "3",
        "4",
        "7",
        "8",
        "9",
        "44",
        "38;5;2",
        "38;2;1;2;3",
        "58;5;2",
    ] {
        let styled = grid(&format!("\x1b[{sgr}mroot@host:~# "), 80, 4);
        assert!(
            row.color(0, &styled.grid()[Line(0)][Column(0)]).is_none(),
            "{sgr}"
        );
        assert!(h.row(&styled, Line(0)).is_none(), "{sgr}");
    }
    assert!(
        h.row(&grid("\x1b[35mroot@host\x1b[0m:~# ", 80, 4), Line(0))
            .is_none()
    );
    assert!(
        h.row(&grid("root@host:~# \x1b[31mls", 80, 4), Line(0))
            .is_some()
    );
}

#[test]
fn unicode_prefix_and_path_use_grid_columns() {
    let mut h = PromptHighlighter::default();
    let term = grid("(环境) root@host:~/项目/e\u{301}# ", 80, 4);
    assert!(learn(&mut h, &term));
    let row = h.row(&term, Line(0)).unwrap();
    assert_eq!(row.identity, 7..16);
    assert_eq!(row.path, 17..25);
    assert_eq!(row.color(25, &term.grid()[Line(0)][Column(25)]), None);
}

#[test]
fn scrollback_resize_reflow_and_continuations_match_at_render_time() {
    let mut h = PromptHighlighter::default();
    let prompt = "root@host:~# ";
    learn(&mut h, &grid(prompt, 80, 4));
    let text = format!(
        "{prompt}echo {}{prompt}fake with a long continuation command\r\noutput\r\n{prompt}ls\r\nfile\r\n{prompt}",
        "x".repeat(80 - prompt.len() - 5)
    );
    let mut term = grid(&text, 80, 4);
    // The command deliberately puts a prompt-looking token at a wrapped column 0.
    for cols in [80, 40, 100, 32, 80] {
        term.resize(Size { cols, rows: 4 });
        term.scroll_display(alacritty_terminal::grid::Scroll::Top);
        let mut count = 0;
        let mut continuations = 0;
        for line in term.topmost_line().0..=term.bottommost_line().0 {
            let line = Line(line);
            if continuation(&term, line) {
                continuations += 1;
                assert!(h.row(&term, line).is_none());
            }
            if let Some(row) = h.row(&term, line) {
                count += 1;
                assert_eq!(
                    row.color(0, &term.grid()[line][Column(0)]),
                    Some(NamedColor::Green)
                );
                assert_eq!(
                    row.color(10, &term.grid()[line][Column(10)]),
                    Some(NamedColor::Blue)
                );
            }
        }
        assert_eq!(count, 3, "width {cols}");
        assert!(continuations > 0);
        assert!(
            h.row(&term, term.topmost_line()).is_some(),
            "scrollback width {cols}"
        );
    }
}

#[test]
fn toggle_off_restores_plain_rendering_without_relearning() {
    let mut h = PromptHighlighter::default();
    let term = grid("root@host:~# ", 80, 4);
    assert!(h.enabled);
    learn(&mut h, &term);
    assert!(h.row(&term, Line(0)).is_some());
    h.enabled = false;
    assert!(h.row(&term, Line(0)).is_none());
    h.enabled = true;
    assert!(h.row(&term, Line(0)).is_some());
}

#[derive(Debug, PartialEq)]
struct Outputs {
    copy: Option<String>,
    search: Vec<RangeInclusive<Point>>,
    cast: Vec<u8>,
    log: Vec<crate::session_log::LogLine>,
    ai: String,
    history: Vec<String>,
    paste: String,
}
use alacritty_terminal::index::{Direction, Point, Side};
use std::ops::RangeInclusive;

fn captured_outputs(enabled: bool) -> Outputs {
    use alacritty_terminal::{
        selection::{Selection, SelectionType},
        term::search::{RegexIter, RegexSearch},
    };
    let mut term = grid("", 80, 4);
    let mut parser = Processor::new();
    let mut h = PromptHighlighter {
        enabled,
        ..Default::default()
    };
    let mut capture = crate::history::Capture::default();
    let mut writer =
        crate::cast::Writer::new(Vec::new(), &crate::cast::Header::new(80, 4)).unwrap();
    let mut logger = crate::session_log::LineTracker::new(80, 4);
    let mut log = Vec::new();
    let mut history = Vec::new();
    for (i, bytes) in [
        b"root@host:~# ".as_slice(),
        b"echo hello",
        b"\r\nhello\r\nroot@host:~# ",
    ]
    .into_iter()
    .enumerate()
    {
        if i == 1 {
            capture.input(&term, bytes);
        }
        if i == 2 {
            capture.input(&term, b"\r");
        }
        history.extend(capture.advance(&mut term, &mut parser, bytes));
        writer.output(i as f64, bytes).unwrap();
        log.extend(logger.feed_at(bytes, 1_700_000_000));
        learn(&mut h, &term);
        let before = term.grid().clone();
        for indexed in term.renderable_content().display_iter {
            if let Some(row) = h.row(&term, indexed.point.line) {
                let _ = row.color(indexed.point.column.0, indexed.cell);
            }
        }
        assert_eq!(*term.grid(), before);
    }
    log.extend(logger.finish());
    let start = Point::new(Line(0), Column(0));
    let end = Point::new(Line(1), Column(4));
    let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
    selection.update(end, Side::Right);
    term.selection = Some(selection);
    let copy = term.selection_to_string();
    let paste = crate::keys::paste_payload(copy.as_deref().unwrap(), term.mode());
    let mut regex = RegexSearch::new("root@host").unwrap();
    let search = RegexIter::new(
        start,
        Point::new(term.bottommost_line(), term.last_column()),
        Direction::Right,
        &term,
        &mut regex,
    )
    .collect();
    Outputs {
        copy,
        paste,
        search,
        cast: writer.finish(3.).unwrap().0,
        log,
        ai: crate::replay::recent_text(&term, 40),
        history,
    }
}

#[test]
fn render_only_preserves_copy_search_cast_logs_ai_history_and_paste() {
    let off = captured_outputs(false);
    let on = captured_outputs(true);
    assert_eq!(on, off);
    assert_eq!(on.copy.as_deref(), Some("root@host:~# echo hello\nhello"));
    assert_eq!(on.search.len(), 2);
    assert_eq!(on.history, ["echo hello"]);
    assert!(on.ai.contains("root@host:~# echo hello"));
    let cast = crate::cast::Cast::read(on.cast.as_slice()).unwrap();
    assert_eq!(
        cast.events[0].data,
        crate::cast::Data::Output("root@host:~# ".into())
    );
}
