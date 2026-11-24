//! Fragment-safe OSC 6973 capture. Production captures stream to a shared disk spool.
//! Only the registered nonce is hidden; PTY stdout/stderr are already merged by the OS.
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const CAP: usize = 256 * 1024;
#[derive(Clone, Default, Debug)]
pub struct Captured {
    pub bytes: Vec<u8>,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    pub error: Option<String>,
}
#[derive(Clone)]
pub struct CaptureHandle {
    pub nonce: String,
    data: Arc<Mutex<Captured>>,
}
impl CaptureHandle {
    pub fn snapshot(&self) -> Captured {
        self.data.lock().clone()
    }
}
pub struct MarkerScanner {
    prefix: Vec<u8>,
    pending: Vec<u8>,
    data: Arc<Mutex<Captured>>,
    spool: Option<shellmate_core::ai_output::Spool>,
    ordinary: Vec<u8>,
    skip_echo: bool,
    ended: bool,
}
impl MarkerScanner {
    pub fn new(nonce: &str) -> (Self, CaptureHandle) {
        let data = Arc::new(Mutex::new(Captured::default()));
        (
            Self {
                prefix: format!("\x1b]6973;{nonce};").into_bytes(),
                pending: Vec::new(),
                data: data.clone(),
                spool: None,
                ordinary: Vec::new(),
                skip_echo: false,
                ended: false,
            },
            CaptureHandle {
                nonce: nonce.into(),
                data,
            },
        )
    }
    pub fn spooled(nonce: &str, spool: shellmate_core::ai_output::Spool) -> (Self, CaptureHandle) {
        let (mut scanner, handle) = Self::new(nonce);
        scanner.spool = Some(spool);
        scanner.skip_echo = true;
        (scanner, handle)
    }
    fn ordinary(&mut self, bytes: &[u8], render: &mut Vec<u8>) {
        render.extend_from_slice(bytes);
        if !self.ended {
            self.ordinary.extend_from_slice(bytes);
        }
    }
    fn capture(&mut self) {
        let mut bytes = self.ordinary.as_slice();
        if self.skip_echo {
            match bytes.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    bytes = &bytes[i + 1..];
                    self.skip_echo = false;
                }
                None => {
                    self.ordinary.clear();
                    return;
                }
            }
        }
        let mut data = self.data.lock();
        if let Some(spool) = &self.spool {
            if data.error.is_none() && !bytes.is_empty() {
                match spool.write(bytes, false) {
                    Ok(true) => {}
                    Ok(false) => data.error = Some("输出超过 5 GB，命令已停止".into()),
                    Err(e) => data.error = Some(format!("保存输出失败：{e}")),
                }
            }
        } else {
            data.bytes.extend_from_slice(bytes);
        }
        self.ordinary.clear();
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut render = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            self.pending.push(byte);
            loop {
                if self.prefix.starts_with(&self.pending) {
                    break;
                }
                if self.pending.starts_with(&self.prefix) {
                    let body = &self.pending[self.prefix.len()..];
                    let terminated = body
                        .strip_suffix(b"\x07")
                        .or_else(|| body.strip_suffix(b"\x1b\\"));
                    if let Some(rc) = terminated
                        && let Ok(rc) = std::str::from_utf8(rc).unwrap_or("").parse::<i32>()
                    {
                        self.capture();
                        self.ended = true;
                        self.data.lock().exit_code = Some(rc);
                        self.pending.clear();
                        break;
                    }
                    // Only an integer and a possible first ST byte can extend a marker.
                    let rc = body.strip_suffix(b"\x1b").unwrap_or(body);
                    if body.len() <= 12
                        && rc
                            .iter()
                            .enumerate()
                            .all(|(i, b)| b.is_ascii_digit() || (i == 0 && *b == b'-'))
                    {
                        break;
                    }
                }
                let first = self.pending.remove(0);
                self.ordinary(&[first], &mut render);
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        self.capture();
        render
    }
    pub fn running(&self) -> bool {
        self.data.lock().exit_code.is_none()
    }
    pub fn flush(&mut self) -> Vec<u8> {
        let bytes = std::mem::take(&mut self.pending);
        let mut render = Vec::new();
        self.ordinary(&bytes, &mut render);
        self.capture();
        render
    }
}
#[derive(Default)]
pub(crate) struct AgentState {
    pub last_output: Option<Instant>,
    pub active: Option<(String, MarkerScanner)>,
}
impl AgentState {
    pub fn busy(&self, alternate: bool) -> bool {
        alternate
            || self.active.is_some()
            || self
                .last_output
                .is_some_and(|t| t.elapsed() < Duration::from_millis(300))
    }
    pub fn output(&mut self, bytes: &[u8]) -> Vec<u8> {
        if !bytes.is_empty() {
            self.last_output = Some(Instant::now());
        }
        self.active
            .as_mut()
            .map_or_else(|| bytes.to_vec(), |(_, scanner)| scanner.feed(bytes))
    }
}
/// Terminal commands containing control bytes are rejected by the app before this point.
/// The leading space opts out of shell history when ignorespace/HIST_IGNORE_SPACE is enabled.
/// A comment or unsupported shell can hide the suffix; the executor reports a missing marker.
pub fn command_line(command: &str, nonce: &str) -> Vec<u8> {
    format!("\x15 {command}; printf '\\033]6973;{nonce};%d\\007' $?\r").into_bytes()
}
pub fn strip_echo(bytes: &[u8]) -> &[u8] {
    bytes
        .iter()
        .position(|b| *b == b'\n')
        .map_or(&[][..], |i| &bytes[i + 1..])
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn marker_every_boundary_bel_st_exit_and_render() {
        for end in ["\x07", "\x1b\\"] {
            let bytes = format!("echo\r\na\r\nb\r\n\x1b]6973;aB3dE6gH9jK2;-7{end}prompt");
            for cut in 0..=bytes.len() {
                let (mut scanner, handle) = MarkerScanner::new("aB3dE6gH9jK2");
                let mut render = scanner.feed(&bytes.as_bytes()[..cut]);
                render.extend(scanner.feed(&bytes.as_bytes()[cut..]));
                assert_eq!(render, b"echo\r\na\r\nb\r\nprompt", "cut {cut}");
                let data = handle.snapshot();
                assert_eq!(data.exit_code, Some(-7));
                assert_eq!(strip_echo(&data.bytes), b"a\r\nb\r\n");
            }
        }
    }
    #[test]
    fn wrong_nonce_and_invalid_code_are_ordinary_output() {
        let (mut scanner, handle) = MarkerScanner::new("aB3dE6gH9jK2");
        let bytes = b"\x1b]6973;zY8wV5tS2qP9;0\x07\x1b]6973;aB3dE6gH9jK2;bad\x07";
        let render: Vec<_> = bytes.iter().flat_map(|b| scanner.feed(&[*b])).collect();
        assert_eq!(render, bytes);
        assert_eq!(handle.snapshot().bytes, bytes);
        assert_eq!(handle.snapshot().exit_code, None);
    }
    #[test]
    fn large_capture_does_not_truncate_render_or_lose_marker() {
        let (mut scanner, handle) = MarkerScanner::new("aB3dE6gH9jK2");
        assert_eq!(scanner.feed(&vec![b'x'; CAP + 30]).len(), CAP + 30);
        assert!(scanner.feed(b"\x1b]6973;aB3dE6gH9jK2;3\x07").is_empty());
        let data = handle.snapshot();
        assert_eq!(data.bytes.len(), CAP + 30);
        assert!(!data.truncated);
        assert_eq!(data.exit_code, Some(3));
    }
    #[test]
    fn busy_alt_screen_recent_output_and_active_capture() {
        let mut state = AgentState::default();
        assert!(!state.busy(false));
        assert!(state.busy(true));
        state.output(b"x");
        assert!(state.busy(false));
        state.last_output = Some(Instant::now() - Duration::from_secs(1));
        assert!(!state.busy(false));
        state.active = Some(("n".into(), MarkerScanner::new("n").0));
        assert!(state.busy(false));
    }
}
