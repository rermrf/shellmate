//! The frames the collector script writes, and the parser that puts them back together.
//!
//! A frame starts with a header line, has sections of raw lines, and ends with an end line. Every
//! marker line starts with `@@` and a token that is different for every run, so that nothing the server
//! prints (a process with `@@end` in its name, a login banner) can pass for one:
//!
//! ```text
//! @@TOKEN tick 7                 every second, so that a stalled script is noticed
//! @@TOKEN stats 190057.84        a frame: its kind and the server's uptime when it was taken
//! @@TOKEN procs 190060.84 4711   (a process frame also says the collector's own process id)
//! @@TOKEN cpu                    a section; its lines follow, raw
//! cpu  40480 0 84515 ...
//! @@TOKEN end
//! ```

/// What a frame carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// System counters: CPU, load, memory, the default network interface, temperatures.
    Stats,
    /// The processes: `/proc/<pid>/stat` lines and what `ps` says about them.
    Procs,
    /// What changes slowly: disk usage.
    Slow,
}

/// A part of a frame: raw lines under a name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Section {
    /// `cpu`, `mem`, `stat`, `ps`, ...
    pub name: String,
    /// The lines, as the server wrote them.
    pub lines: Vec<String>,
}

impl Section {
    /// The lines as one text.
    pub fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }
}

/// One complete frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// What it carries.
    pub kind: FrameKind,
    /// The server's uptime in seconds when it was taken: a clock that the network does not disturb.
    pub stamp: f64,
    /// The process id of the collector script itself (`$$`), which the process frames carry so that the
    /// monitor can leave itself, and what it starts, out of the list.
    pub pid: Option<u32>,
    /// Its sections in the order they came.
    pub sections: Vec<Section>,
}

impl Frame {
    /// The lines of the section called `name`.
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|section| section.name == name)
    }

    /// The text of the section called `name` (empty if there is none).
    pub fn text(&self, name: &str) -> String {
        self.section(name).map(Section::text).unwrap_or_default()
    }
}

/// What the parser makes of a line.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// The script is alive (every second).
    Tick(u64),
    /// A frame is complete.
    Frame(Frame),
}

/// Assembles frames from lines.
pub struct FrameParser {
    prefix: String,
    current: Option<Frame>,
    /// Frames that were cut off by the start of the next one.
    pub dropped: usize,
}

impl FrameParser {
    /// A parser for the frames marked with `token`.
    pub fn new(token: &str) -> Self {
        Self { prefix: format!("@@{token} "), current: None, dropped: 0 }
    }

    /// The prefix of marker lines.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Takes the next line. Lines outside a frame (what a login shell prints, say) are ignored.
    pub fn push(&mut self, line: &str) -> Option<Item> {
        let Some(marker) = line.strip_prefix(&self.prefix) else {
            if let Some(section) = self.current.as_mut().and_then(|frame| frame.sections.last_mut()) {
                section.lines.push(line.to_string());
            }
            return None;
        };
        let (command, argument) = marker.split_once(' ').unwrap_or((marker, ""));
        match command {
            "tick" => argument.trim().parse().ok().map(Item::Tick),
            "stats" | "procs" | "slow" => {
                let kind = match command {
                    "stats" => FrameKind::Stats,
                    "procs" => FrameKind::Procs,
                    _ => FrameKind::Slow,
                };
                if self.current.take().is_some() {
                    self.dropped += 1;
                }
                // A stamp that is not a number makes a frame nobody can compute rates from.
                let mut words = argument.split_whitespace();
                let stamp = words.next()?.parse::<f64>().ok().filter(|stamp| stamp.is_finite() && *stamp >= 0.0)?;
                let pid = words.next().and_then(|word| word.parse::<u32>().ok()).filter(|pid| *pid > 0);
                self.current = Some(Frame { kind, stamp, pid, sections: Vec::new() });
                None
            }
            "end" => self.current.take().map(Item::Frame),
            name => {
                if let Some(frame) = self.current.as_mut() {
                    frame.sections.push(Section { name: name.to_string(), lines: Vec::new() });
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(parser: &mut FrameParser, text: &str) -> Vec<Item> {
        text.lines().filter_map(|line| parser.push(line)).collect()
    }

    #[test]
    fn frames_come_out_with_their_sections() {
        let mut parser = FrameParser::new("abc");
        let items = feed(
            &mut parser,
            "banner from the login shell\n@@abc tick 0\n@@abc stats 12.50\n@@abc cpu\ncpu  1 2 3 4\ncpu0 1 2 3 4\n@@abc load\n0.1 0.2 0.3 1/2 3\n@@abc end\n@@abc tick 1\n",
        );
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], Item::Tick(0));
        let Item::Frame(frame) = &items[1] else { panic!("{items:?}") };
        assert_eq!((frame.kind, frame.stamp), (FrameKind::Stats, 12.5));
        assert_eq!(frame.sections.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["cpu", "load"]);
        assert_eq!(frame.section("cpu").unwrap().lines, ["cpu  1 2 3 4", "cpu0 1 2 3 4"]);
        assert_eq!(frame.text("load"), "0.1 0.2 0.3 1/2 3\n");
        assert_eq!(frame.text("missing"), "");
        assert_eq!(items[2], Item::Tick(1));
    }

    #[test]
    fn a_process_frame_names_the_collector() {
        let mut parser = FrameParser::new("t");
        let items =
            feed(&mut parser, "@@t procs 9.5 4711\n@@t end\n@@t procs 12.5\n@@t end\n@@t procs 15.5 x\n@@t end\n");
        let pids: Vec<_> = items
            .iter()
            .map(|item| match item {
                Item::Frame(frame) => frame.pid,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(pids, [Some(4711), None, None], "older scripts and garbage give no pid, but still a frame");
    }

    #[test]
    fn a_line_that_looks_like_a_marker_but_has_the_wrong_token_is_data() {
        let mut parser = FrameParser::new("secret");
        let items = feed(
            &mut parser,
            "@@secret procs 5.0\n@@secret ps\n 1 0 evil\n@@other end\n@@secret-x end\n@@ end\n2 0 @@secret\n@@secret end\n",
        );
        let [Item::Frame(frame)] = items.as_slice() else { panic!("{items:?}") };
        assert_eq!(
            frame.section("ps").unwrap().lines,
            [" 1 0 evil", "@@other end", "@@secret-x end", "@@ end", "2 0 @@secret"]
        );
    }

    #[test]
    fn a_frame_cut_off_by_the_next_one_is_dropped_and_counted() {
        let mut parser = FrameParser::new("t");
        let items =
            feed(&mut parser, "@@t stats 1\n@@t cpu\ncpu 1 2 3 4\n@@t stats 3\n@@t cpu\ncpu 5 6 7 8\n@@t end\n");
        let [Item::Frame(frame)] = items.as_slice() else { panic!("{items:?}") };
        assert_eq!(frame.stamp, 3.0);
        assert_eq!(parser.dropped, 1);
    }

    #[test]
    fn broken_markers_do_not_make_frames() {
        let mut parser = FrameParser::new("t");
        // A stamp that is not a number, an end without a start, a tick without a number, unknown
        // sections outside of frames.
        let items = feed(&mut parser, "@@t stats nope\n@@t cpu\nline\n@@t end\n@@t end\n@@t tick x\n@@t tick\n");
        assert!(items.is_empty(), "{items:?}");
        assert!(feed(&mut parser, "@@t slow -1\n@@t end\n").is_empty(), "no negative uptime");
    }

    #[test]
    fn the_end_of_a_stream_in_the_middle_of_a_frame_gives_no_frame() {
        let mut parser = FrameParser::new("t");
        assert!(feed(&mut parser, "@@t procs 9.0\n@@t stat\n1 (a) S 0\n").is_empty());
    }
}
