// SPDX-License-Identifier: MPL-2.0

//! The administrator's commands, next to the server:
//!
//! - `qemu-dbus-webrtc hash-password`: asks for a password, and prints its Argon2id hash, for
//!   `[auth] password` in the configuration.

use anyhow::{Result, bail};

/// Run the command `args` names, if it is one of the tools: whether it was.
pub fn run(args: &[String]) -> Result<bool> {
    match args.first().map(String::as_str) {
        Some("hash-password") => hash_password().map(|()| true),
        _ => Ok(false),
    }
}

fn hash_password() -> Result<()> {
    let password = rpassword::prompt_password("password: ")?;
    let again = rpassword::prompt_password("again: ")?;
    if password != again {
        bail!("the two passwords differ");
    }
    println!("{}", crate::access::hash(&password)?);
    Ok(())
}
