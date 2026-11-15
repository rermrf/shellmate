//! The commands the monitor runs on the server (signals, renice, strace, and for later services and
//! containers), built as pure functions: every value comes in as one word, quoted by [`quote`], and
//! a process id is only accepted as a plain number.

use crate::probe::{Probe, Sudo};
use crate::quote::{self, QuoteError};

/// Why a command could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// There is nothing to run.
    #[error("命令是空的")]
    Empty,
    /// An argument contains a NUL byte.
    #[error("参数里含有空字节")]
    Nul,
    /// A process id must be a positive number that fits the kernel's range.
    #[error("无效的进程号")]
    BadPid,
    /// A name that starts with `-` would be read as an option.
    #[error("名称不能以 - 开头")]
    LooksLikeOption,
}

impl From<QuoteError> for BuildError {
    fn from(_: QuoteError) -> Self {
        BuildError::Nul
    }
}

/// A command as a program and its arguments (not yet a shell line).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    argv: Vec<String>,
}

impl Command {
    /// A command from its program and arguments.
    pub fn new<I, S>(argv: I) -> Result<Self, BuildError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        if argv.is_empty() || argv[0].is_empty() {
            return Err(BuildError::Empty);
        }
        if argv.iter().any(|word| word.contains('\0')) {
            return Err(BuildError::Nul);
        }
        Ok(Self { argv })
    }

    /// The words of the command.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The command as a line for a shell: every word quoted where it needs to be.
    pub fn line(&self) -> String {
        quote::join(&self.argv).expect("a command has no NUL bytes")
    }

    /// The command with `prefix` words in front.
    fn prefixed(&self, prefix: &[&str]) -> Self {
        Self { argv: prefix.iter().map(|word| (*word).to_string()).chain(self.argv.iter().cloned()).collect() }
    }
}

/// How to get a command done on this server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Run it on an exec channel; `line` is the command line as it is run (what a toast shows).
    Exec {
        /// The command line.
        line: String,
    },
    /// It needs a password, and the app does not ask for one: type `line` in a terminal tab on the same
    /// connection. The button says so.
    Terminal {
        /// The command line to type, with `sudo` in front.
        line: String,
    },
}

/// What a command needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Privilege {
    /// The login user can do it (a signal to the user's own process).
    User,
    /// It needs root (a signal to somebody else's process, a service, the docker socket).
    Root,
}

/// Decides how `command` is run: as it is for the login user or root, with `sudo -n` if the server
/// allows sudo without a password, and otherwise as a plan to run it in a terminal.
pub fn plan(command: &Command, privilege: Privilege, probe: &Probe) -> Plan {
    match (privilege, probe.sudo) {
        (Privilege::User, _) | (Privilege::Root, Sudo::Root) => Plan::Exec { line: command.line() },
        (Privilege::Root, Sudo::Passwordless) => Plan::Exec { line: command.prefixed(&["sudo", "-n"]).line() },
        (Privilege::Root, Sudo::NeedsPassword | Sudo::Unavailable) => {
            Plan::Terminal { line: command.prefixed(&["sudo"]).line() }
        }
    }
}

/// The line to type in a terminal tab for a command that is interactive by nature (`strace`,
/// `docker exec -it`, a followed log): with `sudo` in front unless the login user is root or does not
/// need it.
pub fn terminal_line(command: &Command, privilege: Privilege, probe: &Probe) -> String {
    match (privilege, probe.sudo) {
        (Privilege::User, _) | (Privilege::Root, Sudo::Root | Sudo::Unavailable) => command.line(),
        (Privilege::Root, Sudo::Passwordless | Sudo::NeedsPassword) => command.prefixed(&["sudo"]).line(),
    }
}

fn pid_word(pid: u32) -> Result<String, BuildError> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(BuildError::BadPid);
    }
    Ok(pid.to_string())
}

// ---- processes -------------------------------------------------------------------------------

/// A signal the process page can send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Ask to end.
    Term,
    /// Reload the configuration.
    Hup,
    /// End at once; cannot be caught.
    Kill,
}

impl Signal {
    /// `TERM`, `HUP` or `KILL`.
    pub fn name(self) -> &'static str {
        match self {
            Signal::Term => "TERM",
            Signal::Hup => "HUP",
            Signal::Kill => "KILL",
        }
    }
}

/// `kill -TERM 2207`.
pub fn kill(pid: u32, signal: Signal) -> Result<Command, BuildError> {
    Command::new(["kill".to_string(), format!("-{}", signal.name()), pid_word(pid)?])
}

/// The priority 5 lower than `current_nice`, as `renice -n 7 -p 2207` (the new value is absolute:
/// that is what `renice -n` takes, on util-linux and BusyBox alike), clamped to what Linux allows.
pub fn lower_priority(pid: u32, current_nice: i32) -> Result<Command, BuildError> {
    let nice = current_nice.saturating_add(5).clamp(-20, 19);
    Command::new(["renice".to_string(), "-n".into(), nice.to_string(), "-p".into(), pid_word(pid)?])
}

/// `strace -f -p 2207`: runs for as long as somebody looks, so it goes to a terminal tab.
pub fn strace(pid: u32) -> Result<Command, BuildError> {
    Command::new(["strace".to_string(), "-f".into(), "-p".into(), pid_word(pid)?])
}

// ---- services and containers (the pages of the next milestone) --------------------------------

fn name_word(name: &str) -> Result<&str, BuildError> {
    if name.is_empty() {
        return Err(BuildError::Empty);
    }
    if name.starts_with('-') {
        return Err(BuildError::LooksLikeOption);
    }
    Ok(name)
}

/// What can be done to a systemd unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    /// `start`
    Start,
    /// `stop`
    Stop,
    /// `restart`
    Restart,
    /// `reload`
    Reload,
    /// `enable`
    Enable,
    /// `disable`
    Disable,
}

impl ServiceAction {
    /// The `systemctl` verb.
    pub fn verb(self) -> &'static str {
        match self {
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
            ServiceAction::Reload => "reload",
            ServiceAction::Enable => "enable",
            ServiceAction::Disable => "disable",
        }
    }
}

/// `systemctl restart nginx.service`.
pub fn systemctl(action: ServiceAction, unit: &str) -> Result<Command, BuildError> {
    Command::new(["systemctl", action.verb(), name_word(unit)?])
}

/// `journalctl -u nginx.service -n 50 -f --no-pager`, for a terminal tab.
pub fn journal_follow(unit: &str, lines: u32) -> Result<Command, BuildError> {
    Command::new([
        "journalctl".to_string(),
        "-u".into(),
        name_word(unit)?.to_string(),
        "-n".into(),
        lines.to_string(),
        "-f".into(),
        "--no-pager".into(),
    ])
}

/// What can be done to a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerAction {
    /// `start`
    Start,
    /// `stop`
    Stop,
    /// `restart`
    Restart,
    /// `rm`
    Remove,
}

impl ContainerAction {
    /// The `docker` verb.
    pub fn verb(self) -> &'static str {
        match self {
            ContainerAction::Start => "start",
            ContainerAction::Stop => "stop",
            ContainerAction::Restart => "restart",
            ContainerAction::Remove => "rm",
        }
    }
}

/// `docker stop web-1` (`runtime` is `docker` or `podman`).
pub fn container(runtime: &str, action: ContainerAction, name: &str) -> Result<Command, BuildError> {
    Command::new([runtime, action.verb(), name_word(name)?])
}

/// `docker image prune -f`: removes dangling images only.
pub fn prune_dangling_images(runtime: &str) -> Result<Command, BuildError> {
    Command::new([runtime, "image", "prune", "-f"])
}

/// `docker logs -f --tail 100 web-1`.
pub fn container_logs(runtime: &str, name: &str, tail: u32) -> Result<Command, BuildError> {
    Command::new([
        runtime.to_string(),
        "logs".into(),
        "-f".into(),
        "--tail".into(),
        tail.to_string(),
        name_word(name)?.to_string(),
    ])
}

/// `docker exec -it web-1 sh` (or `bash`), for a terminal tab.
pub fn container_shell(runtime: &str, name: &str, bash: bool) -> Result<Command, BuildError> {
    Command::new([runtime, "exec", "-it", name_word(name)?, if bash { "bash" } else { "sh" }])
}

/// The disk analysis the overview offers for a full mount point, for a terminal tab:
/// `du -xh --max-depth=1 '/mnt/data' | sort -rh | head`.
pub fn analyze_directory(mount: &str) -> Result<String, BuildError> {
    let mount = quote::quote(mount)?;
    Ok(format!("du -xh --max-depth=1 {mount} | sort -rh | head"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::Probe;

    fn probe(sudo: Sudo) -> Probe {
        Probe { sudo, ..Probe::unknown() }
    }

    #[test]
    fn process_commands() {
        assert_eq!(kill(2207, Signal::Term).unwrap().line(), "kill -TERM 2207");
        assert_eq!(kill(2207, Signal::Hup).unwrap().line(), "kill -HUP 2207");
        assert_eq!(kill(2207, Signal::Kill).unwrap().line(), "kill -KILL 2207");
        assert_eq!(strace(42).unwrap().line(), "strace -f -p 42");
        assert_eq!(lower_priority(42, 0).unwrap().line(), "renice -n 5 -p 42");
        assert_eq!(lower_priority(42, 10).unwrap().line(), "renice -n 15 -p 42");
        assert_eq!(lower_priority(42, 17).unwrap().line(), "renice -n 19 -p 42", "nothing beyond what Linux allows");
        assert_eq!(lower_priority(42, -20).unwrap().line(), "renice -n -15 -p 42");
        assert_eq!(lower_priority(42, i32::MAX).unwrap().line(), "renice -n 19 -p 42");
    }

    #[test]
    fn a_process_id_is_a_plain_positive_number() {
        assert_eq!(kill(0, Signal::Term), Err(BuildError::BadPid), "kill 0 would hit the whole process group");
        assert_eq!(
            strace(u32::MAX),
            Err(BuildError::BadPid),
            "and nothing above the range, which kill would take for -1"
        );
        assert_eq!(lower_priority(0, 0), Err(BuildError::BadPid));
        assert!(kill(1, Signal::Term).is_ok());
    }

    #[test]
    fn the_plan_follows_what_the_server_allows() {
        let command = kill(2207, Signal::Term).unwrap();
        // Your own process: as it is, whoever you are.
        for sudo in [Sudo::Root, Sudo::Passwordless, Sudo::NeedsPassword, Sudo::Unavailable] {
            assert_eq!(plan(&command, Privilege::User, &probe(sudo)), Plan::Exec { line: "kill -TERM 2207".into() });
        }
        // Somebody else's process.
        assert_eq!(plan(&command, Privilege::Root, &probe(Sudo::Root)), Plan::Exec { line: "kill -TERM 2207".into() });
        assert_eq!(
            plan(&command, Privilege::Root, &probe(Sudo::Passwordless)),
            Plan::Exec { line: "sudo -n kill -TERM 2207".into() }
        );
        assert_eq!(
            plan(&command, Privilege::Root, &probe(Sudo::NeedsPassword)),
            Plan::Terminal { line: "sudo kill -TERM 2207".into() },
            "the app does not collect sudo passwords"
        );
        assert_eq!(
            plan(&command, Privilege::Root, &probe(Sudo::Unavailable)),
            Plan::Terminal { line: "sudo kill -TERM 2207".into() }
        );
    }

    #[test]
    fn terminal_commands_get_sudo_when_it_can_help() {
        let strace = strace(9).unwrap();
        assert_eq!(terminal_line(&strace, Privilege::Root, &probe(Sudo::Root)), "strace -f -p 9");
        assert_eq!(terminal_line(&strace, Privilege::Root, &probe(Sudo::Passwordless)), "sudo strace -f -p 9");
        assert_eq!(terminal_line(&strace, Privilege::Root, &probe(Sudo::NeedsPassword)), "sudo strace -f -p 9");
        assert_eq!(terminal_line(&strace, Privilege::Root, &probe(Sudo::Unavailable)), "strace -f -p 9");
        assert_eq!(terminal_line(&strace, Privilege::User, &probe(Sudo::NeedsPassword)), "strace -f -p 9");
    }

    #[test]
    fn names_from_the_server_are_one_word_each() {
        let tricky = "my unit; rm -rf ~ $(id) 'x'.service";
        assert_eq!(
            systemctl(ServiceAction::Restart, tricky).unwrap().line(),
            r"systemctl restart 'my unit; rm -rf ~ $(id) '\''x'\''.service'"
        );
        assert_eq!(systemctl(ServiceAction::Enable, "nginx.service").unwrap().line(), "systemctl enable nginx.service");
        assert_eq!(
            journal_follow("a b.service", 50).unwrap().line(),
            "journalctl -u 'a b.service' -n 50 -f --no-pager"
        );
        assert_eq!(container("docker", ContainerAction::Stop, "web-1").unwrap().line(), "docker stop web-1");
        assert_eq!(container("podman", ContainerAction::Remove, "a$b").unwrap().line(), "podman rm 'a$b'");
        assert_eq!(prune_dangling_images("docker").unwrap().line(), "docker image prune -f");
        assert_eq!(container_logs("docker", "web 1", 100).unwrap().line(), "docker logs -f --tail 100 'web 1'");
        assert_eq!(container_shell("docker", "web-1", true).unwrap().line(), "docker exec -it web-1 bash");
        assert_eq!(container_shell("docker", "web-1", false).unwrap().line(), "docker exec -it web-1 sh");
        assert_eq!(analyze_directory("/mnt/my data").unwrap(), "du -xh --max-depth=1 '/mnt/my data' | sort -rh | head");
        assert_eq!(analyze_directory("/"), Ok("du -xh --max-depth=1 / | sort -rh | head".to_string()));
    }

    #[test]
    fn bad_names_are_refused() {
        assert_eq!(systemctl(ServiceAction::Stop, ""), Err(BuildError::Empty));
        assert_eq!(systemctl(ServiceAction::Stop, "--now"), Err(BuildError::LooksLikeOption));
        assert_eq!(container("docker", ContainerAction::Stop, "-f"), Err(BuildError::LooksLikeOption));
        assert_eq!(container("docker", ContainerAction::Stop, "a\0b"), Err(BuildError::Nul));
        assert_eq!(analyze_directory("/a\0b"), Err(BuildError::Nul));
        assert_eq!(Command::new(Vec::<String>::new()), Err(BuildError::Empty));
    }

    /// A shell reads the built line back as the words it was built from.
    #[test]
    fn a_built_line_is_read_back_by_a_shell_as_its_words() {
        let command = systemctl(ServiceAction::Restart, "it's a $weird; name\nwith newline.service").unwrap();
        let script = format!(
            "f() {{ for a in \"$@\"; do printf '%s\\0' \"$a\"; done; }}; f {}",
            &command.line()["systemctl".len()..]
        );
        let output = std::process::Command::new("sh").arg("-c").arg(script).output().unwrap();
        let words: Vec<String> = output
            .stdout
            .split(|b| *b == 0)
            .filter(|w| !w.is_empty())
            .map(|w| String::from_utf8_lossy(w).into_owned())
            .collect();
        assert_eq!(words, ["restart", "it's a $weird; name\nwith newline.service"]);
    }
}
