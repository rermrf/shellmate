//! Replay fixtures contain only in-memory cast events; no recorder or host I/O.
use shellmate_term::cast::{Cast, Data, Event, Header};

pub fn standard(dark: bool) -> Cast {
    let frames = super::DATA["REC"][0]["frames"].a();
    let mut header = Header::new(100, 26);
    header.timestamp = Some(super::timestamp("09:41"));
    let events = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let mut line = super::ansi_line(&frame["segs"], dark);
            // Leave the final prompt waiting for input, without a synthetic newline.
            if index + 1 < frames.len() {
                line.push_str("\r\n");
            }
            Event {
                at: frame["at"].n(),
                data: Data::Output(line),
            }
        })
        .collect();
    Cast {
        header,
        events,
        duration: frames.last().map_or(0., |f| f["at"].n()) + 2.,
    }
}

pub fn wide() -> Cast {
    let mut events = Vec::new();
    for row in 0..35 {
        let text = if row % 5 == 0 {
            format!("root@host:~/x# echo row-{row:02}\r\n")
        } else {
            format!("row-{row:02} {} END\r\n", "0123456789".repeat(12))
        };
        events.push(Event {
            at: row as f64 * 0.7,
            data: Data::Output(text),
        });
    }
    events.push(Event {
        at: 28.,
        data: Data::Output(format!("root@host:~/x# {}", "x".repeat(120))),
    });
    events.push(Event {
        at: 29.,
        data: Data::Output("\r\x1b[2Kroot@host:~/x# ".into()),
    });
    Cast {
        header: Header::new(142, 36),
        events,
        duration: 29.,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn end_fixture_leaves_cursor_immediately_after_final_prompt() {
        for dark in [false, true] {
            let cast = standard(dark);
            let Data::Output(last) = &cast.events.last().unwrap().data else {
                panic!("expected output");
            };
            assert!(last.contains("deploy@prod-web-01"));
            assert!(last.contains("/var/www/app"));
            assert!(last.ends_with("$ \x1b[0m"));
            assert!(!last.contains('\n'));
            assert!(
                cast.events
                    .iter()
                    .all(|e| !matches!(&e.data, Data::Output(s) if s.contains("\x1b[?25l")))
            );
        }
    }

    #[test]
    fn wide_replay_fixture_has_recorded_grid_long_lines_and_plain_prompts() {
        let cast = wide();
        assert_eq!(
            (cast.header.cols, cast.header.rows, cast.duration),
            (142, 36, 29.)
        );
        assert!(
            cast.events
                .iter()
                .filter(|e| matches!(&e.data, Data::Output(s) if s.starts_with("root@host:~/x# ")))
                .count()
                >= 4
        );
        assert!(
            cast.events
                .iter()
                .any(|e| matches!(&e.data, Data::Output(s) if s.len() > 130))
        );
        assert!(
            cast.events
                .iter()
                .all(|e| !matches!(&e.data, Data::Output(s) if s.contains("\x1b[3")))
        );
        for name in ["rv", "rv-log", "rv-end", "rv-wide", "rv-recording"] {
            assert!(super::super::SCENES.contains(&name));
        }
    }
}
