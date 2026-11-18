//! The two scripts the monitor runs on the server: the probe (once, to learn what the server is and
//! what it allows) and the collector (a loop that writes frames for as long as somebody reads them).
//! Nothing is installed or written on the server: the scripts are handed to `sh -c` and live in its
//! memory.
//!
//! # The collector
//!
//! POSIX `sh` (dash, bash, BusyBox ash) with `LC_ALL=C`. One tick a second, at most **three forks** per
//! tick, on every shell. A baseline system/process pair precedes tick 0 by one short sleep
//! (0.5 s, or 1 s when the host rejects fractions); tick 0 starts the regular cadence:
//!
//! | tick                      | forks                                          |
//! |---------------------------|------------------------------------------------|
//! | every one                 | `sleep`                                        |
//! | system frame, every 2nd   | none: `/proc` and `/sys` are read with `read`  |
//! | process frame, every 3rd  | `cut` (all `/proc/<pid>/stat`), `ps` (see below) |
//! | slow frame, tick 1 of 30  | `df` (and no process frame: the schedule keeps them apart) |
//!
//! * **The collector hides itself.** A process frame says the script's own process id (`$$`); the monitor leaves
//!   that process and its children (`cut`, `ps`, `sleep`) out of the list, the counts and the top list.
//! * **Rates are the client's job.** The script writes raw counters and the server's uptime (read from
//!   `/proc/uptime`, so the clock is the server's and not the network's).
//! * **Process command lines:** `/proc/<pid>/cmdline` cannot be read in a shell without a fork per process
//!   (a NUL byte cannot be held by the shell, and every shell treats it differently), and `ps` is the one
//!   tool that prints one line per process on procps and BusyBox alike. It is called once per process
//!   frame as `ps -eo pid=,uid=,args=` (procps: numeric user ids, resolved with the probe's `passwd`) or,
//!   where that is not understood, `ps -o pid=,user=,args=` (BusyBox has no numeric column; its names are
//!   cut at 8 characters). Which one works is found out once, when the script starts. Without a usable `ps`
//!   the list falls back to the short names in the stat lines.
//! * **Process numbers** come from `cut -d" " -f1-40 /proc/[0-9]*/stat`: one fork for all processes, and the
//!   lines are cut after the fields the monitor reads (a stat line is 300-400 bytes; the monitor needs
//!   the first 24 fields, and a name with spaces in it shifts them: a name has at most 15 characters, so
//!   at most 15 spaces, so at most 16 words, which puts the memory field at 39 at the very most: 40 is
//!   enough for any name). Reading the files with `read` would cost a system call per byte in dash.
//! * **Disks:** `df -P -k` is only asked about local file systems from `/proc/mounts` (not network or
//!   shared-folder mounts: a dead NFS mount cannot hang the script, and a Mac's files shared into a VM are
//!   never touched), once per device however many times it is mounted.
//! * **Stopping:** every tick writes a line, so a closed channel is noticed within a second (SIGPIPE, or
//!   a failed write if SIGPIPE is ignored): the script exits and leaves nothing behind. The first line
//!   of the script (a comment) names it, for `pgrep -f shellmate-collector`.

use crate::quote;

/// Settings of the collector. The defaults are what the app uses; the tests use others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectorConfig {
    /// Marks the frames (see [`crate::frame`]); different for every run.
    pub token: String,
    /// A system frame every this many ticks.
    pub system_every: u32,
    /// A process frame every this many ticks.
    pub process_every: u32,
    /// A slow frame (disks) at tick 1 and then every this many ticks.
    pub slow_every: u32,
    /// What `sleep` is given (seconds, fractions where `sleep` takes them).
    pub tick: String,
    /// Where `/proc` is (tests point it at a copy).
    pub proc_root: String,
    /// Where `/sys` is.
    pub sys_root: String,
    /// How many space-separated fields of a stat line are kept (the monitor reads 24; see the module
    /// documentation for why more).
    pub stat_fields: u32,
}

impl CollectorConfig {
    /// The settings of the app: system data every 2 s, processes every 3 s, disks every 30 s.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            system_every: 2,
            process_every: 3,
            slow_every: 30,
            tick: "1".into(),
            proc_root: "/proc".into(),
            sys_root: "/sys".into(),
            stat_fields: 40,
        }
    }
}

/// The collector as a shell script.
pub fn collector_script(config: &CollectorConfig) -> String {
    // No single quote appears in the script, so that it can be put in single quotes as it is.
    format!(
        r#"# shellmate-collector {token}
LC_ALL=C; export LC_ALL
T="@@{token}"; P={proc}; S={sys}; TICK={tick}
PSA=; PL=; PN=0
for c in "-eo pid=,uid=,args=" "-o pid=,user=,args="; do
  if ps $c >/dev/null 2>&1; then PSA=$c; break; fi
done
up() {{ U=0; read -r U _ < $P/uptime; }}
stats() {{
  up
  printf "%s stats %s\n" "$T" "$U"
  printf "%s cpu\n" "$T"
  while read -r l; do case $l in cpu*) printf "%s\n" "$l";; *) break;; esac; done < $P/stat
  printf "%s load\n" "$T"
  read -r l < $P/loadavg && printf "%s\n" "$l"
  printf "%s mem\n" "$T"
  while read -r k v u; do
    case $k in MemTotal:|MemFree:|MemAvailable:|Buffers:|Cached:|SReclaimable:|Shmem:|SwapTotal:|SwapFree:) printf "%s %s\n" "$k" "$v";; esac
  done < $P/meminfo
  printf "%s net\n" "$T"
  dif=
  while read -r ri rd rg rf rr ru rm rk rest; do
    if [ "$rd" = 00000000 ] && [ "$rk" = 00000000 ]; then dif=$ri; break; fi
  done < $P/net/route
  printf "defaultif %s\n" "$dif"
  while read -r l; do
    case $l in *:*) set -- ${{l%%:*}}; if [ -z "$dif" ] || [ "$1" = "$dif" ]; then printf "%s\n" "$l"; fi;; esac
  done < $P/net/dev
  printf "%s thermal\n" "$T"
  for z in $S/class/thermal/thermal_zone*; do
    [ -r "$z/temp" ] || continue
    t=; k=
    read -r t 2>/dev/null < "$z/temp"
    read -r k 2>/dev/null < "$z/type"
    printf "thermal %s %s %s\n" "${{z##*/}}" "$t" "$k"
  done
  printf "%s end\n" "$T"
}}
procs() {{
  up
  printf "%s procs %s %s\n" "$T" "$U" "$$"
  printf "%s stat\n" "$T"
  cut -d" " -f1-{fields} $P/[0-9]*/stat 2>/dev/null
  if [ -n "$PSA" ]; then
    set -- $P/[0-9]*
    L="$*"
    if [ "$L" != "$PL" ] || [ $PN -ge 10 ]; then
      PL=$L; PN=0
      printf "%s ps\n" "$T"; ps $PSA 2>/dev/null
    else
      PN=$((PN + 1))
    fi
  fi
  printf "%s end\n" "$T"
}}
slow() {{
  up
  printf "%s slow %s\n" "$T" "$U"
  printf "%s df\n" "$T"
  set --
  seen=" "
  while read -r dev mnt typ rest; do
    case $typ in proc|sysfs|devtmpfs|devpts|tmpfs|cgroup|cgroup2|overlay|squashfs|nsfs|autofs|binfmt_misc|bpf|configfs|debugfs|tracefs|securityfs|pstore|mqueue|hugetlbfs|fusectl|selinuxfs|efivarfs|ramfs|rpc_pipefs|nfs|nfs4|cifs|smb3|smbfs|9p|virtiofs|vboxsf|vmhgfs|prl_fs|fuse.*|iso9660|udf|ecryptfs|aufs|rootfs) continue;; esac
    case $mnt in *\\*|/proc*|/sys*|/dev*|/run/*|/snap/*|/var/lib/docker/*|/var/lib/containers/*|/var/lib/kubelet/*) continue;; esac
    case $seen in *" $dev "*) continue;; esac
    seen="$seen$dev "
    set -- "$@" "$mnt"
  done < $P/mounts
  if [ $# -gt 0 ]; then df -P -k "$@" 2>/dev/null; fi
  printf "%s end\n" "$T"
}}
n=0
stats
procs
sleep 0.5 2>/dev/null || sleep 1
while :; do
  printf "%s tick %s\n" "$T" "$n" || exit 0
  [ $((n % {system})) -eq 0 ] && stats
  [ $((n % {process})) -eq 0 ] && procs
  [ $((n % {slow})) -eq 1 ] && slow
  n=$((n + 1))
  sleep $TICK
done
"#,
        token = config.token,
        proc = config.proc_root,
        sys = config.sys_root,
        tick = config.tick,
        fields = config.stat_fields.max(24),
        system = config.system_every.max(1),
        process = config.process_every.max(1),
        slow = config.slow_every.max(2),
    )
}

/// The command that runs `script` with `sh` on the server, whatever the login shell is.
pub fn sh_command(script: &str) -> String {
    format!("sh -c {}", quote::quote(script).expect("a script has no NUL bytes"))
}

/// `command` (a line for the server's shell) wrapped so that it ends when the channel it runs on is closed.
///
/// A command on an exec channel is not stopped by the server when the client closes the channel: it only finds out
/// when it writes to a pipe nobody reads any more (SIGPIPE). The collector writes every second, but
/// `docker events`, `journalctl -f` and `docker logs -f` write only when something happens, and would stay on the
/// server for hours. So the wrapper runs the command as a session of its own (`setsid`, which every Linux has: its
/// process group is then the job's alone) and watches the standard input, which the server closes together with the
/// channel: at the end of it the job's group gets TERM. (Job control, `set -m`, would do the same in bash but not
/// in dash without a terminal.) The command itself gets no standard input. Its exit status is the wrapper's, and
/// what it prints is not touched. Only the job is signalled, never the group of the wrapper: whatever started it
/// is left alone. Without `setsid` the command is run as it is and only the one process is signalled.
pub fn watched(command: &str) -> String {
    const SCRIPT: &str = "exec 3<&0; if command -v setsid >/dev/null 2>&1; then S=setsid; else S=; fi; $S sh -c \"$1\" </dev/null & p=$!; { cat <&3 >/dev/null; kill -TERM -$p 2>/dev/null || kill -TERM $p 2>/dev/null; } & w=$!; wait $p; s=$?; kill $w 2>/dev/null; exit $s";
    format!(
        "sh -c {} sh {}",
        quote::quote(SCRIPT).expect("no NUL"),
        quote::quote(command).expect("a command has no NUL bytes")
    )
}

/// The command line of the collector.
pub fn collector_command(config: &CollectorConfig) -> String {
    sh_command(&collector_script(config))
}

/// The probe: one script that prints labelled sections (see [`crate::probe::parse_probe`]), so that
/// everything is learned in a single round trip.
pub fn probe_script(token: &str) -> String {
    format!(
        r#"# shellmate-probe {token}
LC_ALL=C; export LC_ALL
T="@@{token}"
m() {{ printf "%s %s\n" "$T" "$1"; }}
tmo() {{ if command -v timeout >/dev/null 2>&1; then timeout "$@"; else shift; "$@"; fi; }}
m os
for f in /etc/os-release /usr/lib/os-release; do
  if [ -r $f ]; then
    while read -r l; do case $l in PRETTY_NAME=*) printf "%s\n" "$l";; esac; done < $f
    break
  fi
done
m kernel
uname -sr 2>/dev/null
m procfs
[ -r /proc/stat ] && [ -r /proc/meminfo ] && echo yes
m uptime
if [ -r /proc/uptime ]; then read -r u _ < /proc/uptime; printf "%s\n" "$u"; fi
m user
id -u 2>/dev/null
id -un 2>/dev/null
m groups
id -Gn 2>/dev/null
m sudo
if command -v sudo >/dev/null 2>&1; then
  if tmo 5 sudo -n true >/dev/null 2>&1; then SUDO=1; echo nopasswd; else SUDO=0; echo password; fi
else SUDO=0; echo none; fi
m systemd
v=$(systemctl --version 2>/dev/null | {{ read -r a b rest; printf "%s %s" "$a" "$b"; }})
printf "%s\n" "$v"
if [ -d /run/systemd/system ]; then echo running; fi
m containers
for b in docker podman; do
  p=$(command -v $b 2>/dev/null) || continue
  printf "bin %s %s\n" "$b" "$p"
  if [ $b = docker ]; then f="{{{{.Server.Version}}}}"; else f="{{{{.Version}}}}"; fi
  out=$(tmo 8 $b version --format "$f" 2>&1); rc=$?
  if [ $rc -eq 0 ]; then
    printf "access %s user ok %s\n" "$b" "$out"
  else
    case $out in
      *ermission*denied*) printf "access %s user denied\n" "$b";;
      *annot*connect*|*aemon*running*|*onnection*refused*|*"no such file"*|*"No such file"*) printf "access %s user nodaemon\n" "$b";;
      *) printf "access %s user error\n" "$b";;
    esac
    if [ "$SUDO" = 1 ]; then
      if out=$(tmo 8 sudo -n $b version --format "$f" 2>&1); then printf "access %s sudo ok %s\n" "$b" "$out"; fi
    fi
  fi
done
m page
v=$(getconf PAGESIZE 2>/dev/null) || v=
if [ -z "$v" ] && [ -r /proc/self/smaps ]; then
  while read -r k n u; do case $k in KernelPageSize:) v=$((n * 1024)); break;; esac; done < /proc/self/smaps
fi
printf "%s\n" "$v"
m clk
getconf CLK_TCK 2>/dev/null
m passwd
pw=
if command -v getent >/dev/null 2>&1; then pw=$(getent passwd 2>/dev/null | head -n 2000); fi
if [ -z "$pw" ]; then pw=$(head -n 2000 /etc/passwd 2>/dev/null); fi
printf "%s\n" "$pw"
m end
"#
    )
}

/// The command line of the probe.
pub fn probe_command(token: &str) -> String {
    sh_command(&probe_script(token))
}

/// A token for the markers of one run: 12 hex digits nobody can guess from the server's side.
pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scripts_have_no_single_quote_and_name_themselves() {
        let collector = collector_script(&CollectorConfig::new("abc123"));
        assert!(!collector.contains('\''), "the script is put in single quotes as it is");
        assert!(collector.starts_with("# shellmate-collector abc123\n"));
        let probe = probe_script("abc123");
        assert!(!probe.contains('\''));
        assert!(probe.starts_with("# shellmate-probe abc123\n"));
        assert!(probe.contains("{{.Server.Version}}"), "the go template reaches the server as written");
    }

    #[test]
    fn the_command_is_one_sh_invocation() {
        let command = collector_command(&CollectorConfig::new("t"));
        assert!(command.starts_with("sh -c '# shellmate-collector t\n"));
        assert!(command.ends_with("done\n'"));
        assert_eq!(command.matches('\'').count(), 2, "one pair of quotes around everything");
    }

    #[test]
    fn the_schedule_is_written_into_the_script() {
        let mut config = CollectorConfig::new("t");
        config.tick = "0.05".into();
        config.system_every = 4;
        config.process_every = 5;
        config.slow_every = 7;
        config.proc_root = "/tmp/fake proc".replace(' ', "_");
        let script = collector_script(&config);
        assert!(script.contains("TICK=0.05"));
        assert!(script.contains("$((n % 4))") && script.contains("$((n % 5))") && script.contains("$((n % 7))"));
        assert!(script.contains("P=/tmp/fake_proc"));
        // Zero would divide by zero in the shell.
        config.system_every = 0;
        config.process_every = 0;
        assert!(collector_script(&config).contains("$((n % 1))"));
    }

    #[test]
    fn the_schedule_never_puts_three_forking_jobs_on_one_tick() {
        // The forks of a tick: sleep, plus cut and ps on a process tick, plus df on a slow tick.
        let config = CollectorConfig::new("t");
        let mut busiest = 0;
        for n in 0..3000u32 {
            let forks =
                1 + if n % config.process_every == 0 { 2 } else { 0 } + if n % config.slow_every == 1 { 1 } else { 0 };
            busiest = busiest.max(forks);
            if n % config.slow_every == 1 {
                assert_ne!(n % config.process_every, 0, "tick {n}: disks and processes together");
            }
        }
        assert_eq!(busiest, 3);
    }

    /// Runs `watched(command)` in a real shell with a pipe as its standard input.
    fn spawn_watched(command: &str) -> std::process::Child {
        std::os::unix::process::CommandExt::process_group(&mut std::process::Command::new("sh"), 0)
            .arg("-c")
            .arg(watched(command))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    fn wait_for(child: &mut std::process::Child, secs: u64) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn a_watched_command_ends_when_its_standard_input_does() {
        // `sleep` never writes: only the closed input can end it.
        let mut child = spawn_watched("sleep 30");
        assert!(wait_for(&mut child, 1).is_none(), "it runs while the channel is open");
        drop(child.stdin.take());
        let status = wait_for(&mut child, 5);
        let _ = child.kill();
        assert!(status.is_some(), "the wrapper and its command are gone soon after the input closed");
        assert!(!status.unwrap().success());
    }

    #[test]
    fn a_watched_command_keeps_its_output_and_its_exit_status() {
        let mut child = spawn_watched("echo \"it's $((6 * 7))\"; echo oops 1>&2; exit 3");
        let mut out = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
        let status = wait_for(&mut child, 5).expect("it ended by itself");
        assert_eq!(out, "it's 42\n");
        assert_eq!(status.code(), Some(3));
        drop(child.stdin.take());
        // The merged form the pages use: `2>&1` after the line goes through the wrapper's eval.
        let mut child = spawn_watched("sh -c 'echo to-stderr 1>&2' 2>&1");
        let mut out = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
        assert_eq!(out, "to-stderr\n");
        let _ = wait_for(&mut child, 5);
    }

    #[test]
    fn a_watched_command_does_not_read_the_channel() {
        // The command must not eat the input the watcher is waiting on: `cat` as the command sees an empty input and
        // ends at once (it would otherwise block until the channel closed).
        let mut child = spawn_watched("cat; echo done");
        let mut out = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
        assert_eq!(out, "done\n");
        drop(child.stdin.take());
        let _ = wait_for(&mut child, 5);
    }

    #[test]
    fn tokens_are_short_and_different() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 12);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
