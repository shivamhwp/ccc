//! Encryption at rest for the token store.
//!
//! `store.enc` is `"CCC1"` magic + a 24-byte XChaCha20-Poly1305 nonce + the
//! ciphertext of the store JSON. The 32-byte master key is resolved once per
//! process, in order of preference:
//!   1. the file named by `CCC_KEY_FILE` (tests, headless machines),
//!   2. macOS: the login Keychain item `ccc-store-key`. It is created and
//!      read via the `security` CLI on purpose — items a CLI tool writes that
//!      way are readable through the same tool without ACL prompts, so both
//!      interactive `ccc` and the launchd daemon can use the key silently,
//!      and a binary upgrade doesn't invalidate access,
//!   3. `~/.ccc/key` (0600) elsewhere (Linux `.credentials.json` itself is a
//!      plain file, so a same-permission key file matches the platform's
//!      existing trust model).
//!
//! Key files hold 64 hex chars. A missing key is generated on first use.

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use std::path::Path;
use std::sync::OnceLock;

use crate::paths;

const MAGIC: &[u8; 4] = b"CCC1";
pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// Encrypt `plaintext` under `key` with a fresh random nonce.
pub fn seal(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .map_err(|_| anyhow!("encrypting store"))?;
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a blob produced by [`seal`].
pub fn open(key: &[u8; KEY_LEN], sealed: &[u8]) -> Result<Vec<u8>> {
    let body = sealed
        .strip_prefix(MAGIC.as_slice())
        .ok_or_else(|| anyhow!("not a ccc vault file"))?;
    if body.len() < NONCE_LEN {
        return Err(anyhow!("vault file is truncated"));
    }
    let (nonce, ct) = body.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), ct)
        .map_err(|_| anyhow!("store decryption failed (wrong or missing key)"))
}

/// The master key, resolved once per process and cached. Only a successful
/// resolution is cached, so a transiently unavailable Keychain is retried on
/// the next call instead of poisoning the process.
pub fn master_key() -> Result<[u8; KEY_LEN]> {
    static KEY: OnceLock<[u8; KEY_LEN]> = OnceLock::new();
    if let Some(k) = KEY.get() {
        return Ok(*k);
    }
    let k = resolve_key()?;
    Ok(*KEY.get_or_init(|| k))
}

fn resolve_key() -> Result<[u8; KEY_LEN]> {
    if let Some(path) = std::env::var_os("CCC_KEY_FILE") {
        return key_from_file(Path::new(&path));
    }
    #[cfg(target_os = "macos")]
    {
        match keychain_key() {
            Ok(k) => return Ok(k),
            Err(e) => eprintln!(
                "ccc: Keychain unavailable for the store key ({e:#}); using ~/.ccc/key instead"
            ),
        }
    }
    key_from_file(&paths::ccc_dir()?.join("key"))
}

/// Read a key file (64 hex chars), generating it on first use.
fn key_from_file(path: &Path) -> Result<[u8; KEY_LEN]> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_hex_key(text.trim())
            .with_context(|| format!("parsing key file {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = generate_key();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::creds::write_secret_file(path, hex_encode(&key).as_bytes())
                .with_context(|| format!("writing key file {}", path.display()))?;
            Ok(key)
        }
        Err(e) => Err(e).with_context(|| format!("reading key file {}", path.display())),
    }
}

/// macOS: the key as a Keychain generic password, created on first use.
#[cfg(target_os = "macos")]
fn keychain_key() -> Result<[u8; KEY_LEN]> {
    const SERVICE: &str = "ccc-store-key";
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", SERVICE, "-w"])
        .output()
        .context("running `security` to read the store key")?;
    if out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        return parse_hex_key(text.trim()).context("parsing the Keychain store key");
    }

    // Not found: create it. The secret stored is the hex string of the key;
    // `-X` passes it hex-encoded again so the `security -i` parser never sees
    // anything that needs quoting. `-U` keeps re-runs idempotent.
    let key = generate_key();
    let user = std::env::var("USER").unwrap_or_else(|_| "ccc".into());
    if user.chars().any(char::is_control) {
        return Err(anyhow!("USER contains control characters"));
    }
    let cmd = format!(
        "add-generic-password -U -s \"{SERVICE}\" -a \"{}\" -X {}\n",
        user.replace('\\', "\\\\").replace('"', "\\\""),
        hex_encode(hex_encode(&key).as_bytes()),
    );
    use std::io::Write;
    let mut child = std::process::Command::new("security")
        .arg("-i")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running `security` to store the key")?;
    let write_res = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(cmd.as_bytes());
    let out = child.wait_with_output()?;
    write_res.context("sending the key to `security`")?;
    if !out.status.success() {
        return Err(anyhow!(
            "storing the key in the Keychain failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(key)
}

fn generate_key() -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

fn parse_hex_key(text: &str) -> Result<[u8; KEY_LEN]> {
    let bytes = hex_decode(text)?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("key must be {KEY_LEN} bytes ({} hex chars)", KEY_LEN * 2))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn hex_decode(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(anyhow!("odd-length hex"));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|e| anyhow!("bad hex: {e}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let key = generate_key();
        let sealed = seal(&key, b"{\"version\":1}").unwrap();
        assert!(sealed.starts_with(MAGIC));
        assert_eq!(open(&key, &sealed).unwrap(), b"{\"version\":1}");
    }

    #[test]
    fn tampering_and_wrong_key_fail() {
        let key = generate_key();
        let mut sealed = seal(&key, b"secret").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(open(&key, &sealed).is_err());

        let sealed = seal(&key, b"secret").unwrap();
        let other = generate_key();
        assert!(open(&other, &sealed).is_err());
    }

    #[test]
    fn plaintext_is_not_sealed() {
        assert!(open(&generate_key(), b"{\"version\":1}").is_err());
    }

    #[test]
    fn key_file_is_created_then_stable() {
        let dir = std::env::temp_dir().join(format!("ccc-vault-test-{}", std::process::id()));
        let path = dir.join("key");
        let _ = std::fs::remove_dir_all(&dir);
        let a = key_from_file(&path).unwrap();
        let b = key_from_file(&path).unwrap();
        assert_eq!(a, b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_roundtrip_and_validation() {
        let key = generate_key();
        assert_eq!(parse_hex_key(&hex_encode(&key)).unwrap(), key);
        assert!(parse_hex_key("abc").is_err());
        assert!(parse_hex_key("zz").is_err());
        assert!(parse_hex_key(&"00".repeat(16)).is_err()); // 16 bytes, not 32
    }
}
