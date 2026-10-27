//! Encryption at rest of eidra's own directory, `~/.eidra` (docs/DESIGN.md §18). A random key
//! encrypts what eidra keeps there: the session logs line by line, the user's AGENTS.md, the
//! shell's saved state, and the contents of the snapshots `/undo` puts back. `vault.json` holds the
//! key, encrypted with one made from the user's password (Argon2id), so the password is asked for
//! once each time eidra starts, and changing it would not mean encrypting everything again.
//!
//! Each use gets a key of its own, derived from the vault's (HKDF-SHA256), and everything is
//! sealed with XChaCha20-Poly1305: a random nonce, except for snapshots, whose nonce comes from
//! their content so that git stores an unchanged file once.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

/// The file in eidra's directory that holds the key, encrypted with the password's.
const FILE: &str = "vault.json";
/// The user's AGENTS.md, encrypted. A plain one left beside it is taken in at the next unlock.
const INSTRUCTIONS: &str = "AGENTS.md.enc";
/// Argon2id's cost: 64 MiB and three passes, about half a second on a laptop.
const MEMORY_KIB: u32 = 64 * 1024;
const PASSES: u32 = 3;
const NONCE: usize = 24;
const TAG: usize = 16;
/// What starts a snapshot's encrypted contents, so that contents from before the vault, which
/// lack it, still read as they are.
const SNAPSHOT_MAGIC: &[u8] = b"\0eidra-snapshot-1\0";

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    kdf: Kdf,
    /// The key, encrypted with the password's: the nonce, then the ciphertext, in base64.
    key: String,
}

#[derive(Serialize, Deserialize)]
struct Kdf {
    algorithm: String,
    memory_kib: u32,
    passes: u32,
    parallelism: u32,
    salt: String,
}

/// The vault's key, unlocked.
pub struct Vault {
    key: Zeroizing<[u8; 32]>,
}

/// What a sealed piece of data is: each has a key of its own.
#[derive(Debug, Clone, Copy)]
pub enum Purpose {
    /// A line of a session log.
    Log,
    /// A file of eidra's: the user's AGENTS.md, the shell's state, a snapshot's label.
    File,
}

impl Purpose {
    fn info(self) -> &'static [u8] {
        match self {
            Purpose::Log => b"eidra log",
            Purpose::File => b"eidra file",
        }
    }
}

impl Vault {
    /// Whether eidra's directory `home` is encrypted.
    pub fn exists(home: &Path) -> bool {
        home.join(FILE).is_file()
    }

    /// Makes the vault in `home`, with a new random key kept encrypted with `password`'s.
    pub fn create(home: &Path, password: &str) -> Result<Vault> {
        let path = home.join(FILE);
        if path.exists() {
            bail!("{} exists already", path.display());
        }
        let mut key = Zeroizing::new([0u8; 32]);
        random(&mut key[..])?;
        let mut salt = [0u8; 16];
        random(&mut salt)?;
        let kdf = Kdf {
            algorithm: "argon2id".into(),
            memory_kib: MEMORY_KIB,
            passes: PASSES,
            parallelism: 1,
            salt: STANDARD.encode(salt),
        };
        let wrapping = password_key(password, &kdf)?;
        let sealed = seal_with(&wrapping, FILE.as_bytes(), &key[..])?;
        let stored = Stored { version: 1, kdf, key: STANDARD.encode(sealed) };
        fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
        write_private(&path, &serde_json::to_vec_pretty(&stored)?)?;
        Ok(Vault { key })
    }

    /// The vault in `home`, unlocked with `password`; None if the password is wrong.
    pub fn unlock(home: &Path, password: &str) -> Result<Option<Vault>> {
        let path = home.join(FILE);
        let text = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let stored: Stored =
            serde_json::from_slice(&text).with_context(|| format!("reading {}", path.display()))?;
        if stored.version != 1 || stored.kdf.algorithm != "argon2id" {
            bail!("{} is of a kind this eidra cannot read", path.display());
        }
        let wrapping = password_key(password, &stored.kdf)?;
        let sealed = STANDARD.decode(&stored.key).context("the vault's key is not base64")?;
        let Ok(plain) = open_with(&wrapping, FILE.as_bytes(), &sealed) else {
            return Ok(None);
        };
        let plain = Zeroizing::new(plain);
        let key: [u8; 32] = plain[..].try_into().map_err(|_| anyhow!("the vault's key is cut"))?;
        Ok(Some(Vault { key: Zeroizing::new(key) }))
    }

    /// A vault with this key and no password, as tests need: a password's key takes a moment.
    #[doc(hidden)]
    pub fn with_key(key: [u8; 32]) -> Vault {
        Vault { key: Zeroizing::new(key) }
    }

    /// `plaintext` sealed for `purpose` and bound to `aad`: a random nonce, then the ciphertext.
    pub fn seal(&self, purpose: Purpose, aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        seal_with(&self.subkey(purpose.info()), aad, plaintext).expect("sealing in memory")
    }

    /// What `seal` sealed, if this vault sealed it for `purpose` with `aad` and nothing changed.
    pub fn open(&self, purpose: Purpose, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        open_with(&self.subkey(purpose.info()), aad, sealed)
    }

    /// The key snapshots' contents are sealed with, which git's filter process gets: it opens
    /// nothing else.
    pub fn snapshot_key(&self) -> Zeroizing<[u8; 32]> {
        self.subkey(b"eidra snapshot")
    }

    /// The user's AGENTS.md in `home`, decrypted, if there is one.
    pub fn instructions(&self, home: &Path) -> Result<Option<String>> {
        let path = home.join(INSTRUCTIONS);
        let sealed = match fs::read(&path) {
            Ok(sealed) => sealed,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let plain = self
            .open(Purpose::File, INSTRUCTIONS.as_bytes(), &sealed)
            .with_context(|| format!("decrypting {}", path.display()))?;
        Ok(Some(String::from_utf8(plain).context("the user's AGENTS.md is not UTF-8")?))
    }

    /// Encrypts a plain AGENTS.md left in `home`, in place of the encrypted one, and removes the
    /// plain one. Whether there was one.
    pub fn take_in_instructions(&self, home: &Path) -> Result<bool> {
        let plain_path = home.join("AGENTS.md");
        let Ok(text) = fs::read(&plain_path) else {
            return Ok(false);
        };
        let sealed = self.seal(Purpose::File, INSTRUCTIONS.as_bytes(), &text);
        write_private(&home.join(INSTRUCTIONS), &sealed)?;
        fs::remove_file(&plain_path)
            .with_context(|| format!("removing {}", plain_path.display()))?;
        Ok(true)
    }

    fn subkey(&self, info: &[u8]) -> Zeroizing<[u8; 32]> {
        derive(&self.key, info)
    }
}

/// Snapshot contents, sealed so that the same contents always seal the same way: the nonce is
/// a MAC of them. git then sees an unchanged file as unchanged and stores it once. That shows
/// which snapshotted files are equal, and nothing more about them.
pub struct SnapshotCipher {
    seal: Zeroizing<[u8; 32]>,
    nonce: Zeroizing<[u8; 32]>,
}

impl SnapshotCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        SnapshotCipher { seal: derive(key, b"seal"), nonce: derive(key, b"nonce") }
    }

    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.nonce[..])
            .expect("HMAC takes a key of any length");
        mac.update(plaintext);
        let tag = mac.finalize().into_bytes();
        let nonce: [u8; NONCE] = tag[..NONCE].try_into().expect("SHA-256 is longer than a nonce");
        let cipher = XChaCha20Poly1305::new(&(*self.seal).into());
        let payload = Payload { msg: plaintext, aad: SNAPSHOT_MAGIC };
        let ciphertext = cipher.encrypt(&XNonce::from(nonce), payload).expect("sealing in memory");
        [SNAPSHOT_MAGIC, &nonce, &ciphertext].concat()
    }

    /// The contents `seal` sealed; contents snapshotted before the vault, as they are.
    pub fn open(&self, data: &[u8]) -> Result<Vec<u8>> {
        match data.strip_prefix(SNAPSHOT_MAGIC) {
            Some(sealed) => open_with(&self.seal, SNAPSHOT_MAGIC, sealed),
            None => Ok(data.to_vec()),
        }
    }
}

/// A key for `info` from `key`.
fn derive(key: &[u8; 32], info: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut okm = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(None, key).expand(info, &mut okm[..]).expect("32 bytes is a valid length");
    okm
}

fn password_key(password: &str, kdf: &Kdf) -> Result<Zeroizing<[u8; 32]>> {
    let salt = STANDARD.decode(&kdf.salt).context("the vault's salt is not base64")?;
    let params = Params::new(kdf.memory_kib, kdf.passes, kdf.parallelism, Some(32))
        .map_err(|e| anyhow!("the vault's settings: {e}"))?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), &salt, &mut key[..])
        .map_err(|e| anyhow!("deriving the key from the password: {e}"))?;
    Ok(key)
}

fn seal_with(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE];
    random(&mut nonce)?;
    let cipher = XChaCha20Poly1305::new(&(*key).into());
    let ciphertext = cipher
        .encrypt(&XNonce::from(nonce), Payload { msg: plaintext, aad })
        .map_err(|_| anyhow!("encrypting failed"))?;
    Ok([&nonce[..], &ciphertext].concat())
}

fn open_with(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < NONCE + TAG {
        bail!("too short to be encrypted data");
    }
    let (nonce, ciphertext) = sealed.split_at(NONCE);
    let nonce = XNonce::try_from(nonce).expect("split at the nonce's length");
    XChaCha20Poly1305::new(&(*key).into())
        .decrypt(&nonce, Payload { msg: ciphertext, aad })
        .map_err(|_| anyhow!("it does not decrypt: another key's, or changed since"))
}

fn random(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf).map_err(|e| anyhow!("no random numbers from the system: {e}"))
}

/// Writes a file only its owner can read, whole: written beside it, then moved into place.
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let partial = path.with_extension("partial");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&partial)
        .with_context(|| format!("writing {}", partial.display()))?;
    file.write_all(contents).with_context(|| format!("writing {}", partial.display()))?;
    file.sync_all().ok();
    fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("eidra-vault-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_vault_opens_with_its_password_only() {
        let dir = home("password");
        let vault = Vault::create(&dir, "correct horse").unwrap();
        assert!(Vault::exists(&dir));
        assert!(Vault::create(&dir, "again").is_err(), "one vault per directory");
        let sealed = vault.seal(Purpose::Log, b"s:0", b"a line");
        assert!(Vault::unlock(&dir, "wrong horse").unwrap().is_none());
        let unlocked = Vault::unlock(&dir, "correct horse").unwrap().unwrap();
        assert_eq!(unlocked.open(Purpose::Log, b"s:0", &sealed).unwrap(), b"a line");
        // Bound to its place and use: another line's, or a file's, does not open it.
        assert!(unlocked.open(Purpose::Log, b"s:1", &sealed).is_err());
        assert!(unlocked.open(Purpose::File, b"s:0", &sealed).is_err());
        let mut changed = sealed.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(unlocked.open(Purpose::Log, b"s:0", &changed).is_err());
        // Nothing of the key or the password is in the file.
        let stored = fs::read_to_string(dir.join(FILE)).unwrap();
        assert!(!stored.contains("horse"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plain_agents_md_is_taken_in_and_removed() {
        let dir = home("instructions");
        fs::create_dir_all(&dir).unwrap();
        let vault = Vault::with_key([1; 32]);
        assert_eq!(vault.instructions(&dir).unwrap(), None);
        assert!(!vault.take_in_instructions(&dir).unwrap());
        fs::write(dir.join("AGENTS.md"), "Use tabs.\n").unwrap();
        assert!(vault.take_in_instructions(&dir).unwrap());
        assert!(!dir.join("AGENTS.md").exists());
        assert!(!fs::read(dir.join(INSTRUCTIONS)).unwrap().windows(4).any(|w| w == b"tabs"));
        assert_eq!(vault.instructions(&dir).unwrap().as_deref(), Some("Use tabs.\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshots_seal_the_same_way_each_time_and_older_ones_read_as_they_are() {
        let vault = Vault::with_key([1; 32]);
        let cipher = SnapshotCipher::new(&vault.snapshot_key());
        let sealed = cipher.seal(b"print('hi')\n");
        assert_eq!(sealed, cipher.seal(b"print('hi')\n"));
        assert_ne!(sealed, cipher.seal(b"print('ho')\n"));
        assert!(sealed.starts_with(SNAPSHOT_MAGIC));
        assert_eq!(cipher.open(&sealed).unwrap(), b"print('hi')\n");
        assert_eq!(cipher.open(b"from before the vault").unwrap(), b"from before the vault");
        assert_eq!(cipher.open(&cipher.seal(b"")).unwrap(), b"");
        let other = Vault::with_key([2; 32]);
        assert!(SnapshotCipher::new(&other.snapshot_key()).open(&sealed).is_err());
    }
}
