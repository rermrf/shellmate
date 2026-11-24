//! Conservative, non-executing POSIX-shell policy. Unknown syntax/flags never auto-approve.
use rig_core::serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub enum Class {
    ReadOnly,
    Other,
    RedLine,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub enum PermissionMode {
    #[default]
    Ask,
    Auto,
    Full,
}
impl PermissionMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Full => "full",
        }
    }
    pub fn from_id(id: &str) -> Self {
        match id {
            "auto" => Self::Auto,
            "full" => Self::Full,
            _ => Self::Ask,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "每步审批",
            Self::Auto => "自动审批",
            Self::Full => "完全访问",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Run,
    Ask,
    AskRedLine,
}
pub fn decide(mode: PermissionMode, class: Class) -> Decision {
    match (mode, class) {
        (_, Class::RedLine) => Decision::AskRedLine,
        (PermissionMode::Full, _) | (PermissionMode::Auto, Class::ReadOnly) => Decision::Run,
        _ => Decision::Ask,
    }
}
#[derive(Debug)]
enum Token {
    Word(String),
    Sep,
    Chain,
    Redirect(String),
}
#[derive(Default)]
struct Lexed {
    tokens: Vec<Token>,
    ambiguous: bool,
    substitutions: Vec<String>,
}

fn lex(s: &str) -> Lexed {
    let c: Vec<char> = s.chars().collect();
    let mut out = Lexed::default();
    let (mut i, mut word, mut active, mut quote) = (0, String::new(), false, None);
    fn flush(out: &mut Lexed, word: &mut String, active: &mut bool) {
        if *active {
            out.tokens.push(Token::Word(std::mem::take(word)));
            *active = false;
        }
    }
    while i < c.len() {
        let ch = c[i];
        if ch == '\\' && quote != Some('\'') {
            active = true;
            if i + 1 < c.len() {
                i += 1;
                // In double quotes, only these escapes have shell meaning.
                if quote == Some('"') && !matches!(c[i], '$' | '`' | '"' | '\\' | '\n') {
                    word.push('\\');
                }
                if c[i] != '\n' {
                    word.push(c[i]);
                }
            } else {
                out.ambiguous = true;
            }
        } else if Some(ch) == quote {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
            active = true;
        } else if quote != Some('\'') && (ch == '`' || (ch == '$' && c.get(i + 1) == Some(&'('))) {
            out.ambiguous = true;
            active = true;
            let backtick = ch == '`';
            let start = i + if backtick { 1 } else { 2 };
            let (mut j, mut depth, mut inner_quote) = (start, 1, None);
            while j < c.len() {
                let x = c[j];
                if x == '\\' {
                    j += 2;
                    continue;
                }
                if Some(x) == inner_quote {
                    inner_quote = None;
                } else if inner_quote.is_none() && matches!(x, '\'' | '"') {
                    inner_quote = Some(x);
                } else if inner_quote.is_none() {
                    if backtick && x == '`' {
                        break;
                    }
                    if !backtick {
                        if x == '(' {
                            depth += 1;
                        }
                        if x == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                }
                j += 1;
            }
            let end = j.min(c.len());
            out.substitutions.push(c[start..end].iter().collect());
            word.extend(c[i..(end + 1).min(c.len())].iter());
            i = end;
        } else if ch == '$' && quote != Some('\'') && c.get(i + 1) == Some(&'{') {
            active = true;
            let start = i;
            i += 2;
            while i < c.len() && c[i] != '}' {
                i += 1;
            }
            let end = (i + 1).min(c.len());
            let parameter: String = c[start..end].iter().collect();
            if !plain_expansions(&parameter) {
                out.ambiguous = true;
                // Operators may contain command substitutions of their own.
                // Preserve those for the red-line pass even though this
                // expansion can never make a command read-only.
                out.substitutions
                    .push(parameter[2..].trim_end_matches('}').to_owned());
            }
            word.push_str(&parameter);
        } else if quote.is_some() {
            out.ambiguous |= quote == Some('\'') && ch == '$' && c.get(i + 1) == Some(&'{');
            word.push(ch);
            active = true;
        } else if ch == '#' && !active {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            if i < c.len() {
                out.tokens.push(Token::Sep);
            }
        } else if ch == '\n' || matches!(ch, ';' | '|' | '&') {
            flush(&mut out, &mut word, &mut active);
            if ch == '&' && c.get(i + 1) != Some(&'&') {
                out.ambiguous = true;
            }
            if ch == ';' && (out.tokens.is_empty() || c.get(i + 1) == Some(&';')) {
                out.ambiguous = true;
            }
            if c.get(i + 1) == Some(&ch) {
                i += 1;
            }
            out.tokens.push(if matches!(ch, '|' | '&') {
                Token::Chain
            } else {
                Token::Sep
            });
        } else if matches!(ch, '>' | '<') {
            let fd = if active && word.chars().all(|x| x.is_ascii_digit()) {
                active = false;
                std::mem::take(&mut word)
            } else {
                flush(&mut out, &mut word, &mut active);
                String::new()
            };
            let mut op = format!("{fd}{ch}");
            while c
                .get(i + 1)
                .is_some_and(|x| matches!(x, '>' | '<' | '&' | '|'))
            {
                i += 1;
                op.push(c[i]);
            }
            out.tokens.push(Token::Redirect(op));
        } else if ch.is_whitespace() {
            flush(&mut out, &mut word, &mut active);
        } else if matches!(ch, '(' | ')' | '{' | '}') {
            out.ambiguous = true;
            flush(&mut out, &mut word, &mut active);
            out.tokens.push(Token::Sep);
        } else {
            word.push(ch);
            active = true;
        }
        i += 1;
    }
    out.ambiguous |= quote.is_some();
    flush(&mut out, &mut word, &mut active);
    out
}
pub fn classify(command: &str) -> Class {
    classify_inner(command, 0)
}
fn classify_inner(command: &str, depth: usize) -> Class {
    if depth > 16 {
        return Class::RedLine;
    }
    if command
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .contains(":(){:|:&};:")
    {
        return Class::RedLine;
    }
    let parsed = lex(command);
    if parsed
        .substitutions
        .iter()
        .any(|s| classify_inner(s, depth + 1) == Class::RedLine)
    {
        return Class::RedLine;
    }
    let mut safe = !parsed.ambiguous;
    let mut words = Vec::new();
    let mut any = false;
    let mut need_command = false;
    let mut tokens = parsed.tokens.into_iter();
    loop {
        match tokens.next() {
            Some(Token::Word(w)) => {
                words.push(w);
                need_command = false;
            }
            Some(Token::Redirect(op)) => {
                if let Some(Token::Word(path)) = tokens.next() {
                    if op.contains('>') && disk_path(&path) {
                        return Class::RedLine;
                    }
                    safe &= matches!(
                        (op.as_str(), path.as_str()),
                        (">" | "1>" | "2>", "/dev/null") | ("2>&", "1")
                    );
                } else {
                    safe = false;
                }
            }
            token => {
                if need_command && matches!(token, Some(Token::Sep)) {
                    safe = false;
                }
                if matches!(token, Some(Token::Chain)) {
                    safe &= !words.is_empty();
                    need_command = true;
                }
                if !words.is_empty() {
                    any = true;
                    if red_line(&words, depth) {
                        return Class::RedLine;
                    }
                    safe &= read_only(&words, depth);
                    words.clear();
                }
                if token.is_none() {
                    break;
                }
            }
        }
    }
    if safe && any && !need_command {
        Class::ReadOnly
    } else {
        Class::Other
    }
}
fn program(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}
fn short(args: &[String], flag: char) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a.starts_with('-') && !a.starts_with("--") && a[1..].contains(flag))
}
fn has(args: &[String], s: &str) -> bool {
    args.iter().any(|a| a == s)
}
fn system_path(s: &str) -> bool {
    let s = s.trim_end_matches('/');
    let s = s
        .strip_suffix("/*")
        .filter(|prefix| !prefix.is_empty())
        .unwrap_or(s);
    if matches!(s, "" | "." | ".." | "*" | "~" | "$HOME" | "${HOME}" | "/*") {
        return true;
    }
    let mut parts = Vec::new();
    for p in s.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(p),
        }
    }
    s.starts_with('/')
        && (parts.is_empty()
            || (parts.len() == 1
                && (matches!(
                    parts[0],
                    "etc"
                        | "usr"
                        | "var"
                        | "boot"
                        | "bin"
                        | "sbin"
                        | "home"
                        | "root"
                        | "opt"
                        | "System"
                        | "Users"
                ) || parts[0].starts_with("lib"))))
}
fn disk_path(s: &str) -> bool {
    s.strip_prefix("/dev/").is_some_and(|s| {
        ["sd", "nvme", "disk", "rdisk"]
            .iter()
            .any(|p| s.starts_with(p))
    })
}
// Locate the payload without mistaking a wrapper's option values for commands.
fn option_end(a: &[String], short_values: &str, long_values: &[&str]) -> usize {
    let mut i = 0;
    while let Some(arg) = a.get(i) {
        if arg == "--" {
            return i + 1;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        i += 1;
        if arg.starts_with("--") {
            if !arg.contains('=') && long_values.contains(&arg.as_str()) {
                i = (i + 1).min(a.len());
            }
        } else {
            for (offset, flag) in arg[1..].char_indices() {
                if short_values.contains(flag) {
                    if offset + flag.len_utf8() == arg.len() - 1 {
                        i = (i + 1).min(a.len());
                    }
                    break;
                }
            }
        }
    }
    i
}
fn wrapped_command<'a>(p: &str, a: &'a [String]) -> Option<&'a [String]> {
    let (short_values, long_values): (&str, &[&str]) = match p {
        "timeout" => ("sk", &["--signal", "--kill-after"]),
        "nice" => ("n", &["--adjustment"]),
        "ionice" => (
            "cnpPuU",
            &["--class", "--classdata", "--pid", "--pgid", "--uid"],
        ),
        "stdbuf" => ("ioe", &["--input", "--output", "--error"]),
        "time" => ("fo", &["--format", "--output"]),
        "watch" => ("n", &["--interval"]),
        "chroot" => ("", &["--userspec", "--groups"]),
        "sudo" => (
            "ugphCrDtUTR",
            &[
                "--user",
                "--group",
                "--prompt",
                "--host",
                "--close-from",
                "--chdir",
                "--chroot",
                "--role",
                "--type",
                "--command-timeout",
            ],
        ),
        "doas" => ("uC", &[]),
        "pkexec" => ("", &["--user"]),
        "runuser" | "su" => (
            "ugGs",
            &[
                "--user",
                "--group",
                "--supp-group",
                "--shell",
                "--whitelist-environment",
            ],
        ),
        "ssh" => ("BbcDEeFIiJLlmOopQRSWw", &[]),
        "env" => ("uC", &["--unset", "--chdir"]),
        "xargs" => (
            "aEnLsPId",
            &[
                "--arg-file",
                "--eof",
                "--max-lines",
                "--max-args",
                "--max-procs",
                "--replace",
                "--delimiter",
            ],
        ),
        "exec" => ("a", &[]),
        "command" | "builtin" | "nohup" => ("", &[]),
        _ => return None,
    };
    let mut i = option_end(a, short_values, long_values);
    if matches!(p, "timeout" | "chroot" | "ssh") {
        i = (i + 1).min(a.len()); // duration, root directory, or remote host
    }
    if p == "env" {
        while a.get(i).is_some_and(|s| s.contains('=')) {
            i += 1;
        }
    }
    Some(&a[i..])
}
fn shell(p: &str) -> bool {
    matches!(p, "sh" | "bash" | "zsh" | "dash" | "ksh")
}
fn inline_code<'a>(p: &str, a: &'a [String]) -> Vec<&'a str> {
    let flag = match p {
        "perl" | "ruby" | "node" | "nodejs" | "lua" | "osascript" => 'e',
        "php" => 'r',
        p if p.starts_with("python") || shell(p) || matches!(p, "su" | "runuser") => 'c',
        // awk programs need not be introduced by a flag. Scanning all arguments
        // here also catches programs supplied with -e/--source.
        "awk" | "gawk" | "mawk" | "nawk" => return a.iter().map(String::as_str).collect(),
        _ => return Vec::new(),
    };
    let mut code = Vec::new();
    let mut i = 0;
    while i < a.len() {
        let arg = &a[i];
        if arg == "--" {
            break;
        }
        if matches!(arg.as_str(), "--command" | "--eval") {
            if let Some(value) = a.get(i + 1) {
                code.push(value.as_str());
                i += 1;
            }
        } else if let Some(value) = arg
            .strip_prefix("--command=")
            .or_else(|| arg.strip_prefix("--eval="))
        {
            code.push(value);
        } else if arg.starts_with('-')
            && !arg.starts_with("--")
            && let Some(offset) = arg[1..].find(flag)
        {
            let value = &arg[offset + 2..];
            // Shell -lc/-ce flags are combined flags, not attached code.
            if value.is_empty() || shell(p) || matches!(p, "su" | "runuser") {
                if let Some(value) = a.get(i + 1) {
                    code.push(value.as_str());
                    i += 1;
                }
            } else {
                code.push(value);
            }
        }
        i += 1;
    }
    code
}
fn code_red_line(code: &str, depth: usize) -> bool {
    if code
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| {
            matches!(
                word,
                "reboot" | "shutdown" | "poweroff" | "halt" | "mkfs" | "wipefs"
            )
        })
    {
        return true;
    }
    if classify_inner(code, depth + 1) == Class::RedLine {
        return true;
    }
    // Inline languages can hide shell commands in string literals, function
    // calls, or argv arrays. This is a conservative scan, not an interpreter.
    let normalized: String = code
        .chars()
        .map(|c| {
            if matches!(
                c,
                '\'' | '"' | '`' | '(' | ')' | '{' | '}' | '[' | ']' | ',' | ';' | '+' | '\\'
            ) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let words: Vec<String> = normalized.split_whitespace().map(str::to_owned).collect();
    (0..words.len()).any(|i| red_line(&words[i..], depth + 1))
        || classify_inner(&normalized, depth + 1) == Class::RedLine
}

fn plain_expansions(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '`' {
            return false;
        }
        if c != '$' {
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        if !chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            return false;
        }
        while chars
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            chars.next();
        }
        if braced && chars.next() != Some('}') {
            return false;
        }
    }
    true
}
fn sensitive_path(s: &str) -> bool {
    // Match lexical paths, never the local machine's HOME or filesystem.
    let s = s
        .strip_prefix("--")
        .and_then(|s| s.split_once('=').map(|(_, v)| v))
        .unwrap_or(s);
    let mut parts = Vec::new();
    for part in s.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let Some(base) = parts.last().copied() else {
        return false;
    };
    if let Some(ssh) = parts.iter().position(|s| *s == ".ssh")
        && (ssh + 2 != parts.len()
            || !(base.ends_with(".pub")
                || matches!(base, "known_hosts" | "authorized_keys" | "config")))
    {
        return true;
    }
    matches!(
        base,
        "id_rsa"
            | "id_ed25519"
            | "id_ecdsa"
            | "id_dsa"
            | ".env"
            | ".netrc"
            | ".pgpass"
            | ".git-credentials"
            | ".npmrc"
            | ".pypirc"
    ) || base.starts_with(".env.")
        || base.ends_with("_history")
        || [".pem", ".key", ".p12", ".pfx"]
            .iter()
            .any(|ext| base.ends_with(ext))
        || parts.ends_with(&["etc", "shadow"])
        || parts.ends_with(&["etc", "gshadow"])
        || parts.ends_with(&["etc", "security", "opasswd"])
        || parts.ends_with(&[".aws", "credentials"])
        || parts.ends_with(&[".kube", "config"])
        || parts.ends_with(&[".docker", "config.json"])
}
fn awk_read_only(a: &[String]) -> bool {
    let mut i = 0;
    while let Some(arg) = a.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        if !arg.starts_with('-') {
            break;
        }
        if matches!(arg.as_str(), "-F" | "-v") {
            i += 2;
        } else if arg.starts_with("-F") || arg.starts_with("-v") {
            i += 1;
        } else {
            return false;
        }
    }
    let Some(code) = a.get(i) else {
        return false;
    };
    // Bare identifiers are left conservative; accept recognizable patterns or actions.
    !code.is_empty()
        && !code.chars().all(|c| c.is_ascii_alphabetic() || c == '_')
        && !["system", "getline", "|", ">", "close", "fflush"]
            .iter()
            .any(|s| code.contains(s))
        && a[i + 1..]
            .iter()
            .all(|s| !s.starts_with('-') && plain_expansions(s))
}

// A small, closed sed grammar. Delimiters and escapes are consumed before
// looking for separators, so ';' inside a substitution is not a command.
fn sed_script(script: &str) -> bool {
    fn delimited(chars: &[char], i: &mut usize, delimiter: char) -> bool {
        while let Some(c) = chars.get(*i) {
            *i += 1;
            if *c == '\\' {
                if *i == chars.len() {
                    return false;
                }
                *i += 1;
            } else if *c == delimiter {
                return true;
            } else if *c == '\n' {
                return false;
            }
        }
        false
    }
    fn address(chars: &[char], i: &mut usize) -> Option<u8> {
        match chars.get(*i)? {
            '0'..='9' => {
                while chars.get(*i).is_some_and(char::is_ascii_digit) {
                    *i += 1;
                }
                Some(0)
            }
            '$' => {
                *i += 1;
                Some(1)
            }
            '/' => {
                *i += 1;
                delimited(chars, i, '/').then_some(2)
            }
            _ => None,
        }
    }
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;
    loop {
        while chars.get(i).is_some_and(|c| matches!(c, ' ' | '\t')) {
            i += 1;
        }
        let start = i;
        let addr = address(&chars, &mut i);
        if addr.is_none() && i != start {
            return false;
        }
        if chars.get(i) == Some(&',') {
            i += 1;
            if !matches!(
                (addr, address(&chars, &mut i)),
                (Some(0), Some(0)) | (Some(2), Some(2))
            ) {
                return false;
            }
        }
        match chars.get(i) {
            Some('p') => {
                i += 1;
            }
            Some('s') => {
                i += 1;
                let Some(&delimiter) = chars.get(i) else {
                    return false;
                };
                if delimiter.is_alphanumeric() || delimiter.is_whitespace() || delimiter == '\\' {
                    return false;
                }
                i += 1;
                if !delimited(&chars, &mut i, delimiter) || !delimited(&chars, &mut i, delimiter) {
                    return false;
                }
                while chars
                    .get(i)
                    .is_some_and(|c| matches!(c, 'g' | 'i' | 'I' | 'p') || c.is_ascii_digit())
                {
                    i += 1;
                }
            }
            _ => return false,
        }
        while chars.get(i).is_some_and(|c| matches!(c, ' ' | '\t')) {
            i += 1;
        }
        if i == chars.len() {
            return true;
        }
        if chars[i] != ';' {
            return false;
        }
        i += 1;
    }
}
fn sed_read_only(a: &[String]) -> bool {
    let mut scripts = Vec::new();
    let mut i = 0;
    let mut options = true;
    while let Some(arg) = a.get(i) {
        if options && arg == "--" {
            options = false;
        } else if options && arg.starts_with('-') && arg != "-" {
            for (offset, flag) in arg[1..].char_indices() {
                match flag {
                    'n' | 'E' | 'r' | 's' | 'u' | 'z' => {}
                    'e' => {
                        let attached = &arg[offset + 2..];
                        if attached.is_empty() {
                            i += 1;
                            let Some(script) = a.get(i) else {
                                return false;
                            };
                            scripts.push(script.as_str());
                        } else {
                            scripts.push(attached);
                        }
                        break;
                    }
                    _ => return false,
                }
            }
        } else if scripts.is_empty() {
            scripts.push(arg.as_str());
        }
        i += 1;
    }
    !scripts.is_empty() && scripts.into_iter().all(sed_script)
}

fn red_line(w: &[String], depth: usize) -> bool {
    if depth > 16 {
        return true;
    }
    let p = program(&w[0]);
    let a = &w[1..];
    if inline_code(p, a)
        .iter()
        .any(|code| code_red_line(code, depth))
    {
        return true;
    }
    if let Some(inner) = wrapped_command(p, a)
        && !inner.is_empty()
        && (red_line(inner, depth + 1)
            || (matches!(p, "ssh" | "su" | "runuser" | "watch")
                && classify_inner(&inner.join(" "), depth + 1) == Class::RedLine))
    {
        return true;
    }
    if matches!(p, "then" | "else" | "do" | "if" | "while" | "until" | "!")
        && (1..w.len()).any(|i| red_line(&w[i..], depth + 1))
    {
        return true;
    }
    if (p.contains('=') || p == "find") && (1..w.len()).any(|i| red_line(&w[i..], depth + 1)) {
        return true;
    }
    if p == "eval" && classify_inner(&a.join(" "), depth + 1) == Class::RedLine {
        return true;
    }
    match p {
        "rm" => {
            has(a, "--no-preserve-root")
                || (short(a, 'r')
                    || short(a, 'R')
                    || short(a, 'f')
                    || has(a, "--recursive")
                    || has(a, "--force"))
                    && (has(a, "--no-preserve-root")
                        || a.iter()
                            .filter(|s| !s.starts_with('-'))
                            .any(|s| system_path(s)))
        }
        p if p.starts_with("mkfs") => true,
        "wipefs" | "shutdown" | "reboot" | "poweroff" | "halt" | "userdel" | "nvram" => true,
        "dd" => a.iter().any(|s| s.starts_with("of=/dev/")),
        "fdisk" | "sfdisk" => !has(a, "-l") && !has(a, "--list"),
        "parted" => {
            !a.iter().any(|s| s == "print")
                || a.iter().any(|s| {
                    matches!(
                        s.as_str(),
                        "mklabel" | "mkpart" | "rm" | "resizepart" | "set" | "name"
                    )
                })
        }
        "diskutil" => a
            .iter()
            .any(|s| s.starts_with("erase") || matches!(s.as_str(), "partitionDisk" | "zeroDisk")),
        "init" => has(a, "0") || has(a, "6"),
        "systemctl" => a
            .iter()
            .any(|s| matches!(s.as_str(), "reboot" | "poweroff" | "halt" | "kexec")),
        "iptables" | "ip6tables" => {
            short(a, 'F')
                || short(a, 'X')
                || has(a, "--flush")
                || has(a, "--delete-chain")
                || ((short(a, 'P') || has(a, "--policy")) && has(a, "DROP"))
        }
        "nft" => a.windows(2).any(|v| v == ["flush", "ruleset"]),
        "ufw" => has(a, "disable") || has(a, "reset"),
        "chmod" | "chown" => {
            (short(a, 'R') || has(a, "--recursive"))
                && a.iter()
                    .filter(|s| !s.starts_with('-'))
                    .any(|s| system_path(s))
        }
        "kill" => {
            (has(a, "-9")
                || has(a, "-KILL")
                || has(a, "SIGKILL")
                || has(a, "KILL")
                || a.windows(2).any(|v| v == ["-s", "9"]))
                && (has(a, "1") || has(a, "-1"))
        }
        "passwd" => short(a, 'd') || has(a, "--delete"),
        "crontab" => short(a, 'r'),
        "docker" | "podman" => {
            a.windows(2).any(|v| v == ["volume", "prune"])
                || (a.windows(2).any(|v| v == ["system", "prune"])
                    && (short(a, 'a') || has(a, "--all"))
                    && has(a, "--volumes"))
        }
        "csrutil" => has(a, "disable"),
        _ => false,
    }
}
// Only known non-mutating flags. Values/operands are not interpreted as shell code.
fn flags(a: &[String], short: &str, long: &[&str]) -> bool {
    let mut options = true;
    a.iter().all(|s| {
        if s == "--" {
            options = false;
            return true;
        }
        if !options || !s.starts_with('-') || s == "-" {
            return true;
        }
        if s.starts_with("--") {
            long.contains(&s.split('=').next().unwrap())
        } else {
            s[1..]
                .chars()
                .all(|c| short.contains(c) || c.is_ascii_digit())
        }
    })
}
fn read_only(w: &[String], depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    let p = w[0].as_str();
    let a = &w[1..];
    // Every allowlisted reader uses the same sensitive-path barrier.
    if a.iter().any(|s| sensitive_path(s)) {
        return false;
    }
    if matches!(p, "awk" | "gawk" | "mawk" | "nawk") {
        return awk_read_only(a);
    }
    if p == "sed" {
        return sed_read_only(a);
    }
    if p == "watch" {
        let i = option_end(a, "n", &["--interval"]);
        return !a[i..].is_empty()
            && flags(&a[..i], "n", &["--interval"])
            && if a[i..].len() == 1 {
                classify_inner(&a[i], depth + 1) == Class::ReadOnly
            } else {
                read_only(&a[i..], depth + 1)
            };
    }
    // Permit plain parameter expansions, but never operators or substitutions.
    if w.iter().any(|s| !plain_expansions(s)) {
        return false;
    }
    let sub = a.first().map(String::as_str).unwrap_or("");
    match p {
        "ls" => flags(
            a,
            "aAlhtrSdFipR1",
            &[
                "--all",
                "--almost-all",
                "--human-readable",
                "--color",
                "--sort",
                "--time-style",
            ],
        ),
        "cat" => flags(a, "AbEnstTv", &["--number", "--show-all"]),
        "head" | "tail" => flags(
            a,
            "ncfqvF",
            &["--lines", "--bytes", "--follow", "--pid", "--quiet"],
        ),
        "wc" => flags(a, "clmwL", &["--bytes", "--chars", "--lines", "--words"]),
        "sort" => flags(
            a,
            "bdfghikMnrRsuVz",
            &[
                "--reverse",
                "--numeric-sort",
                "--human-numeric-sort",
                "--unique",
                "--key",
                "--field-separator",
            ],
        ),
        "uniq" => {
            flags(a, "cduiz", &["--count", "--repeated", "--unique"])
                && a.iter().filter(|s| !s.starts_with('-')).count() <= 1
        }
        "cut" => flags(
            a,
            "bcdfnsz",
            &[
                "--bytes",
                "--characters",
                "--delimiter",
                "--fields",
                "--complement",
                "--output-delimiter",
            ],
        ),
        "grep" | "egrep" | "fgrep" => flags(
            a,
            "ABCDEEFGHILPRUVabcdeFhilmnorsuvwxzZ",
            &[
                "--color",
                "--include",
                "--exclude",
                "--exclude-dir",
                "--recursive",
                "--line-number",
                "--ignore-case",
            ],
        ),
        "rg" => flags(
            a,
            "ABCDEFHILMNPSUabcdehilnorsuuvwxz",
            &[
                "--hidden",
                "--no-ignore",
                "--files",
                "--files-with-matches",
                "--count",
                "--color",
                "--glob",
                "--type",
                "--max-count",
                "--max-depth",
                "--stats",
                "--json",
            ],
        ),
        "find" => a.iter().all(|s| {
            !s.starts_with('-')
                || [
                    "-H",
                    "-L",
                    "-P",
                    "-name",
                    "-iname",
                    "-path",
                    "-ipath",
                    "-type",
                    "-size",
                    "-mtime",
                    "-mmin",
                    "-atime",
                    "-ctime",
                    "-maxdepth",
                    "-mindepth",
                    "-print",
                    "-print0",
                    "-ls",
                    "-empty",
                    "-readable",
                    "-writable",
                    "-executable",
                    "-user",
                    "-group",
                    "-perm",
                    "-not",
                    "-and",
                    "-or",
                    "-a",
                    "-o",
                    "-newer",
                    "-xdev",
                ]
                .contains(&s.as_str())
        }),
        "stat" => flags(
            a,
            "fctLx",
            &["--format", "--printf", "--file-system", "--dereference"],
        ),
        "file" => flags(
            a,
            "bhiLNz",
            &["--mime", "--mime-type", "--brief", "--dereference"],
        ),
        "tree" => flags(
            a,
            "adfhilnpqrsuACDFILQ",
            &["--dirsfirst", "--noreport", "--charset"],
        ),
        "readlink" | "realpath" => flags(
            a,
            "efmnqsvz",
            &["--canonicalize", "--relative-to", "--relative-base"],
        ),
        "basename" | "dirname" => flags(a, "asz", &["--multiple", "--suffix", "--zero"]),
        "md5sum" | "sha1sum" | "sha224sum" | "sha256sum" | "sha384sum" | "sha512sum" | "shasum" => {
            flags(
                a,
                "abctw",
                &["--binary", "--text", "--check", "--algorithm"],
            )
        }
        "du" | "df" => flags(
            a,
            "aBcdhiklmPstx",
            &[
                "--human-readable",
                "--max-depth",
                "--total",
                "--summarize",
                "--type",
            ],
        ),
        "mount" => a.is_empty(),
        "hostname" => a.iter().all(|s| {
            matches!(
                s.as_str(),
                "-I" | "-i" | "-f" | "-s" | "-d" | "-A" | "--fqdn" | "--all-ip-addresses"
            )
        }),
        "ps" => true,
        "env" => a.is_empty() || a == ["-0"] || a == ["--null"],
        "date" => {
            let mut value = false;
            a.iter().all(|s| {
                if value {
                    value = false;
                    return true;
                }
                if matches!(s.as_str(), "-d" | "-r" | "--date" | "--reference") {
                    value = true;
                    return true;
                }
                s.starts_with('+')
                    || matches!(s.as_str(), "-u" | "-R" | "-I" | "--utc" | "--iso-8601")
                    || s.starts_with("--date=")
                    || s.starts_with("--reference=")
            }) && !value
        }
        "blkid" => flags(
            a,
            "odilpstuv",
            &["--output", "--match-token", "--list-one", "--probe"],
        ),
        "dmesg" => flags(
            a,
            "HhklTtwx",
            &["--human", "--level", "--ctime", "--follow", "--decode"],
        ),
        "free" | "uptime" | "uname" | "whoami" | "id" | "w" | "who" | "last" | "which" | "type"
        | "printenv" | "nproc" | "lscpu" | "lsblk" | "vmstat" | "iostat" | "pgrep" | "ss"
        | "netstat" | "lsof" | "dig" | "nslookup" | "getent" | "sw_vers" | "system_profiler"
        | "vm_stat" => flags(
            a,
            "aAbBcdefgGhHiklLmMnNopPrRsStTuUvVwxXz",
            &[
                "--all",
                "--bytes",
                "--human",
                "--json",
                "--output",
                "--list",
                "--version",
            ],
        ),
        "echo" => flags(a, "enE", &[]),
        "printf" => !a.first().is_some_and(|s| s == "-v" || s.contains("%n")),
        "top" => (short(a, 'b') && flags(a, "bndHp", &[])) || a == ["-l", "1"],
        "ip" => {
            let i = a
                .iter()
                .take_while(|s| {
                    matches!(
                        s.as_str(),
                        "-br" | "-brief" | "-4" | "-6" | "-s" | "-d" | "-c" | "-j" | "-p"
                    )
                })
                .count();
            let a = &a[i..];
            a.first().is_some_and(|s| {
                [
                    "a", "addr", "address", "r", "ro", "route", "l", "link", "n", "neigh",
                    "neighbor", "rule", "maddr",
                ]
                .contains(&s.as_str())
            }) && (a.len() == 1 || matches!(a[1].as_str(), "show" | "list" | "ls"))
                && flags(&a[1..], "", &[])
        }
        "journalctl" => flags(
            a,
            "abefklnopqrtux",
            &[
                "--since",
                "--until",
                "--unit",
                "--boot",
                "--no-pager",
                "--no-hostname",
                "--lines",
                "--output",
                "--follow",
                "--disk-usage",
                "--list-boots",
            ],
        ),
        "systemctl" => {
            [
                "status",
                "show",
                "list-units",
                "list-unit-files",
                "list-timers",
                "is-active",
                "is-enabled",
                "is-failed",
                "cat",
            ]
            .contains(&sub)
                && flags(
                    &a[1..],
                    "al",
                    &[
                        "--all",
                        "--no-pager",
                        "--full",
                        "--type",
                        "--state",
                        "--property",
                        "--user",
                        "--system",
                    ],
                )
        }
        "docker" | "podman" => container(a),
        "kubectl" => {
            ["get", "describe", "logs", "top", "version", "api-resources"].contains(&sub)
                && !a.iter().any(|s| {
                    s.split(['/', ',', '=', '.'])
                        .any(|v| matches!(v.to_ascii_lowercase().as_str(), "secret" | "secrets"))
                })
                && flags(
                    &a[1..],
                    "Anolcfp",
                    &[
                        "--all-namespaces",
                        "--namespace",
                        "--output",
                        "--selector",
                        "--tail",
                        "--since",
                        "--timestamps",
                        "--follow",
                        "--previous",
                        "--containers",
                    ],
                )
        }
        "git" => match sub {
            "status" | "log" | "diff" | "show" | "rev-parse" => flags(
                &a[1..],
                "bspunMz",
                &[
                    "--short",
                    "--stat",
                    "--oneline",
                    "--all",
                    "--graph",
                    "--decorate",
                    "--name-only",
                    "--name-status",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--cached",
                    "--staged",
                    "--abbrev-ref",
                    "--show-toplevel",
                    "--verify",
                ],
            ),
            "branch" => {
                flags(
                    &a[1..],
                    "arv",
                    &["--all", "--remotes", "--list", "--show-current"],
                ) && a[1..].iter().all(|s| s.starts_with('-'))
            }
            "remote" => a == ["remote", "-v"],
            _ => false,
        },
        "apt" => {
            sub == "list"
                && flags(
                    &a[1..],
                    "a",
                    &["--installed", "--upgradable", "--all-versions"],
                )
        }
        "dpkg" => sub == "-l" && flags(&a[1..], "", &[]),
        "rpm" => sub.starts_with("-q") && flags(a, "qaiflR", &[]),
        "pip" => {
            sub == "list"
                && flags(
                    &a[1..],
                    "",
                    &["--format", "--outdated", "--local", "--user"],
                )
        }
        "npm" => sub == "ls" && flags(&a[1..], "ag", &["--all", "--global", "--depth", "--json"]),
        "brew" => sub == "list" && flags(&a[1..], "1", &["--versions", "--formula", "--cask"]),
        "crontab" => a == ["-l"],
        "diskutil" => matches!(sub, "list" | "info") && flags(&a[1..], "", &[]),
        "launchctl" => sub == "list" && flags(&a[1..], "", &[]),
        _ => false,
    }
}
fn container(a: &[String]) -> bool {
    let Some(sub) = a.first().map(String::as_str) else {
        return false;
    };
    let rest = &a[1..];
    match sub {
        "ps" | "images" | "logs" | "inspect" | "version" | "info" | "top" | "port" => flags(
            rest,
            "afnqstl",
            &[
                "--all",
                "--format",
                "--filter",
                "--tail",
                "--since",
                "--until",
                "--timestamps",
                "--follow",
                "--no-trunc",
                "--size",
                "--quiet",
            ],
        ),
        "stats" => {
            has(rest, "--no-stream")
                && flags(
                    rest,
                    "a",
                    &["--no-stream", "--all", "--format", "--no-trunc"],
                )
        }
        "network" | "volume" => {
            rest.first()
                .is_some_and(|s| matches!(s.as_str(), "ls" | "inspect"))
                && flags(&rest[1..], "qf", &["--format", "--filter", "--quiet"])
        }
        "compose" => {
            rest.first()
                .is_some_and(|s| matches!(s.as_str(), "ps" | "logs" | "config" | "ls"))
                && flags(
                    &rest[1..],
                    "afnqst",
                    &[
                        "--all",
                        "--format",
                        "--tail",
                        "--since",
                        "--timestamps",
                        "--follow",
                        "--services",
                        "--volumes",
                    ],
                )
        }
        _ => false,
    }
}

/// First simple command after Shellmate's wrapper and leading environment assignments.
/// This is status interpretation only; approval continues to use the full command.
pub fn command_words(command: &str) -> Vec<String> {
    let command = command.strip_prefix("export PAGER=cat GIT_PAGER=cat SYSTEMD_PAGER= SYSTEMD_COLORS=0 TERM=dumb LC_ALL=C.UTF-8; ").unwrap_or(command);
    lex(command)
        .tokens
        .into_iter()
        .take_while(|t| matches!(t, Token::Word(_)))
        .filter_map(|t| {
            if let Token::Word(s) = t {
                Some(s)
            } else {
                None
            }
        })
        .skip_while(|s| {
            s.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name.bytes().enumerate().all(|(i, b)| {
                        b == b'_' || b.is_ascii_alphabetic() || (i != 0 && b.is_ascii_digit())
                    })
            })
        })
        .collect()
}
pub fn changes_directory(command: &str) -> bool {
    lex(command).tokens.iter().any(
        |t| matches!(t, Token::Word(s) if matches!(s.as_str(), "cd" | "pushd" | "popd" | "chdir")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allowlist_families_and_excluded_flags() {
        for cmd in [
            "ls -lah",
            "cat /etc/hosts",
            "head -n 10 x",
            "tail -f x",
            "wc -l x",
            "sort -nr x",
            "uniq -c x",
            "cut -d : -f 1 x",
            "grep -E foo x",
            "egrep foo x",
            "fgrep foo x",
            "rg --hidden foo",
            "find /tmp -type f -name '*.log'",
            "stat x",
            "file x",
            "tree -L 2",
            "readlink -f x",
            "realpath x",
            "basename x",
            "dirname x",
            "md5sum x",
            "sha1sum x",
            "sha224sum x",
            "sha256sum x",
            "sha384sum x",
            "sha512sum x",
            "shasum -a 256 x",
            "du -sh /tmp",
            "df -h",
            "free -m",
            "uptime",
            "uname -a",
            "hostname",
            "whoami",
            "id",
            "w",
            "who",
            "last",
            "date -u",
            "which bash",
            "type ls",
            "echo hello",
            "printf '%s\\n' hello",
            "env",
            "env -0",
            "printenv PATH",
            "nproc",
            "lscpu",
            "lsblk",
            "blkid",
            "mount",
            "dmesg -T",
            "vmstat 1 2",
            "iostat 1 2",
            "ps aux",
            "pgrep nginx",
            "top -b -n1",
            "top -b -n 2",
            "ss -lntp",
            "netstat -an",
            "lsof -i :3000",
            "ip addr",
            "ip route show",
            "ip link list",
            "ip neigh show",
            "dig example.test",
            "nslookup example.test",
            "getent hosts example.test",
            "journalctl -u nginx --no-pager",
            "systemctl status nginx",
            "systemctl show nginx",
            "systemctl list-units",
            "systemctl list-unit-files",
            "systemctl list-timers",
            "systemctl is-active nginx",
            "systemctl is-enabled nginx",
            "systemctl is-failed nginx",
            "systemctl cat nginx",
            "docker ps -a",
            "docker images",
            "docker logs -f web",
            "docker inspect web",
            "docker stats --no-stream",
            "docker version",
            "docker info",
            "docker top web",
            "docker port web",
            "docker network ls",
            "docker network inspect x",
            "docker volume ls",
            "docker volume inspect x",
            "docker compose ps",
            "docker compose logs",
            "docker compose config",
            "docker compose ls",
            "podman ps",
            "podman stats --no-stream",
            "podman volume inspect x",
            "kubectl get pods -A",
            "kubectl describe pod web",
            "kubectl logs web",
            "kubectl top pods",
            "kubectl version",
            "kubectl api-resources",
            "git status",
            "git log --oneline",
            "git diff --stat",
            "git show HEAD",
            "git branch -av",
            "git remote -v",
            "git rev-parse HEAD",
            "apt list --installed",
            "dpkg -l",
            "rpm -qa",
            "pip list",
            "npm ls --depth=0",
            "brew list --versions",
            "crontab -l",
            "sw_vers",
            "system_profiler SPHardwareDataType",
            "diskutil list",
            "diskutil info /",
            "launchctl list",
            "vm_stat",
            "top -l 1",
            "ls; ps aux && uname -a || echo failed | cat\ndate",
            "echo 'a;b|c'",
            "echo 'rm -rf /'",
            "cat escaped\\ name",
            "cat x 2>/dev/null",
            "cat x >/dev/null",
            "cat x 2>&1",
            "cat x 2> /dev/null | head",
            "ls # ignored\nwhoami",
        ] {
            assert_eq!(classify(cmd), Class::ReadOnly, "{cmd}");
        }
        for cmd in [
            "find /tmp -delete",
            "find . -exec echo '{}' ';'",
            "find . -execdir ls ';'",
            "find . -ok ls ';'",
            "find . -fprint /tmp/x",
            "find . -fprintf /tmp/x x",
            "find . -fls /tmp/x",
            "git branch -d x",
            "git branch -D x",
            "git branch -m x",
            "git branch new",
            "git diff --output=/tmp/x",
            "git diff --ext-diff",
            "git log --output=x",
            "git show --textconv",
            "journalctl --vacuum-size=1M",
            "journalctl --vacuum-time=1d",
            "journalctl --rotate",
            "journalctl --flush",
            "systemctl restart nginx",
            "kubectl get secret",
            "kubectl get secrets",
            "kubectl get secret/foo",
            "kubectl get pods,secrets",
            "kubectl describe secrets/foo",
            "kubectl get Secret",
            "docker stats",
            "docker compose config --output x",
            "docker run x",
            "ip link set eth0 down",
            "ip route add default via x",
            "hostname new",
            "mount /dev/sda /mnt",
            "dmesg -c",
            "dmesg -C",
            "dmesg -D",
            "date -s now",
            "date 111712302026",
            "blkid -w /tmp/cache",
            "blkid -c /tmp/cache",
            "printf %n NAME",
            "kubectl get secrets.v1",
            "env ls",
            "env X=1 ls",
            "sort -o x y",
            "sort --compress-program=bad",
            "uniq a b",
            "rg --pre bad x",
            "rg --hostname-bin=bad x",
            "printf -v x y",
            "ls --unknown-flag",
            "curl url",
            "wget url",
            "sed x",
            "awk x",
            "less x",
            "more x",
            "vim x",
            "top",
            "sudo ls",
            "sh -c 'ls'",
            "bash -c \"ls\"",
            "xargs ls",
            "eval ls",
            "su x",
            "cat <x",
            "cat <<EOF\nx\nEOF",
            "echo hi >file",
            "ls 2>file",
            "echo hi >>file",
            "ls &",
            "cat 'unclosed",
            "cat x\\",
            "ls && unknown",
            "ls &&",
            "ls |",
            "ls || # comment",
            "ls && || cat",
            "echo $(date)",
            "echo `date`",
            "echo \"$(date)\"",
            "echo '${HOME}'",
            "rm -rf ./build",
            "rm -rf /tmp/x",
            "rm -rf '/tmp/quoted path'",
        ] {
            assert_eq!(classify(cmd), Class::Other, "{cmd}");
        }
    }
    #[test]
    fn red_lines_and_nested_wrappers() {
        for cmd in [
            "rm -rf /",
            "rm -rf -- /",
            "rm -fr /*",
            "rm -r ~/",
            "rm -f ~",
            "rm -r '$HOME'",
            "rm -r .",
            "rm -r ..",
            "rm -f '*'",
            "rm -rf '/etc/'",
            "rm -rf /usr",
            "rm -rf /var",
            "rm -rf /boot",
            "rm -rf /bin",
            "rm -rf /lib64",
            "rm -rf /home",
            "rm -rf /root",
            "rm -rf /opt",
            "rm -rf /System",
            "rm -rf /Users",
            "rm -rf --no-preserve-root /tmp/x",
            "rm --recursive /",
            "rm -rf /tmp/..",
            "mkfs.ext4 /dev/sda",
            "mkfs /dev/disk1",
            "wipefs /dev/sda",
            "dd if=x of=/dev/sda",
            "dd 'of=/dev/disk2'",
            "echo x > /dev/sda",
            "> /dev/nvme0n1",
            "printf x > '/dev/disk1'",
            "fdisk /dev/sda",
            "sfdisk /dev/sda",
            "parted /dev/sda mklabel gpt",
            "diskutil eraseDisk APFS x disk1",
            "diskutil partitionDisk disk1 1 GPT APFS x 100%",
            "diskutil zeroDisk disk1",
            "shutdown now",
            "reboot",
            "poweroff",
            "halt",
            "init 0",
            "init 6",
            "systemctl reboot",
            "systemctl poweroff",
            "systemctl halt",
            "systemctl kexec",
            "iptables -F",
            "ip6tables -X",
            "iptables -P INPUT DROP",
            "nft flush ruleset",
            "ufw disable",
            "ufw reset",
            ":(){ :|:& };:",
            "chmod -R 777 /",
            "chown -R root /etc",
            "kill -9 1",
            "kill -9 -1",
            "userdel demo",
            "passwd -d demo",
            "crontab -r",
            "docker system prune -a --volumes",
            "docker volume prune",
            "csrutil disable",
            "nvram foo=bar",
        ] {
            assert_eq!(classify(cmd), Class::RedLine, "{cmd}");
            assert_eq!(
                classify(&format!("sudo -u root {cmd}")),
                Class::RedLine,
                "sudo {cmd}"
            );
            let quoted = cmd.replace('\'', "'\\''");
            assert_eq!(
                classify(&format!("sh -c '{quoted}'")),
                Class::RedLine,
                "sh {cmd}"
            );
            assert_eq!(
                classify(&format!("printf x | xargs {cmd}")),
                Class::RedLine,
                "xargs {cmd}"
            );
        }
        for cmd in [
            "sudo bash -c \"rm -rf /\"",
            "xargs -0 -n 1 sudo /bin/rm -rf /",
            "echo $(rm -rf /)",
            "echo `rm -rf /`",
            "ls || sudo sh -c 'reboot'",
            "rm -rf '/",
            "echo x >\"/dev/sda\"",
            "echo $(echo $(reboot))",
            "rm -rf /etc/*",
            "rm -rf ~/*",
            "X=1 rm -rf /",
            "if true; then reboot; fi",
            "find /tmp -exec rm -rf / \\;",
        ] {
            assert_eq!(classify(cmd), Class::RedLine, "{cmd}");
        }
    }
    #[test]
    fn review_policy_table() {
        let mut cases: Vec<(String, Class)> = vec![
            (
                "docker ps --format 'table {{.Names}}\\t{{.Image}}\\t{{.Status}}\\t{{.Ports}}'",
                Class::ReadOnly,
            ),
            ("docker ps -a", Class::ReadOnly),
            ("ps aux | grep nginx", Class::ReadOnly),
            ("ps aux --sort=-%mem | head -n 6", Class::ReadOnly),
            ("ss -tlnp | grep 3000", Class::ReadOnly),
            ("lsof -i :3000", Class::ReadOnly),
            ("df -h", Class::ReadOnly),
            ("free -m", Class::ReadOnly),
            ("journalctl -u nginx -n 50 --no-pager", Class::ReadOnly),
            ("systemctl status nginx --no-pager", Class::ReadOnly),
            ("cat /etc/os-release", Class::ReadOnly),
            ("tail -n 100 /var/log/syslog", Class::ReadOnly),
            ("docker logs --tail 100 nginx 2>&1", Class::ReadOnly),
            (
                "du -sh /var/* 2>/dev/null | sort -rh | head",
                Class::ReadOnly,
            ),
            (
                "find / -name '*.log' -size +100M 2>/dev/null",
                Class::ReadOnly,
            ),
            ("uptime && free -h", Class::ReadOnly),
            (
                "docker inspect nginx --format '{{json .NetworkSettings}}'",
                Class::ReadOnly,
            ),
            ("git status", Class::ReadOnly),
            ("ls -la ~", Class::ReadOnly),
            ("echo $PATH", Class::ReadOnly),
            ("systemctl restart nginx", Class::Other),
            ("docker restart nginx", Class::Other),
            ("apt install -y htop", Class::Other),
            ("echo hi > /tmp/x", Class::Other),
            ("cat /etc/passwd | tee /tmp/x", Class::Other),
            ("curl -s https://example.com", Class::Other),
            ("rm -rf /tmp/build", Class::Other),
            ("sed -i 's/a/b/' f", Class::Other),
            ("ls $(cat /tmp/x)", Class::Other),
            ("find . -name x -delete", Class::Other),
            ("sudo ls /root", Class::Other),
            ("kill 1234", Class::Other),
            ("rm -rf /", Class::RedLine),
            ("rm -rf / --no-preserve-root", Class::RedLine),
            ("sudo rm -rf /etc", Class::RedLine),
            ("bash -c 'rm -rf ~'", Class::RedLine),
            ("cd / && rm -rf *", Class::RedLine),
            ("reboot", Class::RedLine),
            ("sudo shutdown -h now", Class::RedLine),
            ("mkfs.ext4 /dev/sdb1", Class::RedLine),
            ("dd if=/dev/zero of=/dev/sda bs=1M", Class::RedLine),
            ("iptables -F", Class::RedLine),
            ("chmod -R 777 /", Class::RedLine),
            (":(){ :|:& };:", Class::RedLine),
            ("echo y | sudo ufw reset", Class::RedLine),
            ("find / -name x | xargs rm -rf", Class::Other),
            ("ls; reboot", Class::RedLine),
            ("ls && sudo -u root /sbin/reboot", Class::RedLine),
            ("systemctl  poweroff", Class::RedLine),
            ("env reboot", Class::RedLine),
            ("nohup reboot &", Class::RedLine),
            ("timeout 5 reboot", Class::RedLine),
            ("docker stats --no-stream", Class::ReadOnly),
            ("top -bn1 | head -20", Class::ReadOnly),
            ("ip a", Class::ReadOnly),
            ("netstat -tulpn", Class::ReadOnly),
            (
                "cat /var/log/nginx/error.log | grep -i error | wc -l",
                Class::ReadOnly,
            ),
            ("journalctl -xe --no-pager | tail -50", Class::ReadOnly),
            ("kubectl get pods -A", Class::ReadOnly),
            ("kubectl get secret -o yaml", Class::Other),
            ("docker compose ps", Class::ReadOnly),
            ("grep -rn 'listen' /etc/nginx/ 2>/dev/null", Class::ReadOnly),
            ("cat /proc/meminfo", Class::ReadOnly),
            ("ps aux | awk '{print $2, $11}'", Class::ReadOnly),
            ("awk 'BEGIN{system(\"reboot\")}'", Class::RedLine),
            ("sed -n '1,20p' /etc/nginx/nginx.conf", Class::ReadOnly),
            ("ls -lh /var/log | tail", Class::ReadOnly),
            ("hostname -I", Class::ReadOnly),
            ("hostname newname", Class::Other),
            ("cat ~/.ssh/id_rsa", Class::Other),
            ("cat /etc/shadow", Class::Other),
            ("printenv", Class::ReadOnly),
            ("ssh other reboot", Class::RedLine),
            ("nice -n 10 reboot", Class::RedLine),
            ("doas reboot", Class::RedLine),
            ("watch -n1 df -h", Class::ReadOnly),
            ("diskutil list", Class::ReadOnly),
            ("diskutil eraseDisk JHFS+ X disk2", Class::RedLine),
            ("launchctl list | grep docker", Class::ReadOnly),
            ("rm -rf $HOME", Class::RedLine),
            ("rm -rf \"$HOME\"/", Class::RedLine),
            ("rm -r -f /usr", Class::RedLine),
            ("rm --recursive --force /var", Class::RedLine),
            ("> /dev/sda", Class::RedLine),
            ("cat /dev/zero > /dev/nvme0n1", Class::RedLine),
        ]
        .into_iter()
        .map(|(cmd, class)| (cmd.to_owned(), class))
        .collect();
        assert_eq!(cases.len(), 85, "include every supplied probe case");
        let mut add = |cmd: String, class| cases.push((cmd, class));
        for wrapper in [
            "timeout 5",
            "timeout -s KILL 5",
            "timeout --signal KILL --kill-after 2 5",
            "nice -n 10",
            "nice --adjustment=10",
            "ionice -c3",
            "ionice -c 3 -n 7",
            "stdbuf -oL -e 0",
            "time -f %E",
            "command --",
            "builtin",
            "exec -a child",
            "chroot /mnt",
            "chroot --userspec root:root /mnt",
            "watch -n1",
            "watch -n 1",
            "doas -u root",
            "doas -C /etc/doas.conf",
            "pkexec --user root",
            "runuser -u root --",
            "sudo -u root",
            "sudo --user root --",
            "ssh other",
            "ssh -p 2222 -l root -i /tmp/key -o BatchMode=yes other",
            "ssh -p2222 -lroot other",
            "timeout -s KILL 5 nice -n 10 doas -u root",
        ] {
            add(format!("{wrapper} reboot"), Class::RedLine);
            add(format!("{wrapper} rm -rf /tmp/build"), Class::Other);
        }
        for cmd in [
            "python3 -c 'os.reboot()'",
            "echo ${VAR:=$(reboot)}",
            "echo \"${VAR:=$(rm -rf /)}\"",
            "watch 'ls; reboot'",
            "su -c 'reboot'",
            "su -s /bin/sh root -c 'rm -rf /'",
            "su root --command 'poweroff'",
            "runuser -u root -c 'shutdown now'",
            "ssh -o BatchMode=yes other 'rm -rf /'",
            "awk 'BEGIN{system(\"reboot\")}'",
            "perl -e 'system(\"shutdown now\")'",
            "python3 -c 'import os; os.system(\"poweroff\")'",
            "python3.12 -c 'subprocess.run([\"rm\",\"-rf\",\"/\"])'",
            "ruby -e 'system(\"halt\")'",
            "node -e 'exec(\"mkfs.ext4 /dev/sdb\")'",
            "php -r 'system(\"dd if=/dev/zero of=/dev/sda\");'",
            "lua -e 'os.execute(\"iptables -F\")'",
            "osascript -e 'do shell script \"ufw reset\"'",
            "bash -c 'if true; then reboot'",
            "sh -c 'system(\"reboot\")'",
            "zsh -c 'unparsed(\"rm -rf /\")'",
            "perl -e'system(\"reboot\")'",
            "node --eval='exec(\"reboot\")'",
        ] {
            add(cmd.into(), Class::RedLine);
        }
        for cmd in [
            "sudo -u reboot ls",
            "nice -n reboot ls",
            "timeout -s reboot 5 ls",
            "ionice -c reboot ls",
            "stdbuf -o reboot ls",
            "time -f reboot ls",
            "exec -a reboot ls",
            "chroot /reboot ls",
            "doas -u reboot ls",
            "pkexec --user reboot ls",
            "runuser -u reboot -- ls",
            "su -c 'ls'",
            "ssh -l reboot other ls",
            "ssh other ls",
            "perl -e 'print 1'",
            "python -c 'print(1)'",
            "ruby -e 'puts 1'",
            "node -e 'console.log(1)'",
            "php -r 'echo 1;'",
            "lua -e 'print(1)'",
            "osascript -e 'get name of current application'",
            "top -n 1",
            "top -H",
            "hostname -I newname",
            "echo ${VAR:=x}",
            "echo ${VAR=x}",
            "echo ${VAR:?x}",
            "echo \"${VAR:=x}\"",
            "echo $(date)",
            "awk -f script.awk data",
            "awk -fscript.awk data",
            "awk 'BEGIN{system(\"ls\")}'",
            "awk '{getline x; print x}'",
            "awk '{print | \"cat\"}'",
            "awk '{print > \"out\"}'",
            "awk '{close(\"out\")}'",
            "awk '{fflush()}'",
            "awk '{printf > \"out\"}'",
            "sed -i 's/a/b/' f",
            "sed --in-place 's/a/b/' f",
            "sed -f file f",
            "sed -ffile f",
            "sed -n '1,20w out' f",
            "sed 's/a/b/w out' f",
            "sed 's/a/b/e' f",
            "sed '1r file' f",
            "sed 'R file' f",
            "sed 'W file' f",
            "sed 'e reboot' f",
            "sed 'd' f",
            "sed 's/a/b/;e ls' f",
            "sed --sandbox 'p' f",
            "sed -n '1,$p' f",
            "sed 's/a/b/;r secret' f",
            "sed -e p -e 'w out' f",
            "sed 's/a/b/\\' f",
            "sed 's/a/b/\nr file' f",
            "sed -n '/unterminated' f",
            "sed -e",
        ] {
            add(cmd.into(), Class::Other);
        }
        for cmd in [
            "ps aux --sort=-%mem",
            "ps --arbitrary-flag",
            "top -b -n1",
            "top -bn1",
            "top -bn 1",
            "top -n 1 -b",
            "top -l 1",
            "top -b",
            "echo $PATH",
            "echo ${PATH}",
            "cat $HOME/log",
            "cat ${HOME}/log",
            "ls ~",
            "awk '/nginx/' data",
            "awk '1' data",
            "awk '$1 == 3' data",
            "watch 'df -h'",
            "watch df -h",
            "watch -n 1 df -h",
            "watch --interval=1 df -h",
            "watch -n1 awk '{print $2}' data",
            "watch -n1 watch -n2 df -h",
            "awk '{print $2, $11}'",
            "awk -F: '{print $1}' /etc/passwd",
            "awk -F : -v n=1 '{print $n}' data",
            "awk 'BEGIN{print 1}'",
            "sed -n '1,20p' f",
            "sed -n '$p' f",
            "sed -n '/start/,/end/p' f",
            "sed '/pattern/p' f",
            "sed 's/a/b/' f",
            "sed 's/a/b/gi2p' f",
            "sed -nE -e '1p;2,5s|a|b|g' -e '$p' f",
            "sed -ne'1p' f",
            "sed -rsuz 's/a/b/I' f",
            "sed 's/a\\/b/c/;p' f",
            "sed 's/a;b/c/;p' f",
            "sed 's/a/write/' f",
            "sed -n '1p; 2p' f",
            "sed 'p' f",
            "cat ~/.ssh/id_rsa.pub",
            "cat ~/.ssh/known_hosts",
            "cat ~/.ssh/authorized_keys",
            "cat ~/.ssh/config",
            "cat /home/user/.ssh/id_ecdsa.pub",
            "cat /etc/passwd",
        ] {
            add(cmd.into(), Class::ReadOnly);
        }
        for object in [
            "a", "addr", "address", "r", "ro", "route", "l", "link", "n", "neigh", "rule", "maddr",
        ] {
            for verb in ["", " show", " list", " ls"] {
                add(format!("ip {object}{verb}"), Class::ReadOnly);
            }
        }
        for option in ["-br", "-brief", "-4", "-6", "-s", "-d", "-c", "-j", "-p"] {
            add(format!("ip {option} a show"), Class::ReadOnly);
        }
        add("ip -4 -br -j address show".into(), Class::ReadOnly);
        for flag in [
            "-I",
            "-i",
            "-f",
            "-s",
            "-d",
            "-A",
            "--fqdn",
            "--all-ip-addresses",
        ] {
            add(format!("hostname {flag}"), Class::ReadOnly);
        }
        for path in [
            "~/.ssh/id_rsa",
            "$HOME/.ssh/private",
            "${HOME}/.ssh/private",
            "/root/.ssh/private",
            "/home/alice/.ssh/private",
            "~/.ssh/subdir/config",
            "/etc/shadow",
            "/etc/gshadow",
            "/etc/security/opasswd",
            "cert.pem",
            "cert.key",
            "cert.p12",
            "cert.pfx",
            "id_rsa",
            "id_ed25519",
            "id_ecdsa",
            "id_dsa",
            ".env",
            ".env.production",
            "~/.aws/credentials",
            "$HOME/.kube/config",
            "~/.netrc",
            "~/.pgpass",
            "~/.git-credentials",
            "~/.docker/config.json",
            "~/.npmrc",
            "~/.pypirc",
            "~/.bash_history",
            "/root/.zsh_history",
            "/home/alice/.aws/credentials",
            "/tmp/.env.local",
            "./cert.pem",
            "~/.ssh/../.aws/credentials",
        ] {
            for reader in [
                "cat",
                "head",
                "tail",
                "grep x",
                "less",
                "strings",
                "xxd",
                "base64",
                "od",
                "wc",
                "sort",
                "cut -c 1",
                "awk '{print}'",
                "sed -n p",
            ] {
                add(format!("{reader} {path}"), Class::Other);
            }
        }
        let failures: Vec<_> = cases
            .iter()
            .filter_map(|(cmd, want)| {
                let got = classify(cmd);
                (got != *want).then(|| format!("{cmd:?}: want {want:?}, got {got:?}"))
            })
            .collect();
        println!(
            "review policy table: {} cases (including all 85 probes)",
            cases.len()
        );
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
    #[test]
    fn permission_matrix_and_stable_ids() {
        for (mode, id, label, expected) in [
            (
                PermissionMode::Ask,
                "ask",
                "每步审批",
                [Decision::Ask, Decision::Ask, Decision::AskRedLine],
            ),
            (
                PermissionMode::Auto,
                "auto",
                "自动审批",
                [Decision::Run, Decision::Ask, Decision::AskRedLine],
            ),
            (
                PermissionMode::Full,
                "full",
                "完全访问",
                [Decision::Run, Decision::Run, Decision::AskRedLine],
            ),
        ] {
            assert_eq!(mode.id(), id);
            assert_eq!(mode.label(), label);
            assert_eq!(PermissionMode::from_id(id), mode);
            for (class, decision) in [Class::ReadOnly, Class::Other, Class::RedLine]
                .into_iter()
                .zip(expected)
            {
                assert_eq!(decide(mode, class), decision);
            }
        }
        assert_eq!(PermissionMode::from_id("future"), PermissionMode::Ask);
    }
}
