//! Bundle encryption: Argon2id KDF + AES-256-GCM.
//!
//! Wire format:
//! ```text
//! [magic 7B "CLWDR1\0"] [salt 16B] [nonce 12B] [ciphertext+tag]
//! ```
//!
//! The 35-byte header (magic + salt + nonce) is fed to AES-GCM as AAD, so
//! tampering with any of those bytes — e.g. flipping a salt bit to coerce
//! a different KDF derivation — fails authentication on decrypt. The `tag`
//! is the standard 16-byte AES-GCM authentication tag, appended by the
//! AEAD encrypt step (and consumed by the decrypt step).
//!
//! KDF parameters (Argon2id, m=64 MiB / t=3 / p=1 → 32-byte key) are baked
//! in: bumping them is a bundle-format-version change, not a runtime knob.
//! Each export gets a fresh salt and nonce drawn from the OS RNG, so
//! re-encrypting the same plaintext with the same passphrase still
//! produces unrelated ciphertext (and avoids the cardinal AES-GCM sin of
//! reusing a (key, nonce) pair).
//!
//! When the binary is built without `--features encrypt`, both
//! [`encrypt_bundle`] and [`decrypt_bundle`] return a clear error rather
//! than silently degrading to plaintext.

use std::io::IsTerminal;

use crate::error::ClewdrError;

/// 7-byte magic that prefixes every encrypted bundle. Held in the import
/// path *before* this module existed (commit #7) so old binaries already
/// reject the encrypted form with a "supported in a follow-up commit"
/// hint instead of feeding it to `serde_json`.
pub const ENCRYPTED_BUNDLE_MAGIC: &[u8] = b"CLWDR1\0";
pub const SALT_LEN: usize = 16;
pub const NONCE_LEN: usize = 12;
pub const HEADER_LEN: usize = ENCRYPTED_BUNDLE_MAGIC.len() + SALT_LEN + NONCE_LEN;
pub const TAG_LEN: usize = 16;

#[cfg(feature = "encrypt")]
mod imp {
    use super::*;
    use aes_gcm::{
        Aes256Gcm, KeyInit, Nonce,
        aead::{Aead, Payload},
    };
    use argon2::{Algorithm, Argon2, Params, Version};
    use rand::RngExt;

    const KEY_LEN: usize = 32;
    /// 64 MiB. Bumping is a format-version change.
    const ARGON2_M_KIB: u32 = 64 * 1024;
    const ARGON2_T: u32 = 3;
    const ARGON2_P: u32 = 1;

    fn derive_key(passphrase: &str, salt: &[u8]) -> [u8; KEY_LEN] {
        let params = Params::new(ARGON2_M_KIB, ARGON2_T, ARGON2_P, Some(KEY_LEN))
            .expect("argon2 params constants are valid");
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut key = [0u8; KEY_LEN];
        argon2
            .hash_password_into(passphrase.as_bytes(), salt, &mut key)
            .expect("argon2 cannot fail with fixed-size salt + output");
        key
    }

    pub fn encrypt_bundle(plaintext: &[u8], passphrase: &str) -> Result<Vec<u8>, ClewdrError> {
        let mut salt = [0u8; SALT_LEN];
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill(&mut salt);
        rand::rng().fill(&mut nonce);

        let key = derive_key(passphrase, &salt);
        let cipher =
            Aes256Gcm::new_from_slice(&key).expect("AES-256-GCM key length is fixed at 32 bytes");

        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(ENCRYPTED_BUNDLE_MAGIC);
        header.extend_from_slice(&salt);
        header.extend_from_slice(&nonce);

        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &header,
                },
            )
            .map_err(|_| ClewdrError::BadRequest {
                msg: "AES-GCM encryption failed (unexpected; report a bug)",
            })?;

        let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    pub fn decrypt_bundle(raw: &[u8], passphrase: &str) -> Result<Vec<u8>, ClewdrError> {
        if raw.len() < HEADER_LEN + TAG_LEN {
            return Err(ClewdrError::BadRequest {
                msg: "encrypted bundle is too short to contain a valid header + AEAD tag",
            });
        }
        if !raw.starts_with(ENCRYPTED_BUNDLE_MAGIC) {
            return Err(ClewdrError::BadRequest {
                msg: "missing CLWDR1 magic — file is not an encrypted clewdr bundle",
            });
        }
        let header = &raw[..HEADER_LEN];
        let salt = &header[ENCRYPTED_BUNDLE_MAGIC.len()..ENCRYPTED_BUNDLE_MAGIC.len() + SALT_LEN];
        let nonce = &header[ENCRYPTED_BUNDLE_MAGIC.len() + SALT_LEN..];
        let ciphertext = &raw[HEADER_LEN..];

        let key = derive_key(passphrase, salt);
        let cipher =
            Aes256Gcm::new_from_slice(&key).expect("AES-256-GCM key length is fixed at 32 bytes");
        cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: header,
                },
            )
            .map_err(|_| ClewdrError::BadRequest {
                // GCM doesn't distinguish "wrong key" from "tampered tag";
                // both surface the same generic AEAD failure. The error
                // message covers both so the operator isn't told their
                // passphrase is wrong when the file was actually corrupted.
                msg: "passphrase or bundle is invalid (AEAD authentication failed)",
            })
    }
}

#[cfg(not(feature = "encrypt"))]
mod imp {
    use super::*;

    pub fn encrypt_bundle(_: &[u8], _: &str) -> Result<Vec<u8>, ClewdrError> {
        Err(ClewdrError::BadRequest {
            msg: "binary built without --features encrypt; pass --no-encrypt or rebuild with default features",
        })
    }

    pub fn decrypt_bundle(_: &[u8], _: &str) -> Result<Vec<u8>, ClewdrError> {
        Err(ClewdrError::BadRequest {
            msg: "this bundle is encrypted but the binary was built without --features encrypt; rebuild with default features to decrypt",
        })
    }
}

pub use imp::{decrypt_bundle, encrypt_bundle};

const PROMPT_HINT: &str = "Bundle passphrase (input is hidden): ";
const CONFIRM_HINT: &str = "Confirm passphrase: ";
const DECRYPT_HINT: &str = "Bundle passphrase: ";

/// Read a passphrase for `export-config`. Confirms via a second prompt on
/// the TTY path; `--passphrase-stdin` skips the confirmation since the
/// caller already controls the value being piped in.
pub fn read_export_passphrase(stdin: bool) -> Result<String, ClewdrError> {
    if stdin {
        return read_one_line_from_stdin();
    }
    if !std::io::stdin().is_terminal() {
        return Err(ClewdrError::BadRequest {
            msg: "no TTY available for passphrase prompt — pass --passphrase-stdin or --no-encrypt",
        });
    }
    let pwd = rpassword::prompt_password(PROMPT_HINT)?;
    if pwd.is_empty() {
        return Err(ClewdrError::BadRequest {
            msg: "passphrase cannot be empty",
        });
    }
    let confirm = rpassword::prompt_password(CONFIRM_HINT)?;
    if pwd != confirm {
        return Err(ClewdrError::BadRequest {
            msg: "passphrases do not match",
        });
    }
    Ok(pwd)
}

/// Read a passphrase for `import-config`. No confirmation prompt — there's
/// nothing to confirm against, and a wrong value surfaces immediately as
/// AEAD authentication failure.
pub fn read_import_passphrase(stdin: bool) -> Result<String, ClewdrError> {
    if stdin {
        return read_one_line_from_stdin();
    }
    if !std::io::stdin().is_terminal() {
        return Err(ClewdrError::BadRequest {
            msg: "no TTY available for passphrase prompt — pass --passphrase-stdin",
        });
    }
    let pwd = rpassword::prompt_password(DECRYPT_HINT)?;
    if pwd.is_empty() {
        return Err(ClewdrError::BadRequest {
            msg: "passphrase cannot be empty",
        });
    }
    Ok(pwd)
}

fn read_one_line_from_stdin() -> Result<String, ClewdrError> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let pwd = line.trim_end_matches(['\r', '\n']).to_string();
    if pwd.is_empty() {
        return Err(ClewdrError::BadRequest {
            msg: "empty passphrase received on stdin",
        });
    }
    Ok(pwd)
}

#[cfg(test)]
#[cfg(feature = "encrypt")]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let plaintext = b"the quick brown fox jumps over the lazy dog";
        let ciphertext = encrypt_bundle(plaintext, "correct horse battery staple").unwrap();
        assert!(ciphertext.starts_with(ENCRYPTED_BUNDLE_MAGIC));
        assert!(ciphertext.len() >= HEADER_LEN + TAG_LEN + plaintext.len());
        let restored = decrypt_bundle(&ciphertext, "correct horse battery staple").unwrap();
        assert_eq!(restored, plaintext);
    }

    #[test]
    fn each_encryption_uses_fresh_salt_and_nonce() {
        // Same plaintext + same passphrase must produce different
        // ciphertext on each call. Otherwise we'd be reusing the
        // (key, nonce) pair across exports — the cardinal AES-GCM sin
        // (reveals plaintext xor + lets the attacker forge tags).
        let plaintext = b"identical plaintext, two encryptions";
        let a = encrypt_bundle(plaintext, "pwd").unwrap();
        let b = encrypt_bundle(plaintext, "pwd").unwrap();
        assert_ne!(a, b);
        // The salt segment also differs by construction.
        let salt_a = &a[ENCRYPTED_BUNDLE_MAGIC.len()..ENCRYPTED_BUNDLE_MAGIC.len() + SALT_LEN];
        let salt_b = &b[ENCRYPTED_BUNDLE_MAGIC.len()..ENCRYPTED_BUNDLE_MAGIC.len() + SALT_LEN];
        assert_ne!(salt_a, salt_b);
    }

    #[test]
    fn wrong_passphrase_rejected() {
        let ciphertext = encrypt_bundle(b"data", "right-pwd").unwrap();
        assert!(decrypt_bundle(&ciphertext, "wrong-pwd").is_err());
    }

    #[test]
    fn header_tamper_rejected() {
        // Flipping any header byte must invalidate the AEAD tag, since the
        // 35-byte header is bound as AAD. Without that binding an attacker
        // could swap the salt for one whose key derivation matches a
        // pre-computed table.
        let mut ciphertext = encrypt_bundle(b"data", "pwd").unwrap();
        let salt_byte = ENCRYPTED_BUNDLE_MAGIC.len();
        ciphertext[salt_byte] ^= 0x01;
        assert!(decrypt_bundle(&ciphertext, "pwd").is_err());
    }

    #[test]
    fn nonce_tamper_rejected() {
        let mut ciphertext = encrypt_bundle(b"data", "pwd").unwrap();
        let nonce_byte = ENCRYPTED_BUNDLE_MAGIC.len() + SALT_LEN;
        ciphertext[nonce_byte] ^= 0x01;
        assert!(decrypt_bundle(&ciphertext, "pwd").is_err());
    }

    #[test]
    fn ciphertext_tamper_rejected() {
        let mut ciphertext = encrypt_bundle(b"data", "pwd").unwrap();
        *ciphertext.last_mut().unwrap() ^= 0x01;
        assert!(decrypt_bundle(&ciphertext, "pwd").is_err());
    }

    #[test]
    fn missing_magic_rejected() {
        assert!(decrypt_bundle(b"not an encrypted bundle whatsoever", "pwd").is_err());
    }

    #[test]
    fn truncated_input_rejected() {
        let mut ciphertext = encrypt_bundle(b"data", "pwd").unwrap();
        ciphertext.truncate(HEADER_LEN + 4); // shorter than the AEAD tag alone
        assert!(decrypt_bundle(&ciphertext, "pwd").is_err());
    }

    #[test]
    fn empty_plaintext_roundtrips() {
        let ciphertext = encrypt_bundle(b"", "pwd").unwrap();
        assert_eq!(ciphertext.len(), HEADER_LEN + TAG_LEN);
        assert_eq!(
            decrypt_bundle(&ciphertext, "pwd").unwrap(),
            Vec::<u8>::new()
        );
    }
}
