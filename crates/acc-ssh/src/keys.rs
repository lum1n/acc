//! Making a key of the app's own, for hosts to list in `authorized_keys`.

use russh::keys::{Algorithm, PrivateKey, ssh_key::LineEnding};

use crate::{Error, Result};

/// A new ed25519 key pair.
#[derive(Clone)]
pub struct KeyPair {
    /// The private key in OpenSSH format, for [`crate::Auth::key`].
    pub private_key: String,
    /// The `authorized_keys` line: `ssh-ed25519 AAAA… <comment>`.
    pub public_key: String,
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPair")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

/// Generates an ed25519 key pair whose public key carries `comment`.
pub fn generate_key(comment: &str) -> Result<KeyPair> {
    let mut key =
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).map_err(|e| Error::Key(e.to_string()))?;
    key.set_comment(comment);
    let private_key = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| Error::Key(e.to_string()))?
        .to_string();
    let public_key = key
        .public_key()
        .to_openssh()
        .map_err(|e| Error::Key(e.to_string()))?;
    Ok(KeyPair {
        private_key,
        public_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_load_and_carry_the_comment() -> Result<()> {
        let pair = generate_key("nudge@test")?;
        assert!(pair.public_key.starts_with("ssh-ed25519 "));
        assert!(pair.public_key.ends_with(" nudge@test"));
        crate::Auth::key(&pair.private_key, None)?;
        Ok(())
    }
}
