//! Public metadata of private keys, and OpenSSH fingerprint visualizations.

use russh::keys::{PrivateKey, ssh_key::Fingerprint};
use std::path::Path;
use zeroize::Zeroizing;

/// Metadata inspected without decrypting a private key or retaining its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    /// OpenSSH algorithm label.
    pub algorithm: String,
    /// Whether a passphrase is required.
    pub encrypted: bool,
    /// Encryption cipher, e.g. `aes256-ctr`.
    pub cipher: String,
    /// Key derivation function, e.g. `bcrypt-pbkdf`.
    pub kdf: String,
}

impl KeyInfo {
    /// Read an explicitly selected key. The byte buffer is zeroized even on parse failure.
    pub fn read(path: &Path) -> Result<Self, String> {
        let bytes = Zeroizing::new(std::fs::read(path).map_err(|e| format!("无法读取私钥：{e}"))?);
        let key = PrivateKey::from_openssh(&*bytes).map_err(|e| format!("无法解析私钥：{e}"))?;
        Ok(Self {
            algorithm: key.algorithm().as_str().strip_prefix("ssh-").unwrap_or(key.algorithm().as_str()).to_uppercase(),
            encrypted: key.is_encrypted(),
            cipher: key.cipher().to_string(),
            kdf: match key.kdf().algorithm().as_str() {
                "bcrypt" => "bcrypt-pbkdf".into(),
                name => name.into(),
            },
        })
    }

    /// Description displayed next to the key type badge.
    pub fn description(&self) -> String {
        if self.encrypted {
            format!("已加密 · {} + {}，需要口令", self.kdf, self.cipher)
        } else {
            "未加密".into()
        }
    }
}

/// Render the OpenSSH 17×9 drunken-bishop field from a SHA256 fingerprint.
/// `header` includes the brackets, for example `[ED25519 256]`.
pub fn randomart(fingerprint: &str, header: &str) -> Result<String, String> {
    fingerprint.parse::<Fingerprint>().map(|fp| fp.to_randomart(header)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::{Algorithm, key::safe_rng, ssh_key::LineEnding};

    #[test]
    fn metadata_of_generated_plain_and_encrypted_keys_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("selected-key");
        let key = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
        std::fs::write(&path, key.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
        let info = KeyInfo::read(&path).unwrap();
        assert_eq!(info.algorithm, "ED25519");
        assert!(!info.encrypted);
        assert_eq!(info.description(), "未加密");
        let encrypted = key.encrypt(&mut safe_rng(), "test-passphrase").unwrap();
        std::fs::write(&path, encrypted.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
        let info = KeyInfo::read(&path).unwrap();
        assert!(info.encrypted);
        assert_eq!(info.kdf, "bcrypt-pbkdf");
        assert!(info.description().contains(&info.cipher));
        std::fs::write(&path, b"not a key").unwrap();
        assert!(KeyInfo::read(&path).unwrap_err().starts_with("无法解析私钥"));
        assert!(KeyInfo::read(&dir.path().join("missing")).unwrap_err().starts_with("无法读取私钥"));
    }

    #[test]
    fn randomart_agrees_with_installed_openssh_for_a_temporary_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
        let path = dir.path().join("fixture.pub");
        std::fs::write(&path, key.public_key().to_openssh().unwrap()).unwrap();
        let output = match std::process::Command::new("/usr/bin/ssh-keygen")
            .args(["-lv", "-E", "sha256", "-f"])
            .arg(path)
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("ssh-keygen: {error}"),
        };
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        let expected = output.lines().skip(1).collect::<Vec<_>>().join("\n");
        assert_eq!(
            randomart(&key.fingerprint(russh::keys::HashAlg::Sha256).to_string(), "[ED25519 256]").unwrap(),
            expected
        );
    }

    #[test]
    fn randomart_matches_openssh_sha256_output() {
        // OpenSSH ssh-keygen -lv -E sha256 output; 17 columns, 9 rows, start/end markers.
        let expected =
            "+--[ED25519 256]--+\n|o+oO==+ o..      |\n|.o++Eo+o..       |\n|. +.oO.o . .     |\n| . o..B.. . .    |";
        let expected = format!(
            "{expected}\n|  ...+ .S. o     |\n|  .o. . . . .    |\n|  o..    o       |\n|   B      .      |\n|  .o*            |\n+----[SHA256]-----+"
        );
        assert_eq!(randomart("SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ", "[ED25519 256]").unwrap(), expected);
        assert!(randomart("invalid", "[ED25519 256]").is_err());
    }
}
