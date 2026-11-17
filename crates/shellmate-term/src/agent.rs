//! Fragment-safe, bounded OSC 6973 capture. Only the registered nonce is hidden.
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const CAP: usize = 256 * 1024;
#[derive(Clone, Default, Debug)]
pub struct Captured {
    pub bytes: Vec<u8>,
    pub exit_code: Option<i32>,
    pub truncated: bool,
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
}
impl MarkerScanner {
    pub fn new(nonce: &str) -> (Self, CaptureHandle) {
        let data = Arc::new(Mutex::new(Captured::default()));
        (
            Self {
                prefix: format!("\x1b]6973;{nonce};").into_bytes(),
                pending: Vec::new(),
                data: data.clone(),
            },
            CaptureHandle {
                nonce: nonce.into(),
                data,
            },
        )
    }
    fn ordinary(&self, bytes: &[u8], render: &mut Vec<u8>) {
        render.extend_from_slice(bytes);
        let mut data = self.data.lock();
        if data.exit_code.is_none() {
            let room = CAP.saturating_sub(data.bytes.len());
            data.truncated |= bytes.len() > room;
            data.bytes
                .extend_from_slice(&bytes[..room.min(bytes.len())]);
        }
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
        render
    }
    pub fn running(&self) -> bool {
        self.data.lock().exit_code.is_none()
    }
    pub fn flush(&mut self) -> Vec<u8> {
        let bytes = std::mem::take(&mut self.pending);
        let mut render = Vec::new();
        self.ordinary(&bytes, &mut render);
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
    fn capture_cap_does_not_truncate_render_or_lose_marker() {
        let (mut scanner, handle) = MarkerScanner::new("aB3dE6gH9jK2");
        assert_eq!(scanner.feed(&vec![b'x'; CAP + 30]).len(), CAP + 30);
        assert!(scanner.feed(b"\x1b]6973;aB3dE6gH9jK2;3\x07").is_empty());
        let data = handle.snapshot();
        assert_eq!(data.bytes.len(), CAP);
        assert!(data.truncated);
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
