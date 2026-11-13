// SPDX-License-Identifier: MPL-2.0

//! Who may open a VM's console, the simple way: one user and its password, the server's, in its
//! configuration, whatever runs the VMs. The password is kept as an Argon2id hash, never in
//! clear: `qemu-dbus-webrtc hash-password` makes it.

use anyhow::{Result, bail};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

/// The server's user, and its password's Argon2id hash in the PHC format.
#[derive(Debug, Clone)]
pub struct Password {
    pub user: String,
    pub hash: String,
}

impl Password {
    /// The user and the password's hash, as the configuration gives them: checked now, so that a
    /// wrong hash fails the start rather than every login.
    pub fn new(user: String, hash: String) -> Result<Self> {
        if user.is_empty() {
            bail!("an empty user");
        }
        if PasswordHash::new(&hash).is_err() {
            bail!(
                "the password is not an Argon2 hash: make one with `qemu-dbus-webrtc hash-password`"
            );
        }
        Ok(Self { user, hash })
    }

    /// Whether `user` and `password` are the server's. Slow on purpose (Argon2): call it off
    /// the async threads. The hash is checked whatever the user, so that the time tells
    /// nothing.
    pub fn check(&self, user: &str, password: &str) -> bool {
        let Ok(hash) = PasswordHash::new(&self.hash) else {
            return false;
        };
        let password_ok = Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok();
        password_ok & (user == self.user)
    }
}

/// A password's Argon2id hash, with a random salt, in the PHC format.
pub fn hash(password: &str) -> Result<String> {
    if password.is_empty() {
        bail!("an empty password");
    }
    let salt = SaltString::generate(&mut OsRng);
    // RFC 9106 section 4, the second recommended option, for when 2 GiB per hash is too much:
    // Argon2id, t=3 iterations, p=4 lanes, m=2^16 KiB (64 MiB). Checking a password takes the
    // parameters from its hash: hashes made with others still check.
    let params = Params::new(64 * 1024, 3, 4, None)
        .map_err(|e| anyhow::anyhow!("the Argon2 parameters: {e}"))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing the password: {e}"))?
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_checked() {
        let password = Password::new("alice".into(), hash("secret").unwrap()).unwrap();
        // RFC 9106's parameters, in the hash.
        assert!(password.hash.starts_with("$argon2id$v=19$m=65536,t=3,p=4$"));
        assert!(password.check("alice", "secret"));
        assert!(!password.check("alice", "wrong"));
        assert!(!password.check("bob", "secret"));
        assert!(Password::new("alice".into(), "secret".into()).is_err());
        assert!(Password::new(String::new(), hash("secret").unwrap()).is_err());
        // A hash made with other parameters still checks.
        let older = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$"; // salt "somesalt"
        let older = format!(
            "{older}{}",
            Argon2::default()
                .hash_password(b"secret", &SaltString::from_b64("c29tZXNhbHQ").unwrap())
                .unwrap()
                .hash
                .unwrap()
        );
        assert!(
            Password::new("alice".into(), older)
                .unwrap()
                .check("alice", "secret")
        );
    }
}
