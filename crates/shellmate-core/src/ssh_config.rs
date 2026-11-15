//! Parser for OpenSSH client configuration (`~/.ssh/config`), for importing hosts.
//!
//! Pure text in, data out: no filesystem access (the caller reads the file and
//! passes the home directory for `~` expansion).
//!
//! Supported: `Host` blocks with several names (one entry per name), `HostName`,
//! `User`, `Port`, `IdentityFile`, `ProxyJump` (comma list, `none`), `LocalForward`,
//! `RemoteForward` and `DynamicForward`; keywords are case-insensitive; `Key Value` and
//! `Key=Value`; double-quoted values; comments.
//!
//! Settings follow OpenSSH: for each alias, all `Host` blocks are walked in file
//! order; a block applies if one of its patterns (`*` and `?` wildcards) matches
//! the alias and none of its `!negated` patterns does, and every keyword takes the
//! **first** value found. Settings before the first `Host` apply to everyone, and
//! so does `Host *`, wherever it is (so a `Host *` at the top beats the specific
//! blocks below it, exactly as in `ssh`).
//!
//! Handling of everything else:
//! - Only plain names after `Host` become entries. Wildcard and negated patterns
//!   are reported in [`ParsedConfig::skipped_patterns`] (a bare `*` is not), but
//!   their settings still apply to the aliases they match.
//! - `Match` blocks are skipped up to the next `Host` ([`ParsedConfig::match_blocks`]).
//! - `Include` is not followed; its arguments are reported in [`ParsedConfig::includes`].
//! - Unlike the other keywords, forwardings add up: every forwarding of every block that
//!   applies to an alias belongs to it. The ones that cannot be imported (Unix-domain socket
//!   paths, `RemoteForward` with a port only — OpenSSH's remote SOCKS mode — and malformed lines)
//!   are counted in [`SshConfigEntry::skipped_forwards`].

use std::path::{Path, PathBuf};

/// Which of the three kinds of forwarding a config line asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardDirective {
    /// `LocalForward`.
    Local,
    /// `RemoteForward`.
    Remote,
    /// `DynamicForward`.
    Dynamic,
}

/// One port forwarding of a host, as written: the addresses are not checked here (the forwarding
/// engine's parser does that, with its own error messages).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigForward {
    /// The kind of forwarding.
    pub kind: ForwardDirective,
    /// `[bind:]port` (`[::1]:8080`, `127.0.0.1:8080`, `8080`); a leading `*:` is written `0.0.0.0:`.
    pub listen: String,
    /// `host:port` the forwarding leads to; `None` for `DynamicForward`.
    pub target: Option<String>,
}

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
    /// Every `LocalForward` / `RemoteForward` / `DynamicForward` that applies, in file order.
    pub forwards: Vec<ConfigForward>,
    /// How many forwardings that apply could not be taken over (see the module docs).
    pub skipped_forwards: usize,
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
    /// Forwardings add up instead of the first one winning.
    forwards: Vec<ConfigForward>,
    skipped_forwards: usize,
}

impl Settings {
    /// Fills the gaps from `other`; existing values win.
    fn fill_from(&mut self, other: &Settings) {
        self.hostname = self.hostname.take().or_else(|| other.hostname.clone());
        self.user = self.user.take().or_else(|| other.user.clone());
        self.port = self.port.or(other.port);
        self.identity_file = self.identity_file.take().or_else(|| other.identity_file.clone());
        self.proxy_jump = self.proxy_jump.take().or_else(|| other.proxy_jump.clone());
        self.forwards.extend(other.forwards.iter().cloned());
        self.skipped_forwards += other.skipped_forwards;
    }
}

/// A `Host` block (or the global section, whose single pattern is `*`).
struct HostBlock {
    patterns: Vec<String>,
    settings: Settings,
}

impl HostBlock {
    /// Whether the block applies to `alias`: some pattern matches and no `!pattern` does.
    fn applies_to(&self, alias: &str) -> bool {
        let mut matched = false;
        for pattern in &self.patterns {
            match pattern.strip_prefix('!') {
                Some(negated) if glob_match(negated, alias) => return false,
                Some(_) => {}
                None => matched |= glob_match(pattern, alias),
            }
        }
        matched
    }
}

/// Where the settings of the current line go.
enum Current {
    /// Index of the block being filled.
    Block(usize),
    /// A `Match` block: ignored.
    Match,
}

/// Parses `text`. `home` is used to expand a leading `~` in `IdentityFile`
/// (left as is when `None`).
pub fn parse_ssh_config(text: &str, home: Option<&Path>) -> ParsedConfig {
    let mut parsed = ParsedConfig::default();
    let mut aliases: Vec<String> = Vec::new();
    // Block 0 is the global section: it comes before every `Host` and matches everything.
    let mut blocks = vec![HostBlock { patterns: vec!["*".to_string()], settings: Settings::default() }];
    let mut current = Current::Block(0);

    for raw in text.lines() {
        let Some((keyword, args)) = split_line(raw) else { continue };
        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                for pattern in &args {
                    if pattern.contains(['*', '?']) || pattern.starts_with('!') {
                        if pattern != "*" && !parsed.skipped_patterns.contains(pattern) {
                            parsed.skipped_patterns.push(pattern.clone());
                        }
                    } else if !aliases.contains(pattern) {
                        aliases.push(pattern.clone());
                    }
                }
                blocks.push(HostBlock { patterns: args, settings: Settings::default() });
                current = Current::Block(blocks.len() - 1);
            }
            "match" => {
                parsed.match_blocks += 1;
                current = Current::Match;
            }
            "include" => parsed.includes.extend(args),
            key @ ("localforward" | "remoteforward" | "dynamicforward") => {
                if let Current::Block(index) = current {
                    let settings = &mut blocks[index].settings;
                    match parse_forward(key, &args) {
                        Some(forward) => settings.forwards.push(forward),
                        None => settings.skipped_forwards += 1,
                    }
                }
            }
            key @ ("hostname" | "user" | "port" | "identityfile" | "proxyjump") => {
                // A jump list may be written with spaces around the commas: take it whole.
                let value = if key == "proxyjump" { Some(args.join(",")) } else { args.into_iter().next() };
                let Some(value) = value.filter(|v| !v.is_empty()) else { continue };
                if let Current::Block(index) = current {
                    apply_setting(&mut blocks[index].settings, key, &value, home);
                }
            }
            _ => {}
        }
    }

    parsed.entries = aliases
        .into_iter()
        .map(|alias| {
            let mut resolved = Settings::default();
            for block in blocks.iter().filter(|b| b.applies_to(&alias)) {
                resolved.fill_from(&block.settings);
            }
            SshConfigEntry {
                alias,
                hostname: resolved.hostname,
                user: resolved.user,
                port: resolved.port,
                identity_file: resolved.identity_file,
                proxy_jump: resolved.proxy_jump.unwrap_or_default(),
                forwards: resolved.forwards,
                skipped_forwards: resolved.skipped_forwards,
            }
        })
        .collect();
    parsed
}

/// Reads the arguments of a forwarding line. `None` for what is not taken over: Unix-domain
/// socket paths, `RemoteForward` with only a port (remote SOCKS) and the wrong number of arguments.
fn parse_forward(keyword: &str, args: &[String]) -> Option<ConfigForward> {
    if args.iter().any(|arg| arg.contains('/')) {
        return None;
    }
    // `*` binds to every address.
    let listen = |arg: &str| arg.strip_prefix("*:").map_or_else(|| arg.to_string(), |rest| format!("0.0.0.0:{rest}"));
    match (keyword, args) {
        ("localforward", [bind, target]) => {
            Some(ConfigForward { kind: ForwardDirective::Local, listen: listen(bind), target: Some(target.clone()) })
        }
        ("remoteforward", [bind, target]) => {
            Some(ConfigForward { kind: ForwardDirective::Remote, listen: listen(bind), target: Some(target.clone()) })
        }
        ("dynamicforward", [bind]) => {
            Some(ConfigForward { kind: ForwardDirective::Dynamic, listen: listen(bind), target: None })
        }
        _ => None,
    }
}

/// Case-insensitive `*` / `?` wildcard match, as used for host patterns.
fn glob_match(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.to_ascii_lowercase(), text.to_ascii_lowercase());
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&c) if c == b'?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
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
                forwards: Vec::new(),
                skipped_forwards: 0,
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
    fn host_star_first_wins_like_openssh() {
        let parsed = parse(
            "Host *\n  User admin\n  Port 2200\n  IdentityFile ~/.ssh/default\nHost web\n  User deploy\n  HostName w.example.com\n  Port 22\n",
        );
        let web = entry(&parsed, "web");
        assert_eq!(web.user.as_deref(), Some("admin"), "the first value wins, and Host * came first");
        assert_eq!(web.port, Some(2200));
        assert_eq!(
            web.hostname.as_deref(),
            Some("w.example.com"),
            "what Host * does not set comes from the block below"
        );
        assert_eq!(web.identity_file, Some(PathBuf::from("/Users/me/.ssh/default")));
        assert_eq!(parsed.entries.len(), 1, "Host * itself is not an entry");
        assert!(parsed.skipped_patterns.is_empty(), "a bare * is not reported as skipped");
    }

    #[test]
    fn host_star_last_only_fills_the_gaps() {
        let parsed = parse(
            "Host web\n  User deploy\n  HostName w.example.com\nHost db\n  HostName d.example.com\n  Port 5432\nHost *\n  User admin\n  Port 2200\n  ProxyJump hop\n",
        );
        let web = entry(&parsed, "web");
        assert_eq!((web.user.as_deref(), web.port), (Some("deploy"), Some(2200)));
        assert_eq!(web.proxy_jump, ["hop"]);
        let db = entry(&parsed, "db");
        assert_eq!((db.user.as_deref(), db.port), (Some("admin"), Some(5432)));
    }

    #[test]
    fn wildcard_blocks_apply_to_the_aliases_they_match() {
        let parsed = parse(
            "Host db.corp.io\n  HostName 10.0.0.1\nHost other.example.com\n  HostName 10.0.0.2\nHost *.corp.io\n  User corp\n  ProxyJump gateway\n",
        );
        let db = entry(&parsed, "db.corp.io");
        assert_eq!((db.user.as_deref(), db.proxy_jump.as_slice()), (Some("corp"), &["gateway".to_string()][..]));
        assert_eq!(entry(&parsed, "other.example.com").user, None);
        assert_eq!(parsed.skipped_patterns, ["*.corp.io"], "the wildcard block itself is not an import candidate");
        assert_eq!(parsed.entries.len(), 2);
    }

    #[test]
    fn negated_patterns_exclude_aliases() {
        let parsed = parse("Host bastion\n  HostName b\nHost web\n  HostName w\nHost * !bastion\n  User common\n");
        assert_eq!(entry(&parsed, "web").user.as_deref(), Some("common"));
        assert_eq!(entry(&parsed, "bastion").user, None, "!bastion keeps the block away from bastion");
        assert_eq!(parsed.skipped_patterns, ["!bastion"]);
    }

    #[test]
    fn a_negated_pattern_alone_matches_nothing() {
        let parsed = parse("Host web\n  HostName w\nHost !web\n  User nobody\n");
        assert_eq!(entry(&parsed, "web").user, None);
    }

    #[test]
    fn several_matching_blocks_combine_in_file_order() {
        let parsed = parse(
            "Host web\n  HostName w.example.com\nHost w*\n  User wuser\n  Port 2201\nHost *\n  User everyone\n  Port 2200\n  IdentityFile /keys/all\nHost web\n  IdentityFile /keys/late\n  ProxyJump j\n",
        );
        let web = entry(&parsed, "web");
        assert_eq!(web.hostname.as_deref(), Some("w.example.com"));
        assert_eq!(web.user.as_deref(), Some("wuser"), "w* came before Host *");
        assert_eq!(web.port, Some(2201));
        assert_eq!(web.identity_file, Some(PathBuf::from("/keys/all")), "the later Host web block loses to Host *");
        assert_eq!(web.proxy_jump, ["j"], "but still contributes what nothing before it set");
    }

    #[test]
    fn patterns_match_case_insensitively_and_with_question_marks() {
        let parsed = parse("Host Web01 web2\n  HostName x\nHost WEB0?\n  User q\n");
        assert_eq!(entry(&parsed, "Web01").user.as_deref(), Some("q"));
        assert_eq!(entry(&parsed, "web2").user, None);
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("*.example.com", "A.Example.COM"));
        assert!(glob_match("10.0.?.*", "10.0.1.99"));
        assert!(!glob_match("*.example.com", "example.org"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
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

    fn forwards(parsed: &ParsedConfig, alias: &str) -> Vec<(ForwardDirective, String, Option<String>)> {
        entry(parsed, alias).forwards.iter().map(|f| (f.kind, f.listen.clone(), f.target.clone())).collect()
    }

    #[test]
    fn the_three_forwardings_in_their_usual_spellings() {
        let parsed = parse(
            "Host db\n  HostName 10.0.2.8\n  LocalForward 3306 10.0.2.8:3306\n  LocalForward 127.0.0.1:5432 db.internal:5432\n  LocalForward [::1]:6379 [fd00::5]:6379\n  RemoteForward 9000 localhost:3000\n  RemoteForward 0.0.0.0:8080 [::1]:80\n  DynamicForward 1080\n  DynamicForward [::1]:1081\n  DynamicForward *:1082\n",
        );
        use ForwardDirective::*;
        let line = |kind, listen: &str, target: Option<&str>| (kind, listen.to_string(), target.map(String::from));
        assert_eq!(
            forwards(&parsed, "db"),
            [
                line(Local, "3306", Some("10.0.2.8:3306")),
                line(Local, "127.0.0.1:5432", Some("db.internal:5432")),
                line(Local, "[::1]:6379", Some("[fd00::5]:6379")),
                line(Remote, "9000", Some("localhost:3000")),
                line(Remote, "0.0.0.0:8080", Some("[::1]:80")),
                line(Dynamic, "1080", None),
                line(Dynamic, "[::1]:1081", None),
                line(Dynamic, "0.0.0.0:1082", None),
            ]
        );
        assert_eq!(entry(&parsed, "db").skipped_forwards, 0);
    }

    #[test]
    fn forwarding_keywords_are_case_insensitive_and_take_equals() {
        let parsed = parse("Host a\n  localforward 1 h:2\n  LOCALFORWARD=3 h:4\n  DynamicForward = 5\n");
        assert_eq!(entry(&parsed, "a").forwards.len(), 3);
        assert_eq!(entry(&parsed, "a").forwards[1].listen, "3");
    }

    #[test]
    fn quoted_arguments_keep_their_spaces_out_of_the_way() {
        let parsed = parse("Host a\n  LocalForward \"8080\" \"web.internal:80\"\n");
        assert_eq!(
            forwards(&parsed, "a"),
            [(ForwardDirective::Local, "8080".to_string(), Some("web.internal:80".to_string()))]
        );
    }

    #[test]
    fn unix_sockets_remote_socks_and_malformed_lines_are_counted_not_imported() {
        let parsed = parse(
            "Host a\n  LocalForward 5432 /var/run/postgresql/.s.PGSQL.5432\n  LocalForward /tmp/local.sock host:22\n  RemoteForward 1080\n  RemoteForward 9000\n  DynamicForward /tmp/socks.sock\n  LocalForward 3306\n  LocalForward 1 h:2 extra\n  DynamicForward\n  LocalForward 7000 ok:7000\n",
        );
        assert_eq!(
            forwards(&parsed, "a"),
            [(ForwardDirective::Local, "7000".to_string(), Some("ok:7000".to_string()))],
            "only the well-formed one is taken over"
        );
        assert_eq!(
            entry(&parsed, "a").skipped_forwards,
            8,
            "every other line is counted (an empty `DynamicForward` has no argument at all)"
        );
    }

    #[test]
    fn forwardings_of_every_applicable_block_add_up_in_file_order() {
        let parsed = parse(
            "Host *\n  LocalForward 9999 monitor:9999\nHost web\n  HostName w\n  LocalForward 8080 w:80\nHost w*\n  DynamicForward 1080\nHost db\n  HostName d\n  LocalForward 3306 d:3306\n",
        );
        assert_eq!(forwards(&parsed, "web").len(), 3, "the shared ones and its own");
        assert_eq!(forwards(&parsed, "web")[0].1, "9999", "in file order: Host * came first");
        assert_eq!(forwards(&parsed, "web")[1].1, "8080");
        assert_eq!(forwards(&parsed, "web")[2].0, ForwardDirective::Dynamic);
        assert_eq!(forwards(&parsed, "db").iter().map(|f| f.1.as_str()).collect::<Vec<_>>(), ["9999", "3306"]);
    }

    #[test]
    fn skipped_forwardings_of_a_shared_block_count_for_each_host() {
        let parsed = parse("Host *\n  RemoteForward 1080\nHost a\n  HostName a\nHost b\n  HostName b\n");
        assert_eq!((entry(&parsed, "a").skipped_forwards, entry(&parsed, "b").skipped_forwards), (1, 1));
    }

    #[test]
    fn forwardings_after_a_match_block_belong_to_nobody() {
        let parsed = parse("Host a\n  LocalForward 1 h:1\nMatch all\n  LocalForward 2 h:2\n");
        assert_eq!(forwards(&parsed, "a").len(), 1);
    }
}
