//! The language of a file, from its name: the badge in the editor's title row and the name the
//! highlighter knows it by.

/// What the editor knows about a file's language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lang {
    /// The label of the badge (`Nginx`, `Shell`, `纯文本`).
    pub badge: &'static str,
    /// What to tell the highlighter (an extension or name its syntax collection knows).
    pub token: &'static str,
}

const PLAIN: Lang = Lang { badge: "纯文本", token: "text" };

const fn lang(badge: &'static str, token: &'static str) -> Lang {
    Lang { badge, token }
}

/// The language for a file called `name` (the last part of its path).
pub fn language(name: &str) -> Lang {
    let lower = name.to_ascii_lowercase();
    // Whole names first: files that are known by what they are called, not by an extension.
    match lower.as_str() {
        "nginx.conf" | "mime.types" => return lang("Nginx", "nginx"),
        "dockerfile" | "containerfile" => return lang("Docker", "dockerfile"),
        "makefile" | "gnumakefile" => return lang("Make", "makefile"),
        ".bashrc" | ".bash_profile" | ".bash_aliases" | ".zshrc" | ".zprofile" | ".zshenv" | ".profile"
        | ".bash_logout" => {
            return lang("Shell", "bash");
        }
        ".gitignore" | ".dockerignore" | ".gitattributes" => return lang("Git", "gitignore"),
        "cargo.lock" => return lang("TOML", "toml"),
        _ => {}
    }
    if lower == ".env" || lower.starts_with(".env.") {
        return lang("Env", "bash");
    }
    let Some((stem, extension)) = lower.rsplit_once('.') else { return PLAIN };
    if stem.is_empty() {
        // `.hidden`: a name, not an extension.
        return PLAIN;
    }
    match extension {
        "sh" | "bash" | "zsh" | "ksh" => lang("Shell", "bash"),
        "py" | "pyw" => lang("Python", "python"),
        "js" | "mjs" | "cjs" | "jsx" => lang("JavaScript", "js"),
        "ts" | "mts" | "cts" => lang("TypeScript", "ts"),
        "tsx" => lang("TSX", "tsx"),
        "json" | "jsonc" => lang("JSON", "json"),
        "yaml" | "yml" => lang("YAML", "yaml"),
        "toml" => lang("TOML", "toml"),
        "md" | "markdown" | "mdx" => lang("Markdown", "md"),
        "rs" => lang("Rust", "rust"),
        "go" => lang("Go", "go"),
        "c" | "h" => lang("C", "c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => lang("C++", "cpp"),
        "java" => lang("Java", "java"),
        "kt" | "kts" => lang("Kotlin", "kotlin"),
        "rb" => lang("Ruby", "ruby"),
        "php" => lang("PHP", "php"),
        "lua" => lang("Lua", "lua"),
        "pl" | "pm" => lang("Perl", "perl"),
        "html" | "htm" => lang("HTML", "html"),
        "css" => lang("CSS", "css"),
        "scss" => lang("SCSS", "scss"),
        "xml" | "plist" | "svg" => lang("XML", "xml"),
        "sql" => lang("SQL", "sql"),
        "ini" | "cfg" | "conf" | "properties" => lang("INI", "ini"),
        "service" | "socket" | "timer" => lang("systemd", "ini"),
        "log" => lang("日志", "log"),
        "diff" | "patch" => lang("Diff", "diff"),
        "txt" | "text" => PLAIN,
        _ => PLAIN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn badge(name: &str) -> &'static str {
        language(name).badge
    }

    #[test]
    fn files_known_by_name_get_their_language() {
        assert_eq!(language("nginx.conf"), Lang { badge: "Nginx", token: "nginx" });
        assert_eq!(language("Dockerfile").badge, "Docker");
        assert_eq!(language("Makefile").badge, "Make");
        assert_eq!(language(".bashrc").badge, "Shell");
        assert_eq!(language(".zshrc").token, "bash");
        assert_eq!(language("Cargo.lock").badge, "TOML");
        assert_eq!(language(".env").badge, "Env");
        assert_eq!(language(".env.production").badge, "Env");
        assert_eq!(language(".gitignore").badge, "Git");
    }

    #[test]
    fn the_extension_decides_for_everything_else() {
        for (name, expected) in [
            ("deploy.sh", "Shell"),
            ("app.py", "Python"),
            ("index.js", "JavaScript"),
            ("main.ts", "TypeScript"),
            ("package.json", "JSON"),
            ("docker-compose.yml", "YAML"),
            ("config.yaml", "YAML"),
            ("Cargo.toml", "TOML"),
            ("README.md", "Markdown"),
            ("main.rs", "Rust"),
            ("page.html", "HTML"),
            ("site.css", "CSS"),
            ("query.sql", "SQL"),
            ("php.ini", "INI"),
            ("sshd.conf", "INI"),
            ("app.log", "日志"),
            ("fix.patch", "Diff"),
        ] {
            assert_eq!(badge(name), expected, "{name}");
        }
    }

    #[test]
    fn case_does_not_matter() {
        assert_eq!(badge("NGINX.CONF"), "Nginx");
        assert_eq!(badge("Script.SH"), "Shell");
        assert_eq!(badge("DOCKERFILE"), "Docker");
    }

    #[test]
    fn the_last_extension_counts_and_unknown_files_are_plain_text() {
        assert_eq!(badge("archive.tar.sh"), "Shell");
        assert_eq!(badge("notes.txt"), "纯文本");
        assert_eq!(badge("LICENSE"), "纯文本", "no extension");
        assert_eq!(badge(".hidden"), "纯文本", "a dotfile is a name, not an extension");
        assert_eq!(badge("data.xyz"), "纯文本");
        assert_eq!(badge("trailing."), "纯文本");
        assert_eq!(language("notes.txt").token, "text");
    }
}
