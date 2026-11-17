//! Per-terminal output tap. Lives across backend generations; never sees keyboard input.
use crate::{
    cast::{self, Header},
    session_log::{LineTracker, LogWriter},
};
use std::{
    fs::File,
    io::{self, BufWriter},
    path::PathBuf,
    time::Instant,
};
pub struct RecordingResult {
    pub path: PathBuf,
    pub duration: f64,
    pub size: u64,
}
struct Recording {
    writer: cast::Writer<BufWriter<File>>,
    path: PathBuf,
    start: Instant,
}
#[derive(Default)]
pub(crate) struct OutputCapture {
    recording: Option<Recording>,
    logger: Option<(LineTracker, LogWriter)>,
    pending_log: Option<LogWriter>,
    error: Option<String>,
}
impl OutputCapture {
    pub fn start(
        &mut self,
        path: PathBuf,
        file: File,
        header: &Header,
        seed: Option<&str>,
    ) -> io::Result<()> {
        let mut writer = cast::Writer::new(BufWriter::new(file), header)?;
        if let Some(seed) = seed {
            writer.output(0., seed.as_bytes())?;
        }
        self.recording = Some(Recording {
            writer,
            path,
            start: Instant::now(),
        });
        Ok(())
    }
    pub fn seconds(&self) -> Option<f64> {
        self.recording
            .as_ref()
            .map(|r| r.start.elapsed().as_secs_f64())
    }
    pub fn playback_duration(&self) -> Option<f64> {
        self.recording.as_ref().map(|r| r.writer.duration())
    }
    pub fn stop(&mut self) -> io::Result<Option<RecordingResult>> {
        let Some(r) = self.recording.take() else {
            return Ok(None);
        };
        let (writer, duration) = r.writer.finish(r.start.elapsed().as_secs_f64())?;
        let size = writer.get_ref().metadata()?.len();
        Ok(Some(RecordingResult {
            path: r.path,
            duration,
            size,
        }))
    }
    pub fn logging(&mut self, tracker: LineTracker, writer: LogWriter) {
        self.pending_log = None;
        self.logger = Some((tracker, writer));
    }
    pub fn defer_logging(&mut self, writer: LogWriter) {
        self.pending_log = Some(writer);
    }
    pub fn sync_logger<T>(&mut self, term: &alacritty_terminal::term::Term<T>) {
        use alacritty_terminal::{grid::Dimensions, term::TermMode};
        if !term.mode().contains(TermMode::ALT_SCREEN)
            && let Some(writer) = self.pending_log.take()
        {
            let mut tracker = LineTracker::new(term.columns() as u16, term.screen_lines() as u16);
            tracker.seed(term);
            self.logger = Some((tracker, writer));
        }
    }
    pub fn stop_logging(&mut self) {
        self.pending_log = None;
        if let Some((mut tracker, mut writer)) = self.logger.take() {
            let result = writer.lines(&tracker.finish()).and_then(|_| writer.flush());
            if let Err(e) = result {
                self.error = Some(format!("会话日志保存失败：{e}"));
            }
        }
    }
    pub fn output(&mut self, bytes: &[u8]) {
        if let Some(r) = &mut self.recording
            && let Err(e) = r.writer.output(r.start.elapsed().as_secs_f64(), bytes)
        {
            self.error = Some(format!("录制已停止：{e}"));
            self.recording = None;
        }
        if let Some((tracker, writer)) = &mut self.logger
            && let Err(e) = writer.lines(&tracker.feed(bytes))
        {
            self.error = Some(format!("会话日志已停止：{e}"));
            self.logger = None;
        }
    }
    /// Application notices and local clear affect the log's grid, but are not remote recording output.
    pub fn log_output(&mut self, bytes: &[u8]) {
        if let Some((tracker, writer)) = &mut self.logger
            && let Err(e) = writer.lines(&tracker.feed(bytes))
        {
            self.error = Some(format!("会话日志已停止：{e}"));
            self.logger = None;
        }
    }
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if let Some(r) = &mut self.recording
            && let Err(e) = r.writer.resize(r.start.elapsed().as_secs_f64(), cols, rows)
        {
            self.error = Some(format!("录制已停止：{e}"));
            self.recording = None;
        }
        if let Some((tracker, writer)) = &mut self.logger
            && let Err(e) = writer.lines(&tracker.resize(cols, rows))
        {
            self.error = Some(format!("会话日志已停止：{e}"));
            self.logger = None;
        }
    }
    pub fn flush(&mut self) {
        if let Some(r) = &mut self.recording
            && let Err(e) = r.writer.flush()
        {
            self.error = Some(format!("录制已停止：{e}"));
            self.recording = None;
        }
        if let Some((_, w)) = &mut self.logger
            && let Err(e) = w.flush()
        {
            self.error = Some(format!("会话日志已停止：{e}"));
            self.logger = None;
        }
    }
    pub fn error(&mut self) -> Option<String> {
        self.error.take()
    }
}
impl Drop for OutputCapture {
    fn drop(&mut self) {
        let _ = self.stop();
        self.stop_logging();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive;
    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "shellmate-output-{}",
            shellmate_core::HostId::generate()
        ))
    }
    #[test]
    fn recording_seed_is_first_output_and_never_enters_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let (path, file) = archive::recording_file(dir.path(), "host", 1700000000).unwrap();
        let logs = dir.path().join("logs");
        let mut capture = OutputCapture::default();
        capture.logging(
            LineTracker::new(80, 24),
            LogWriter::new(logs.clone(), "host"),
        );
        let seed = "root@host:~# \x1b[0m\r\x1b[13C";
        capture
            .start(path.clone(), file, &Header::new(80, 24), Some(seed))
            .unwrap();
        capture.output(b"pwd\r\n");
        capture.stop().unwrap();
        capture.stop_logging();
        let cast = cast::Cast::open(&path).unwrap();
        assert_eq!(cast.events.len(), 2);
        assert_eq!(cast.events[0].recorded_at, 0.);
        assert_eq!(cast.events[0].data, cast::Data::Output(seed.into()));
        assert_eq!(cast.events[1].data, cast::Data::Output("pwd\r\n".into()));
        let files = crate::session_log::list(&logs).unwrap();
        let text = std::fs::read_to_string(&files[0].path).unwrap();
        assert!(text.ends_with("  pwd\n"));
        assert!(!text.contains("root@host"));
    }
    #[test]
    fn stream_flush_stop_and_output_only_events() {
        let dir = root();
        let (path, file) = archive::recording_file(&dir, "host", 1_700_000_000).unwrap();
        let mut capture = OutputCapture::default();
        capture
            .start(path.clone(), file, &Header::new(80, 24), None)
            .unwrap();
        capture.output(b"\x1b[31mhello\x1b[0m\r\n");
        capture.resize(100, 30);
        capture.flush();
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        let result = capture.stop().unwrap().unwrap();
        assert_eq!(result.path, path);
        assert!(result.size > 0);
        assert!(capture.seconds().is_none());
        let cast = cast::Cast::open(&path).unwrap();
        assert_eq!(cast.events.len(), 2);
        assert_eq!(result.duration, cast.duration);
        assert_eq!(cast::Cast::playback_duration(&path).unwrap(), cast.duration);
        assert_eq!(
            cast.events[0].data,
            cast::Data::Output("\x1b[31mhello\x1b[0m\r\n".into())
        );
        assert_eq!(cast.events[1].data, cast::Data::Resize(100, 30));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn stop_returns_compressed_event_duration_for_the_catalog() {
        let dir = root();
        let (path, file) = archive::recording_file(&dir, "host", 1_700_000_000).unwrap();
        let mut capture = OutputCapture::default();
        capture
            .start(path.clone(), file, &Header::new(80, 24), None)
            .unwrap();
        let recording = capture.recording.as_mut().unwrap();
        recording.writer.output(1., b"first").unwrap();
        recording.writer.output(5., b"last").unwrap();
        assert_eq!(capture.playback_duration(), Some(3.));
        let stopped = capture.stop().unwrap().unwrap();
        assert_eq!(stopped.duration, 3.);
        assert_eq!(cast::Cast::open(&path).unwrap().duration, 3.);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_writer_stops_and_reports_once() {
        let dir = root();
        let (path, file) = archive::recording_file(&dir, "host", 1_700_000_000).unwrap();
        drop(file);
        let file = File::open(&path).unwrap();
        let mut capture = OutputCapture::default();
        capture
            .start(path, file, &Header::new(80, 24), None)
            .unwrap();
        capture.output(&vec![b'x'; 16_384]);
        assert!(capture.seconds().is_none());
        assert!(capture.error().unwrap().starts_with("录制已停止"));
        assert!(capture.error().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn close_finalizes_nonempty_primary_lines_and_flushes() {
        let dir = root();
        let mut capture = OutputCapture::default();
        capture.logging(
            LineTracker::new(80, 24),
            LogWriter::new(dir.clone(), "host"),
        );
        capture.output(b"final output");
        capture.stop_logging();
        let files = crate::session_log::list(&dir).unwrap();
        assert_eq!(files.len(), 1);
        assert!(
            std::fs::read_to_string(&files[0].path)
                .unwrap()
                .ends_with("  final output\n")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
