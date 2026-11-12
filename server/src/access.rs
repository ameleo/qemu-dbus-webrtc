// SPDX-License-Identifier: MPL-2.0

//! Who may open a VM's console, the simple way: one user and its password, the server's, in its
//! configuration, whatever runs the VMs. The password is kept as an Argon2id hash, never in
//! clear: `qemu-dbus-webrtc hash-password` makes it.

use anyhow::{Result, bail};
use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

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
    Ok(Argon2::default()
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
        assert!(password.hash.starts_with("$argon2id$"));
        assert!(password.check("alice", "secret"));
        assert!(!password.check("alice", "wrong"));
        assert!(!password.check("bob", "secret"));
        assert!(Password::new("alice".into(), "secret".into()).is_err());
        assert!(Password::new(String::new(), hash("secret").unwrap()).is_err());
    }
}
