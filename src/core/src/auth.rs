//! Passphrase hashing for the optional desktop lock. This does not encrypt stored chats.
use argon2::{password_hash::SaltString, Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use rand::{rngs::OsRng, RngCore};
use thiserror::Error;
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("Choose a passphrase of at least 10 characters and at most 1,024 bytes.")]
    Length,
    #[error("The passphrase could not be protected. Try again.")]
    Hash,
}
pub fn hash(passphrase: &str) -> Result<String, AuthError> {
    if passphrase.chars().count() < 10 || passphrase.len() > 1024 {
        return Err(AuthError::Length);
    }
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let salt = SaltString::encode_b64(&bytes).map_err(|_| AuthError::Hash)?;
    Argon2::default()
        .hash_password(passphrase.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| AuthError::Hash)
}
pub fn verify(passphrase: &str, hash: &str) -> bool {
    if passphrase.len() > 1024 || hash.len() > 512 {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    if parsed.algorithm.as_str() != "argon2id"
        || !parsed.params.get_decimal("m").is_some_and(|v| v <= 65536)
        || !parsed.params.get_decimal("t").is_some_and(|v| v <= 6)
        || !parsed.params.get_decimal("p").is_some_and(|v| v <= 4)
    {
        return false;
    }
    Argon2::default()
        .verify_password(passphrase.as_bytes(), &parsed)
        .is_ok()
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn salted_hashes_verify_without_retaining_plaintext() {
        let a = hash("a long test passphrase").unwrap();
        let b = hash("a long test passphrase").unwrap();
        assert_ne!(a, b);
        assert!(verify("a long test passphrase", &a));
        assert!(!verify("wrong", &a));
        assert!(hash("short").is_err());
        assert!(!a.contains("passphrase"));
    }
}
