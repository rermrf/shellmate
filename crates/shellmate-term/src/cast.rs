//! Streaming asciinema v2 output/resize writer and validated reader.
use serde_json::{self as json, Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    path::Path,
};
#[derive(Clone, Debug, PartialEq)]
pub struct Header {
    pub cols: u16,
    pub rows: u16,
    pub timestamp: Option<i64>,
    pub title: Option<String>,
    pub env: BTreeMap<String, String>,
}
impl Header {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            timestamp: None,
            title: None,
            env: BTreeMap::new(),
        }
    }
    fn value(&self) -> Value {
        let mut v = json!({"version":2,"width":self.cols,"height":self.rows});
        if let Some(t) = self.timestamp {
            v["timestamp"] = json!(t);
        }
        if let Some(t) = &self.title {
            v["title"] = json!(t);
        }
        if !self.env.is_empty() {
            v["env"] = json!(self.env);
        }
        v
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum Data {
    Output(String),
    Resize(u16, u16),
}
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Playback offset after idle-gap compression.
    pub at: f64,
    /// Original cast offset, used for line timestamps.
    pub recorded_at: f64,
    pub data: Data,
}
#[derive(Default)]
struct Utf8 {
    pending: Vec<u8>,
}
impl Utf8 {
    fn push(&mut self, bytes: &[u8], finish: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut result = String::new();
        let mut at = 0;
        while at < self.pending.len() {
            match std::str::from_utf8(&self.pending[at..]) {
                Ok(text) => {
                    result.push_str(text);
                    at = self.pending.len();
                }
                Err(e) => {
                    let end = at + e.valid_up_to();
                    result.push_str(std::str::from_utf8(&self.pending[at..end]).expect("validated prefix"));
                    at = end;
                    match e.error_len() {
                        Some(n) => {
                            result.push('\u{fffd}');
                            at += n;
                        }
                        None if finish => {
                            result.push('\u{fffd}');
                            at = self.pending.len();
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..at);
        result
    }
}
pub struct Writer<W: Write> {
    writer: W,
    utf8: Utf8,
    clock: EventClock,
}

/// One convention for writer metadata and reader/player time: cap each inter-event gap at 2 s.
#[derive(Default)]
struct EventClock {
    last: f64,
    duration: f64,
}
impl EventClock {
    fn advance(&mut self, at: f64) -> f64 {
        let at = at.max(self.last);
        self.duration += (at - self.last).min(2.);
        self.last = at;
        at
    }
}
impl<W: Write> Writer<W> {
    pub fn new(mut writer: W, header: &Header) -> io::Result<Self> {
        json::to_writer(&mut writer, &header.value())?;
        writer.write_all(b"\n")?;
        Ok(Self {
            writer,
            utf8: Utf8::default(),
            clock: EventClock::default(),
        })
    }
    fn event(&mut self, at: f64, kind: &str, text: &str) -> io::Result<()> {
        let at = at.max(self.clock.last);
        json::to_writer(&mut self.writer, &json!([at, kind, text]))?;
        self.writer.write_all(b"\n")?;
        self.clock.advance(at);
        Ok(())
    }
    pub fn output(&mut self, at: f64, bytes: &[u8]) -> io::Result<()> {
        let text = self.utf8.push(bytes, false);
        if !text.is_empty() {
            self.event(at, "o", &text)?;
        }
        Ok(())
    }
    pub fn resize(&mut self, at: f64, cols: u16, rows: u16) -> io::Result<()> {
        self.event(at, "r", &format!("{cols}x{rows}"))
    }
    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
    pub fn duration(&self) -> f64 {
        self.clock.duration
    }
    pub fn finish(mut self, at: f64) -> io::Result<(W, f64)> {
        let tail = self.utf8.push(&[], true);
        if !tail.is_empty() {
            self.event(at, "o", &tail)?;
        }
        self.writer.flush()?;
        Ok((self.writer, self.clock.duration))
    }
}
#[derive(Clone)]
pub struct Cast {
    pub header: Header,
    pub events: Vec<Event>,
    pub duration: f64,
}
impl Cast {
    pub fn read(reader: impl BufRead) -> Result<Self, String> {
        let mut events = Vec::new();
        let (header, duration) = Self::read_events(reader, |event| events.push(event))?;
        Ok(Self {
            header,
            events,
            duration,
        })
    }
    /// Scan old catalog entries without retaining their output events in memory.
    pub fn playback_duration(path: &Path) -> Result<f64, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("无法打开录制：{e}"))?;
        Self::read_events(std::io::BufReader::new(file), |_| {}).map(|(_, duration)| duration)
    }
    fn read_events(reader: impl BufRead, mut emit: impl FnMut(Event)) -> Result<(Header, f64), String> {
        let mut lines = reader.lines();
        let header: Value = json::from_str(&lines.next().ok_or("录制文件为空")?.map_err(|e| e.to_string())?)
            .map_err(|_| "录制头部无效")?;
        if header["version"] != 2 {
            return Err("仅支持 asciinema v2".into());
        }
        fn dimension(v: &Value) -> Result<u16, String> {
            v.as_u64()
                .and_then(|v| u16::try_from(v).ok())
                .filter(|v| *v > 0 && *v <= 4096)
                .ok_or("录制尺寸无效".into())
        }
        let mut h = Header::new(dimension(&header["width"])?, dimension(&header["height"])?);
        if u32::from(h.cols) * u32::from(h.rows) > 1_000_000 {
            return Err("录制尺寸过大".into());
        }
        h.timestamp = header["timestamp"].as_i64();
        h.title = header["title"].as_str().map(str::to_string);
        if let Some(env) = header["env"].as_object() {
            for (k, v) in env {
                if let Some(v) = v.as_str() {
                    h.env.insert(k.clone(), v.into());
                }
            }
        }
        let mut clock = EventClock::default();
        for (index, line) in lines.enumerate() {
            let line = line.map_err(|e| e.to_string())?;
            if line.trim().is_empty() {
                continue;
            }
            let err = || format!("录制第 {} 行无效", index + 2);
            let v: Value = json::from_str(&line).map_err(|_| err())?;
            let a = v.as_array().filter(|a| a.len() == 3).ok_or_else(err)?;
            let at = a[0]
                .as_f64()
                .filter(|t| t.is_finite() && *t >= clock.last)
                .ok_or_else(err)?;
            let kind = a[1].as_str().ok_or_else(err)?;
            let text = a[2].as_str().ok_or_else(err)?;
            clock.advance(at);
            let data = match kind {
                "o" => Data::Output(text.into()),
                "r" => {
                    let (c, r) = text.split_once('x').ok_or_else(err)?;
                    let (c, r) = (
                        c.parse::<u16>().map_err(|_| err())?,
                        r.parse::<u16>().map_err(|_| err())?,
                    );
                    if c == 0 || r == 0 || c > 4096 || r > 4096 || u32::from(c) * u32::from(r) > 1_000_000 {
                        return Err(err());
                    }
                    Data::Resize(c, r)
                }
                "i" | "m" => continue,
                _ => return Err(err()),
            };
            emit(Event {
                at: clock.duration,
                recorded_at: at,
                data,
            });
        }
        Ok((h, clock.duration))
    }
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("无法打开录制：{e}"))?;
        Self::read(std::io::BufReader::new(file))
    }
}
#[derive(Default)]
pub struct Playback {
    pub position: f64,
    pub playing: bool,
    pub speed: u8,
}
impl Playback {
    pub fn tick(&mut self, elapsed: f64, duration: f64) {
        if self.playing {
            self.position = (self.position + elapsed * self.speed.max(1) as f64).min(duration);
            if self.position >= duration {
                self.playing = false;
            }
        }
    }
    pub fn seek(&mut self, time: f64, duration: f64) {
        self.position = time.clamp(0., duration);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_utf8_invalid_resize_and_header() {
        let mut h = Header::new(80, 24);
        h.timestamp = Some(123);
        h.title = Some("中文".into());
        h.env.insert("TERM".into(), "xterm-256color".into());
        let mut w = Writer::new(Vec::new(), &h).unwrap();
        let b = "中文✓".as_bytes();
        for (i, b) in b.iter().enumerate() {
            w.output(i as f64 / 10., &[*b]).unwrap();
        }
        w.output(1., b"\xff\xe4").unwrap();
        w.resize(1.2, 100, 30).unwrap();
        let (bytes, duration) = w.finish(1.4).unwrap();
        let c = Cast::read(bytes.as_slice()).unwrap();
        assert_eq!(c.header, h);
        assert_eq!(duration, c.duration);
        let text: String = c
            .events
            .iter()
            .filter_map(|e| {
                if let Data::Output(s) = &e.data {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "中文✓��");
        assert!(c.events.iter().any(|e| e.data == Data::Resize(100, 30)));
    }
    #[test]
    fn format_example_and_idle_compression() {
        let sample=b"{\"version\":2,\"width\":80,\"height\":24,\"timestamp\":1504467315,\"env\":{\"SHELL\":\"/bin/zsh\",\"TERM\":\"xterm-256color\"}}\n[0.0,\"o\",\"hello \"]\n[1.0,\"o\",\"world!\\r\\n\"]\n[10.0,\"r\",\"100x30\"]\n";
        let c = Cast::read(&sample[..]).unwrap();
        assert_eq!(c.duration, 3.);
        assert_eq!(c.events[2].at, 3.);
    }
    #[test]
    fn writer_duration_matches_playback_including_resize_and_final_utf8_fragment() {
        for events in [vec![], vec![0.5], vec![0., 5., 5.5, 20.]] {
            let mut writer = Writer::new(Vec::new(), &Header::new(80, 24)).unwrap();
            for at in events {
                writer.output(at, b"output").unwrap();
            }
            writer.resize(25., 100, 30).unwrap();
            let before = writer.duration();
            writer.output(26., b"\xe4").unwrap();
            assert_eq!(
                writer.duration(),
                before,
                "an incomplete UTF-8 fragment is not an event yet"
            );
            let (bytes, duration) = writer.finish(30.).unwrap();
            assert_eq!(duration, Cast::read(bytes.as_slice()).unwrap().duration);
            assert_eq!(duration, before + 2.);
        }
    }
    #[test]
    fn stopping_without_output_does_not_add_idle_time() {
        let mut writer = Writer::new(Vec::new(), &Header::new(80, 24)).unwrap();
        writer.output(1., b"first").unwrap();
        writer.output(5., b"last").unwrap();
        assert_eq!(writer.duration(), 3.);
        let (bytes, duration) = writer.finish(60.).unwrap();
        assert_eq!(duration, 3.);
        assert_eq!(Cast::read(bytes.as_slice()).unwrap().duration, 3.);
        let (bytes, duration) = Writer::new(Vec::new(), &Header::new(80, 24))
            .unwrap()
            .finish(60.)
            .unwrap();
        assert_eq!(duration, 0.);
        assert_eq!(Cast::read(bytes.as_slice()).unwrap().duration, 0.);
    }

    #[test]
    fn corrupt_input_is_an_error() {
        for text in [
            "",
            "{}",
            "{\"version\":3}",
            "{\"version\":2,\"width\":80,\"height\":24}\n[1,\"r\",\"0x1\"]",
            "{\"version\":2,\"width\":80,\"height\":24}\n[2,\"o\",\"x\"]\n[1,\"o\",\"y\"]",
        ] {
            assert!(Cast::read(text.as_bytes()).is_err());
        }
    }
    #[test]
    fn playback_speed_and_end() {
        for speed in [1, 2, 4] {
            let mut p = Playback {
                position: 0.,
                playing: true,
                speed,
            };
            p.tick(0.5, 5.);
            assert_eq!(p.position, speed as f64 * 0.5);
            p.seek(99., 5.);
            p.tick(1., 5.);
            assert!(!p.playing);
            assert_eq!(p.position, 5.);
        }
    }
    #[test]
    fn writer_errors_are_returned() {
        struct Bad;
        impl Write for Bad {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::WriteZero.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::Other.into())
            }
        }
        assert!(Writer::new(Bad, &Header::new(80, 24)).is_err());
    }
}
