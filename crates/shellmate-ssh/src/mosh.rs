//! Mosh bootstrap over the existing SSH connection. The UDP protocol belongs to
//! mosh-client, never this crate. Bootstrap output/key never enters a terminal.
use crate::{Connection, ExecOptions};
use shellmate_core::{Program, Secret};
use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use zeroize::Zeroize;
/// Missing local executable, shown before any remote process is started.
pub const MISSING_CLIENT: &str = "本机未安装 mosh（brew install mosh）";
/// Remote executable prerequisite.
pub const MISSING_SERVER: &str = "远端未安装 mosh-server（例如 sudo apt install mosh）";
/// A jump transports SSH TCP, not Mosh UDP.
pub const JUMP_REFUSAL: &str = "Mosh 需要直接访问主机的 UDP 端口，经跳板机时无法使用";
/// Nonpersistent executable check, also used by the editor.
pub const SERVER_CHECK: &str = "command -v mosh-server";
/// Installed locales on the server.
pub const LOCALES: &str = "locale -a";
const LIMIT: ExecOptions = ExecOptions {
    timeout: Duration::from_secs(10),
    max_output: 32768,
};
/// Reject indirect access before authentication/bootstrap.
pub fn validate_direct(has_jumps: bool) -> Result<(), String> {
    if has_jumps {
        Err(JUMP_REFUSAL.into())
    } else {
        Ok(())
    }
}
/// Candidates cover Finder's minimal PATH as well as common package managers.
pub fn client_candidates(path: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut candidates = path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|p| p.is_absolute())
        .map(|p| p.join("mosh-client"))
        .collect::<Vec<_>>();
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"] {
        let p = Path::new(dir).join("mosh-client");
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    }
    candidates
}
/// Injectable executable check, no process launch or network access.
pub fn discover(
    candidates: impl IntoIterator<Item = PathBuf>,
    executable: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    candidates
        .into_iter()
        .find(|p| executable(p))
        .ok_or_else(|| MISSING_CLIENT.into())
}
/// Find an executable file on this machine (including Finder launch paths).
pub fn find_client() -> Result<PathBuf, String> {
    discover(
        client_candidates(std::env::var_os("PATH").as_deref()),
        is_executable,
    )
}
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
/// Select a server-installed UTF-8 locale, preserving its exact spelling.
pub fn select_locale(output: &str) -> Result<String, String> {
    let locales = output
        .lines()
        .map(str::trim)
        .filter(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-@".contains(&b))
        })
        .filter(|s| {
            let s = s.to_ascii_lowercase();
            s.ends_with(".utf-8") || s.ends_with(".utf8")
        })
        .collect::<Vec<_>>();
    for preferred in ["c.utf8", "en_us.utf8"] {
        if let Some(locale) = locales
            .iter()
            .find(|s| s.to_ascii_lowercase().replace('-', "") == preferred)
        {
            return Ok((*locale).into());
        }
    }
    locales
        .first()
        .map(|s| (*s).into())
        .ok_or_else(|| "远端没有可用的 UTF-8 locale，请安装 C.UTF-8 或 en_US.UTF-8".into())
}
/// The bootstrap reply; Debug redacts the key.
#[derive(Debug)]
pub struct Reply {
    /// UDP port advertised by mosh-server.
    pub port: u16,
    /// PID from its detached banner, used only for this server's cleanup.
    pub pid: u32,
    /// 128-bit Mosh session key, never a command-line argument.
    pub key: Secret,
}
/// Detached PID, independent of stdout and unrelated stderr noise.
pub fn parse_pid(stderr: &str) -> Option<u32> {
    stderr.lines().find_map(|line| {
        let rest = line.split_once("[mosh-server detached, pid = ")?.1;
        let pid = rest.split_once(']')?.0.trim().parse::<u32>().ok()?;
        (pid > 1 && pid <= i32::MAX as u32).then_some(pid)
    })
}
/// Parse a single unambiguous CONNECT line and the stderr PID. Errors never
/// interpolate stdout or a key. Short stderr excerpts redact key-shaped words.
pub fn parse_reply(stdout: &str, stderr: &str) -> Result<Reply, String> {
    let mut replies = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with("MOSH CONNECT "));
    let parsed = replies.next().and_then(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 4 {
            return None;
        }
        let port = fields[2].parse::<u16>().ok().filter(|p| *p > 0)?;
        let key = fields[3].trim_end_matches('=');
        let decoded =
            zeroize::Zeroizing::new(data_encoding::BASE64_NOPAD.decode(key.as_bytes()).ok()?);
        if decoded.len() != 16 {
            return None;
        }
        Some((port, Secret::new(fields[3])))
    });
    match (parsed, replies.next(), parse_pid(stderr)) {
        (Some((port, key)), None, Some(pid)) => Ok(Reply { port, pid, key }),
        _ => Err(format!(
            "Mosh 启动失败：没有有效的 MOSH CONNECT / pid 信息 · {}",
            stderr_excerpt(stderr)
        )),
    }
}
fn stderr_excerpt(stderr: &str) -> String {
    let mut clean = String::new();
    for line in stderr.lines().filter(|l| !l.contains("MOSH CONNECT")) {
        for word in line.split_whitespace() {
            let sensitive = word
                .split(|c: char| !c.is_ascii_alphanumeric() && !"+/=".contains(c))
                .any(|w| w.len() >= 20);
            clean.push_str(if sensitive { "[已隐藏]" } else { word });
            clean.push(' ');
        }
    }
    clean
        .chars()
        .filter(|c| !c.is_control())
        .take(180)
        .collect::<String>()
        .trim()
        .to_string()
}
/// Build an argv/env specification. The local macOS locale is independent of
/// the server's installed locale. Keep prediction adaptive for this milestone.
pub fn client_program(path: PathBuf, ip: IpAddr, reply: &Reply) -> Program {
    Program {
        executable: path,
        args: vec![ip.to_string(), reply.port.to_string()],
        env: [
            ("MOSH_KEY", reply.key.clone()),
            ("TERM", Secret::new("xterm-256color")),
            ("LANG", Secret::new("en_US.UTF-8")),
            ("LC_ALL", Secret::new("en_US.UTF-8")),
            ("MOSH_PREDICTION_DISPLAY", Secret::new("adaptive")),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect(),
        cwd: None,
    }
}
/// Availability/locale probe. Starts nothing persistent.
pub async fn prerequisites(connection: &Connection) -> Result<String, String> {
    let check = connection
        .exec_with(SERVER_CHECK, LIMIT)
        .await
        .map_err(|e| format!("Mosh 检查失败：{e}"))?;
    if !check.status.success() || check.stdout.is_empty() {
        return Err(MISSING_SERVER.into());
    }
    let locales = connection
        .exec_with(LOCALES, LIMIT)
        .await
        .map_err(|e| format!("无法读取远端 locale：{e}"))?;
    if !locales.status.success() || locales.truncated {
        return Err("无法读取远端 UTF-8 locale（locale -a 失败）".into());
    }
    select_locale(&locales.stdout_text())
}
/// An armed lease cleans up a bootstrapped server if the UI goes away, spawn
/// fails, or the client dies before connecting. A normal shell exit disarms it.
#[derive(Clone)]
pub struct ServerLease(Arc<LeaseInner>);
struct LeaseInner {
    connection: Mutex<Connection>,
    pid: u32,
    armed: AtomicBool,
}
impl Drop for LeaseInner {
    fn drop(&mut self) {
        if self.armed.swap(false, Ordering::AcqRel) {
            let connection = self.connection.lock().unwrap().clone();
            let pid = self.pid;
            connection.inner.runtime.clone().spawn(async move {
                let _ = kill_server(&connection, pid).await;
            });
        }
    }
}
impl ServerLease {
    fn new(connection: Connection, pid: u32) -> Self {
        Self(Arc::new(LeaseInner {
            connection: Mutex::new(connection),
            pid,
            armed: AtomicBool::new(true),
        }))
    }
    /// Use the replacement SSH connection for future cleanup, without touching UDP.
    pub fn reconnect(&self, connection: Connection) {
        *self.0.connection.lock().unwrap() = connection;
    }
    /// PID owned by this lease (never kill every server of a user).
    pub fn pid(&self) -> u32 {
        self.0.pid
    }
    /// Explicit cleanup for a failed client; safe to call repeatedly.
    pub async fn cleanup(&self) -> Result<(), String> {
        if !self.0.armed.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let connection = self.0.connection.lock().unwrap().clone();
        let result = kill_server(&connection, self.0.pid).await;
        if result.is_err() {
            self.0.armed.store(true, Ordering::Release);
        }
        result
    }
    /// Called only after a successful, non-immediate exit. The remote shell
    /// ends its own mosh-server; avoid sending a late signal to a recycled PID.
    pub fn normal_exit(&self) {
        self.0.armed.store(false, Ordering::Release);
    }
    /// Client exit cleanup policy. Even status 0 in the first three seconds is
    /// startup failure (e.g. the 0×0 regression); ordinary exits disarm cleanup.
    pub fn client_exited(&self, code: Option<u32>, elapsed: Duration) {
        if code == Some(0) && elapsed >= Duration::from_secs(3) {
            self.normal_exit();
            return;
        }
        let lease = self.clone();
        let runtime = self.0.connection.lock().unwrap().inner.runtime.clone();
        runtime.spawn(async move {
            if lease.cleanup().await.is_err() {
                log::warn!("Mosh 服务端清理失败");
            }
        });
    }
}
async fn kill_server(connection: &Connection, pid: u32) -> Result<(), String> {
    // Validate the executable before signaling: never a user-wide pkill.
    let cmd = format!(
        "case \"$(ps -p {pid} -o comm= 2>/dev/null)\" in *mosh-server*) kill -TERM {pid} 2>/dev/null ;; *) : ;; esac"
    );
    let output = connection
        .exec_with(&cmd, LIMIT)
        .await
        .map_err(|_| "无法清理远端 mosh-server（SSH 连接不可用）".to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err("无法清理远端 mosh-server".into())
    }
}
/// Prepared local launch and the remote process it owns.
pub struct Prepared {
    /// Command/env handed to the local PTY; Debug is secret-safe.
    pub program: Program,
    /// Cleanup lease must outlive the local client.
    pub lease: ServerLease,
    /// UDP port displayed by the terminal footer.
    pub port: u16,
}
/// Check local availability before any remote command. The bounded bootstrap
/// runs to completion even if its UI waiter is cancelled: an unclaimed Prepared
/// then drops its lease and cleans up the detached server.
pub async fn prepare(connection: Connection, client: PathBuf) -> Result<Prepared, String> {
    if !is_executable(&client) {
        return Err(MISSING_CLIENT.into());
    }
    validate_direct(connection.hops().len() != 1)?;
    let runtime = connection.inner.runtime.clone();
    runtime
        .spawn(async move {
            let ip = connection
                .peer_addr()
                .ok_or("无法确定 SSH 连接的服务器 IP")?
                .ip();
            let locale = prerequisites(&connection).await?;
            let command = format!("mosh-server new -s -c 256 -l LANG={locale}");
            let mut output = connection
                .exec_with(&command, LIMIT)
                .await
                .map_err(|e| format!("Mosh 启动失败：{e}"))?;
            let stdout = zeroize::Zeroizing::new(output.stdout_text());
            let stderr = zeroize::Zeroizing::new(output.stderr_text());
            let pid = parse_pid(&stderr);
            let lease = pid.map(|pid| ServerLease::new(connection.clone(), pid));
            let reply = parse_reply(&stdout, &stderr);
            output.stdout.zeroize();
            output.stderr.zeroize();
            if !output.status.success() || output.truncated {
                return Err(format!(
                    "Mosh 启动失败，请检查远端 locale 和 UDP 端口 · {}",
                    stderr_excerpt(&stderr)
                ));
            }
            let reply = reply?;
            Ok(Prepared {
                program: client_program(client, ip, &reply),
                lease: lease.ok_or("Mosh 未返回服务端 pid")?,
                port: reply.port,
            })
        })
        .await
        .map_err(|_| "Mosh 启动任务已中止".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str = "abcdefghijklmnopqrstuw"; // valid 128-bit fixture base64
    #[test]
    fn parse_noisy_stdout_stderr_crlf_and_redact_failures() {
        let reply = parse_reply(
            &format!("noise\r\nMOSH CONNECT 60001 {KEY}\r\n"),
            "mosh-server 1.4.0\r\n[mosh-server detached, pid = 12345]\r\n",
        )
        .unwrap();
        assert_eq!((reply.port, reply.pid), (60001, 12345));
        assert!(!format!("{reply:?}").contains(KEY));
        for stdout in [
            "nothing".to_string(),
            format!("MOSH CONNECT 0 {KEY}"),
            format!("MOSH CONNECT 65536 {KEY}"),
            "MOSH CONNECT 60001 bad".into(),
            format!("MOSH CONNECT 60001 {KEY}\nMOSH CONNECT 60002 {KEY}"),
        ] {
            assert!(parse_reply(&stdout, "[mosh-server detached, pid = 12345]").is_err());
        }
        let error =
            parse_reply("", &format!("warning key={KEY}\nMOSH CONNECT 60001 {KEY}")).unwrap_err();
        assert!(!error.contains(KEY));
        assert!(error.len() < 400);
        assert!(parse_reply(&format!("MOSH CONNECT 60001 {KEY}"), "missing pid").is_err());
        assert_eq!(parse_pid("[mosh-server detached, pid = 1]"), None);
    }
    #[test]
    fn locale_preference_and_safe_shell_word() {
        assert_eq!(select_locale("en_US.UTF-8\nC\nC.utf8\n").unwrap(), "C.utf8");
        assert_eq!(
            select_locale("zh_CN.UTF-8\nen_US.utf8\n").unwrap(),
            "en_US.utf8"
        );
        assert_eq!(select_locale("C\nzh_CN.UTF-8\n").unwrap(), "zh_CN.UTF-8");
        assert!(select_locale("C\nPOSIX\nx;echo.utf8\n").is_err());
    }
    #[test]
    fn discover_injected_candidates_and_finder_fallbacks() {
        let candidates = client_candidates(Some(std::ffi::OsStr::new("/usr/bin:/bin:relative")));
        assert!(candidates.contains(&PathBuf::from("/opt/homebrew/bin/mosh-client")));
        assert_eq!(
            discover(candidates, |p| p == Path::new("/opt/local/bin/mosh-client")).unwrap(),
            PathBuf::from("/opt/local/bin/mosh-client")
        );
        assert_eq!(discover(Vec::new(), |_| true).unwrap_err(), MISSING_CLIENT);
        assert_eq!(validate_direct(true).unwrap_err(), JUMP_REFUSAL);
        assert!(validate_direct(false).is_ok());
    }
    #[test]
    fn ipv6_and_secret_only_in_environment() {
        let reply = Reply {
            port: 60001,
            pid: 12345,
            key: Secret::new(KEY),
        };
        let p = client_program("/tmp/client".into(), "2001:db8::5".parse().unwrap(), &reply);
        assert_eq!(p.args, ["2001:db8::5", "60001"]);
        assert!(!p.args.join(" ").contains(KEY));
        assert_eq!(p.env["MOSH_KEY"].expose(), KEY);
        assert_eq!(p.env["TERM"].expose(), "xterm-256color");
        assert_eq!(p.env["LC_ALL"].expose(), "en_US.UTF-8");
        assert_eq!(p.env["MOSH_PREDICTION_DISPLAY"].expose(), "adaptive");
        assert!(!format!("{p:?}").contains(KEY));
    }
}
