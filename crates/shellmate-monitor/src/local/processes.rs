use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::Process;

const COLUMNS: &str = "pid,ppid,rss,state,nice,time,lstart,args";
type CpuByIdentity = HashMap<(u32, u64), f64>;

fn seconds(text: &str) -> Option<f64> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u32>().ok()?, clock),
        None => (0, text),
    };
    let mut parts = clock.rsplit(':');
    let secs = parts.next()?.parse::<f64>().ok()?;
    let minutes = parts.next()?.parse::<u64>().ok()?;
    let hours = parts.next().map(str::parse::<u32>).transpose().ok()?.unwrap_or(0);
    (parts.next().is_none() && (0.0..60.0).contains(&secs))
        .then_some(f64::from(days) * 86400.0 + f64::from(hours) * 3600.0 + minutes as f64 * 60.0 + secs)
}

fn word<'a>(rest: &mut &'a str) -> Option<&'a str> {
    let trimmed = rest.trim_start();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    *rest = &trimmed[end..];
    (end != 0).then_some(&trimmed[..end])
}

fn row(bytes: &[u8], owner_column: usize, total: u64) -> Option<(Process, f64)> {
    let (exe, tail) = bytes.split_at_checked(owner_column)?;
    let exe = String::from_utf8_lossy(exe);
    let name = exe.trim().rsplit('/').next()?.trim_matches(['(', ')']).to_owned();
    let tail = String::from_utf8_lossy(tail);
    let mut rest = tail.as_ref();
    let user = word(&mut rest)?.to_owned();
    let pid = word(&mut rest)?.parse().ok()?;
    let ppid = word(&mut rest)?.parse().ok()?;
    let rss = word(&mut rest)?.parse::<u64>().ok()?.checked_mul(1024)?;
    let state = word(&mut rest)?.chars().next()?;
    let nice = word(&mut rest)?.parse().ok()?;
    let cpu = seconds(word(&mut rest)?)?;
    let mut birth = DefaultHasher::new();
    for _ in 0..5 {
        word(&mut rest)?.hash(&mut birth);
    }
    let args = rest.trim_start();
    let command = if args.is_empty() { &name } else { args }.chars().take(2048).collect();
    Some((
        Process {
            pid,
            ppid,
            user,
            name,
            command,
            state,
            nice,
            start_time: birth.finish(),
            threads: 0,
            rss_bytes: Some(rss),
            cpu_percent: None,
            mem_percent: (total > 0).then(|| (100.0 * rss as f64 / total as f64) as f32),
        },
        cpu,
    ))
}

#[derive(Default)]
pub(super) struct Processes {
    previous: Option<(Instant, CpuByIdentity)>,
}

impl Processes {
    pub(super) async fn sample(&mut self, total: u64) -> io::Result<Vec<Process>> {
        tokio::time::timeout(Duration::from_secs(10), self.read(total)).await?
    }

    async fn read(&mut self, total: u64) -> io::Result<Vec<Process>> {
        let at = Instant::now();
        // Darwin sizes non-final comm to 16 bytes unless its heading asks for more.
        let exe_column = format!("comm={}", "X".repeat(libc::PATH_MAX as usize));
        let mut child = tokio::process::Command::new("/bin/ps")
            .env("LC_ALL", "C")
            .env("TZ", "UTC0")
            .args(["-axww", "-o", &exe_column, "-o", "user=OWNER", "-o", COLUMNS])
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut line = Vec::new();
        lines.read_until(b'\n', &mut line).await?;
        let column = line
            .windows(5)
            .position(|word| word == b"OWNER")
            .ok_or_else(|| io::Error::other("/bin/ps: missing header"))?;
        let mut rows = Vec::new();
        loop {
            line.clear();
            if lines.read_until(b'\n', &mut line).await? == 0 {
                break;
            }
            if let Some(process) = row(line.strip_suffix(b"\n").unwrap_or(&line), column, total) {
                rows.push(process);
            }
        }
        let status = child.wait().await?;
        if !status.success() || rows.is_empty() {
            return Err(io::Error::other(format!("/bin/ps: no usable process list ({status})")));
        }
        Ok(self.apply(at, rows.into_iter()))
    }

    fn apply(&mut self, at: Instant, rows: impl Iterator<Item = (Process, f64)>) -> Vec<Process> {
        let mut next = HashMap::new();
        let mut processes = Vec::new();
        for (mut process, cpu) in rows {
            let key = (process.pid, process.start_time);
            if let Some((before, values)) = &self.previous
                && let Some(old) = values.get(&key)
            {
                let elapsed = (at - *before).as_secs_f64();
                process.cpu_percent = (elapsed > 0.0 && cpu >= *old).then(|| (100.0 * (cpu - old) / elapsed) as f32);
            }
            next.insert(key, cpu);
            processes.push(process);
        }
        self.previous = Some((at, next));
        processes.sort_unstable_by_key(|process| process.pid);
        processes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample(pid: u32, cpu: &str, birth: &str, args: &str) -> (Process, f64) {
        let line = format!(
            "{:<40} chen {pid} 1 512 S+ 5 {cpu} Wed Nov 18 {birth} 2026 {args}",
            "/Applications/My App"
        );
        row(line.as_bytes(), 41, 1024 * 1024).unwrap()
    }

    #[test]
    fn ps_preserves_spaced_names_and_long_args() {
        let args = format!("/Applications/My App --arg  has spaces {}", "x".repeat(5000));
        let (process, _) = sample(11, "0:12.34", "11:12:13", &args);
        assert_eq!(process.name, "My App");
        assert_eq!(process.command, args[..2048]);
        assert_eq!((process.mem_percent, process.user.as_str()), (Some(50.), "chen"));
        let exe = format!("/Library/{}/Long Executable Name", "component/".repeat(60));
        let width = libc::PATH_MAX as usize;
        let line = format!("{exe:<width$} root 123 1 0 S 0 0:00.00 Wed Nov 18 10:01:00 2026 {exe} --test");
        let (parsed, _) = row(line.as_bytes(), width + 1, 0).unwrap();
        assert_eq!(parsed.name, "Long Executable Name");
        assert_eq!(parsed.command, format!("{exe} --test"));
    }

    #[test]
    fn ps_time_and_etime_formats() {
        for (text, value) in [
            ("123:45.67", 7425.67),
            ("01:02:03", 3723.),
            ("3-02:01:00", 266460.),
            ("00:01", 1.),
        ] {
            assert_eq!(seconds(text), Some(value));
        }
        assert_eq!(seconds("--"), None);
    }

    #[test]
    fn ps_delta_appeared_vanished_and_reused() {
        let now = Instant::now();
        let mut processes = Processes::default();
        processes.apply(
            now,
            [1, 2, 3].map(|pid| sample(pid, "0:01", "10:01:00", "")).into_iter(),
        );
        let result = processes.apply(
            now + Duration::from_millis(500),
            [
                sample(1, "0:02", "10:01:00", ""),
                sample(3, "0:02", "11:00:00", ""),
                sample(4, "0:01", "11:00:01", ""),
            ]
            .into_iter(),
        );
        assert_eq!(
            result.iter().map(|p| (p.pid, p.cpu_percent)).collect::<Vec<_>>(),
            [(1, Some(200.)), (3, None), (4, None)]
        );
    }
}
