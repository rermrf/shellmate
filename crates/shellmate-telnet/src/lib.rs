//! Async Telnet transport. No UI, no filesystem, no credentials in diagnostics.
pub mod login;
pub mod protocol;
use shellmate_core::{OutputSink, PtyDims, Secret, TerminalBackend};
use std::{
    io,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};
use zeroize::Zeroize;

pub async fn connect(addr: &str, port: u16) -> Result<TcpStream, String> {
    match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((addr, port))).await {
        Err(_) => Err("连接超时，请检查地址和网络".into()),
        Ok(Err(e)) => Err(match e.kind() {
            io::ErrorKind::ConnectionRefused => "连接被拒绝，请检查端口和服务".into(),
            _ => format!("连接失败：{e}"),
        }),
        Ok(Ok(s)) => Ok(s),
    }
}
enum Command {
    Data(Vec<u8>),
    Resize(u16, u16),
}
pub struct Channel {
    tx: mpsc::UnboundedSender<Command>,
    task: tokio::task::JoinHandle<()>,
}
impl Channel {
    pub fn start(
        stream: TcpStream,
        sink: OutputSink,
        dims: PtyDims,
        user: String,
        password: Option<Secret>,
        runtime: &tokio::runtime::Handle,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let start = Instant::now();
        let task = runtime.spawn(async move {
            let mut stream = stream;
            let mut parser = protocol::Parser::new(dims);
            let mut login = login::AutoLogin::new(user, password);
            let mut buf = [0; 8192];
            let result: io::Result<()> = async {
                loop {
                    let deadline = tokio::time::Instant::from_std(start + login.deadline());
                    tokio::select! {
                        data = stream.read(&mut buf) => {
                            let n = data?;
                            if n == 0 {
                                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "服务器已关闭连接"));
                            }
                            login.received(start.elapsed());
                            let (out, reply) = parser.feed(&buf[..n]);
                            stream.write_all(&reply).await?;
                            for response in login.feed(&out, start.elapsed()) {
                                let mut wire = protocol::encode(response.expose().as_bytes());
                                let result = stream.write_all(&wire).await;
                                wire.zeroize();
                                result?;
                            }
                            sink.output(&out);
                        },
                        cmd = rx.recv() => match cmd {
                            None => return Ok(()),
                            Some(Command::Data(bytes)) => {
                                login.cancel();
                                stream.write_all(&protocol::encode(&bytes)).await?;
                            },
                            Some(Command::Resize(c, r)) => stream.write_all(&parser.resize(c, r)).await?,
                        },
                        _ = tokio::time::sleep_until(deadline), if login.pending() => login.cancel(),
                    }
                }
            }.await;
            if let Err(e) = result {
                sink.disconnected(format!("Telnet 连接已断开：{e}"));
            }
        });
        Self { tx, task }
    }
}
impl TerminalBackend for Channel {
    fn write(&mut self, b: &[u8]) -> io::Result<()> {
        self.tx
            .send(Command::Data(b.to_vec()))
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }
    fn resize(&mut self, c: u16, r: u16, _: u16, _: u16) -> io::Result<()> {
        self.tx
            .send(Command::Resize(c, r))
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }
    fn close(&mut self) {
        self.task.abort();
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
    }
}
/// Connection test consumes prompts and sends credentials, without opening a tab
/// or running a command. Telnet cannot prove authentication without a server-
/// specific shell prompt; report that limitation instead of declaring success.
pub async fn test_connection(
    addr: &str,
    port: u16,
    user: String,
    password: Option<Secret>,
) -> Result<String, String> {
    let mut stream = connect(addr, port).await?;
    if password.is_none() {
        return Ok("连接成功 · 未提供密码，请在终端手动登录".into());
    }
    let mut login = login::AutoLogin::new(user, password);
    let mut parser = protocol::Parser::new(PtyDims::default());
    let start = Instant::now();
    let mut buf = [0; 4096];
    let mut auth_tail = String::new();
    while start.elapsed() < login.deadline() {
        let n = tokio::time::timeout(
            login.deadline().saturating_sub(start.elapsed()),
            stream.read(&mut buf),
        )
        .await
        .map_err(|_| "自动登录超时，请在终端手动登录".to_string())?
        .map_err(|e| format!("连接失败：{e}"))?;
        if n == 0 {
            return Err("服务器已关闭连接".into());
        }
        login.received(start.elapsed());
        let (out, reply) = parser.feed(&buf[..n]);
        stream.write_all(&reply).await.map_err(|e| e.to_string())?;
        for response in login.feed(&out, start.elapsed()) {
            let mut wire = protocol::encode(response.expose().as_bytes());
            let result = stream.write_all(&wire).await;
            wire.zeroize();
            result.map_err(|e| e.to_string())?;
        }
        if login.password_sent() {
            auth_tail.push_str(&String::from_utf8_lossy(&out));
            if auth_tail.len() > 4096 {
                auth_tail = auth_tail
                    .chars()
                    .rev()
                    .take(1024)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
            }
            match login::authentication_result(&auth_tail) {
                Some(true) => return Ok("连接成功 · 认证方式 密码（已检测到终端提示符）".into()),
                Some(false) => return Err("认证失败，请检查用户名和密码".into()),
                None => {}
            }
        }
    }
    Err("自动登录超时，无法确认认证结果，请在终端手动登录".into())
}
