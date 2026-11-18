//! Domain-qualified identities and action words shared by the native collector and the service detail.

use std::path::Path;

use crate::commands::{BuildError, Command, ServiceAction};

/// Metadata for a launch agent or daemon. `target` is `gui/UID/label` or `system/label`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// Qualified launchctl service target (also the selection key).
    pub target: String,
    /// Source file, when discovered.
    pub plist: Option<String>,
    /// A KeepAlive rule was present; unknown for jobs without a readable plist.
    pub keep_alive: Option<bool>,
    /// Whether the service is registered, independent of whether it has a PID.
    pub loaded: Option<bool>,
    /// Configured log file, stderr preferred.
    pub log: Option<String>,
}

impl Job {
    /// Unqualified plist label.
    pub fn label(&self) -> &str {
        self.target.rsplit('/').next().unwrap_or_default()
    }

    /// Whether actions need a sudo line in the originating tab.
    pub fn system(&self) -> bool {
        self.target.starts_with("system/")
    }

    /// Apple jobs are opt-in even if their names do not begin with `com.apple.`.
    pub fn apple(&self) -> bool {
        self.label().starts_with("com.apple.")
            || self
                .plist
                .as_ref()
                .is_some_and(|path| Path::new(path).starts_with("/System/Library"))
    }

    /// A running app LaunchServices registered (`application.com.vendor.App.123.456`), not a service.
    pub fn app_instance(&self) -> bool {
        self.label().starts_with("application.")
    }

    /// Homebrew formula, version suffix intact.
    pub fn formula(&self) -> Option<&str> {
        self.label()
            .strip_prefix("homebrew.mxcl.")
            .filter(|name| !name.is_empty())
    }

    /// Validated words, never a shell fragment. Unknown stop semantics must not silently become a TERM.
    pub fn command(&self, action: ServiceAction) -> Result<Command, BuildError> {
        let invalid = BuildError::InvalidJob;
        let (domain, label) = self.target.rsplit_once('/').ok_or(invalid)?;
        if !(domain == "system"
            || domain
                .strip_prefix("gui/")
                .is_some_and(|uid| uid.parse::<u32>().is_ok()))
            || label.is_empty()
            || label.starts_with('-')
            || label.chars().any(char::is_control)
        {
            return Err(invalid);
        }
        let target = self.target.as_str();
        match (action, self.loaded) {
            (ServiceAction::Start, Some(false)) => {
                let path = self.plist.as_deref().filter(|path| safe_path(path)).ok_or(invalid)?;
                Command::new(["launchctl", "bootstrap", domain, path])
            }
            (ServiceAction::Start, Some(true)) => Command::new(["launchctl", "kickstart", target]),
            (ServiceAction::Restart, Some(true)) => Command::new(["launchctl", "kickstart", "-k", target]),
            (ServiceAction::Stop, Some(true)) => match self.keep_alive {
                Some(true) => Command::new(["launchctl", "bootout", target]),
                Some(false) => Command::new(["launchctl", "kill", "TERM", target]),
                None => Err(invalid),
            },
            (ServiceAction::Enable, _) => Command::new(["launchctl", "enable", target]),
            (ServiceAction::Disable, _) => Command::new(["launchctl", "disable", target]),
            _ => Err(invalid),
        }
    }
}

/// Safe absolute path for a file operation or a single-line terminal suggestion.
pub fn safe_path(path: &str) -> bool {
    Path::new(path).is_absolute() && !path.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::Job;

    fn job(target: &str) -> Job {
        Job { target: target.into(), plist: None, keep_alive: None, loaded: None, log: None }
    }

    #[test]
    fn launched_apps_are_not_services() {
        assert!(job("gui/501/application.com.tencent.WeWorkMac.100836245.100836250").app_instance());
        assert!(job("gui/501/application.com.apple.Terminal.1152921500311915865.1").app_instance());
        assert!(!job("gui/501/homebrew.mxcl.redis").app_instance());
        assert!(!job("gui/501/88L2Q4487U.com.tencent.WeWorkMac.IPCHelper").app_instance());
    }
}
