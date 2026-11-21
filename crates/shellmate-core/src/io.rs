//! Backend abstraction between a terminal view and whatever feeds it: a local
//! PTY, an SSH shell channel, and later Mosh / Telnet / serial.
//!
//! The terminal model hands every backend an [`OutputSink`] when it attaches
//! one. The backend pushes bytes and lifecycle notifications into the sink from
//! its own thread or task; the terminal in turn drives the backend through
//! [`TerminalBackend`] (keyboard input, resizes, shutdown).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

/// Size of the pseudo terminal, in cells and (informational) pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtyDims {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
    /// Width in pixels (0 if unknown).
    pub pixel_width: u16,
    /// Height in pixels (0 if unknown).
    pub pixel_height: u16,
}

impl Default for PtyDims {
    fn default() -> Self {
        Self { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 }
    }
}

/// The process currently in the foreground of a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Process id.
    pub pid: i32,
    /// Executable name, e.g. `vim`.
    pub name: String,
    /// Current working directory, if it could be read.
    pub cwd: Option<PathBuf>,
}

/// Fresh state for a destructive close, independent of title/output caches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseActivity {
    Idle,
    Busy(Vec<ProcessInfo>),
    /// The backend cannot establish that the terminal is idle.
    Unknown,
}

/// Something that can be wired to a terminal: it receives input and resizes,
/// and reports output / exit / disconnects through the [`OutputSink`] it was
/// created with.
pub trait TerminalBackend: Send {
    /// Sends user input to the remote side. Called on the UI thread, so it must
    /// not block; asynchronous failures are reported via [`OutputSink::disconnected`].
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// The terminal grid changed size.
    fn resize(&mut self, cols: u16, rows: u16, pixel_width: u16, pixel_height: u16) -> io::Result<()>;

    /// Shuts the backend down (kills the process, closes the channel).
    /// Idempotent. After a deliberate `close` the backend must not call back
    /// into its sink.
    fn close(&mut self);

    /// PID of the PTY child, distinct from the current foreground process group.
    fn child_pid(&self) -> Option<u32> { None }

    /// Foreground process of the terminal; only meaningful for local PTYs.
    fn foreground_process(&self) -> Option<ProcessInfo> {
        None
    }

    fn close_activity(&self) -> CloseActivity { CloseActivity::Unknown }
}

/// Receiver of everything a backend produces. Implemented by the terminal model.
pub trait OutputHandler: Send + Sync {
    /// Raw bytes from the remote side (VT escape sequences included).
    fn output(&self, bytes: &[u8]);
    /// A transport with a display filter (serial HEX/timestamps). Capture uses
    /// raw bytes; a simple sink may display only the formatted bytes.
    fn transformed_output(&self, _raw: &[u8], display: &[u8]) {self.output(display);}

    /// The remote program ended; `None` when the status is unknown.
    fn exited(&self, code: Option<u32>);
    /// The transport broke (or could not be established) for the given reason.
    fn disconnected(&self, reason: String);
}

/// Cloneable, thread-safe handle a backend uses to talk to its terminal.
#[derive(Clone)]
pub struct OutputSink(Arc<dyn OutputHandler>);

impl OutputSink {
    /// Wraps a handler. The terminal creates one sink per attached backend.
    pub fn new(handler: impl OutputHandler + 'static) -> Self {
        Self(Arc::new(handler))
    }

    /// Delivers output bytes. May be called from any thread; runs the VT parser
    /// on the calling thread.
    pub fn output(&self, bytes: &[u8]) {
        self.0.output(bytes);
    }

    /// Delivers original and formatted serial bytes without confusing capture.
    pub fn transformed_output(&self, raw: &[u8], display: &[u8]) {self.0.transformed_output(raw, display);}

    /// Reports that the remote program exited with `code`.
    pub fn exited(&self, code: Option<u32>) {
        self.0.exited(code);
    }

    /// Reports a broken or failed connection.
    pub fn disconnected(&self, reason: impl Into<String>) {
        self.0.disconnected(reason.into());
    }
}

impl std::fmt::Debug for OutputSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OutputSink")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl OutputHandler for Arc<Recorder> {
        fn output(&self, bytes: &[u8]) {
            self.0.lock().unwrap().push(format!("out:{}", String::from_utf8_lossy(bytes)));
        }
        fn exited(&self, code: Option<u32>) {
            self.0.lock().unwrap().push(format!("exit:{code:?}"));
        }
        fn disconnected(&self, reason: String) {
            self.0.lock().unwrap().push(format!("disc:{reason}"));
        }
    }

    #[test]
    fn sink_forwards_to_handler_from_clones() {
        let recorder = Arc::new(Recorder::default());
        let sink = OutputSink::new(recorder.clone());
        let clone = sink.clone();
        sink.output(b"hi");
        clone.exited(Some(3));
        clone.disconnected("boom");
        assert_eq!(*recorder.0.lock().unwrap(), ["out:hi", "exit:Some(3)", "disc:boom"]);
    }

    struct Dummy;

    impl TerminalBackend for Dummy {
        fn write(&mut self, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn resize(&mut self, _: u16, _: u16, _: u16, _: u16) -> io::Result<()> {
            Ok(())
        }
        fn close(&mut self) {}
    }

    #[test]
    fn foreground_process_defaults_to_none() {
        assert_eq!(Dummy.foreground_process(), None);
        assert_eq!(Dummy.close_activity(), CloseActivity::Unknown);
    }
}
