//! Reading and writing the OpenSSH `known_hosts` file.
//!
//! Supported: plain, `[host]:port`, hashed (`|1|salt|hash`) and wildcard host
//! patterns, comma separated lists and `!negation`. Lines with a `@marker`
//! (`@cert-authority`, `@revoked`) and lines that cannot be parsed are ignored
//! for lookups and preserved verbatim when the file is rewritten.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{Algorithm, PublicKey};
use sha1::Sha1;

/// Result of looking a host key up in `known_hosts`.
#[derive(Clone, Debug, PartialEq)]
#[allow(missing_docs)] // the fields of the struct variants are described on the variants
pub enum Lookup {
    /// The exact key is recorded for this host.
    Known,
    /// No key of this algorithm is recorded. `other` holds recorded keys of
    /// other algorithms for the same host (usually empty).
    New { other: Vec<PublicKey> },
    /// A different key of the same algorithm is recorded: the server's key
    /// changed (or someone is impersonating it).
    Changed { recorded: Vec<PublicKey> },
}

/// A `known_hosts` file at an injectable path.
#[derive(Clone, Debug)]
pub struct KnownHosts {
    path: PathBuf,
}

impl KnownHosts {
    /// Uses the file at `path`; it does not have to exist yet.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `~/.ssh/known_hosts`, shared with OpenSSH.
    pub fn default_path() -> PathBuf {
        std::env::home_dir().unwrap_or_default().join(".ssh").join("known_hosts")
    }

    /// Path of the file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Compares `key` with the keys recorded for `host:port`.
    pub fn lookup(&self, host: &str, port: u16, key: &PublicKey) -> io::Result<Lookup> {
        let name = host_name(host, port);
        let text = self.read()?;
        let (mut same, mut other) = (Vec::new(), Vec::new());
        for raw in text.lines() {
            let Some(line) = parse_line(raw) else { continue };
            if line.marker.is_some() || match_patterns(line.patterns, &name) == Hit::No {
                continue;
            }
            let Some(recorded) = line.key() else { continue };
            if recorded.key_data() == key.key_data() {
                return Ok(Lookup::Known);
            }
            if recorded.algorithm() == key.algorithm() {
                same.push(recorded);
            } else {
                other.push(recorded);
            }
        }
        Ok(if same.is_empty() { Lookup::New { other } } else { Lookup::Changed { recorded: same } })
    }

    /// Algorithms of the keys recorded for `host:port`, used to ask the server
    /// for a key type we already know (like OpenSSH does).
    pub fn recorded_algorithms(&self, host: &str, port: u16) -> io::Result<Vec<Algorithm>> {
        let name = host_name(host, port);
        let text = self.read()?;
        let mut algorithms = Vec::new();
        for raw in text.lines() {
            let Some(line) = parse_line(raw) else { continue };
            if line.marker.is_some() || match_patterns(line.patterns, &name) == Hit::No {
                continue;
            }
            if let Some(key) = line.key()
                && !algorithms.contains(&key.algorithm())
            {
                algorithms.push(key.algorithm());
            }
        }
        Ok(algorithms)
    }

    /// Appends `key` for `host:port`, creating the file (mode 0600) and its
    /// directory (mode 0700) if needed.
    pub fn add(&self, host: &str, port: u16, key: &PublicKey) -> io::Result<()> {
        let entry = entry_line(host, port, key)?;
        let mut text = self.read_strict()?;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&entry);
        text.push('\n');
        self.write(&text)
    }

    /// Records `key` for `host:port` in place of the recorded keys of the same
    /// algorithm. Entries that list other hosts too keep those hosts with the
    /// old key; everything else in the file is preserved.
    pub fn replace(&self, host: &str, port: u16, key: &PublicKey) -> io::Result<()> {
        let name = host_name(host, port);
        let entry = entry_line(host, port, key)?;
        let text = self.read_strict()?;
        let mut out: Vec<String> = Vec::new();
        let mut inserted = false;
        for raw in text.lines() {
            let stale = parse_line(raw).filter(|line| {
                line.marker.is_none()
                    && line.key().is_some_and(|k| k.algorithm() == key.algorithm() && k.key_data() != key.key_data())
            });
            let Some(line) = stale else {
                out.push(raw.to_string());
                continue;
            };
            let all: Vec<&str> = line.patterns.split(',').collect();
            let kept: Vec<&str> =
                all.iter().copied().filter(|p| match_patterns(p, &name) != Hit::Exact || p.starts_with('!')).collect();
            if kept.len() == all.len() {
                out.push(raw.to_string());
                continue;
            }
            if !kept.is_empty() {
                out.push(raw.replacen(line.patterns, &kept.join(","), 1));
            }
            if !inserted {
                out.push(entry.clone());
                inserted = true;
            }
        }
        if !inserted {
            out.push(entry);
        }
        let mut text = out.join("\n");
        text.push('\n');
        self.write(&text)
    }

    /// File content, empty if the file does not exist. Invalid UTF-8 is replaced.
    fn read(&self) -> io::Result<String> {
        match fs::read(&self.path) {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(err) => Err(err),
        }
    }

    /// Like [`read`](Self::read) but refuses non-UTF-8 content, so a rewrite
    /// can never corrupt it.
    fn read_strict(&self) -> io::Result<String> {
        match fs::read(&self.path) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "known_hosts 不是有效的 UTF-8 文本")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(err) => Err(err),
        }
    }

    /// Replaces the file atomically (temp file + rename), following symlinks
    /// and keeping the permissions of an existing file.
    fn write(&self, text: &str) -> io::Result<()> {
        let target = fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir(dir)?;
        }
        let tmp = target.with_extension("shellmate-tmp");
        let result = (|| {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&tmp)?;
            if let Ok(meta) = fs::metadata(&target) {
                let _ = fs::set_permissions(&tmp, meta.permissions());
            }
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, &target)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// `host` or `[host]:port`, lowercased: the form OpenSSH matches and hashes.
fn host_name(host: &str, port: u16) -> String {
    let host = host.to_lowercase();
    if port == 22 { host } else { format!("[{host}]:{port}") }
}

/// `<host> <algorithm> <base64>` without the key comment.
fn entry_line(host: &str, port: u16, key: &PublicKey) -> io::Result<String> {
    let openssh = key.to_openssh().map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    let mut parts = openssh.split_whitespace();
    let (Some(algorithm), Some(blob)) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "无法编码主机密钥"));
    };
    Ok(format!("{} {algorithm} {blob}", host_name(host, port)))
}

struct Line<'a> {
    marker: Option<&'a str>,
    patterns: &'a str,
    algorithm: &'a str,
    blob: &'a str,
}

impl Line<'_> {
    fn key(&self) -> Option<PublicKey> {
        PublicKey::from_openssh(&format!("{} {}", self.algorithm, self.blob)).ok()
    }
}

fn parse_line(raw: &str) -> Option<Line<'_>> {
    let line = raw.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split_whitespace();
    let first = fields.next()?;
    let (marker, patterns) = if first.starts_with('@') { (Some(first), fields.next()?) } else { (None, first) };
    Some(Line { marker, patterns, algorithm: fields.next()?, blob: fields.next()? })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Hit {
    No,
    Wildcard,
    Exact,
}

fn match_patterns(patterns: &str, name: &str) -> Hit {
    let mut hit = Hit::No;
    for pattern in patterns.split(',') {
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        let this = match_pattern(pattern, name);
        if this == Hit::No {
            continue;
        }
        if negated {
            return Hit::No;
        }
        hit = hit.max(this);
    }
    hit
}

fn match_pattern(pattern: &str, name: &str) -> Hit {
    if let Some(hashed) = pattern.strip_prefix("|1|") {
        let mut parts = hashed.split('|');
        let (Some(salt), Some(hash)) = (parts.next(), parts.next()) else { return Hit::No };
        let (Ok(salt), Ok(hash)) = (BASE64.decode(salt.as_bytes()), BASE64.decode(hash.as_bytes())) else {
            return Hit::No;
        };
        let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&salt) else { return Hit::No };
        mac.update(name.as_bytes());
        return if mac.verify_slice(&hash).is_ok() { Hit::Exact } else { Hit::No };
    }
    if pattern.contains(['*', '?']) {
        return if glob(pattern.to_lowercase().as_bytes(), name.as_bytes()) { Hit::Wildcard } else { Hit::No };
    }
    if pattern.eq_ignore_ascii_case(name) { Hit::Exact } else { Hit::No }
}

/// `*` / `?` wildcard match.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
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
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use russh::keys::key::safe_rng;
    use russh::keys::{EcdsaCurve, PrivateKey};

    use super::*;

    fn key(algorithm: Algorithm) -> PublicKey {
        PrivateKey::random(&mut safe_rng(), algorithm).unwrap().public_key().clone()
    }

    fn line(host: &str, key: &PublicKey) -> String {
        format!("{host} {}", entry_line("x", 22, key).unwrap().split_once(' ').unwrap().1)
    }

    fn file(dir: &tempfile::TempDir, content: &str) -> KnownHosts {
        let path = dir.path().join("known_hosts");
        fs::write(&path, content).unwrap();
        KnownHosts::new(path)
    }

    #[test]
    fn glob_matching() {
        assert!(glob(b"*.example.com", b"a.example.com"));
        assert!(glob(b"10.0.?.*", b"10.0.1.99"));
        assert!(!glob(b"*.example.com", b"example.org"));
        assert!(glob(b"*", b""));
    }

    #[test]
    fn hashed_hosts_match() {
        // `ssh-keygen -H` style entry for "example.com" (salt/hash from russh's own test data).
        let pattern = "|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak=";
        assert_eq!(match_patterns(pattern, "example.com"), Hit::Exact);
        assert_eq!(match_patterns(pattern, "example.org"), Hit::No);
    }

    #[test]
    fn negated_and_listed_patterns() {
        assert_eq!(match_patterns("a,b", "b"), Hit::Exact);
        assert_eq!(match_patterns("*.x.com,!bad.x.com", "bad.x.com"), Hit::No);
        assert_eq!(match_patterns("*.x.com,!bad.x.com", "good.x.com"), Hit::Wildcard);
    }

    #[test]
    fn lookup_known_new_and_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, other_alg) =
            (key(Algorithm::Ed25519), key(Algorithm::Ed25519), key(Algorithm::Ecdsa { curve: EcdsaCurve::NistP256 }));
        let hosts =
            file(&dir, &format!("# comment\n{}\n[h2]:2222 {}\n", line("h1", &a).replace("h1", "H1"), "garbage"));
        assert_eq!(hosts.lookup("h1", 22, &a).unwrap(), Lookup::Known);
        assert_eq!(hosts.lookup("h1", 22, &b).unwrap(), Lookup::Changed { recorded: vec![a.clone()] });
        assert_eq!(hosts.lookup("h1", 22, &other_alg).unwrap(), Lookup::New { other: vec![a.clone()] });
        assert_eq!(hosts.lookup("unknown", 22, &a).unwrap(), Lookup::New { other: vec![] });
        assert_eq!(hosts.lookup("h1", 2222, &a).unwrap(), Lookup::New { other: vec![] });
        assert_eq!(hosts.recorded_algorithms("h1", 22).unwrap(), vec![Algorithm::Ed25519]);
    }

    #[test]
    fn missing_file_means_new() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = KnownHosts::new(dir.path().join("nope/known_hosts"));
        assert_eq!(hosts.lookup("h", 22, &key(Algorithm::Ed25519)).unwrap(), Lookup::New { other: vec![] });
    }

    #[test]
    fn add_creates_private_file_and_keeps_existing_lines() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = KnownHosts::new(dir.path().join("sub/known_hosts"));
        let (a, b) = (key(Algorithm::Ed25519), key(Algorithm::Ed25519));
        hosts.add("h1", 22, &a).unwrap();
        hosts.add("h2", 2222, &b).unwrap();
        let text = fs::read_to_string(hosts.path()).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().nth(1).unwrap().starts_with("[h2]:2222 ssh-ed25519 "));
        assert_eq!(hosts.lookup("h1", 22, &a).unwrap(), Lookup::Known);
        assert_eq!(hosts.lookup("h2", 2222, &b).unwrap(), Lookup::Known);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(hosts.path()).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(hosts.path().parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn add_appends_after_a_missing_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = file(&dir, "# no newline at the end");
        hosts.add("h", 22, &key(Algorithm::Ed25519)).unwrap();
        let text = fs::read_to_string(hosts.path()).unwrap();
        assert!(text.starts_with("# no newline at the end\nh ssh-ed25519 "));
    }

    #[test]
    fn replace_swaps_the_recorded_key_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new, other) = (key(Algorithm::Ed25519), key(Algorithm::Ed25519), key(Algorithm::Ed25519));
        let hosts = file(&dir, &format!("# top\n{}\n{}\n# bottom\n", line("h1", &old), line("h2", &other)));
        hosts.replace("h1", 22, &new).unwrap();
        let text = fs::read_to_string(hosts.path()).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert_eq!(lines[0], "# top");
        assert!(lines[1].starts_with("h1 ssh-ed25519 "));
        assert_eq!(lines[3], "# bottom");
        assert_eq!(hosts.lookup("h1", 22, &new).unwrap(), Lookup::Known);
        assert_eq!(hosts.lookup("h1", 22, &old).unwrap(), Lookup::Changed { recorded: vec![new] });
        assert_eq!(hosts.lookup("h2", 22, &other).unwrap(), Lookup::Known);
    }

    #[test]
    fn replace_splits_shared_entries_and_keeps_other_algorithms() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new, other_alg) =
            (key(Algorithm::Ed25519), key(Algorithm::Ed25519), key(Algorithm::Ecdsa { curve: EcdsaCurve::NistP256 }));
        let hosts = file(&dir, &format!("{}\n{}\n", line("h1,h2", &old), line("h1", &other_alg)));
        hosts.replace("h1", 22, &new).unwrap();
        assert_eq!(hosts.lookup("h1", 22, &new).unwrap(), Lookup::Known);
        assert_eq!(hosts.lookup("h2", 22, &old).unwrap(), Lookup::Known, "other host keeps the old key");
        assert_eq!(hosts.lookup("h1", 22, &other_alg).unwrap(), Lookup::Known, "other algorithm untouched");
        assert!(fs::read_to_string(hosts.path()).unwrap().lines().any(|l| l.starts_with("h2 ssh-ed25519 ")));
    }

    #[test]
    fn replace_appends_when_nothing_matches() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = file(&dir, "# only a comment\n");
        let k = key(Algorithm::Ed25519);
        hosts.replace("h", 22, &k).unwrap();
        assert_eq!(hosts.lookup("h", 22, &k).unwrap(), Lookup::Known);
    }

    #[test]
    fn replace_works_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real_known_hosts");
        let (old, new) = (key(Algorithm::Ed25519), key(Algorithm::Ed25519));
        fs::write(&real, format!("{}\n", line("h", &old))).unwrap();
        let link = dir.path().join("known_hosts");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let hosts = KnownHosts::new(&link);
        hosts.replace("h", 22, &new).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(KnownHosts::new(&real).lookup("h", 22, &new).unwrap(), Lookup::Known);
    }
}
