//! The rule model: what to forward, from where to where.

use std::fmt;

use crate::addr::{AddrError, Endpoint, parse_listen, parse_target};

/// Stable identifier of a forwarding rule.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuleId(String);

impl RuleId {
    /// A fresh random id (UUID v4).
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Wraps an id read back from storage (or a test fixture) without validating it.
    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The id as text, e.g. for database columns.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The three kinds of forwarding, named after the `ssh` options.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ForwardKind {
    /// `-L`: a port on this machine leads to a target the server connects to.
    Local,
    /// `-R`: a port on the server leads to a target on this machine.
    Remote,
    /// `-D`: a SOCKS5 proxy on this machine; the client chooses the target.
    Dynamic,
}

impl ForwardKind {
    /// `L`, `R` or `D` (as stored).
    pub fn letter(self) -> &'static str {
        match self {
            ForwardKind::Local => "L",
            ForwardKind::Remote => "R",
            ForwardKind::Dynamic => "D",
        }
    }

    /// The kind stored as `letter`.
    pub fn from_letter(letter: &str) -> Option<Self> {
        match letter {
            "L" => Some(ForwardKind::Local),
            "R" => Some(ForwardKind::Remote),
            "D" => Some(ForwardKind::Dynamic),
            _ => None,
        }
    }

    /// `-L`, `-R` or `-D`.
    pub fn flag(self) -> &'static str {
        match self {
            ForwardKind::Local => "-L",
            ForwardKind::Remote => "-R",
            ForwardKind::Dynamic => "-D",
        }
    }

    /// The name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            ForwardKind::Local => "本地转发",
            ForwardKind::Remote => "远程转发",
            ForwardKind::Dynamic => "动态转发",
        }
    }
}

/// One forwarding rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardRule {
    /// Identity of the rule.
    pub id: RuleId,
    /// Name shown to the user.
    pub name: String,
    /// What kind of forwarding this is.
    pub kind: ForwardKind,
    /// Where to listen: on this machine for `-L` / `-D`, on the server for `-R`.
    pub listen: Endpoint,
    /// Where connections go: resolved by the server for `-L`, by this machine for `-R`; `None`
    /// for `-D` (the SOCKS client names the target).
    pub target: Option<Endpoint>,
    /// Whether the rule starts by itself when the connection comes up.
    pub enabled: bool,
}

impl ForwardRule {
    /// A rule from what the user typed. A dynamic forward ignores `target`; the others need
    /// one. The name defaults to [`default_name`](Self::default_name) when blank.
    pub fn parse(
        id: RuleId,
        name: &str,
        kind: ForwardKind,
        listen: &str,
        target: &str,
        enabled: bool,
    ) -> Result<Self, AddrError> {
        let listen = parse_listen(listen, kind)?;
        let target = match kind {
            ForwardKind::Dynamic => None,
            ForwardKind::Local | ForwardKind::Remote => Some(parse_target(target)?),
        };
        let mut rule = Self { id, name: name.trim().to_string(), kind, listen, target, enabled };
        if rule.name.is_empty() {
            rule.name = rule.default_name();
        }
        Ok(rule)
    }

    /// `-L 3306`: the flag and the listen port.
    pub fn default_name(&self) -> String {
        format!("{} {}", self.kind.flag(), self.listen.port)
    }

    /// `listen → target` as shown in the panel (`SOCKS5` for dynamic forwards).
    pub fn route_text(&self) -> String {
        match &self.target {
            Some(target) => format!("{} → {}", self.listen, target),
            None => format!("{} → SOCKS5", self.listen),
        }
    }

    /// Whether this rule makes a port reachable beyond this machine: a local or dynamic
    /// forward listening on anything but loopback.
    pub fn exposes_lan(&self) -> bool {
        self.kind != ForwardKind::Remote && !self.listen.is_loopback()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(kind: ForwardKind, listen: &str, target: &str) -> Result<ForwardRule, AddrError> {
        ForwardRule::parse(RuleId::from_raw("r"), "", kind, listen, target, true)
    }

    #[test]
    fn rules_are_built_from_text() {
        let local = rule(ForwardKind::Local, "5433", "db.internal:5432").unwrap();
        assert_eq!(local.listen, Endpoint::new("127.0.0.1", 5433));
        assert_eq!(local.target, Some(Endpoint::new("db.internal", 5432)));
        assert_eq!(local.name, "-L 5433");
        assert_eq!(local.route_text(), "127.0.0.1:5433 → db.internal:5432");

        let remote = rule(ForwardKind::Remote, "0.0.0.0:9000", "localhost:3000").unwrap();
        assert_eq!(remote.name, "-R 9000");
        assert_eq!(remote.listen.host, "0.0.0.0");

        let dynamic = rule(ForwardKind::Dynamic, "1080", "ignored garbage").unwrap();
        assert_eq!(dynamic.target, None);
        assert_eq!(dynamic.route_text(), "127.0.0.1:1080 → SOCKS5");

        let named =
            ForwardRule::parse(RuleId::generate(), "  生产库  ", ForwardKind::Local, "1", "h:2", false).unwrap();
        assert_eq!(named.name, "生产库");
        assert!(!named.enabled);
    }

    #[test]
    fn bad_input_names_the_field_that_is_wrong() {
        assert_eq!(rule(ForwardKind::Local, "0", "h:1"), Err(AddrError::ZeroPort));
        assert_eq!(rule(ForwardKind::Local, "1", ""), Err(AddrError::Empty));
        assert_eq!(rule(ForwardKind::Remote, "1", "5000"), Err(AddrError::MissingHost));
        assert!(rule(ForwardKind::Remote, "0", "h:1").is_ok(), "remote port 0 is fine");
    }

    #[test]
    fn kinds_round_trip_through_their_letter() {
        for kind in [ForwardKind::Local, ForwardKind::Remote, ForwardKind::Dynamic] {
            assert_eq!(ForwardKind::from_letter(kind.letter()), Some(kind));
            assert_eq!(kind.flag(), format!("-{}", kind.letter()));
        }
        assert_eq!(ForwardKind::from_letter("X"), None);
    }

    #[test]
    fn only_non_loopback_local_listeners_are_visible_to_the_lan() {
        assert!(!rule(ForwardKind::Local, "3306", "h:1").unwrap().exposes_lan());
        assert!(!rule(ForwardKind::Local, "[::1]:3306", "h:1").unwrap().exposes_lan());
        assert!(rule(ForwardKind::Local, "0.0.0.0:3306", "h:1").unwrap().exposes_lan());
        assert!(rule(ForwardKind::Dynamic, "192.168.1.5:1080", "").unwrap().exposes_lan());
        assert!(!rule(ForwardKind::Remote, "0.0.0.0:9000", "h:1").unwrap().exposes_lan());
    }
}
