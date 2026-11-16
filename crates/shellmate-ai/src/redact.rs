//! Conservative text-only rules. KEY matches a delimiter component or a suffix
//! (including apiKey/privatekey), never a prefix: KEYBOARD is not sensitive.
//! No terminal escapes, files, environment variables or external stores are read.
use std::collections::BTreeMap;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    PrivateKey,
    Environment,
    Authorization,
    CredentialPair,
    CommandArgument,
    Aws,
    GitHub,
    Slack,
    ApiKey,
    Google,
    Jwt,
    UrlCredentials,
}
impl Rule {
    pub fn label(self) -> &'static str {
        match self {
            Self::PrivateKey => "私钥",
            Self::Environment => ".env",
            Self::Authorization => "授权头",
            Self::CredentialPair => "凭据",
            Self::CommandArgument => "命令行凭据",
            Self::Aws => "AWS 密钥",
            Self::GitHub => "GitHub Token",
            Self::Slack => "Slack Token",
            Self::ApiKey => "API Key",
            Self::Google => "Google Key",
            Self::Jwt => "JWT",
            Self::UrlCredentials => "URL 凭据",
        }
    }
}
pub const RULES: &[(Rule, &str)] = &[
    (
        Rule::PrivateKey,
        "PEM / OpenSSH 私钥块（不完整块也隐藏到末尾）",
    ),
    (
        Rule::Environment,
        "cat .env / *.env 后的输出，直到可识别的 shell 提示符",
    ),
    (Rule::Authorization, "Authorization: Bearer"),
    (
        Rule::CredentialPair,
        "敏感键的 = 或 : 值，包括 *_PWD；保留 PWD / OLDPWD；KEY 只匹配独立部分或后缀，不匹配 KEYBOARD",
    ),
    (
        Rule::CommandArgument,
        "敏感长参数；redis-cli -a；MySQL 系列 -p值；sshpass -p；curl -u / --user 的密码（保留用户名）",
    ),
    (Rule::Aws, "AKIA/ASIA 访问密钥与 AWS secret-key 上下文"),
    (Rule::GitHub, "ghp_/gho_/ghu_/ghs_/ghr_/github_pat_"),
    (Rule::Slack, "xox?-"),
    (Rule::ApiKey, "sk- / sk-ant-"),
    (Rule::Google, "AIza"),
    (Rule::Jwt, "三段 base64url JWT"),
    (Rule::UrlCredentials, "URL 的 user:password@"),
];
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub counts: BTreeMap<Rule, usize>,
}
impl Redacted {
    pub fn count(&self) -> usize {
        self.counts.values().sum()
    }
    fn mark(&mut self, rule: Rule) -> String {
        *self.counts.entry(rule).or_default() += 1;
        format!("[已去除:{}]", rule.label())
    }
}
pub fn redact(text: &str) -> Redacted {
    redact_impl(text, false)
}
fn redact_impl(text: &str, preserve_source_lines: bool) -> Redacted {
    let mut out = Redacted::default();
    let mut pem = String::new();
    let mut rest = text;
    const BEGIN: &str = "-----BEGIN ";
    while let Some(start) = rest.find(BEGIN) {
        pem.push_str(&rest[..start]);
        let after = &rest[start..];
        let label_end = after[BEGIN.len()..].find("-----").map(|n| BEGIN.len() + n);
        let Some(label_end) =
            label_end.filter(|&end| after[BEGIN.len()..end].ends_with("PRIVATE KEY"))
        else {
            pem.push_str(BEGIN);
            rest = &after[BEGIN.len()..];
            continue;
        };
        let label = &after[BEGIN.len()..label_end];
        let end_marker = format!("-----END {label}-----");
        let body_start = label_end + "-----".len();
        let end = after[body_start..]
            .find(&end_marker)
            .map_or(after.len(), |n| body_start + n + end_marker.len());
        let marker = out.mark(Rule::PrivateKey);
        pem.push_str(&marker);
        if preserve_source_lines {
            // recent_context selects source display lines before collapsing this
            // block, so a long private key cannot pull older output into scope.
            for _ in 0..after[..end].bytes().filter(|b| *b == b'\n').count() {
                pem.push('\n');
                pem.push_str(&marker);
            }
        }
        rest = &after[end..];
    }
    pem.push_str(rest);
    let mut env = false;
    let mut lines = Vec::new();
    for line in pem.split('\n') {
        if env {
            if is_prompt(line) {
                env = false
            } else {
                lines.push(format!("[已去除:{}]", Rule::Environment.label()));
                continue;
            }
        }
        let redacted = redact_line(line, &mut out);
        lines.push(redacted);
        if reads_env(line) {
            env = true;
            *out.counts.entry(Rule::Environment).or_default() += 1;
        }
    }
    out.text = lines.join("\n");
    out
}
/// Redact before taking the tail, so .env or a private-key block that began
/// above the last 40 lines cannot expose its continuation. Only the tail leaves
/// this function. Counts describe markers visible in that retained snapshot.
pub fn recent_context(displayed: &str, limit: usize) -> Redacted {
    let redacted = redact_impl(displayed, true);
    let lines = redacted.text.lines().collect::<Vec<_>>();
    let mut retained = Vec::new();
    for &line in &lines[lines.len().saturating_sub(limit)..] {
        if line == "[已去除:私钥]" && retained.last() == Some(&line) {
            continue;
        }
        retained.push(line);
    }
    let text = retained.join("\n");
    let mut counts = BTreeMap::new();
    for &(rule, _) in RULES {
        let n = text.matches(&format!("[已去除:{}]", rule.label())).count();
        if n > 0 {
            counts.insert(rule, n);
        }
    }
    Redacted { text, counts }
}
fn is_prompt(line: &str) -> bool {
    let line = line.trim_start();
    if ["$ ", "% ", "❯ ", "➜ "].iter().any(|p| line.starts_with(p)) {
        return true;
    }
    let Some(i) = line.find(['$', '#', '%']) else {
        return false;
    };
    let prefix = &line[..i];
    !prefix.contains(['=', '"', '\''])
        && (prefix.contains('@')
            || prefix.starts_with('[') && prefix.contains(']')
            || prefix.trim() == "root")
        && line
            .as_bytes()
            .get(i + 1)
            .is_none_or(u8::is_ascii_whitespace)
}
fn reads_env(line: &str) -> bool {
    let words = line.split_whitespace().collect::<Vec<_>>();
    words
        .iter()
        .position(|word| *word == "cat")
        .is_some_and(|i| {
            words[i + 1..].iter().any(|word| {
                let word = word.trim_matches(['\'', '"', ';']);
                let name = word.rsplit('/').next().unwrap_or(word);
                name == ".env" || name.ends_with(".env") || name.starts_with(".env.")
            })
        })
}
fn sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "secret",
        "token",
        "password",
        "passwd",
        "pass",
        "credential",
    ]
    .iter()
    .any(|s| key.contains(s))
        || key
            .strip_suffix("_pwd")
            .is_some_and(|prefix| !prefix.is_empty())
        || key.ends_with("key")
        || key.split(['_', '-', '.']).any(|s| s == "key")
}

/// These flags have a credential argument regardless of the executable. Short
/// flags are deliberately scoped below: e.g. grep -a and ls -p are harmless.
fn secret_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--password"
            | "--passwd"
            | "--pass"
            | "--token"
            | "--api-key"
            | "--apikey"
            | "--secret"
            | "--client-secret"
            | "--access-token"
    )
}
struct ShellWord {
    range: std::ops::Range<usize>,
    value: zeroize::Zeroizing<String>,
    boundary: bool,
}
/// Small, non-executing shell lexer. Retain source spans, consume whole quoted
/// or backslash-escaped values, and reset command scope at shell separators.
fn shell_words(line: &str) -> Vec<ShellWord> {
    let mut words = Vec::new();
    let mut i = 0;
    while i < line.len() {
        let c = line[i..].chars().next().unwrap();
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let start = i;
        if matches!(c, ';' | '|' | '&' | '(' | ')') {
            i += 1;
            words.push(ShellWord {
                range: start..i,
                value: zeroize::Zeroizing::new(c.to_string()),
                boundary: true,
            });
            continue;
        }
        let mut value = zeroize::Zeroizing::new(String::new());
        let mut quote = None;
        while i < line.len() {
            let c = line[i..].chars().next().unwrap();
            if quote.is_none() && (c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')')) {
                break;
            }
            if line[i..].starts_with("[已去除:")
                && let Some(end) = line[i..].find(']')
            {
                value.push_str(&line[i..i + end + 1]);
                i += end + 1;
                continue;
            }
            if c == '\\' && quote != Some('\'') {
                i += 1;
                if i < line.len() {
                    let escaped = line[i..].chars().next().unwrap();
                    value.push(escaped);
                    i += escaped.len_utf8();
                }
                continue;
            }
            if matches!(c, '\'' | '"') {
                if quote == Some(c) {
                    quote = None;
                    i += 1;
                    continue;
                }
                if quote.is_none() {
                    quote = Some(c);
                    i += 1;
                    continue;
                }
            }
            value.push(c);
            i += c.len_utf8();
        }
        words.push(ShellWord {
            range: start..i,
            value,
            boundary: false,
        });
    }
    words
}
fn redact_arguments(line: &str, out: &mut Redacted) -> String {
    let words = shell_words(line);
    let mut replacements = Vec::new();
    let mut program = None;
    let mut i = 0;
    while i < words.len() {
        let word = &words[i];
        let value = word.value.as_str();
        if word.boundary {
            program = None;
            i += 1;
            continue;
        }
        if program.is_none() && !value.starts_with('-') {
            // Environment prefixes, common shell prompts and simple wrappers
            // precede the executable rather than changing its flag meanings.
            if value.contains('=')
                || matches!(
                    value,
                    "$" | "%" | "#" | "❯" | "➜" | "sudo" | "env" | "command" | "exec"
                )
                || value.ends_with(['$', '#', '%'])
            {
                i += 1;
                continue;
            }
            program = Some(value.rsplit('/').next().unwrap_or(value));
        }
        let (flag, attached) = value
            .split_once('=')
            .map_or((value, None), |(flag, v)| (flag, Some(v)));
        let mysql = matches!(
            program,
            Some("mysql" | "mysqldump" | "mariadb" | "mysqladmin")
        );
        let curl = program == Some("curl");
        let user_flag = curl && matches!(flag, "-u" | "--user");
        let takes_value = secret_flag(flag)
            || (program == Some("redis-cli") && flag == "-a")
            || (program == Some("sshpass") && flag == "-p")
            || user_flag;
        if takes_value {
            if let Some(attached) = attached {
                if let Some(replacement) = argument_replacement(attached, user_flag, out) {
                    replacements.push((word.range.clone(), format!("{flag}={replacement}")));
                }
            } else if let Some(next) = words.get(i + 1).filter(|w| !w.boundary) {
                if let Some(replacement) = argument_replacement(&next.value, user_flag, out) {
                    replacements.push((next.range.clone(), replacement));
                }
                i += 1;
            }
        } else if (mysql || program == Some("sshpass"))
            && value.starts_with("-p")
            && value.len() > 2
        {
            replacements.push((
                word.range.clone(),
                format!("-p{}", out.mark(Rule::CommandArgument)),
            ));
        } else if program == Some("redis-cli") && value.starts_with("-a") && value.len() > 2 {
            replacements.push((
                word.range.clone(),
                format!("-a{}", out.mark(Rule::CommandArgument)),
            ));
        } else if curl
            && value.starts_with("-u")
            && value.len() > 2
            && let Some(replacement) = argument_replacement(&value[2..], true, out)
        {
            replacements.push((word.range.clone(), format!("-u{replacement}")));
        }
        i += 1;
    }
    let mut result = String::new();
    let mut cursor = 0;
    for (range, replacement) in replacements {
        result.push_str(&line[cursor..range.start]);
        result.push_str(&replacement);
        cursor = range.end;
    }
    result.push_str(&line[cursor..]);
    result
}
fn argument_replacement(value: &str, keep_user: bool, out: &mut Redacted) -> Option<String> {
    if keep_user {
        let (user, password) = value.split_once(':')?;
        if password.is_empty() {
            return None;
        }
        Some(format!("{user}:{}", out.mark(Rule::CommandArgument)))
    } else if value.is_empty() {
        None
    } else {
        Some(out.mark(Rule::CommandArgument))
    }
}

fn redact_line(line: &str, out: &mut Redacted) -> String {
    let command_line = redact_arguments(line, out);
    let line = command_line.as_str();
    let lower = line.to_ascii_lowercase();
    if let Some(start) = lower.find("authorization")
        && let Some(colon) = line[start..].find(':')
    {
        let value = start + colon + 1;
        let tail = line[value..].trim_start().trim_start_matches(['"', '\'']);
        if tail.to_ascii_lowercase().starts_with("bearer ") {
            let prefix = value + line[value..].len() - tail.len() + 7;
            let end = line[prefix..]
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ','))
                .map_or(line.len(), |i| prefix + i);
            return format!(
                "{}{}{}",
                &line[..prefix],
                out.mark(Rule::Authorization),
                redact_line(&line[end..], out)
            );
        }
    }
    if lower.contains("aws secret access key")
        && let Some(colon) = line.rfind(':')
    {
        return format!("{} {}", &line[..=colon], out.mark(Rule::Aws));
    }
    // Resolve pairs before standalone tokens so the same value is not counted twice.
    let mut pairs = String::new();
    let mut cursor = 0;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !matches!(bytes[i], b'=' | b':') {
            i += 1;
            continue;
        }
        let mut end = i;
        while end > 0 && bytes[end - 1].is_ascii_whitespace() {
            end -= 1
        }
        if end > 0 && matches!(bytes[end - 1], b'"' | b'\'') {
            end -= 1
        }
        let mut start = end;
        while start > 0
            && (bytes[start - 1].is_ascii_alphanumeric()
                || matches!(bytes[start - 1], b'_' | b'-' | b'.'))
        {
            start -= 1
        }
        if start == end || !sensitive(&line[start..end]) {
            i += 1;
            continue;
        }
        let mut begin = i + 1;
        while begin < bytes.len() && bytes[begin].is_ascii_whitespace() {
            begin += 1
        }
        if begin == bytes.len() {
            i += 1;
            continue;
        }
        // The CLI pass has already removed these values. Do not recount them
        // as pairs, but still redact literal --password keys in quoted text/JSON.
        const COMMAND_MARKER: &str = "[已去除:命令行凭据]";
        if bytes[i] == b'='
            && secret_flag(&line[start..end])
            && line[begin..].starts_with(COMMAND_MARKER)
        {
            let after = begin + COMMAND_MARKER.len();
            if after == line.len()
                || bytes[after].is_ascii_whitespace()
                || matches!(bytes[after], b',' | b';' | b'}')
            {
                i = after;
                continue;
            }
        }
        let quoted = matches!(bytes[begin], b'"' | b'\'');
        let quote = bytes[begin];
        let value_start = begin + usize::from(quoted);
        let mut finish = value_start;
        if quoted {
            while finish < bytes.len() {
                if bytes[finish] == b'\\' {
                    finish = (finish + 2).min(bytes.len());
                    continue;
                }
                if bytes[finish] == quote {
                    break;
                }
                finish += 1
            }
        } else {
            while finish < bytes.len()
                && !matches!(bytes[finish], b',' | b';' | b'}')
                && !(bytes[i] == b'=' && bytes[finish].is_ascii_whitespace())
            {
                finish += 1
            }
        }
        if finish > value_start {
            pairs.push_str(&line[cursor..value_start]);
            let rule = if lower[start..end].contains("aws") {
                Rule::Aws
            } else {
                Rule::CredentialPair
            };
            pairs.push_str(&out.mark(rule));
            cursor = finish;
            i = finish + usize::from(quoted && finish < bytes.len());
        } else {
            i += 1
        }
    }
    pairs.push_str(&line[cursor..]);
    let mut urls = String::new();
    let mut rest = pairs.as_str();
    while let Some(scheme) = rest.find("://") {
        let begin = scheme + 3;
        urls.push_str(&rest[..begin]);
        rest = &rest[begin..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '/' | '"' | '\'' | '<' | '>'))
            .unwrap_or(rest.len());
        if let Some(at) = rest[..end].rfind('@')
            && rest[..at].contains(':')
        {
            urls.push_str(&out.mark(Rule::UrlCredentials));
            urls.push('@');
            rest = &rest[at + 1..];
        } else {
            urls.push_str(&rest[..end]);
            rest = &rest[end..];
        }
    }
    urls.push_str(rest);
    let mut result = String::new();
    let mut cursor = 0;
    for (start, _) in urls.match_indices(|c: char| c.is_ascii_alphanumeric()) {
        if start < cursor || start > 0 && is_token(urls.as_bytes()[start - 1]) {
            continue;
        }
        let end = urls.as_bytes()[start..]
            .iter()
            .position(|b| !is_token(*b))
            .map_or(urls.len(), |n| start + n);
        let token = &urls[start..end];
        if let Some(rule) = token_rule(token) {
            result.push_str(&urls[cursor..start]);
            result.push_str(&out.mark(rule));
            cursor = end;
        }
    }
    result.push_str(&urls[cursor..]);
    result
}
fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}
fn token_rule(s: &str) -> Option<Rule> {
    if (s.starts_with("AKIA") || s.starts_with("ASIA")) && s.len() >= 20 {
        return Some(Rule::Aws);
    }
    if ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"]
        .iter()
        .any(|p| s.starts_with(p))
        && s.len() >= 12
    {
        return Some(Rule::GitHub);
    }
    if s.starts_with("xox") && s.as_bytes().get(4) == Some(&b'-') && s.len() >= 12 {
        return Some(Rule::Slack);
    }
    if s.starts_with("sk-") && s.len() >= 12 {
        return Some(Rule::ApiKey);
    }
    if s.starts_with("AIza") && s.len() >= 20 {
        return Some(Rule::Google);
    }
    let parts = s.trim_end_matches('.').split('.').collect::<Vec<_>>();
    if s.starts_with("eyJ")
        && parts.len() == 3
        && parts[0].len() >= 4
        && parts[1].len() >= 2
        && parts[2].len() >= 8
    {
        return Some(Rule::Jwt);
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rule_table() {
        for (input, rule, secret) in [
            (
                "-----BEGIN OPENSSH PRIVATE KEY-----\nTOPSECRET\n-----END OPENSSH PRIVATE KEY-----",
                Rule::PrivateKey,
                "TOPSECRET",
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\ntruncated-secret",
                Rule::PrivateKey,
                "truncated-secret",
            ),
            (
                "MYSQL_PWD=abc123 mysql -u root",
                Rule::CredentialPair,
                "abc123",
            ),
            (
                "redis-cli -a S3cretPass ping",
                Rule::CommandArgument,
                "S3cretPass",
            ),
            ("mysql -uroot -pMyP4ss db", Rule::CommandArgument, "MyP4ss"),
            ("mysqldump -pMyP4ss db", Rule::CommandArgument, "MyP4ss"),
            ("mariadb -pMyP4ss db", Rule::CommandArgument, "MyP4ss"),
            (
                "mysqladmin -pMyP4ss status",
                Rule::CommandArgument,
                "MyP4ss",
            ),
            (
                "sshpass -p 'hunter2' ssh root@host",
                Rule::CommandArgument,
                "hunter2",
            ),
            (
                "curl -u user:pass https://example.test",
                Rule::CommandArgument,
                ":pass",
            ),
            (
                "curl --user user:pass https://example.test",
                Rule::CommandArgument,
                ":pass",
            ),
            (
                "export API_KEY='pair-secret'",
                Rule::CredentialPair,
                "pair-secret",
            ),
            (
                "{\"Password\": \"json secret\"}",
                Rule::CredentialPair,
                "json secret",
            ),
            (
                "accessKey: yaml long secret",
                Rule::CredentialPair,
                "yaml long secret",
            ),
            (
                "AWS_SECRET_ACCESS_KEY=awsSecretValue",
                Rule::Aws,
                "awsSecretValue",
            ),
            (
                "AWS Secret Access Key [None]: hidden-value",
                Rule::Aws,
                "hidden-value",
            ),
            ("AKIAABCDEFGHIJKLMNOP", Rule::Aws, "AKIAABCDEFGHIJKLMNOP"),
            ("ghp_0123456789abcdef", Rule::GitHub, "ghp_0123456789abcdef"),
            (
                "github_pat_0123456789abcdef",
                Rule::GitHub,
                "github_pat_0123456789abcdef",
            ),
            (
                "xoxb-123456789-secret",
                Rule::Slack,
                "xoxb-123456789-secret",
            ),
            ("sk-123456789abcdef", Rule::ApiKey, "sk-123456789abcdef"),
            (
                "sk-ant-123456789abcdef",
                Rule::ApiKey,
                "sk-ant-123456789abcdef",
            ),
            (
                "AIza0123456789abcdefghijklmnopqrstuvw",
                Rule::Google,
                "AIza0123456789abcdefghijklmnopqrstuvw",
            ),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VyIjoxfQ.signature123",
                Rule::Jwt,
                "signature123",
            ),
            (
                "https://alice:secret@host.test/a",
                Rule::UrlCredentials,
                "alice:secret",
            ),
            (
                "Authorization: Bearer sensitive-token",
                Rule::Authorization,
                "sensitive-token",
            ),
        ] {
            let r = redact(input);
            assert!(!r.text.contains(secret), "rule {rule:?}");
            assert_eq!(r.counts.get(&rule), Some(&1), "{input}");
        }
    }
    #[test]
    fn sensitive_long_flags_remove_whole_values_in_every_form() {
        for flag in [
            "--password",
            "--passwd",
            "--pass",
            "--token",
            "--api-key",
            "--apikey",
            "--secret",
            "--client-secret",
            "--access-token",
        ] {
            for value in [
                "S3cretPass",
                "'S3cret Pass'",
                "\"S3cret Pass\"",
                "\"秘密 ; trailing value\"",
                "'first'\"second\"",
                "escaped\\ value",
            ] {
                for separator in [" ", "="] {
                    let input = format!("tool {flag}{separator}{value} --verbose");
                    let expected = format!("tool {flag}{separator}[已去除:命令行凭据] --verbose");
                    let redacted = redact(&input);
                    assert_eq!(redacted.text, expected, "{input}");
                    assert_eq!(redacted.count(), 1, "one whole argument");
                    assert_eq!(redact(&redacted.text).text, redacted.text, "idempotent");
                }
            }
            assert_eq!(
                redact(&format!("{flag}=value")).text,
                format!("{flag}=[已去除:命令行凭据]")
            );
            assert_eq!(
                redact(&format!("tool '{flag}=secret text' --verbose")).text,
                format!("tool {flag}=[已去除:命令行凭据] --verbose")
            );
        }
    }
    #[test]
    fn command_scopes_quotes_and_curl_usernames() {
        for (input, expected) in [
            (
                "redis-cli -a 'S3cret Pass' ping",
                "redis-cli -a [已去除:命令行凭据] ping",
            ),
            (
                "redis-cli -a \"S3cret Pass\" ping",
                "redis-cli -a [已去除:命令行凭据] ping",
            ),
            (
                "mysql -uroot -p'My P4ss' db",
                "mysql -uroot -p[已去除:命令行凭据] db",
            ),
            (
                r#"mysql -p"My \"quoted\" P4ss" db"#,
                "mysql -p[已去除:命令行凭据] db",
            ),
            (
                "sshpass -p 'hunter2' ssh root@host",
                "sshpass -p [已去除:命令行凭据] ssh root@host",
            ),
            (
                "sshpass -p \"hunter 2\" ssh root@host",
                "sshpass -p [已去除:命令行凭据] ssh root@host",
            ),
            (
                "curl -u 'user:pass word' https://example.test",
                "curl -u user:[已去除:命令行凭据] https://example.test",
            ),
            (
                "curl --user=\"user:pass:word\" https://example.test",
                "curl --user=user:[已去除:命令行凭据] https://example.test",
            ),
            (
                "curl -uuser:password123 https://example.test",
                "curl -uuser:[已去除:命令行凭据] https://example.test",
            ),
            (
                "me@host:~$ sudo /usr/bin/mysql -psecret db",
                "me@host:~$ sudo /usr/bin/mysql -p[已去除:命令行凭据] db",
            ),
            (
                "redis-cli -a secret ping; grep -a pattern | mysql -pprivate db",
                "redis-cli -a [已去除:命令行凭据] ping; grep -a pattern | mysql -p[已去除:命令行凭据] db",
            ),
        ] {
            let redacted = redact(input);
            assert_eq!(redacted.text, expected, "{input}");
            assert_eq!(
                redact(&redacted.text).text,
                redacted.text,
                "idempotent: {input}"
            );
        }
    }
    #[test]
    fn quoted_long_option_assignments_still_use_pair_redaction() {
        for text in [
            r#"echo 'curl --token=secret-value'"#,
            r#"{"--password":"secret-value"}"#,
        ] {
            let redacted = redact(text);
            assert!(!redacted.text.contains("secret-value"));
            assert_eq!(redact(&redacted.text).text, redacted.text);
        }
    }
    #[test]
    fn private_key_block_is_exactly_one_marker_and_retains_surroundings() {
        for kind in ["OPENSSH", "RSA", "EC", "ENCRYPTED", ""] {
            let label = if kind.is_empty() {
                "PRIVATE KEY".into()
            } else {
                format!("{kind} PRIVATE KEY")
            };
            for newline in ["\n", "\r\n"] {
                let block =
                    format!("-----BEGIN {label}-----{newline}b3Bl…{newline}-----END {label}-----");
                let r = redact(&block);
                assert_eq!(r.text, "[已去除:私钥]");
                assert_eq!(r.count(), 1);
                assert_eq!(
                    redact(&format!("before{newline}{block}{newline}after")).text,
                    format!("before{newline}[已去除:私钥]{newline}after")
                );
                assert_eq!(redact(&format!("{block}after")).text, "[已去除:私钥]after");
            }
        }
        let block = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl…\n-----END OPENSSH PRIVATE KEY-----";
        assert_eq!(
            redact(&format!("{block}\n{block}")).text,
            "[已去除:私钥]\n[已去除:私钥]"
        );
        assert_eq!(
            redact("-----BEGIN OPENSSH PRIVATE KEY-----\nunfinished").text,
            "[已去除:私钥]"
        );
        let source = format!(
            "older-not-in-snapshot\n-----BEGIN PRIVATE KEY-----\n{}-----END PRIVATE KEY-----\nafter",
            "hidden\n".repeat(80)
        );
        assert_eq!(recent_context(&source, 40).text, "[已去除:私钥]\nafter");
    }
    #[test]
    fn ordinary_text_is_untouched() {
        for s in [
            "ls -la",
            "PWD=/home/u",
            "OLDPWD=/home/previous",
            "_PWD=ordinary",
            "ls -p",
            "grep -a pattern",
            "mysql -p",
            "mysql -p db",
            "mysqldump -p db",
            "mariadb -p db",
            "mysqladmin -p status",
            "echo mysql -pordinary-text",
            "curl --user user-without-password https://example.test",
            "/Users/demo/project/src/main.rs",
            "KEYBOARD=us",
            "MONITOR=ok",
            "https://example.test/path",
            "public key documentation",
            "ssh user@host",
        ] {
            assert_eq!(redact(s).text, s);
            assert_eq!(redact(s).count(), 0)
        }
    }
    #[test]
    fn env_until_prompt() {
        let r = redact(
            "me@host:~$ cat .env\nNAME=private-name\n# comment\nOTHER=hidden\nme@host:~$ ls -la\nvisible",
        );
        assert!(!r.text.contains("private-name"));
        assert!(!r.text.contains("hidden"));
        assert!(r.text.contains("ls -la\nvisible"));
        assert_eq!(r.counts.get(&Rule::Environment), Some(&1));
        for command in ["$ cat *.env", "$ cat /tmp/.env.production"] {
            assert!(
                !redact(&format!("{command}\nhello-secret"))
                    .text
                    .contains("hello-secret")
            );
        }
    }
    #[test]
    fn multiple_pairs_and_unicode() {
        let r = redact("中文 TOKEN=abc PASS=def KEYBOARD=us");
        assert_eq!(r.count(), 2);
        assert!(r.text.ends_with("KEYBOARD=us"));
        assert_eq!(redact(&r.text).text, r.text);
    }
}

#[cfg(test)]
mod boundaries {
    use super::*;
    #[test]
    fn env_above_snapshot_and_private_key_continuations_do_not_leak() {
        let text = format!(
            "$ cat .env\n{}",
            (0..80)
                .map(|i| format!("NORMAL_NAME_{i}=unrecognized-private-data-{i}\n"))
                .collect::<String>()
        );
        let r = recent_context(&text, 40);
        assert!(!r.text.contains("unrecognized-private-data"));
        assert!(r.text.lines().count() <= 40);
        assert!(r.count() > 0);
        let key = format!(
            "-----BEGIN PRIVATE KEY-----\n{}",
            "unrecognized-key-material\n".repeat(80)
        );
        assert!(
            !recent_context(&key, 40)
                .text
                .contains("unrecognized-key-material")
        );
    }
    #[test]
    fn env_comments_are_not_shell_prompts() {
        let text = "$ cat .env\n# ordinary comment\nrandom # comment\nsecret unlabelled text\nme@host:~$ echo done\ndone";
        let r = redact(text);
        assert!(!r.text.contains("secret unlabelled text"));
        assert!(r.text.ends_with("done"));
    }
    #[test]
    fn overlapping_and_quoted_values_are_safe() {
        for text in [
            "TOKEN=\"abc def\"",
            "secretKey: 'escaped \\' value'",
            "{\"apiKey\":\"中文值\",\"KEYBOARD\":\"us\"}",
            "pass = abc",
            "token: value with spaces",
        ] {
            assert!(redact(text).count() > 0);
            assert_eq!(redact(&redact(text).text).text, redact(text).text);
        }
    }
}
