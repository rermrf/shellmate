//! Serial byte-stream backend. A single worker owns the port; manual close
//! releases the OS handle, while unplug keeps the terminal/recording alive and
//! retries the same path once a second. No UI or network dependencies.
#[cfg(all(test, unix))]
mod pty_tests;
pub mod sequence;
pub mod transform;
use serialport::{SerialPort, SerialPortType};
use shellmate_core::{Newline, OutputSink, SerialSettings, TerminalBackend};
use std::{
    io::{self, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub path: String,
    pub description: String,
}
pub fn devices() -> Result<Vec<Device>, String> {
    let ports = serialport::available_ports().map_err(|e| format!("无法列出串口：{e}"))?;
    let mut result = ports
        .into_iter()
        .map(|p| {
            let description = match p.port_type {
                SerialPortType::UsbPort(u) => format!(
                    "{} · {:04X}:{:04X}",
                    [u.manufacturer, u.product]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · "),
                    u.vid,
                    u.pid
                ),
                SerialPortType::BluetoothPort => "蓝牙串口".into(),
                _ => "串口设备".into(),
            };
            Device {
                path: p.port_name,
                description,
            }
        })
        .collect::<Vec<_>>();
    if cfg!(target_os = "macos") {
        let callout = result
            .iter()
            .map(|d| d.path.clone())
            .collect::<std::collections::HashSet<_>>();
        result.retain(|d| {
            !d.path
                .strip_prefix("/dev/tty.")
                .is_some_and(|suffix| callout.contains(&format!("/dev/cu.{suffix}")))
        });
    }
    result.sort_by(|a, b| {
        (!a.path.starts_with("/dev/cu."))
            .cmp(&!b.path.starts_with("/dev/cu."))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(result)
}
fn open(path: &str, s: &SerialSettings) -> Result<Box<dyn SerialPort>, String> {
    s.validate().map_err(str::to_string)?;
    let pty = is_pty(path);
    let mut p = serialport::new(
        path,
        if cfg!(target_os = "macos") && pty {
            0
        } else {
            s.baud
        },
    )
    .data_bits(match s.data_bits {
        5 => serialport::DataBits::Five,
        6 => serialport::DataBits::Six,
        7 => serialport::DataBits::Seven,
        _ => serialport::DataBits::Eight,
    })
    .parity(match s.parity {
        'E' => serialport::Parity::Even,
        'O' => serialport::Parity::Odd,
        _ => serialport::Parity::None,
    })
    .stop_bits(if s.stop_bits == 2 {
        serialport::StopBits::Two
    } else {
        serialport::StopBits::One
    })
    .flow_control(match s.flow.as_str() {
        "hardware" => serialport::FlowControl::Hardware,
        "software" => serialport::FlowControl::Software,
        _ => serialport::FlowControl::None,
    })
    .timeout(Duration::from_millis(40))
    .open()
    .map_err(|e| format!("无法打开串口：{e}"))?;
    // A PTY has no modem pins. serialport also requires baud 0 to skip the
    // macOS-only IOSSIOSPEED ioctl there. Physical devices never skip setup.
    if !pty {
        p.write_data_terminal_ready(true)
            .map_err(|e| format!("无法设置 DTR：{e}"))?;
        p.write_request_to_send(false)
            .map_err(|e| format!("无法设置 RTS：{e}"))?;
    }
    Ok(p)
}
fn is_pty(path: &str) -> bool {
    // Resolve only the user-selected path; no enumeration or unrelated files.
    std::fs::canonicalize(path).is_ok_and(|p| {
        p.to_str()
            .is_some_and(|p| p.starts_with("/dev/ttys") || p.starts_with("/dev/pts/"))
    })
}
pub fn test_connection(path: &str, s: &SerialSettings) -> Result<(), String> {
    drop(open(path, s)?);
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub open: bool,
    pub retrying: bool,
    pub settings: SerialSettings,
    pub hex: bool,
    pub timestamps: bool,
    pub dtr: bool,
    pub rts: bool,
    pub received: u64,
    pub notice: Option<String>,
}
impl State {
    pub fn new(settings: SerialSettings) -> Self {
        Self {
            open: false,
            retrying: false,
            settings,
            hex: false,
            timestamps: false,
            dtr: true,
            rts: false,
            received: 0,
            notice: None,
        }
    }
    pub fn status(&self) -> &'static str {
        if self.open {
            "● 已打开"
        } else if self.retrying {
            "设备已断开"
        } else {
            "○ 已关闭"
        }
    }
}
#[derive(Clone, Debug)]
pub enum Command {
    Open(bool),
    Baud(u32),
    Dtr(bool),
    Rts(bool),
    Newline(Newline),
    Display { hex: bool, timestamps: bool },
    Reset,
    Download,
    Data(Vec<u8>),
    Shutdown,
}
#[derive(Clone)]
pub struct Control {
    tx: mpsc::Sender<Command>,
    state: Arc<Mutex<State>>,
}
impl Control {
    pub fn send(&self, c: Command) -> io::Result<()> {
        self.tx
            .send(c)
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }
    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }
    pub fn take_notice(&self) -> Option<String> {
        self.state.lock().unwrap().notice.take()
    }
}
pub struct Channel {
    control: Control,
    closing: Arc<AtomicBool>,
}
impl Channel {
    pub fn start(path: String, settings: SerialSettings, sink: OutputSink) -> (Self, Control) {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(State::new(settings)));
        let control = Control {
            tx,
            state: state.clone(),
        };
        let closing = Arc::new(AtomicBool::new(false));
        let sink = OutputSink::new(GuardedOutput {
            sink,
            closing: closing.clone(),
        });
        std::thread::spawn(move || worker(path, state, rx, sink));
        (
            Self {
                control: control.clone(),
                closing,
            },
            control,
        )
    }
}
impl TerminalBackend for Channel {
    fn write(&mut self, b: &[u8]) -> io::Result<()> {
        if !self.control.state().open {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "串口未打开"));
        }
        self.control.send(Command::Data(b.to_vec()))
    }
    fn resize(&mut self, _: u16, _: u16, _: u16, _: u16) -> io::Result<()> {
        Ok(())
    }
    fn close(&mut self) {
        self.closing.store(true, Ordering::Release);
        let _ = self.control.send(Command::Shutdown);
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
    }
}
struct GuardedOutput {
    sink: OutputSink,
    closing: Arc<AtomicBool>,
}
impl shellmate_core::OutputHandler for GuardedOutput {
    fn output(&self, bytes: &[u8]) {
        if !self.closing.load(Ordering::Acquire) {
            self.sink.output(bytes);
        }
    }
    fn transformed_output(&self, raw: &[u8], display: &[u8]) {
        if !self.closing.load(Ordering::Acquire) {
            self.sink.transformed_output(raw, display);
        }
    }
    fn exited(&self, _: Option<u32>) {}
    fn disconnected(&self, _: String) {}
}
fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // localtime_r writes only our own stack allocation.
    let tm = unsafe {
        if libc::localtime_r(&secs, tm.as_mut_ptr()).is_null() {
            return "00:00:00.000".into();
        }
        tm.assume_init()
    };
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        now.subsec_millis()
    )
}
struct PortPins<'a>(&'a mut dyn SerialPort);
impl sequence::Pins for PortPins<'_> {
    type Error = serialport::Error;
    fn dtr(&mut self, on: bool) -> Result<(), Self::Error> {
        self.0.write_data_terminal_ready(on)
    }
    fn rts(&mut self, on: bool) -> Result<(), Self::Error> {
        self.0.write_request_to_send(on)
    }
    fn wait(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}
fn worker(path: String, state: Arc<Mutex<State>>, rx: mpsc::Receiver<Command>, sink: OutputSink) {
    let mut port: Option<Box<dyn SerialPort>> = None;
    let mut wanted = true;
    let mut retry = Instant::now();
    let mut display = transform::Display::default();
    let mut sender = transform::Sender::default();
    let mut buf = [0; 4096];
    loop {
        // With no port, wait for a command (and wake at least once a second).
        let commands = if port.is_none() {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(c) => vec![c],
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(_) => vec![],
            }
        } else {
            rx.try_iter().collect()
        };
        for cmd in commands {
            let mut s = state.lock().unwrap().clone();
            let result: Result<(), String> = match cmd {
                Command::Shutdown => {
                    sink.transformed_output(&[], &display.flush(&timestamp()));
                    return;
                }
                Command::Open(on) => {
                    wanted = on;
                    retry = Instant::now();
                    if !on {
                        sink.transformed_output(&[], &display.flush(&timestamp()));
                        port = None;
                        s.open = false;
                        s.retrying = false;
                        s.notice = Some("端口已关闭 · 可以用 esptool 烧录固件了".into());
                    }
                    Ok(())
                }
                Command::Newline(n) => {
                    s.settings.newline = n;
                    Ok(())
                }
                Command::Display { hex, timestamps } => {
                    sink.transformed_output(&[], &display.flush(&timestamp()));
                    display.options(hex, timestamps);
                    s.hex = hex;
                    s.timestamps = timestamps;
                    Ok(())
                }
                Command::Baud(baud) => {
                    if baud == 0 {
                        Err("波特率必须大于 0".into())
                    } else {
                        let result = port
                            .as_mut()
                            .map_or(Ok(()), |p| p.set_baud_rate(baud))
                            .map_err(|e| e.to_string());
                        if result.is_ok() {
                            s.settings.baud = baud;
                            s.notice = Some(format!("波特率已切换为 {baud} · 无需重新打开端口"));
                        }
                        result
                    }
                }
                Command::Dtr(on) | Command::Rts(on) => {
                    let dtr = matches!(cmd, Command::Dtr(_));
                    let result = port
                        .as_mut()
                        .ok_or_else(|| "串口未打开".to_string())
                        .and_then(|p| {
                            if dtr {
                                p.write_data_terminal_ready(on)
                            } else {
                                p.write_request_to_send(on)
                            }
                            .map_err(|e| e.to_string())
                        });
                    if result.is_ok() {
                        if dtr {
                            s.dtr = on;
                        } else {
                            s.rts = on;
                        }
                    }
                    result
                }
                Command::Reset | Command::Download => {
                    let download = matches!(cmd, Command::Download);
                    let result = port
                        .as_mut()
                        .ok_or_else(|| "串口未打开".to_string())
                        .and_then(|p| {
                            sequence::run(
                                &mut PortPins(&mut **p),
                                if download {
                                    sequence::DOWNLOAD
                                } else {
                                    sequence::RESET
                                },
                            )
                            .map_err(|e| e.to_string())
                        });
                    if result.is_ok() {
                        s.dtr = false;
                        s.rts = false;
                        s.notice = Some(
                            if download {
                                "已进入下载模式"
                            } else {
                                "已复位 EN"
                            }
                            .into(),
                        );
                    }
                    result
                }
                Command::Data(bytes) => port
                    .as_mut()
                    .ok_or_else(|| "串口未打开".to_string())
                    .and_then(|p| {
                        p.write_all(&sender.feed(&bytes, s.settings.newline))
                            .map_err(|e| e.to_string())
                    }),
            };
            if let Err(e) = result {
                s.notice = Some(e);
            }
            *state.lock().unwrap() = s;
        }
        if wanted && port.is_none() && Instant::now() >= retry {
            let settings = state.lock().unwrap().settings.clone();
            let result = open(&path, &settings);
            let mut s = state.lock().unwrap();
            match result {
                Ok(p) => {
                    port = Some(p);
                    s.open = true;
                    s.retrying = false;
                    s.dtr = true;
                    s.rts = false;
                    s.notice = Some("端口已打开".into());
                }
                Err(e) => {
                    if !s.retrying {
                        s.notice = Some(e);
                    }
                    s.open = false;
                    s.retrying = true;
                }
            }
            retry = Instant::now() + Duration::from_secs(1);
        }
        if let Some(p) = port.as_mut() {
            match p.read(&mut buf) {
                Ok(n) if n > 0 => {
                    state.lock().unwrap().received += n as u64;
                    sink.transformed_output(&buf[..n], &display.feed(&buf[..n], &timestamp()));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    sink.transformed_output(&[], &display.flush(&timestamp()));
                }
                _ => {
                    sink.transformed_output(&[], &display.flush(&timestamp()));
                    port = None;
                    let mut s = state.lock().unwrap();
                    s.open = false;
                    s.retrying = true;
                    s.notice = Some("设备已断开".into());
                    retry = Instant::now() + Duration::from_secs(1);
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn toolbar_state() {
        let mut s = State::new(SerialSettings::default());
        assert_eq!(s.status(), "○ 已关闭");
        s.open = true;
        assert_eq!(s.status(), "● 已打开");
        assert_eq!(s.settings.label(), "115200 · 8N1");
        s.open = false;
        s.retrying = true;
        assert_eq!(s.status(), "设备已断开");
    }
}
