use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::commands::{Command as Words, Plan};
use crate::{ActionOutcome, ContainerAccess, ContainerRuntime, RuntimeKind};
use shellmate_ssh::{ExecOutput, ExecStatus};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

fn resolve(path: &OsStr, home: Option<&Path>, executable: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .chain([PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")])
        .chain(home.map(|home| home.join(".orbstack/bin")))
        .chain(home.map(|home| home.join(".docker/bin")))
        .chain([PathBuf::from("/Applications/Docker.app/Contents/Resources/bin")])
        .map(|dir| dir.join("docker"))
        .find(|candidate| executable(candidate))
}

pub(crate) fn docker_path() -> Option<PathBuf> {
    resolve(
        &std::env::var_os("PATH").unwrap_or_default(),
        std::env::var_os("HOME").as_deref().map(Path::new),
        |path| {
            path.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        },
    )
}

pub(crate) async fn docker_probe(path: &Path) -> ContainerRuntime {
    let result = output(
        Command::new(path).args(["version", "--format", "{{.Server.Version}}"]),
        Duration::from_secs(4),
        8192,
    )
    .await;
    let version = result
        .ok()
        .filter(|out| out.status.success())
        .map(|out| out.stdout_text().trim().to_owned());
    ContainerRuntime {
        kind: RuntimeKind::Docker,
        path: path.to_string_lossy().into_owned(),
        access: if version.is_some() {
            ContainerAccess::User
        } else {
            ContainerAccess::NoDaemon
        },
        version,
    }
}

async fn bounded(mut pipe: impl AsyncRead + Unpin, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut data = Vec::new();
    (&mut pipe).take(limit as u64 + 1).read_to_end(&mut data).await?;
    let truncated = data.len() > limit;
    if truncated {
        data.truncate(limit);
        tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await?;
    }
    Ok((data, truncated))
}

pub(crate) async fn output(cmd: &mut Command, timeout: Duration, limit: usize) -> io::Result<ExecOutput> {
    let mut child = cmd
        .env("LC_ALL", "C")
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let ((stdout, large_out), (stderr, large_err), status) = tokio::time::timeout(timeout, async {
        tokio::try_join!(bounded(stdout, limit), bounded(stderr, limit), child.wait())
    })
    .await??;
    Ok(ExecOutput {
        stdout,
        stderr,
        status: status
            .code()
            .map_or(ExecStatus::Unknown, |code| ExecStatus::Code(code as u32)),
        truncated: large_out || large_err,
    })
}

pub(crate) fn shell(line: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", &format!("exec {line}")]);
    cmd
}

pub(crate) struct Stream {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Stream {
    pub(crate) fn open(line: &str) -> io::Result<Self> {
        let mut child = shell(line)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Ok(Self { child, lines })
    }
    pub(crate) async fn next_line(&mut self) -> Option<String> {
        self.lines.next_line().await.ok().flatten()
    }
    pub(crate) async fn finish(&mut self) -> String {
        match self.child.wait().await {
            Ok(status) if status.success() => String::new(),
            Ok(status) => status.to_string(),
            Err(err) => err.to_string(),
        }
    }
}

pub(crate) async fn process_action<F, Fut>(words: Words, run: F) -> ActionOutcome
where
    F: FnOnce(Command) -> Fut,
    Fut: Future<Output = io::Result<ExecOutput>>,
{
    let argv = words.argv();
    let program = match argv[0].as_str() {
        "kill" => "/bin/kill",
        "renice" => "/usr/bin/renice",
        _ => return ActionOutcome::Error("不支持的进程操作".into()),
    };
    let mut cmd = Command::new(program);
    cmd.args(&argv[1..]);
    let result = run(cmd).await;
    let denied = match &result {
        Ok(out) if !out.status.success() => {
            let msg = out.stderr_text().to_ascii_lowercase();
            msg.contains("operation not permitted") || msg.contains("permission denied")
        }
        Err(err) => err.kind() == io::ErrorKind::PermissionDenied,
        _ => false,
    };
    if denied {
        ActionOutcome::Terminal(Plan::Terminal {
            line: words.prefixed(&["sudo"]).line(),
        })
    } else {
        super::super::monitor::outcome(result.map_err(|err| err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Signal, kill};

    #[test]
    fn docker_resolution_order_with_injected_paths() {
        let paths = OsStr::new("relative:/custom/bin");
        let home = Some(Path::new("/fake-home"));
        for (index, expected) in [
            "/custom/bin/docker",
            "/opt/homebrew/bin/docker",
            "/usr/local/bin/docker",
            "/fake-home/.orbstack/bin/docker",
        ]
        .into_iter()
        .enumerate()
        {
            let candidates = [
                "/custom/bin/docker",
                "/opt/homebrew/bin/docker",
                "/usr/local/bin/docker",
                "/fake-home/.orbstack/bin/docker",
            ];
            assert_eq!(
                resolve(paths, home, |path| candidates[index..]
                    .iter()
                    .any(|candidate| path == Path::new(candidate))),
                Some(expected.into())
            );
        }
        assert_eq!(resolve(paths, home, |_| false), None);
    }

    #[tokio::test]
    async fn signal_only_our_child_and_simulate_denied() {
        let mut child = Command::new("/bin/sleep").arg("30").kill_on_drop(true).spawn().unwrap();
        let pid = child.id().unwrap();
        let words = kill(pid, Signal::Term).unwrap();
        let denied = process_action(words.clone(), |_| async {
            Ok(ExecOutput {
                stdout: vec![],
                stderr: b"Operation not permitted".into(),
                status: ExecStatus::Code(1),
                truncated: false,
            })
        })
        .await;
        assert_eq!(
            denied,
            ActionOutcome::Terminal(Plan::Terminal {
                line: format!("sudo kill -TERM {pid}")
            })
        );
        let sent = process_action(words, |mut cmd| async move {
            output(&mut cmd, Duration::from_secs(3), 4096).await
        })
        .await;
        assert!(matches!(sent, ActionOutcome::Success { .. }));
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGTERM));
    }
}
