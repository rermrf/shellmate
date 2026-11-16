//! Versioned authenticated encryption. This crate does not access application data.
//! All decrypted buffers, passwords and keys must remain in zeroizing owners.
pub mod biometric;
mod idle;
mod strength;
use argon2::{Algorithm, Argon2, Block, Params, Version};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
pub use idle::{Clock, IdleLock, MonotonicClock};
use std::{
    fmt,
    time::{Duration, Instant},
};
pub use strength::{PASSWORD_MINIMUM, Strength, acceptable, strength};
use zeroize::Zeroizing;

pub type Password = Zeroizing<String>;
pub type Plaintext = Zeroizing<Vec<u8>>;
pub const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 64;
const BUNDLE_MAGIC: &[u8; 8] = b"SMBUNDLE";
const KEY_MAGIC: &[u8; 8] = b"SMVAULTK";
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Authentication,
    UnsupportedVersion,
    InvalidFormat,
    Random,
    Kdf,
    WeakPassword,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Authentication => "口令不正确，或文件已损坏 / 被篡改",
            Self::UnsupportedVersion => "不支持此格式版本，请升级 Shellmate",
            Self::InvalidFormat => "文件格式无效或已损坏",
            Self::Random => "系统随机数不可用",
            Self::Kdf => "密钥派生参数无效或超出安全限制",
            Self::WeakPassword => PASSWORD_MINIMUM,
        })
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}
impl Default for KdfParams {
    fn default() -> Self {
        Self {
            memory_kib: 65536,
            iterations: 3,
            parallelism: 1,
        }
    }
}
impl KdfParams {
    fn params(self) -> Result<Params> {
        // Bound unauthenticated headers before allocating or deriving (hostile files).
        if !(8192..=262144).contains(&self.memory_kib)
            || !(1..=12).contains(&self.iterations)
            || !(1..=4).contains(&self.parallelism)
        {
            return Err(Error::Kdf);
        }
        Params::new(self.memory_kib, self.iterations, self.parallelism, Some(32))
            .map_err(|_| Error::Kdf)
    }
}
fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| Error::Random)?;
    Ok(bytes)
}
fn derive(password: &str, salt: &[u8], params: KdfParams) -> Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0; 32]);
    let params = params.params()?;
    let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(
            password.as_bytes(),
            salt,
            key.as_mut(),
            memory.as_mut_slice(),
        )
        .map_err(|_| Error::Kdf)?;
    Ok(key)
}
fn seal(key: &[u8; 32], nonce: &[u8; 24], aad: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
    XChaCha20Poly1305::new(key.into())
        .encrypt(XNonce::from_slice(nonce), Payload { msg: plain, aad })
        .map_err(|_| Error::Authentication)
}
fn open(key: &[u8; 32], nonce: &[u8; 24], aad: &[u8], cipher: &[u8]) -> Result<Plaintext> {
    XChaCha20Poly1305::new(key.into())
        .decrypt(XNonce::from_slice(nonce), Payload { msg: cipher, aad })
        .map(Zeroizing::new)
        .map_err(|_| Error::Authentication)
}
/// Never implements Debug; dropping a key wipes its bytes.
pub struct DataKey(Zeroizing<[u8; 32]>);
impl DataKey {
    pub fn generate() -> Result<Self> {
        let mut key = Zeroizing::new([0; 32]);
        getrandom::fill(key.as_mut()).map_err(|_| Error::Random)?;
        Ok(Self(key))
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            return Err(Error::InvalidFormat);
        }
        let mut key = Zeroizing::new([0; 32]);
        key.copy_from_slice(bytes);
        Ok(Self(key))
    }
    /// For the device-key adapter only. Callers must never format or retain a plain copy.
    pub fn expose(&self) -> &[u8] {
        self.0.as_ref()
    }
    pub fn encrypt(&self, id: &str, plain: &[u8]) -> Result<EncryptedEntry> {
        let nonce = random()?;
        let ciphertext = seal(&self.0, &nonce, &entry_aad(id), plain)?;
        Ok(EncryptedEntry { nonce, ciphertext })
    }
    pub fn decrypt(&self, id: &str, entry: &EncryptedEntry) -> Result<Plaintext> {
        open(&self.0, &entry.nonce, &entry_aad(id), &entry.ciphertext)
    }
    pub fn wrap(&self, password: &str, params: KdfParams) -> Result<Vec<u8>> {
        password_encrypt(KEY_MAGIC, password, params, self.expose())
    }
    pub fn unwrap(password: &str, blob: &[u8]) -> Result<Self> {
        Self::from_bytes(&password_decrypt(KEY_MAGIC, password, blob)?)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedEntry {
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
}
fn entry_aad(id: &str) -> Vec<u8> {
    let mut aad = b"Shellmate credential\0".to_vec();
    aad.extend(FORMAT_VERSION.to_le_bytes());
    aad.extend(id.as_bytes());
    aad
}
fn password_encrypt(
    magic: &[u8; 8],
    password: &str,
    params: KdfParams,
    plain: &[u8],
) -> Result<Vec<u8>> {
    if !acceptable(password) {
        return Err(Error::WeakPassword);
    }
    if plain.len() > MAX_PAYLOAD {
        return Err(Error::InvalidFormat);
    }
    let salt = random::<16>()?;
    let nonce = random::<24>()?;
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend(magic);
    header.extend(FORMAT_VERSION.to_le_bytes());
    for n in [params.memory_kib, params.iterations, params.parallelism] {
        header.extend(n.to_le_bytes());
    }
    header.extend(salt);
    header.extend(nonce);
    let kek = derive(password, &salt, params)?;
    let cipher = seal(&kek, &nonce, &header, plain)?;
    header.extend(cipher);
    Ok(header)
}
fn password_decrypt(magic: &[u8; 8], password: &str, blob: &[u8]) -> Result<Plaintext> {
    let (params, salt, nonce) = header(magic, blob)?;
    let kek = derive(password, salt, params)?;
    open(&kek, nonce, &blob[..HEADER_LEN], &blob[HEADER_LEN..])
}
fn header<'a>(magic: &[u8; 8], blob: &'a [u8]) -> Result<(KdfParams, &'a [u8], &'a [u8; 24])> {
    if blob.len() < HEADER_LEN + 16
        || blob.len() > HEADER_LEN + 16 + MAX_PAYLOAD
        || &blob[..8] != magic
    {
        return Err(Error::InvalidFormat);
    }
    let n = |offset| u32::from_le_bytes(blob[offset..offset + 4].try_into().unwrap());
    if n(8) != FORMAT_VERSION {
        return Err(Error::UnsupportedVersion);
    }
    let params = KdfParams {
        memory_kib: n(12),
        iterations: n(16),
        parallelism: n(20),
    };
    params.params()?;
    Ok((params, &blob[24..40], blob[40..64].try_into().unwrap()))
}
pub fn wrapped_parameters(blob: &[u8]) -> Result<(KdfParams, Vec<u8>)> {
    let (p, s, _) = header(KEY_MAGIC, blob)?;
    Ok((p, s.to_vec()))
}
pub fn encrypt_bundle(password: &str, params: KdfParams, json: &[u8]) -> Result<Vec<u8>> {
    password_encrypt(BUNDLE_MAGIC, password, params, json)
}
pub fn decrypt_bundle(password: &str, blob: &[u8]) -> Result<Plaintext> {
    password_decrypt(BUNDLE_MAGIC, password, blob)
}
/// Starts at 64 MiB / 3 / 1 and targets 0.3–0.8 seconds on the current
/// machine/build. Memory never falls below 64 MiB or iterations below 3. Parameters travel with the
/// wrapped key; later unlocks never re-calibrate an existing vault.
/// The result is cached once per process, including concurrent calls. The first
/// caller must run off the UI thread; Setup, Change and Export share this cache.
pub fn calibrate() -> Result<(KdfParams, Duration)> {
    static CALIBRATED: std::sync::OnceLock<Result<(KdfParams, Duration)>> =
        std::sync::OnceLock::new();
    *CALIBRATED.get_or_init(calibrate_uncached)
}

#[cfg(test)]
static CALIBRATION_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn calibrate_uncached() -> Result<(KdfParams, Duration)> {
    #[cfg(test)]
    CALIBRATION_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut params = KdfParams::default();
    for _ in 0..12 {
        let start = Instant::now();
        let _key = derive("calibration only", &[0x42; 16], params)?;
        let elapsed = start.elapsed();
        if (Duration::from_millis(300)..=Duration::from_millis(800)).contains(&elapsed) {
            return Ok((params, elapsed));
        }
        if elapsed > Duration::from_millis(800) {
            if params.iterations == 3 {
                return Ok((params, elapsed));
            }
            params.iterations -= 1;
        } else if params.iterations < 12 {
            params.iterations = ((params.iterations as f64 * 0.5 / elapsed.as_secs_f64()).round()
                as u32)
                .clamp(params.iterations + 1, 12);
        } else if params.memory_kib < 262144 {
            params.memory_kib *= 2;
        } else {
            return Ok((params, elapsed));
        }
    }
    let start = Instant::now();
    let _key = derive("calibration only", &[0x42; 16], params)?;
    Ok((params, start.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fast() -> KdfParams {
        KdfParams {
            memory_kib: 8192,
            iterations: 1,
            parallelism: 1,
        }
    }
    #[test]
    fn roundtrip_wrong_password_and_rewrap() {
        let key = DataKey::generate().unwrap();
        let entry = key.encrypt("one", b"secret").unwrap();
        let wrapped = key.wrap("Long9!MeadowRiver", fast()).unwrap();
        assert!(DataKey::unwrap("wrong", &wrapped).is_err());
        let key = DataKey::unwrap("Long9!MeadowRiver", &wrapped).unwrap();
        let wrapped = key.wrap("Replacement9!River", fast()).unwrap();
        assert!(DataKey::unwrap("Long9!MeadowRiver", &wrapped).is_err());
        assert_eq!(
            &*DataKey::unwrap("Replacement9!River", &wrapped)
                .unwrap()
                .decrypt("one", &entry)
                .unwrap(),
            b"secret"
        );
    }
    #[test]
    fn both_master_and_export_passwords_reject_long_weak_values() {
        let key = DataKey::generate().unwrap();
        for weak in ["1234567890", "aaaaaaaaaaaaaaaa", "Password123!"] {
            assert_eq!(key.wrap(weak, fast()).err(), Some(Error::WeakPassword));
            assert_eq!(
                encrypt_bundle(weak, fast(), b"{}").err(),
                Some(Error::WeakPassword)
            );
        }
    }
    #[test]
    fn binds_id_nonce_and_ciphertext() {
        let key = DataKey::generate().unwrap();
        let e = key.encrypt("one", b"secret").unwrap();
        assert!(key.decrypt("two", &e).is_err());
        let mut n = e.clone();
        n.nonce[0] ^= 1;
        assert!(key.decrypt("one", &n).is_err());
        let mut c = e;
        c.ciphertext[0] ^= 1;
        assert!(key.decrypt("one", &c).is_err());
    }
    #[test]
    fn bundle_authenticates_all_header_fields() {
        let blob = encrypt_bundle("Export9!Meadow", fast(), br#"{"hosts":[]}"#).unwrap();
        assert_eq!(
            &*decrypt_bundle("Export9!Meadow", &blob).unwrap(),
            br#"{"hosts":[]}"#
        );
        assert_eq!(
            decrypt_bundle("wrong", &blob).err(),
            Some(Error::Authentication)
        );
        for i in [0, 8, 12, 16, 20, 24, 40, 64] {
            let mut bad = blob.clone();
            bad[i] ^= 1;
            assert!(decrypt_bundle("Export9!Meadow", &bad).is_err(), "byte {i}");
        }
        assert_eq!(
            decrypt_bundle("x", &blob[..30]).err(),
            Some(Error::InvalidFormat)
        );
    }
    #[test]
    fn hostile_kdf_is_bounded() {
        let mut blob = encrypt_bundle("Export9!Meadow", fast(), b"{}").unwrap();
        blob[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decrypt_bundle("x", &blob).err(), Some(Error::Kdf));
    }
    #[test]
    fn calibration_is_shared_by_concurrent_and_later_operations() {
        let results = std::thread::scope(|scope| {
            let jobs = (0..8).map(|_| scope.spawn(calibrate)).collect::<Vec<_>>();
            jobs.into_iter()
                .map(|job| job.join().unwrap().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(results.iter().all(|result| *result == results[0]));
        assert_eq!(calibrate().unwrap(), results[0]);
        assert_eq!(
            CALIBRATION_RUNS.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(results[0].0.memory_kib >= 65536 && results[0].0.iterations >= 3);
    }
    #[test]
    #[ignore = "explicit CPU calibration; no user data"]
    fn measure_unlock() {
        let (p, d) = calibrate().unwrap();
        println!("Argon2id: {p:?}, {} ms", d.as_millis());
    }
}
