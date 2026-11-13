//! Parser for OpenSSH client configuration (`~/.ssh/config`), for importing hosts.
//!
//! Pure text in, data out: no filesystem access (the caller reads the file and
//! passes the home directory for `~` expansion).
//!
//! Supported: `Host` blocks with several names (one entry per name), `HostName`,
//! `User`, `Port`, `IdentityFile`, `ProxyJump` (comma list, `none`); keywords are
//! case-insensitive; `Key Value` and `Key=Value`; double-quoted values; comments.
//!
//! Handling of everything else:
//! - `Host *` (and settings before the first `Host`) act as defaults for all
//!   entries. Unlike OpenSSH, which uses the first value it meets, a specific
//!   entry always wins over the defaults, wherever `Host *` appears.
//! - Host patterns with `*`, `?` or a leading `!` are skipped (reported in
//!   [`ParsedConfig::skipped_patterns`]); so are the settings of blocks that only
//!   have such patterns.
//! - `Match` blocks are skipped up to the next `Host` ([`ParsedConfig::match_blocks`]).
//! - `Include` is not followed; its arguments are reported in [`ParsedConfig::includes`].
//! - If an alias appears in several blocks, the first value of each keyword wins.

use std::path::{Path, PathBuf};

/// One host alias with its settings, defaults already applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshConfigEntry {
    /// The name after `Host`.
    pub alias: String,
    /// `HostName`, if set (the alias itself is the address otherwise).
    pub hostname: Option<String>,
    /// `User`.
    pub user: Option<String>,
    /// `Port`.
    pub port: Option<u16>,
    /// First `IdentityFile`, `~` expanded.
    pub identity_file: Option<PathBuf>,
    /// `ProxyJump` entries in order (`user@host:port` specs or aliases); empty for none.
    pub proxy_jump: Vec<String>,
}

/// Result of parsing a configuration file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedConfig {
    /// Importable hosts in file order.
    pub entries: Vec<SshConfigEntry>,
    /// Arguments of `Include` lines, which were not followed.
    pub includes: Vec<String>,
    /// Host patterns skipped because they contain `*`, `?` or `!`.
    pub skipped_patterns: Vec<String>,
    /// Number of `Match` blocks that were skipped.
    pub match_blocks: usize,
}

/// Settings collected from one or several blocks; `None` means "not set".
#[derive(Clone, Debug, Default)]
struct Settings {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<PathBuf>,
    proxy_jump: Option<Vec<String>>,
}

impl Settings {
    /// Fills the gaps from `other`; existing values win.
    fn fill_from(&mut self, other: &Settings) {
        self.hostname = self.hostname.take().or_else(|| other.hostname.clone());
        self.user = self.user.take().or_else(|| other.user.clone());
        self.port = self.port.or(other.port);
        self.identity_file = self.identity_file.take().or_else(|| other.identity_file.clone());
        self.proxy_jump = self.proxy_jump.take().or_else(|| other.proxy_jump.clone());
    }
}

/// Where the settings of the current line go.
enum Block {
    /// Before the first `Host`: applies to everything, like `Host *`.
    Global,
    /// A `Host` block: indexes into the entry list, plus whether `*` was one of its patterns.
    Host { targets: Vec<usize>, defaults: bool },
    /// A `Match` block: ignored.
    Match,
}

/// Parses `text`. `home` is used to expand a leading `~` in `IdentityFile`
/// (left as is when `None`).
pub fn parse_ssh_config(text: &str, home: Option<&Path>) -> ParsedConfig {
    let mut parsed = ParsedConfig::default();
    let mut settings: Vec<Settings> = Vec::new();
    let mut defaults = Settings::default();
    let mut block = Block::Global;

    for raw in text.lines() {
        let Some((keyword, args)) = split_line(raw) else { continue };
        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                let mut targets = Vec::new();
                let mut is_defaults = false;
                for pattern in args {
                    if pattern == "*" {
                        is_defaults = true;
                    } else if pattern.contains(['*', '?']) || pattern.starts_with('!') {
                        parsed.skipped_patterns.push(pattern);
                    } else {
                        let index = match parsed.entries.iter().position(|e| e.alias == pattern) {
                            Some(index) => index,
                            None => {
                                parsed.entries.push(SshConfigEntry { alias: pattern, ..Default::default() });
                                settings.push(Settings::default());
                                settings.len() - 1
                            }
                        };
                        targets.push(index);
                    }
                }
                block = Block::Host { targets, defaults: is_defaults };
            }
            "match" => {
                parsed.match_blocks += 1;
                block = Block::Match;
            }
            "include" => parsed.includes.extend(args),
            key @ ("hostname" | "user" | "port" | "identityfile" | "proxyjump") => {
                // A jump list may be written with spaces around the commas: take it whole.
                let value = if key == "proxyjump" { Some(args.join(",")) } else { args.into_iter().next() };
                let Some(value) = value.filter(|v| !v.is_empty()) else { continue };
                let apply = |target: &mut Settings| apply_setting(target, key, &value, home);
                match &block {
                    Block::Global => apply(&mut defaults),
                    Block::Host { targets, defaults: is_defaults } => {
                        for &index in targets {
                            apply(&mut settings[index]);
                        }
                        if *is_defaults {
                            apply(&mut defaults);
                        }
                    }
                    Block::Match => {}
                }
            }
            _ => {}
        }
    }

    for (entry, mut own) in parsed.entries.iter_mut().zip(settings) {
        own.fill_from(&defaults);
        entry.hostname = own.hostname;
        entry.user = own.user;
        entry.port = own.port;
        entry.identity_file = own.identity_file;
        entry.proxy_jump = own.proxy_jump.unwrap_or_default();
    }
    parsed
}

/// Sets one keyword unless it already has a value (first value wins).
fn apply_setting(target: &mut Settings, key: &str, value: &str, home: Option<&Path>) {
    match key {
        "hostname" => {
            target.hostname.get_or_insert_with(|| value.to_string());
        }
        "user" => {
            target.user.get_or_insert_with(|| value.to_string());
        }
        "port" if target.port.is_none() => target.port = value.parse().ok(),
        "identityfile" if target.identity_file.is_none() && !value.eq_ignore_ascii_case("none") => {
            target.identity_file = Some(expand_tilde(value, home));
        }
        "proxyjump" if target.proxy_jump.is_none() => {
            target.proxy_jump = Some(if value.eq_ignore_ascii_case("none") {
                Vec::new()
            } else {
                value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
            });
        }
        _ => {}
    }
}

fn expand_tilde(value: &str, home: Option<&Path>) -> PathBuf {
    match (value.strip_prefix("~/"), value == "~", home) {
        (Some(rest), _, Some(home)) => home.join(rest),
        (None, true, Some(home)) => home.to_path_buf(),
        _ => PathBuf::from(value),
    }
}

/// Splits a line into keyword and arguments. Returns `None` for blank lines and comments.
fn split_line(raw: &str) -> Option<(String, Vec<String>)> {
    let line = raw.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // Keyword ends at whitespace or `=`; the separator may be `=` with spaces around it.
    let end = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    Some((keyword.to_string(), tokenize(rest)))
}

/// Splits arguments on whitespace; double quotes group words (with `\"` and `\\`
/// escapes inside). An unquoted `#` at the start of a word begins a comment.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        match chars.peek() {
            None | Some('#') => break,
            _ => {}
        }
        let mut token = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() && !quoted {
                break;
            }
            chars.next();
            match c {
                '"' => quoted = !quoted,
                '\\' if quoted && matches!(chars.peek(), Some('"' | '\\')) => token.extend(chars.next()),
                _ => token.push(c),
            }
        }
        tokens.push(token);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/me";

    fn parse(text: &str) -> ParsedConfig {
        parse_ssh_config(text, Some(Path::new(HOME)))
    }

    fn entry<'a>(parsed: &'a ParsedConfig, alias: &str) -> &'a SshConfigEntry {
        parsed.entries.iter().find(|e| e.alias == alias).unwrap_or_else(|| panic!("no entry {alias}"))
    }

    #[test]
    fn basic_block() {
        let parsed = parse(
            "Host web\n    HostName 10.0.1.21\n    User deploy\n    Port 2222\n    IdentityFile ~/.ssh/web_ed25519\n    ProxyJump bastion\n",
        );
        assert_eq!(
            parsed.entries,
            [SshConfigEntry {
                alias: "web".into(),
                hostname: Some("10.0.1.21".into()),
                user: Some("deploy".into()),
                port: Some(2222),
                identity_file: Some(PathBuf::from("/Users/me/.ssh/web_ed25519")),
                proxy_jump: vec!["bastion".into()],
            }]
        );
        assert!(parsed.includes.is_empty() && parsed.skipped_patterns.is_empty() && parsed.match_blocks == 0);
    }

    #[test]
    fn multiple_names_make_one_entry_each() {
        let parsed = parse("Host a b  c\n  User root\n  HostName shared.example.com\n");
        assert_eq!(parsed.entries.iter().map(|e| e.alias.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(parsed.entries.iter().all(|e| e.user.as_deref() == Some("root")));
        assert!(parsed.entries.iter().all(|e| e.hostname.as_deref() == Some("shared.example.com")));
    }

    #[test]
    fn keywords_are_case_insensitive_and_values_keep_their_case() {
        let parsed = parse("hOsT Web\n  HOSTNAME Example.COM\n  uSeR Deploy\n  PORT 22\n");
        let e = entry(&parsed, "Web");
        assert_eq!((e.hostname.as_deref(), e.user.as_deref(), e.port), (Some("Example.COM"), Some("Deploy"), Some(22)));
    }

    #[test]
    fn equals_syntax_with_and_without_spaces() {
        let parsed = parse("Host=a\n  HostName=1.2.3.4\n  User = bob\n  Port =2200\nHost = b\n  Port= 2201\n");
        let a = entry(&parsed, "a");
        assert_eq!((a.hostname.as_deref(), a.user.as_deref(), a.port), (Some("1.2.3.4"), Some("bob"), Some(2200)));
        assert_eq!(entry(&parsed, "b").port, Some(2201));
    }

    #[test]
    fn quoted_values_and_comments() {
        let parsed = parse(
            "# my servers\nHost \"spaced name\" plain # trailing comment\n  IdentityFile \"~/my keys/id rsa\"\n  User \"quo\\\"ted\"  # another\n\n   # indented comment\n",
        );
        assert_eq!(parsed.entries.iter().map(|e| e.alias.as_str()).collect::<Vec<_>>(), ["spaced name", "plain"]);
        let e = entry(&parsed, "plain");
        assert_eq!(e.identity_file, Some(PathBuf::from("/Users/me/my keys/id rsa")));
        assert_eq!(e.user.as_deref(), Some("quo\"ted"));
    }

    #[test]
    fn hash_inside_a_word_is_not_a_comment() {
        let parsed = parse("Host a\n  HostName host#1.example.com\n");
        assert_eq!(entry(&parsed, "a").hostname.as_deref(), Some("host#1.example.com"));
    }

    #[test]
    fn star_block_provides_defaults_wherever_it_is() {
        let parsed = parse(
            "Host *\n  User admin\n  Port 2200\n  IdentityFile ~/.ssh/default\nHost web\n  User deploy\n  HostName w.example.com\nHost db\n  HostName d.example.com\n  Port 5432\nHost *\n  ProxyJump hop\n",
        );
        let web = entry(&parsed, "web");
        assert_eq!(web.user.as_deref(), Some("deploy"), "own value beats the default");
        assert_eq!(web.port, Some(2200), "gaps are filled from Host *");
        assert_eq!(web.identity_file, Some(PathBuf::from("/Users/me/.ssh/default")));
        assert_eq!(web.proxy_jump, ["hop"], "a later Host * block counts as well");
        let db = entry(&parsed, "db");
        assert_eq!((db.user.as_deref(), db.port), (Some("admin"), Some(5432)));
        assert_eq!(parsed.entries.len(), 2, "Host * itself is not an entry");
    }

    #[test]
    fn settings_before_the_first_host_are_defaults() {
        let parsed = parse("User everyone\nHost a\n  HostName x\n");
        assert_eq!(entry(&parsed, "a").user.as_deref(), Some("everyone"));
    }

    #[test]
    fn wildcard_and_negated_patterns_are_skipped() {
        let parsed =
            parse("Host a *.example.com b? !c\n  User root\nHost prod-*\n  User nobody\nHost real\n  HostName r\n");
        assert_eq!(parsed.entries.iter().map(|e| e.alias.as_str()).collect::<Vec<_>>(), ["a", "real"]);
        assert_eq!(parsed.skipped_patterns, ["*.example.com", "b?", "!c", "prod-*"]);
        assert_eq!(entry(&parsed, "a").user.as_deref(), Some("root"));
        assert_eq!(entry(&parsed, "real").user, None, "the settings of a wildcard-only block go nowhere");
    }

    #[test]
    fn match_blocks_are_skipped_until_the_next_host() {
        let parsed = parse(
            "Host a\n  User one\nMatch host b exec \"true\"\n  User two\n  HostName ignored\nHost c\n  User three\nMatch all\n  Port 1\n",
        );
        assert_eq!(parsed.match_blocks, 2);
        assert_eq!(entry(&parsed, "a").user.as_deref(), Some("one"));
        assert_eq!(entry(&parsed, "c").user.as_deref(), Some("three"));
        assert_eq!(entry(&parsed, "c").port, None, "settings after Match do not leak into the previous Host");
        assert_eq!(parsed.entries.iter().find(|e| e.hostname.as_deref() == Some("ignored")), None);
    }

    #[test]
    fn include_is_reported_not_followed() {
        let parsed = parse("Include ~/.ssh/config.d/*\nHost a\n  Include extra.conf \"other file\"\n  User u\n");
        assert_eq!(parsed.includes, ["~/.ssh/config.d/*", "extra.conf", "other file"]);
        assert_eq!(entry(&parsed, "a").user.as_deref(), Some("u"));
    }

    #[test]
    fn proxy_jump_lists_and_none() {
        let parsed = parse(
            "Host a\n  ProxyJump bastion,ops@jump.example.com:2222 , last\nHost b\n  ProxyJump none\nHost c\n  HostName c\nHost *\n  ProxyJump default-hop\n",
        );
        assert_eq!(entry(&parsed, "a").proxy_jump, ["bastion", "ops@jump.example.com:2222", "last"]);
        assert!(entry(&parsed, "b").proxy_jump.is_empty(), "none overrides the default");
        assert_eq!(entry(&parsed, "c").proxy_jump, ["default-hop"]);
    }

    #[test]
    fn tilde_expansion_needs_a_home() {
        let text = "Host a\n  IdentityFile ~/.ssh/id\nHost b\n  IdentityFile ~\nHost c\n  IdentityFile /abs/key\nHost d\n  IdentityFile ~other/key\nHost e\n  IdentityFile none\n";
        let parsed = parse(text);
        assert_eq!(entry(&parsed, "a").identity_file, Some(PathBuf::from("/Users/me/.ssh/id")));
        assert_eq!(entry(&parsed, "b").identity_file, Some(PathBuf::from("/Users/me")));
        assert_eq!(entry(&parsed, "c").identity_file, Some(PathBuf::from("/abs/key")));
        assert_eq!(entry(&parsed, "d").identity_file, Some(PathBuf::from("~other/key")));
        assert_eq!(entry(&parsed, "e").identity_file, None);
        let without_home = parse_ssh_config(text, None);
        assert_eq!(entry(&without_home, "a").identity_file, Some(PathBuf::from("~/.ssh/id")));
    }

    #[test]
    fn first_value_wins_and_repeated_aliases_merge() {
        let parsed = parse(
            "Host a\n  User first\n  User second\n  IdentityFile one\n  IdentityFile two\nHost a\n  User third\n  Port 99\n",
        );
        assert_eq!(parsed.entries.len(), 1);
        let a = entry(&parsed, "a");
        assert_eq!((a.user.as_deref(), a.port), (Some("first"), Some(99)));
        assert_eq!(a.identity_file, Some(PathBuf::from("one")));
    }

    #[test]
    fn invalid_port_and_unknown_keywords_are_ignored() {
        let parsed = parse(
            "Host a\n  Port notaport\n  ForwardAgent yes\n  ProxyCommand ssh -W %h:%p x\n  User u\n  Port 2222\n",
        );
        let a = entry(&parsed, "a");
        assert_eq!((a.port, a.user.as_deref()), (Some(2222), Some("u")), "a bad Port does not block a later valid one");
    }

    #[test]
    fn crlf_and_empty_input() {
        let parsed = parse("Host a\r\n  User u\r\n");
        assert_eq!(entry(&parsed, "a").user.as_deref(), Some("u"));
        assert_eq!(parse(""), ParsedConfig::default());
        assert_eq!(parse("# nothing but comments\n\n"), ParsedConfig::default());
    }

    #[test]
    fn keyword_without_value_is_ignored() {
        let parsed = parse("Host a\n  User\n  HostName\n  Port\n");
        assert_eq!(parsed.entries, [SshConfigEntry { alias: "a".into(), ..Default::default() }]);
    }
}
