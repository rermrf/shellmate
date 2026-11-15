//! Shell quoting for everything that comes from the server or the user (service names, container
//! names, paths): such a value is always a single word of the command line, whatever it contains.

/// Why a value cannot be put on a command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QuoteError {
    /// A shell word cannot contain a NUL byte.
    #[error("参数里含有空字节")]
    Nul,
}

/// `arg` as one shell word: unchanged if it only has characters that are safe everywhere, otherwise in
/// single quotes with each `'` written as `'\''`. Everything inside single quotes is literal to a POSIX
/// shell (and to bash, zsh and fish, the usual login shells): spaces, `$`, `;`, backticks, newlines,
/// non-ASCII text.
pub fn quote(arg: &str) -> Result<String, QuoteError> {
    if arg.contains('\0') {
        return Err(QuoteError::Nul);
    }
    let plain = !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
    if plain {
        return Ok(arg.to_string());
    }
    Ok(format!("'{}'", arg.replace('\'', "'\\''")))
}

/// The words quoted and joined with spaces.
pub fn join<S: AsRef<str>>(args: &[S]) -> Result<String, QuoteError> {
    args.iter().map(|arg| quote(arg.as_ref())).collect::<Result<Vec<_>, _>>().map(|words| words.join(" "))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn plain_words_stay_as_they_are() {
        for word in ["kill", "-TERM", "/var/log/syslog", "nginx.service", "a_b-c.d:e@f", "docker-compose.yml"] {
            assert_eq!(quote(word).unwrap(), word);
        }
    }

    #[test]
    fn other_values_are_single_quoted() {
        assert_eq!(quote("").unwrap(), "''");
        assert_eq!(quote("my service").unwrap(), "'my service'");
        assert_eq!(quote("it's").unwrap(), r"'it'\''s'");
        assert_eq!(quote("'").unwrap(), r"''\'''");
        assert_eq!(quote("$HOME;rm -rf /").unwrap(), "'$HOME;rm -rf /'");
        assert_eq!(quote("a\nb").unwrap(), "'a\nb'");
        assert_eq!(quote("日本語").unwrap(), "'日本語'");
        assert_eq!(quote("*").unwrap(), "'*'");
        assert_eq!(quote("~").unwrap(), "'~'", "no tilde expansion");
        assert_eq!(quote("a\0b"), Err(QuoteError::Nul));
        assert_eq!(join(&["echo", "a b", "c'd"]).unwrap(), r"echo 'a b' 'c'\''d'");
    }

    /// The real test of a quoter: what a shell makes of it. Every value must come out of `sh` as one
    /// argument, byte for byte.
    #[test]
    fn a_shell_reads_back_exactly_what_was_quoted() {
        let values = [
            "plain",
            "",
            " ",
            "two words",
            "  leading and trailing  ",
            "it's",
            "''",
            "\"double\" 'single' `back` \\slash",
            "$HOME ${PATH} $(id) `id` $((1+1))",
            "a;b && c || d | e > f < g & h",
            "line one\nline two\n",
            "tab\tseparated",
            "glob * ? [a-z] {a,b} ~ ~root",
            "!history !! #comment",
            "日本語 ünïcödé 🙂",
            "-n",
            "--help",
            "=foo",
            "a=b",
        ];
        let script = format!("for a in {}; do printf '%s\\0' \"$a\"; done", join(&values).unwrap());
        let output = Command::new("sh").arg("-c").arg(&script).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let got: Vec<&[u8]> = output.stdout.split(|b| *b == 0).collect();
        // The output ends with a NUL, so the last piece is empty.
        assert_eq!(got.len(), values.len() + 1);
        for (value, got) in values.iter().zip(&got) {
            assert_eq!(*got, value.as_bytes(), "{value:?}");
        }
    }

    #[test]
    fn a_hostile_name_cannot_end_the_command() {
        let evil = "x'; echo pwned; '";
        let line = format!("printf %s {}", quote(evil).unwrap());
        let output = Command::new("sh").arg("-c").arg(&line).output().unwrap();
        assert_eq!(output.stdout, evil.as_bytes());
    }
}
