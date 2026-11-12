// SPDX-License-Identifier: MPL-2.0

//! Who may open a VM's console: accounts per VM, by UUID, in a file of the server's that only
//! root reads (`/etc/qemu-dbus-webrtc/accounts.toml` by default), not in the VM's definition,
//! which many read. A password is kept as an Argon2id hash. The file is read at each login:
//! `qemu-dbus-webrtc password` changes it with no restart.
//!
//! ```toml
//! [[account]]
//! vm = "b8ff54ee-77f5-43dd-aa19-de8eaed5eb75"
//! user = "alice"
//! password = "$argon2id$v=19$m=19456,t=2,p=1$..."
//! # Watch only: the account opens shadow sessions.
//! shadow_only = false
//! ```

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};
use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use serde::{Deserialize, Serialize};

/// The accounts file.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accounts {
    #[serde(default, rename = "account")]
    pub accounts: Vec<Account>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// The VM's UUID, in lower case.
    pub vm: String,
    pub user: String,
    /// Its password's Argon2id hash, in the PHC format.
    pub password: String,
    /// The account only watches: its sessions are shadow sessions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shadow_only: bool,
}

/// Who logged in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Granted {
    pub user: String,
    pub shadow_only: bool,
}

impl Accounts {
    /// The accounts in `path`; none if it does not exist.
    pub fn read(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write the accounts to `path`, readable by its owner only, replacing the file whole.
    pub fn write(&self, path: &Path) -> Result<()> {
        let text = format!(
            "# qemu-dbus-webrtc accounts: written by `qemu-dbus-webrtc password`.\n\n{}",
            toml::to_string(self)?
        );
        let temporary = path.with_extension("toml.new");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path).with_context(|| format!("writing {}", path.display()))
    }

    /// The accounts of the VM `uuid`.
    pub fn of<'a>(&'a self, uuid: &'a str) -> impl Iterator<Item = &'a Account> {
        self.accounts
            .iter()
            .filter(move |a| a.vm.eq_ignore_ascii_case(uuid))
    }

    /// Set the password of `user` on the VM `uuid`, adding the account if it is new.
    pub fn set(&mut self, uuid: &str, user: &str, password: &str, shadow_only: bool) -> Result<()> {
        let hash = hash(password)?;
        match self
            .accounts
            .iter_mut()
            .find(|a| a.vm.eq_ignore_ascii_case(uuid) && a.user == user)
        {
            Some(account) => {
                account.password = hash;
                account.shadow_only = shadow_only;
            }
            None => self.accounts.push(Account {
                vm: uuid.to_ascii_lowercase(),
                user: user.to_owned(),
                password: hash,
                shadow_only,
            }),
        }
        Ok(())
    }

    /// Remove the account `user` of the VM `uuid`: whether there was one.
    pub fn remove(&mut self, uuid: &str, user: &str) -> bool {
        let before = self.accounts.len();
        self.accounts
            .retain(|a| !(a.vm.eq_ignore_ascii_case(uuid) && a.user == user));
        self.accounts.len() != before
    }

    /// Who `user` is on the VM `uuid`, if `password` is theirs. Slow on purpose (Argon2): call
    /// it off the async threads.
    pub fn check(&self, uuid: &str, user: &str, password: &str) -> Option<Granted> {
        let Some(account) = self.of(uuid).find(|a| a.user == user) else {
            // As long for an unknown user as for a wrong password: the time does not tell
            // which users exist.
            let _ = Argon2::default().verify_password(password.as_bytes(), &dummy());
            return None;
        };
        let hash = PasswordHash::new(&account.password).ok()?;
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .ok()?;
        Some(Granted {
            user: account.user.clone(),
            shadow_only: account.shadow_only,
        })
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

/// A hash to check unknown users against.
fn dummy() -> PasswordHash<'static> {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hash = DUMMY.get_or_init(|| hash("not a password").unwrap_or_default());
    PasswordHash::new(hash).expect("a hash of ours")
}

#[cfg(test)]
mod tests {
    use super::*;

    const VM: &str = "b8ff54ee-77f5-43dd-aa19-de8eaed5eb75";

    #[test]
    fn passwords_are_checked() {
        let mut accounts = Accounts::default();
        accounts.set(VM, "alice", "secret", false).unwrap();
        accounts.set(VM, "support", "watch", true).unwrap();
        assert!(accounts.accounts[0].password.starts_with("$argon2id$"));
        assert_eq!(
            accounts.check(&VM.to_uppercase(), "alice", "secret"),
            Some(Granted {
                user: "alice".into(),
                shadow_only: false
            })
        );
        assert!(accounts.check(VM, "support", "watch").unwrap().shadow_only);
        assert_eq!(accounts.check(VM, "alice", "wrong"), None);
        assert_eq!(accounts.check(VM, "bob", "secret"), None);
        assert_eq!(
            accounts.check("00000000-0000-0000-0000-000000000000", "alice", "secret"),
            None
        );
        // A new password replaces the last.
        accounts.set(VM, "alice", "other", false).unwrap();
        assert_eq!(accounts.accounts.len(), 2);
        assert_eq!(accounts.check(VM, "alice", "secret"), None);
        assert!(accounts.remove(VM, "alice"));
        assert!(!accounts.remove(VM, "alice"));
    }

    #[test]
    fn the_file_is_the_owners() {
        let dir = std::env::temp_dir().join(format!("qdw-accounts-{}", std::process::id()));
        let path = dir.join("accounts.toml");
        let mut accounts = Accounts::default();
        accounts.set(VM, "alice", "secret", false).unwrap();
        accounts.write(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let read = Accounts::read(&path).unwrap();
        assert!(read.check(VM, "alice", "secret").is_some());
        assert!(
            Accounts::read(&dir.join("missing.toml"))
                .unwrap()
                .accounts
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
