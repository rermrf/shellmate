//! The process page without a window: the rows (filtered, sorted, as a list or a tree), the selection that
//! survives refreshes, and the buttons for the selected process with the commands behind them.

use std::collections::{HashMap, HashSet};

use shellmate_monitor::commands::{self, BuildError, Command, Plan, Privilege, Signal};
use shellmate_monitor::snapshot::Process;
use shellmate_monitor::{Probe, Snapshot, Sudo};

use super::format;
use super::model::Tone;

/// What the table is sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    /// CPU, the busiest first.
    Cpu,
    /// Resident memory, the largest first.
    Mem,
    /// Process id, the lowest first.
    Pid,
}

impl SortKey {
    /// The choices, in the order of the control.
    pub const ALL: [SortKey; 3] = [SortKey::Cpu, SortKey::Mem, SortKey::Pid];

    /// The label of the segment.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Cpu => "CPU",
            SortKey::Mem => "内存",
            SortKey::Pid => "PID",
        }
    }
}

/// Which process: its number, and when it started (another process may get the number later).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcKey {
    /// Process id.
    pub pid: u32,
    /// Start time in clock ticks after boot.
    pub start: u64,
}

impl ProcKey {
    /// The key of `process`.
    pub fn of(process: &Process) -> Self {
        Self { pid: process.pid, start: process.start_time }
    }
}

/// A row of the table.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Which process.
    pub key: ProcKey,
    /// Parent process id.
    pub ppid: u32,
    /// Owner.
    pub user: String,
    /// The command line.
    pub command: String,
    /// `13.0`, or `—`.
    pub cpu: String,
    /// The CPU bar, 0 to 1 (relative to the busiest process in the table, with a floor of 10 %).
    pub cpu_bar: f32,
    /// `1.1`, or `—`.
    pub mem: String,
    /// `91 MB`, or `—`.
    pub rss: String,
    /// `S`, `R`, `Z`, ...
    pub state: char,
    /// Green for running, red for a zombie.
    pub state_tone: Tone,
    /// How deep in the tree (0 in the list).
    pub depth: usize,
    /// Drawn before the command in the tree (`└ `).
    pub prefix: &'static str,
    /// The row is only here because a process below it matches the filter (tree view).
    pub context: bool,
}

/// The rows and how many of the processes they are.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    /// What to draw.
    pub rows: Vec<Row>,
    /// How many processes match the filter.
    pub matches: usize,
    /// How many processes there are.
    pub total: usize,
}

impl Table {
    /// `15 / 15 个进程 · 每 3 秒刷新`
    pub fn count_text(&self) -> String {
        format!("{} / {} 个进程 · 每 3 秒刷新", self.matches, self.total)
    }

    /// The index of the row of `key`.
    pub fn position(&self, key: ProcKey) -> Option<usize> {
        self.rows.iter().position(|row| row.key == key)
    }
}

fn matches(process: &Process, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    process.pid.to_string().contains(query)
        || process.user.to_lowercase().contains(query)
        || process.command.to_lowercase().contains(query)
}

fn ordering(sort: SortKey) -> impl Fn(&&Process, &&Process) -> std::cmp::Ordering {
    move |a, b| {
        let by = match sort {
            SortKey::Cpu => b.cpu_percent.unwrap_or(-1.0).total_cmp(&a.cpu_percent.unwrap_or(-1.0)),
            SortKey::Mem => b.rss_bytes.unwrap_or(0).cmp(&a.rss_bytes.unwrap_or(0)),
            SortKey::Pid => a.pid.cmp(&b.pid),
        };
        by.then(a.pid.cmp(&b.pid))
    }
}

/// The table for `snapshot`: the processes that match `query` (command, user or process id, any case), sorted
/// by `sort`; with `tree` as a tree (children below their parent, sorted among themselves), where the
/// parents of the matching processes are kept as context.
pub fn table(snapshot: &Snapshot, query: &str, sort: SortKey, tree: bool) -> Table {
    let query = query.trim().to_lowercase();
    let total = snapshot.processes.len();
    let matching: HashSet<u32> = snapshot.processes.iter().filter(|p| matches(p, &query)).map(|p| p.pid).collect();
    let order = ordering(sort);

    let ordered: Vec<(&Process, usize, bool)> = if tree {
        tree_order(snapshot, &matching, &order)
    } else {
        let mut list: Vec<&Process> = snapshot.processes.iter().filter(|p| matching.contains(&p.pid)).collect();
        list.sort_by(order);
        list.into_iter().map(|p| (p, 0, false)).collect()
    };

    let top = ordered.iter().filter_map(|(p, _, _)| p.cpu_percent).fold(10.0f32, f32::max);
    let rows = ordered
        .into_iter()
        .map(|(p, depth, context)| Row {
            key: ProcKey::of(p),
            ppid: p.ppid,
            user: p.user.clone(),
            command: p.command.clone(),
            cpu: format::percent_cell(p.cpu_percent),
            cpu_bar: p.cpu_percent.map_or(0.0, |v| (v / top).clamp(0.0, 1.0)),
            mem: format::percent_cell(p.mem_percent),
            rss: p.rss_bytes.map_or_else(|| "—".into(), format::rss),
            state: p.state,
            state_tone: match p.state {
                'R' => Tone::Ok,
                'Z' => Tone::Err,
                'D' => Tone::Warn,
                _ => Tone::Plain,
            },
            depth,
            prefix: if depth > 0 { "└ " } else { "" },
            context,
        })
        .collect();
    Table { rows, matches: matching.len(), total }
}

/// The processes in tree order with their depth, those that match or have a descendant that matches. A
/// process whose parent is not in the list (or is itself) is a root; processes that form a cycle without a
/// root are started from their lowest number, so that nothing is lost and nothing loops.
fn tree_order<'a>(
    snapshot: &'a Snapshot,
    matching: &HashSet<u32>,
    order: &impl Fn(&&'a Process, &&'a Process) -> std::cmp::Ordering,
) -> Vec<(&'a Process, usize, bool)> {
    let by_pid: HashMap<u32, &Process> = snapshot.processes.iter().map(|p| (p.pid, p)).collect();

    // What is shown: the matches and every parent above them (the walk up stops at a process seen already,
    // which also ends a cycle).
    let mut shown: HashSet<u32> = HashSet::new();
    for pid in matching {
        let mut current = *pid;
        while let Some(process) = by_pid.get(&current) {
            if !shown.insert(current) {
                break;
            }
            current = process.ppid;
        }
    }

    let mut children: HashMap<u32, Vec<&Process>> = HashMap::new();
    let mut roots: Vec<&Process> = Vec::new();
    for process in snapshot.processes.iter().filter(|p| shown.contains(&p.pid)) {
        if process.ppid != process.pid && by_pid.contains_key(&process.ppid) {
            children.entry(process.ppid).or_default().push(process);
        } else {
            roots.push(process);
        }
    }
    roots.sort_by(order);
    for list in children.values_mut() {
        list.sort_by(order);
    }

    let mut out = Vec::new();
    let mut visited: HashSet<u32> = HashSet::new();
    let walk = |start: &'a Process, visited: &mut HashSet<u32>, out: &mut Vec<(&'a Process, usize, bool)>| {
        let mut stack = vec![(start, 0usize)];
        while let Some((process, depth)) = stack.pop() {
            if !visited.insert(process.pid) {
                continue;
            }
            out.push((process, depth, !matching.contains(&process.pid)));
            if let Some(kids) = children.get(&process.pid) {
                // Pushed backwards so that the first child comes out first.
                for kid in kids.iter().rev() {
                    stack.push((kid, depth + 1));
                }
            }
        }
    };
    for root in &roots {
        walk(root, &mut visited, &mut out);
    }
    // What is left forms cycles (a parent that is its own descendant): start from the lowest number.
    let mut rest: Vec<&Process> =
        snapshot.processes.iter().filter(|p| shown.contains(&p.pid) && !visited.contains(&p.pid)).collect();
    rest.sort_by_key(|p| p.pid);
    for process in rest {
        walk(process, &mut visited, &mut out);
    }
    out
}

/// The process of `key` in `snapshot`, if it is still there: the selection after a refresh.
pub fn resolve(snapshot: &Snapshot, key: ProcKey) -> Option<&Process> {
    snapshot.process(key.pid).filter(|process| process.start_time == key.start)
}

// ---- the buttons --------------------------------------------------------------------------------

/// What can be done to a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessAction {
    /// 结束 TERM
    Term,
    /// 重载 HUP
    Hup,
    /// 降低优先级
    Nice,
    /// strace 跟踪
    Strace,
    /// 强制结束 KILL
    Kill,
}

impl ProcessAction {
    /// The buttons, in the order of the prototype.
    pub const ALL: [ProcessAction; 5] =
        [ProcessAction::Term, ProcessAction::Hup, ProcessAction::Nice, ProcessAction::Strace, ProcessAction::Kill];

    /// The label of the button.
    pub fn label(self) -> &'static str {
        match self {
            ProcessAction::Term => "结束 TERM",
            ProcessAction::Hup => "重载 HUP",
            ProcessAction::Nice => "降低优先级",
            ProcessAction::Strace => "strace 跟踪",
            ProcessAction::Kill => "强制结束 KILL",
        }
    }
}

/// How a button looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonTone {
    /// A grey button.
    Plain,
    /// The blue one (strace).
    Accent,
    /// The red one (KILL).
    Danger,
}

/// How the command gets to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// As the login user (or root).
    Direct,
    /// With `sudo -n`: the server lets the user without a password.
    Sudo,
    /// In a terminal tab, where the user types the password.
    Terminal,
}

/// A button of the bar under the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionButton {
    /// What it does.
    pub action: ProcessAction,
    /// Its text (`结束 TERM`, `结束 TERM · 终端`).
    pub label: String,
    /// How it looks.
    pub tone: ButtonTone,
    /// Whether it can be pressed.
    pub enabled: bool,
    /// How it will run.
    pub via: Via,
    /// What a hover says: how it runs, or why it cannot.
    pub tooltip: Option<String>,
}

const ZOMBIE_NOTE: &str = "僵尸进程已经结束，只是还没被父进程回收：对它发信号没有用，需要处理它的父进程";
const TERMINAL_NOTE: &str = "需要 sudo 密码，应用不会收集：在新会话里打好命令，由你回车并输入密码";

/// Why `action` cannot be done to `process`, if it cannot.
pub fn unavailable(process: &Process) -> Option<&'static str> {
    if process.state == 'Z' { Some(ZOMBIE_NOTE) } else { None }
}

/// Whether the process belongs to somebody else than the login user (the server may want root for it).
fn foreign(process: &Process, probe: &Probe) -> bool {
    !process.user.is_empty() && process.user != probe.user
}

fn command_of(action: ProcessAction, process: &Process) -> Result<Command, BuildError> {
    match action {
        ProcessAction::Term => commands::kill(process.pid, Signal::Term),
        ProcessAction::Hup => commands::kill(process.pid, Signal::Hup),
        ProcessAction::Kill => commands::kill(process.pid, Signal::Kill),
        ProcessAction::Nice => commands::lower_priority(process.pid, process.nice),
        ProcessAction::Strace => commands::strace(process.pid),
    }
}

fn privilege_of(action: ProcessAction, process: &Process, probe: &Probe) -> Privilege {
    // Attaching to a process needs more than owning it (ptrace_scope on most distributions).
    if action == ProcessAction::Strace || foreign(process, probe) { Privilege::Root } else { Privilege::User }
}

/// How the command of `action` gets to run, and the line that will be run.
fn route(action: ProcessAction, process: &Process, probe: &Probe) -> Result<(Via, String, bool), BuildError> {
    let command = command_of(action, process)?;
    let privilege = privilege_of(action, process, probe);
    if action == ProcessAction::Strace {
        let line = commands::terminal_line(&command, privilege, probe);
        // `sudo` asks for a password only when the server wants one: then the user presses Enter.
        let enter = !(line.starts_with("sudo ") && probe.sudo == Sudo::NeedsPassword);
        return Ok((Via::Terminal, line, enter));
    }
    Ok(match commands::plan(&command, privilege, probe) {
        Plan::Exec { line } if line.starts_with("sudo -n ") => (Via::Sudo, line, true),
        Plan::Exec { line } => (Via::Direct, line, true),
        Plan::Terminal { line } => (Via::Terminal, line, false),
    })
}

/// The buttons for `process`, as far as the probe tells how they would run.
pub fn buttons(process: &Process, probe: Option<&Probe>) -> Vec<ActionButton> {
    ProcessAction::ALL
        .iter()
        .map(|action| {
            let tone = match action {
                ProcessAction::Strace => ButtonTone::Accent,
                ProcessAction::Kill => ButtonTone::Danger,
                _ => ButtonTone::Plain,
            };
            let off = |why: &str| ActionButton {
                action: *action,
                label: action.label().into(),
                tone,
                enabled: false,
                via: Via::Direct,
                tooltip: Some(why.into()),
            };
            let Some(probe) = probe else { return off("还没有读到这台主机的信息") };
            if let Some(why) = unavailable(process) {
                return off(why);
            }
            match route(*action, process, probe) {
                Err(_) => off("这个进程号不能用于操作"),
                Ok((via, line, _)) => {
                    let tooltip = match (action, via) {
                        (ProcessAction::Strace, _) => Some(format!("在新会话里运行：{line}")),
                        (_, Via::Sudo) => Some(format!("以 sudo -n 执行：{line}")),
                        (_, Via::Terminal) => Some(format!("{TERMINAL_NOTE}：{line}")),
                        (_, Via::Direct) => None,
                    };
                    let label = if via == Via::Terminal && *action != ProcessAction::Strace {
                        format!("{} · 终端", action.label())
                    } else {
                        action.label().to_string()
                    };
                    ActionButton { action: *action, label, tone, enabled: true, via, tooltip }
                }
            }
        })
        .collect()
}

/// The question before a dangerous action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    /// `强制结束进程 2207？`
    pub title: String,
    /// The command line of the process and the warning.
    pub text: String,
    /// The label of the button that goes ahead.
    pub label: &'static str,
}

/// What to do for a button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Run {
    /// Run `line` on an exec channel and say how it went.
    Exec {
        /// The command line.
        line: String,
        /// What the toast says after the command line when it worked (`已发送`).
        done: &'static str,
    },
    /// Open a terminal tab on the same connection and type `line` there; press Enter for the user if `enter`.
    Terminal {
        /// The command line.
        line: String,
        /// Whether it is run, or left for the user to run.
        enter: bool,
        /// The title of the tab.
        title: String,
    },
}

/// A button, worked out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    /// What to do.
    pub run: Run,
    /// What to ask first.
    pub confirm: Option<Confirm>,
}

/// What pressing `action` does to `process` on a server that was probed as `probe`.
pub fn plan(action: ProcessAction, process: &Process, probe: &Probe) -> Option<Planned> {
    if unavailable(process).is_some() {
        return None;
    }
    let (via, line, enter) = route(action, process, probe).ok()?;
    let run = match (action, via) {
        (ProcessAction::Strace, _) | (_, Via::Terminal) => Run::Terminal {
            line,
            enter,
            title: format!(
                "{} {}",
                match action {
                    ProcessAction::Strace => "strace",
                    ProcessAction::Term => "TERM",
                    ProcessAction::Hup => "HUP",
                    ProcessAction::Nice => "renice",
                    ProcessAction::Kill => "KILL",
                },
                process.pid
            ),
        },
        _ => Run::Exec { line, done: if action == ProcessAction::Nice { "优先级已降低" } else { "已发送" } },
    };
    let confirm = (action == ProcessAction::Kill).then(|| Confirm {
        title: format!("强制结束进程 {}？", process.pid),
        text: format!("{}\nSIGKILL 无法被进程捕获，未落盘的数据会丢失。", process.command),
        label: "发送 SIGKILL",
    });
    Some(Planned { run, confirm })
}

/// `PID 2207  deploy · node /var/www/app/dist/server.js`: the left of the bar under the table.
pub fn selection_text(process: &Process) -> (String, String) {
    let who = if process.user.is_empty() { String::new() } else { format!("{} · ", process.user) };
    (format!("PID {}", process.pid), format!("  {who}{}", process.command))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, command: &str) -> Process {
        Process {
            pid,
            ppid,
            user: "deploy".into(),
            name: command.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("").into(),
            command: command.into(),
            state: 'S',
            cpu_percent: Some(0.0),
            mem_percent: Some(0.1),
            rss_bytes: Some(5 << 20),
            threads: 1,
            nice: 0,
            start_time: u64::from(pid) * 10,
        }
    }

    fn with(mut process: Process, cpu: Option<f32>, rss_mb: u64, user: &str) -> Process {
        process.cpu_percent = cpu;
        process.rss_bytes = Some(rss_mb << 20);
        process.user = user.into();
        process
    }

    fn snapshot(processes: Vec<Process>) -> Snapshot {
        Snapshot { processes, ..Snapshot::default() }
    }

    fn pids(table: &Table) -> Vec<u32> {
        table.rows.iter().map(|r| r.key.pid).collect()
    }

    fn sample() -> Snapshot {
        snapshot(vec![
            with(p(1, 0, "/sbin/init"), Some(0.7), 12, "root"),
            with(p(1184, 1, "nginx: master process /usr/sbin/nginx"), Some(0.2), 14, "root"),
            with(p(1185, 1184, "nginx: worker process"), Some(10.5), 96, "www-data"),
            with(p(1186, 1184, "nginx: worker process"), Some(13.0), 91, "www-data"),
            with(p(2207, 1, "node /var/www/app/dist/server.js"), Some(11.5), 532, "deploy"),
            with(p(4416, 4415, "-bash"), None, 5, "deploy"),
            with(p(4415, 4410, "sshd: deploy@pts/0"), Some(0.0), 7, "deploy"),
            with(p(4410, 1, "sshd: deploy [priv]"), Some(0.1), 11, "root"),
        ])
    }

    #[test]
    fn the_list_is_sorted_by_cpu_memory_or_pid() {
        let snapshot = sample();
        assert_eq!(
            pids(&table(&snapshot, "", SortKey::Cpu, false)),
            [1186, 2207, 1185, 1, 1184, 4410, 4415, 4416],
            "the busiest first; a process without a rate is last"
        );
        assert_eq!(pids(&table(&snapshot, "", SortKey::Mem, false)), [2207, 1185, 1186, 1184, 1, 4410, 4415, 4416]);
        assert_eq!(pids(&table(&snapshot, "", SortKey::Pid, false)), [1, 1184, 1185, 1186, 2207, 4410, 4415, 4416]);
        let t = table(&snapshot, "", SortKey::Cpu, false);
        assert_eq!((t.matches, t.total), (8, 8));
        assert_eq!(t.count_text(), "8 / 8 个进程 · 每 3 秒刷新");
    }

    #[test]
    fn the_rows_say_what_the_columns_show() {
        let mut snapshot = sample();
        snapshot.processes[3].state = 'R';
        snapshot.processes[7].state = 'Z';
        let t = table(&snapshot, "", SortKey::Pid, false);
        let row = |pid: u32| t.rows.iter().find(|r| r.key.pid == pid).unwrap();
        let busy = row(1186);
        assert_eq!(
            (busy.cpu.as_str(), busy.mem.as_str(), busy.rss.as_str(), busy.user.as_str()),
            ("13.0", "0.1", "91 MB", "www-data")
        );
        assert_eq!((busy.state, busy.state_tone), ('R', Tone::Ok));
        assert_eq!(row(4410).state_tone, Tone::Err);
        assert_eq!(row(2207).state_tone, Tone::Plain);
        assert_eq!(row(4416).cpu, "—", "a process without a rate shows a dash");
        assert_eq!(row(4416).cpu_bar, 0.0);
        assert!((busy.cpu_bar - 1.0).abs() < 1e-6, "the busiest has the full bar: {}", busy.cpu_bar);
        assert!((row(2207).cpu_bar - 11.5 / 13.0).abs() < 1e-6);
        assert_eq!(busy.key, ProcKey { pid: 1186, start: 11860 });
    }

    #[test]
    fn the_bars_of_an_idle_server_are_not_blown_up() {
        let snapshot =
            snapshot(vec![with(p(1, 0, "init"), Some(0.5), 1, "root"), with(p(2, 1, "x"), Some(0.1), 1, "root")]);
        let t = table(&snapshot, "", SortKey::Cpu, false);
        assert!((t.rows[0].cpu_bar - 0.05).abs() < 1e-6, "0.5 % of at least 10 %: {}", t.rows[0].cpu_bar);
    }

    #[test]
    fn the_filter_looks_at_command_user_and_pid_in_any_case() {
        let snapshot = sample();
        assert_eq!(
            pids(&table(&snapshot, "NGINX", SortKey::Pid, false)),
            [1184, 1185, 1186],
            "the command, in any case"
        );
        assert_eq!(pids(&table(&snapshot, "www-data", SortKey::Pid, false)), [1185, 1186], "the user");
        assert_eq!(pids(&table(&snapshot, "2207", SortKey::Pid, false)), [2207], "the pid");
        assert_eq!(
            pids(&table(&snapshot, "  worker ", SortKey::Pid, false)),
            [1185, 1186],
            "spaces around it do not count"
        );
        assert!(table(&snapshot, "no such thing", SortKey::Pid, false).rows.is_empty());
        let t = table(&snapshot, "root", SortKey::Pid, false);
        assert_eq!((t.matches, t.total, t.count_text().as_str()), (3, 8, "3 / 8 个进程 · 每 3 秒刷新"));
        // A number matches pids and also commands that have it in them.
        assert_eq!(pids(&table(&snapshot, "44", SortKey::Pid, false)), [4410, 4415, 4416]);
    }

    #[test]
    fn the_tree_puts_children_under_their_parent() {
        let snapshot = sample();
        let t = table(&snapshot, "", SortKey::Cpu, true);
        // Roots by CPU: init (0.7 %) has the busy node and nginx below it; the sshd chain hangs off init as well.
        assert_eq!(pids(&t), [1, 2207, 1184, 1186, 1185, 4410, 4415, 4416]);
        assert_eq!(t.rows.iter().map(|r| r.depth).collect::<Vec<_>>(), [0, 1, 1, 2, 2, 1, 2, 3]);
        assert_eq!(t.rows[0].prefix, "");
        assert_eq!(t.rows[3].prefix, "└ ");
        let by_pid = table(&snapshot, "", SortKey::Pid, true);
        assert_eq!(pids(&by_pid), [1, 1184, 1185, 1186, 2207, 4410, 4415, 4416]);
        assert!(t.rows.iter().all(|r| !r.context));
        assert_eq!((t.matches, t.total), (8, 8));
    }

    #[test]
    fn orphans_cycles_and_self_parents_are_still_listed_once() {
        // 10's parent is not in the list; 20 is its own parent; 30 and 31 are each other's parents; 40 is below 31.
        let snapshot = snapshot(vec![
            p(10, 999, "orphan"),
            p(20, 20, "self"),
            p(30, 31, "a"),
            p(31, 30, "b"),
            p(40, 31, "c"),
            p(50, 10, "child of the orphan"),
        ]);
        let t = table(&snapshot, "", SortKey::Pid, true);
        assert_eq!(
            pids(&t),
            [10, 50, 20, 30, 31, 40],
            "every process appears exactly once: roots first, then the cycle from its lowest number"
        );
        assert_eq!(t.rows.iter().map(|r| r.depth).collect::<Vec<_>>(), [0, 1, 0, 0, 1, 2]);
        let mut seen = pids(&t);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 6);
        assert!(table(&snapshot, "", SortKey::Cpu, true).rows.len() == 6);
    }

    #[test]
    fn the_filter_in_a_tree_keeps_the_parents_as_context() {
        let snapshot = sample();
        let t = table(&snapshot, "bash", SortKey::Pid, true);
        assert_eq!(pids(&t), [1, 4410, 4415, 4416], "bash and everything above it");
        assert_eq!(t.rows.iter().map(|r| r.context).collect::<Vec<_>>(), [true, true, true, false]);
        assert_eq!((t.matches, t.total), (1, 8), "the count is of the matches");
        assert_eq!(t.rows.iter().map(|r| r.depth).collect::<Vec<_>>(), [0, 1, 2, 3]);
        // Two matches under one parent.
        let t = table(&snapshot, "worker", SortKey::Pid, true);
        assert_eq!(pids(&t), [1, 1184, 1185, 1186]);
        assert_eq!(t.matches, 2);
        // A cycle with a match terminates.
        let cyclic = snapshot_of_cycle();
        assert_eq!(pids(&table(&cyclic, "a", SortKey::Pid, true)), [30, 31]);
    }

    fn snapshot_of_cycle() -> Snapshot {
        snapshot(vec![p(30, 31, "a"), p(31, 30, "b")])
    }

    #[test]
    fn the_selection_survives_refreshes_by_pid_and_start_time() {
        let before = sample();
        let key = ProcKey::of(before.process(2207).unwrap());
        // The next sample: the same process has other numbers; the row moves, the selection follows.
        let mut after = sample();
        after.processes[4].cpu_percent = Some(99.0);
        assert_eq!(resolve(&after, key).map(|p| p.pid), Some(2207));
        let t = table(&after, "", SortKey::Cpu, false);
        assert_eq!(t.position(key), Some(0), "now the busiest: the row is first");
        // The process is gone: nothing is selected. The number went to a new process (another start time): also nothing.
        after.processes.retain(|p| p.pid != 2207);
        assert!(resolve(&after, key).is_none());
        after.processes.push(p(2207, 1, "another program"));
        after.processes.sort_by_key(|p| p.pid);
        after.processes.iter_mut().find(|p| p.pid == 2207).unwrap().start_time = 999_999;
        assert!(resolve(&after, key).is_none(), "a new process with the old number is not the selected one");
        assert!(t.position(ProcKey { pid: 1, start: 1 }).is_none());
    }

    fn probe(sudo: Sudo) -> Probe {
        Probe { user: "deploy".into(), uid: Some(1000), sudo, is_linux: true, ..Probe::unknown() }
    }

    fn button(buttons: &[ActionButton], action: ProcessAction) -> &ActionButton {
        buttons.iter().find(|b| b.action == action).unwrap()
    }

    #[test]
    fn buttons_for_your_own_process_run_as_they_are() {
        let process = with(p(2207, 1, "node server.js"), Some(1.0), 5, "deploy");
        for sudo in [Sudo::Root, Sudo::Passwordless, Sudo::NeedsPassword, Sudo::Unavailable] {
            let bs = buttons(&process, Some(&probe(sudo)));
            assert_eq!(bs.len(), 5);
            for action in [ProcessAction::Term, ProcessAction::Hup, ProcessAction::Nice, ProcessAction::Kill] {
                let b = button(&bs, action);
                assert!(b.enabled && b.via == Via::Direct && b.tooltip.is_none(), "{sudo:?} {action:?}: {b:?}");
                assert_eq!(b.label, action.label());
            }
        }
        // The look of the buttons.
        let bs = buttons(&process, Some(&probe(Sudo::Root)));
        assert_eq!(button(&bs, ProcessAction::Strace).tone, ButtonTone::Accent);
        assert_eq!(button(&bs, ProcessAction::Kill).tone, ButtonTone::Danger);
        assert_eq!(button(&bs, ProcessAction::Term).tone, ButtonTone::Plain);
        assert_eq!(
            bs.iter().map(|b| b.label.as_str()).collect::<Vec<_>>(),
            ["结束 TERM", "重载 HUP", "降低优先级", "strace 跟踪", "强制结束 KILL"]
        );
    }

    #[test]
    fn buttons_for_somebody_elses_process_follow_what_the_server_allows() {
        let root_process = with(p(1, 0, "/sbin/init"), Some(1.0), 5, "root");
        // root: as they are. Passwordless sudo: with sudo -n. A password: in a terminal. No sudo: in a terminal too (it will fail there, in front of the user).
        let direct = buttons(&root_process, Some(&probe(Sudo::Root)));
        assert!(
            direct.iter().all(|b| b.via == if b.action == ProcessAction::Strace { Via::Terminal } else { Via::Direct })
        );
        let sudo = buttons(&root_process, Some(&probe(Sudo::Passwordless)));
        let term = button(&sudo, ProcessAction::Term);
        assert_eq!(
            (term.via, term.label.as_str(), term.tooltip.as_deref()),
            (Via::Sudo, "结束 TERM", Some("以 sudo -n 执行：sudo -n kill -TERM 1"))
        );
        let password = buttons(&root_process, Some(&probe(Sudo::NeedsPassword)));
        let term = button(&password, ProcessAction::Term);
        assert_eq!(
            (term.via, term.label.as_str()),
            (Via::Terminal, "结束 TERM · 终端"),
            "the button says where it runs"
        );
        assert!(term.enabled && term.tooltip.as_deref().unwrap().contains("sudo kill -TERM 1"));
        assert_eq!(button(&password, ProcessAction::Kill).label, "强制结束 KILL · 终端");
        assert_eq!(button(&password, ProcessAction::Strace).label, "strace 跟踪", "strace is in a terminal anyway");
        assert_eq!(
            button(&password, ProcessAction::Strace).tooltip.as_deref(),
            Some("在新会话里运行：sudo strace -f -p 1")
        );
        let none = buttons(&root_process, Some(&probe(Sudo::Unavailable)));
        assert_eq!(button(&none, ProcessAction::Term).via, Via::Terminal);
    }

    #[test]
    fn some_processes_cannot_be_acted_on() {
        let mut zombie = with(p(5120, 4410, "[php-fpm8.2] <defunct>"), Some(0.0), 0, "root");
        zombie.state = 'Z';
        let bs = buttons(&zombie, Some(&probe(Sudo::Root)));
        assert!(bs.iter().all(|b| !b.enabled && b.tooltip.as_deref().unwrap().contains("僵尸进程")));
        assert!(plan(ProcessAction::Kill, &zombie, &probe(Sudo::Root)).is_none());
        // Before the probe nothing can be said about how a command would run.
        let bs = buttons(&p(1, 0, "x"), None);
        assert!(bs.iter().all(|b| !b.enabled));
        // A process number that no command may use (0).
        let zero = p(0, 0, "swapper");
        assert!(buttons(&zero, Some(&probe(Sudo::Root))).iter().all(|b| !b.enabled));
        assert!(plan(ProcessAction::Term, &zero, &probe(Sudo::Root)).is_none());
    }

    #[test]
    fn the_plans_of_the_buttons() {
        let own = with(p(2207, 1, "node /var/www/app/dist/server.js"), Some(1.0), 5, "deploy");
        let probe_user = probe(Sudo::NeedsPassword);
        assert_eq!(
            plan(ProcessAction::Term, &own, &probe_user).unwrap(),
            Planned { run: Run::Exec { line: "kill -TERM 2207".into(), done: "已发送" }, confirm: None }
        );
        assert_eq!(
            plan(ProcessAction::Hup, &own, &probe_user).unwrap().run,
            Run::Exec { line: "kill -HUP 2207".into(), done: "已发送" }
        );
        assert_eq!(
            plan(ProcessAction::Nice, &own, &probe_user).unwrap().run,
            Run::Exec { line: "renice -n 5 -p 2207".into(), done: "优先级已降低" }
        );
        let mut niced = own.clone();
        niced.nice = 12;
        assert_eq!(
            plan(ProcessAction::Nice, &niced, &probe_user).unwrap().run,
            Run::Exec { line: "renice -n 17 -p 2207".into(), done: "优先级已降低" }
        );

        // KILL asks first, in the prototype's words.
        let kill = plan(ProcessAction::Kill, &own, &probe_user).unwrap();
        assert_eq!(kill.run, Run::Exec { line: "kill -KILL 2207".into(), done: "已发送" });
        assert_eq!(
            kill.confirm,
            Some(Confirm {
                title: "强制结束进程 2207？".into(),
                text: "node /var/www/app/dist/server.js\nSIGKILL 无法被进程捕获，未落盘的数据会丢失。".into(),
                label: "发送 SIGKILL"
            })
        );

        // strace: a terminal tab. With a password sudo the line is typed but not run.
        let strace = plan(ProcessAction::Strace, &own, &probe_user).unwrap();
        assert_eq!(
            strace.run,
            Run::Terminal { line: "sudo strace -f -p 2207".into(), enter: false, title: "strace 2207".into() }
        );
        assert_eq!(
            plan(ProcessAction::Strace, &own, &probe(Sudo::Passwordless)).unwrap().run,
            Run::Terminal { line: "sudo strace -f -p 2207".into(), enter: true, title: "strace 2207".into() }
        );
        assert_eq!(
            plan(ProcessAction::Strace, &own, &probe(Sudo::Root)).unwrap().run,
            Run::Terminal { line: "strace -f -p 2207".into(), enter: true, title: "strace 2207".into() }
        );
        assert_eq!(
            plan(ProcessAction::Strace, &own, &probe(Sudo::Unavailable)).unwrap().run,
            Run::Terminal { line: "strace -f -p 2207".into(), enter: true, title: "strace 2207".into() }
        );

        // Somebody else's process.
        let other = with(p(1, 0, "/sbin/init"), Some(1.0), 5, "root");
        assert_eq!(
            plan(ProcessAction::Term, &other, &probe(Sudo::Passwordless)).unwrap().run,
            Run::Exec { line: "sudo -n kill -TERM 1".into(), done: "已发送" }
        );
        assert_eq!(
            plan(ProcessAction::Term, &other, &probe(Sudo::Root)).unwrap().run,
            Run::Exec { line: "kill -TERM 1".into(), done: "已发送" }
        );
        let asked = plan(ProcessAction::Kill, &other, &probe(Sudo::NeedsPassword)).unwrap();
        assert_eq!(
            asked.run,
            Run::Terminal { line: "sudo kill -KILL 1".into(), enter: false, title: "KILL 1".into() },
            "typed, not run: the user presses Enter and types the password"
        );
        assert!(asked.confirm.is_some(), "and KILL asks first either way");
        // A process whose owner is not known is treated as the user's own.
        let unknown = with(p(9, 1, "x"), None, 1, "");
        assert_eq!(
            plan(ProcessAction::Term, &unknown, &probe_user).unwrap().run,
            Run::Exec { line: "kill -TERM 9".into(), done: "已发送" }
        );
    }

    #[test]
    fn the_bar_under_the_table_names_the_process() {
        let process = with(p(2207, 1, "node /var/www/app/dist/server.js"), Some(1.0), 5, "deploy");
        assert_eq!(
            selection_text(&process),
            ("PID 2207".to_string(), "  deploy · node /var/www/app/dist/server.js".to_string())
        );
        let nobody = with(p(5, 1, "x"), None, 1, "");
        assert_eq!(selection_text(&nobody).1, "  x");
    }
}
