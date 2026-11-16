//! macOS PTYs exercise the same serialport driver, worker and reconnection path.
//! They cannot emulate electrical modem lines or verify actual baud timing.
use super::*;
use std::{ffi::CStr, fs::File, os::fd::FromRawFd, path::PathBuf};
struct Pty {
    master: File,
    path: PathBuf,
}
impl Pty {
    fn new() -> Self {
        let (mut master, mut slave) = (0, 0);
        let mut name = [0i8; 1024];
        // openpty allocates only a fresh pseudo-terminal owned by this test.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                name.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "{}", io::Error::last_os_error());
        let path = PathBuf::from(unsafe { CStr::from_ptr(name.as_ptr()) }.to_str().unwrap());
        unsafe {
            libc::close(slave);
            libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK);
        }
        Self {
            master: unsafe { File::from_raw_fd(master) },
            path,
        }
    }
}
#[derive(Clone, Default)]
struct Bytes(Arc<Mutex<Vec<u8>>>);
impl shellmate_core::OutputHandler for Bytes {
    fn output(&self, b: &[u8]) {
        self.0.lock().unwrap().extend(b);
    }
    fn exited(&self, _: Option<u32>) {}
    fn disconnected(&self, _: String) {
        panic!("unplug must leave terminal attached");
    }
}
#[track_caller]
fn until(mut predicate: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < end, "condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn real_pty_read_write_close_reopen_and_device_reappearance() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("serial-device");
    let mut pty = Pty::new();
    std::os::unix::fs::symlink(&pty.path, &path).unwrap();
    let bytes = Bytes::default();
    let (mut channel, control) = Channel::start(
        path.to_string_lossy().into(),
        SerialSettings::default(),
        OutputSink::new(bytes.clone()),
    );
    until(|| control.state().open);
    pty.master.write_all("中文 ✓\r\n".as_bytes()).unwrap();
    until(|| bytes.0.lock().unwrap().ends_with("中文 ✓\r\n".as_bytes()));
    channel.write(b"hello\r").unwrap();
    let mut received = Vec::new();
    until(|| {
        let mut b = [0; 128];
        if let Ok(n) = pty.master.read(&mut b) {
            received.extend(&b[..n]);
        }
        received.ends_with(b"hello\r\n")
    });
    control.take_notice();
    control.send(Command::Baud(9600)).unwrap();
    if cfg!(target_os = "macos") {
        until(|| control.state().notice.is_some());
        assert_eq!(
            control.state().settings.baud,
            115200,
            "failed baud change must retain prior value"
        );
        assert!(control.state().open);
    } else {
        until(|| control.state().settings.baud == 9600);
    }
    control.send(Command::Open(false)).unwrap();
    until(|| !control.state().open);
    // A second owner can acquire the released descriptor (esptool use case).
    let other = open(path.to_str().unwrap(), &SerialSettings::default()).unwrap();
    drop(other);
    control.send(Command::Open(true)).unwrap();
    until(|| control.state().open);
    std::fs::remove_file(&path).unwrap();
    drop(pty);
    until(|| control.state().retrying);
    let mut next = Pty::new();
    std::os::unix::fs::symlink(&next.path, &path).unwrap();
    until(|| control.state().open);
    next.master.write_all(b"reappeared\r\n").unwrap();
    until(|| bytes.0.lock().unwrap().ends_with(b"reappeared\r\n"));
    channel.close();
    until(|| control.send(Command::Baud(115200)).is_err());
}
