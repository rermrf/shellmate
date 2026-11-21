//! Inspect only processes attached to this PTY; never infer idleness from output.
use super::startup::{PROC_FLAG_EXEC, name_after_exec, snapshot};
use shellmate_core::io::{CloseActivity, ProcessInfo};
use std::mem::size_of;

const PROC_TTY_ONLY: u32 = 3; // macOS SDK sys/proc_info.h

pub(super) fn inspect(pid: u32, initial_program: &str) -> CloseActivity {
    inspect_inner(pid, initial_program).unwrap_or(CloseActivity::Unknown)
}

fn inspect_inner(pid: u32, initial_program: &str) -> Option<CloseActivity> {
    let root = snapshot(pid.try_into().ok()?)?;
    if root.pbi_status == libc::SZOMB {
        return Some(CloseActivity::Idle);
    }
    // The child is still between fork and exec: no shell command has run yet.
    if root.pbi_flags & PROC_FLAG_EXEC == 0 {
        return Some(CloseActivity::Idle);
    }
    if root.e_tdev == u32::MAX {
        return None;
    }
    let mut processes = vec![root];
    for other in tty_pids(root.e_tdev)? {
        if other <= 0 || other as u32 == pid {
            continue;
        }
        match snapshot(other) {
            Some(info) if info.e_tdev == root.e_tdev => processes.push(info),
            Some(_) => {} // Process detached from the PTY after enumeration.
            None if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) => {}
            None => return None,
        }
    }
    // Check that the initial process did not exec/exit during enumeration.
    let current = snapshot(pid as i32)?;
    if current.pbi_start_tvsec != root.pbi_start_tvsec
        || current.pbi_start_tvusec != root.pbi_start_tvusec
        || name_after_exec(&current) != name_after_exec(&root)
    {
        return None;
    }
    Some(classify(pid, initial_program, &processes))
}

fn tty_pids(tty: u32) -> Option<Vec<i32>> {
    // A bounded retry handles processes joining the terminal during enumeration.
    let mut capacity = 32;
    for _ in 0..8 {
        let mut pids = vec![0_i32; capacity];
        let bytes = (pids.len() * size_of::<i32>()) as i32;
        // SAFETY: the buffer is writable for exactly bytes; PROC_TTY_ONLY takes a dev_t.
        let used =
            unsafe { libc::proc_listpids(PROC_TTY_ONLY, tty, pids.as_mut_ptr().cast(), bytes) };
        if used <= 0 || used % size_of::<i32>() as i32 != 0 {
            return None;
        }
        if used < bytes {
            pids.truncate(used as usize / size_of::<i32>());
            return Some(pids);
        }
        capacity *= 2;
    }
    None
}

fn classify(pid: u32, initial_program: &str, processes: &[libc::proc_bsdinfo]) -> CloseActivity {
    let mut jobs = Vec::new();
    for info in processes {
        if info.pbi_status == libc::SZOMB {
            continue;
        }
        let Some(name) = name_after_exec(info) else {
            // A child still starting is work too; it must not look like an idle shell.
            return CloseActivity::Unknown;
        };
        if info.pbi_pid == pid && name == initial_program && is_shell(&name) {
            continue;
        }
        jobs.push(ProcessInfo {
            pid: info.pbi_pid as i32,
            name,
            cwd: None,
        });
    }
    jobs.sort_by_key(|job| job.pid);
    if jobs.is_empty() {
        CloseActivity::Idle
    } else {
        CloseActivity::Busy(jobs)
    }
}

fn is_shell(name: &str) -> bool {
    matches!(
        name,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ksh"
            | "csh"
            | "tcsh"
            | "nu"
            | "elvish"
            | "xonsh"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalPty;
    use portable_pty::CommandBuilder;
    use shellmate_core::io::{PtyDims, TerminalBackend};
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    fn process(pid: u32, name: &str, status: u32) -> libc::proc_bsdinfo {
        // SAFETY: proc_bsdinfo consists only of integer fields/arrays.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        info.pbi_pid = pid;
        info.pbi_flags = 0x4000;
        info.pbi_status = status;
        for (dst, byte) in info.pbi_name.iter_mut().zip(name.bytes()) {
            *dst = byte as _;
        }
        info
    }

    #[test]
    fn only_initial_shell_is_idle_and_same_pid_exec_is_busy() {
        assert_eq!(
            classify(10, "zsh", &[process(10, "zsh", 2)]),
            CloseActivity::Idle
        );
        for name in ["vim", "codex", "claude", "node", "sleep"] {
            assert!(matches!(
                classify(10, "zsh", &[process(10, name, 2)]),
                CloseActivity::Busy(_)
            ));
        }
        assert!(matches!(
            classify(10, "codex", &[process(10, "codex", 2)]),
            CloseActivity::Busy(_)
        ));
        assert!(matches!(
            classify(10, "zsh", &[process(10, "zsh", 2), process(11, "zsh", 2)]),
            CloseActivity::Busy(_)
        ));
    }

    #[test]
    fn suspended_and_background_jobs_count_but_zombies_do_not() {
        let shell = process(10, "zsh", 2);
        for status in [2, 3, 4] {
            assert!(matches!(
                classify(10, "zsh", &[shell, process(11, "vim", status)]),
                CloseActivity::Busy(_)
            ));
        }
        assert_eq!(
            classify(10, "zsh", &[shell, process(11, "sleep", libc::SZOMB)]),
            CloseActivity::Idle
        );
        let mut starting = process(11, "zsh", 2);
        starting.pbi_flags = 0;
        assert_eq!(
            classify(10, "zsh", &[shell, starting]),
            CloseActivity::Unknown
        );
        assert_eq!(inspect(u32::MAX, "zsh"), CloseActivity::Unknown);
    }

    fn wait(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "PTY activity did not reach expected state"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn real_pty_idle_foreground_background_stopped_and_exec() {
        let dir = tempfile::tempdir().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let recorded = output.clone();
        let mut command = CommandBuilder::new("/bin/zsh");
        command.args(["-d", "-f", "-i"]);
        command.cwd(dir.path());
        command.env("ZDOTDIR", dir.path());
        command.env("PS1", "ready> ");
        let mut pty = LocalPty::spawn_builder(
            command,
            PtyDims::default(),
            move |bytes| recorded.lock().unwrap().extend_from_slice(bytes),
            |_| {},
        )
        .unwrap();
        wait(|| output.lock().unwrap().windows(7).any(|w| w == b"ready> "));
        assert_eq!(pty.close_activity(), CloseActivity::Idle);
        pty.write(b"sleep 30\r").unwrap();
        wait(
            || matches!(pty.close_activity(), CloseActivity::Busy(ref jobs) if jobs.iter().any(|j| j.name == "sleep")),
        );
        pty.write(b"\x1a").unwrap();
        wait(|| pty.foreground_process().is_some_and(|p| p.name == "zsh"));
        assert!(matches!(pty.close_activity(), CloseActivity::Busy(_)));
        pty.write(b"kill -KILL %1\r").unwrap();
        wait(|| pty.close_activity() == CloseActivity::Idle);
        pty.write(b"sleep 30 &\r").unwrap();
        wait(|| matches!(pty.close_activity(), CloseActivity::Busy(_)));
        wait(|| pty.foreground_process().is_some_and(|p| p.name == "zsh"));
        assert!(matches!(pty.close_activity(), CloseActivity::Busy(_)));
        pty.write(b"kill -KILL %1\r").unwrap();
        wait(|| pty.close_activity() == CloseActivity::Idle);
        pty.write(b"exec sleep 30\r").unwrap();
        wait(
            || matches!(pty.close_activity(), CloseActivity::Busy(ref jobs) if jobs.iter().any(|j| j.name == "sleep" && j.pid as u32 == pty.child_pid().unwrap())),
        );
    }
}
