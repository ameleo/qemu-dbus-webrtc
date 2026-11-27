// SPDX-License-Identifier: MPL-2.0

//! Who may open which VM's console: accounts in the server's configuration, each with its
//! password and the VMs it may open, whatever runs the VMs. A VM that no account names is
//! open to none. The passwords are kept as Argon2id hashes, never in clear:
//! `qemu-dbus-webrtc hash-password` makes them.

use anyhow::{Result, bail};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

/// The VMs an account may open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// Every VM of the host ("*").
    All,
    /// These VMs, each by its UUID or its name.
    Only(Vec<String>),
}

impl Grant {
    /// The VMs as the configuration lists them: "*" alone for all of them.
    pub fn new(vms: Vec<String>) -> Result<Self> {
        if vms.is_empty() {
            bail!("no VM: list the VMs by UUID or name, or \"*\" for all of them");
        }
        if vms.iter().any(|vm| vm == "*") {
            if vms.len() > 1 {
                bail!("\"*\" stands for all the VMs: alone in the list");
            }
            return Ok(Self::All);
        }
        if vms.iter().any(|vm| vm.trim().is_empty()) {
            bail!("an empty VM name");
        }
        Ok(Self::Only(vms))
    }
}

/// A user, its password's Argon2id hash in the PHC format, the VMs it may open, and whether it
/// may ask their power actions.
#[derive(Debug, Clone)]
pub struct Account {
    pub user: String,
    hash: String,
    pub grant: Grant,
    /// It may reset the VMs it opens, from the console. Not unless said.
    pub power: bool,
}

impl Account {
    /// The account as the configuration gives it: checked now, so that a wrong hash fails the
    /// start rather than every login.
    pub fn new(user: String, hash: String, grant: Grant) -> Result<Self> {
        if user.is_empty() {
            bail!("an empty user");
        }
        if PasswordHash::new(&hash).is_err() {
            bail!(
                "the password of {user} is not an Argon2 hash: make one with \
                 `qemu-dbus-webrtc hash-password`"
            );
        }
        Ok(Self {
            user,
            hash,
            grant,
            power: false,
        })
    }
}

/// The server's accounts.
#[derive(Debug, Clone)]
pub struct Accounts {
    accounts: Vec<Account>,
    /// What an unknown user's password is checked against: a hash of nothing anyone knows, so
    /// that a login takes as long whether the user exists or not.
    unknown: String,
}

impl Accounts {
    /// At least one account, each user once.
    pub fn new(accounts: Vec<Account>) -> Result<Self> {
        if accounts.is_empty() {
            bail!("no account");
        }
        for (i, account) in accounts.iter().enumerate() {
            if accounts[..i].iter().any(|a| a.user == account.user) {
                bail!("the user {} has two accounts", account.user);
            }
        }
        let unknown = hash(SaltString::generate(&mut OsRng).as_ref())?;
        Ok(Self { accounts, unknown })
    }

    /// The account of `user`, if `password` is its. Slow on purpose (Argon2): call it off the
    /// async threads. One password is checked whatever the user, an unknown user's against a
    /// hash of nothing, and the users are compared in constant time: the time tells neither
    /// which users exist nor how much of a name matched.
    pub fn check(&self, user: &str, password: &str) -> Option<&Account> {
        let mut found = None;
        for account in &self.accounts {
            let same: bool =
                subtle::ConstantTimeEq::ct_eq(user.as_bytes(), account.user.as_bytes()).into();
            if same {
                found = Some(account);
            }
        }
        let hash = found.map_or(self.unknown.as_str(), |account| account.hash.as_str());
        let password_ok = PasswordHash::new(hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        });
        found.filter(|_| password_ok)
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

    fn account(user: &str, password: &str, vms: &[&str]) -> Account {
        let grant = Grant::new(vms.iter().map(|v| v.to_string()).collect()).unwrap();
        Account::new(user.into(), hash(password).unwrap(), grant).unwrap()
    }

    #[test]
    fn passwords_are_checked() {
        let accounts = Accounts::new(vec![
            account("alice", "secret", &["win11-alice"]),
            account("admin", "root", &["*"]),
        ])
        .unwrap();
        // RFC 9106's parameters, in the hash.
        assert!(
            accounts.accounts[0]
                .hash
                .starts_with("$argon2id$v=19$m=65536,t=3,p=4$")
        );
        assert_eq!(accounts.check("alice", "secret").unwrap().user, "alice");
        assert_eq!(accounts.check("admin", "root").unwrap().grant, Grant::All);
        assert!(accounts.check("alice", "root").is_none());
        assert!(accounts.check("admin", "secret").is_none());
        assert!(accounts.check("bob", "secret").is_none());
        assert!(accounts.check("", "").is_none());
        assert!(accounts.check("alic", "secret").is_none());
    }

    #[test]
    fn accounts_are_checked_at_start() {
        let grant = || Grant::All;
        assert!(Account::new("alice".into(), "secret".into(), grant()).is_err());
        assert!(Account::new(String::new(), hash("secret").unwrap(), grant()).is_err());
        assert!(Accounts::new(Vec::new()).is_err());
        let twice = vec![account("alice", "a", &["*"]), account("alice", "b", &["x"])];
        assert!(Accounts::new(twice).is_err());
    }

    #[test]
    fn grants() {
        let list = |vms: &[&str]| Grant::new(vms.iter().map(|v| v.to_string()).collect());
        assert_eq!(list(&["*"]).unwrap(), Grant::All);
        assert_eq!(
            list(&["win11", "b8ff54ee-77f5-43dd-aa19-de8eaed5eb75"]).unwrap(),
            Grant::Only(vec![
                "win11".into(),
                "b8ff54ee-77f5-43dd-aa19-de8eaed5eb75".into()
            ])
        );
        assert!(list(&[]).is_err());
        assert!(list(&["*", "win11"]).is_err());
        assert!(list(&["win11", " "]).is_err());
    }

    #[test]
    fn older_hashes_still_check() {
        let older = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$"; // salt "somesalt"
        let older = format!(
            "{older}{}",
            Argon2::default()
                .hash_password(b"secret", &SaltString::from_b64("c29tZXNhbHQ").unwrap())
                .unwrap()
                .hash
                .unwrap()
        );
        let accounts = Accounts::new(vec![
            Account::new("alice".into(), older, Grant::All).unwrap(),
        ])
        .unwrap();
        assert!(accounts.check("alice", "secret").is_some());
    }
}
