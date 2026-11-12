// SPDX-License-Identifier: MPL-2.0

//! The administrator's commands, next to the server:
//!
//! - `qemu-dbus-webrtc password [--config <file>] <vm> <user> [--shadow-only] [--remove]`: set
//!   (asking for it) or remove the password of an account of a VM, by its name or UUID;
//! - `qemu-dbus-webrtc accounts [--config <file>]`: list the accounts, and the VMs they are of.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::access::Accounts;
use crate::config::{self, Auth};
use crate::providers::Providers;

const PASSWORD_USAGE: &str = "usage: qemu-dbus-webrtc password [--config <file.toml>] <vm name or uuid> \
                              <user> [--shadow-only] [--remove]";

/// Run the command `args` names, if it is one of the tools: whether it was.
pub async fn run(args: &[String]) -> Result<bool> {
    match args.first().map(String::as_str) {
        Some("password") => password(&args[1..]).await.map(|()| true),
        Some("accounts") => accounts(&args[1..]).await.map(|()| true),
        _ => Ok(false),
    }
}

/// The `--config` option of a tool, and its other arguments.
fn split_config(args: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut config = Vec::new();
    let mut rest = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--config" {
            let path = args.next().context("--config takes a file")?;
            config = vec![arg.clone(), path.clone()];
        } else {
            rest.push(arg.clone());
        }
    }
    Ok((config, rest))
}

/// The accounts file and the VM providers of the configuration.
fn setup(config: Vec<String>) -> Result<(PathBuf, Providers)> {
    let config = config::parse(config)?;
    let path = match config.auth {
        Auth::Accounts(path) => path,
        Auth::None => {
            eprintln!(
                "note: [auth] method is \"none\": these accounts are not asked for until it is \"accounts\""
            );
            config::DEFAULT_ACCOUNTS.into()
        }
    };
    Ok((path, config.providers))
}

async fn password(args: &[String]) -> Result<()> {
    let (config, rest) = split_config(args)?;
    let mut shadow_only = false;
    let mut remove = false;
    let mut positional = Vec::new();
    for arg in rest {
        match arg.as_str() {
            "--shadow-only" => shadow_only = true,
            "--remove" => remove = true,
            _ if arg.starts_with("--") => bail!(PASSWORD_USAGE),
            _ => positional.push(arg),
        }
    }
    let [vm, user] = positional.as_slice() else {
        bail!(PASSWORD_USAGE)
    };
    let (path, providers) = setup(config)?;
    let uuid = match crate::vms::normalize(vm) {
        Some(uuid) => uuid,
        None => providers
            .uuid_of(vm)
            .await?
            .with_context(|| format!("no VM named {vm} on this host"))?,
    };
    // With the VM when its manager keeps them (libvirt), in the accounts file otherwise.
    let kept = providers.accounts(&uuid).await?;
    let in_vm = kept.is_some();
    let mut accounts = match kept {
        Some(accounts) => accounts,
        None => Accounts::read(&path)?,
    };
    if remove {
        if !accounts.remove(&uuid, user) {
            bail!("{vm} has no account {user}");
        }
        let at = save(&providers, in_vm, &uuid, &accounts, &path).await?;
        println!("{user} removed from {vm} ({uuid}), {at}");
        return Ok(());
    }
    let password = rpassword::prompt_password(format!("password of {user} on {vm}: "))?;
    let again = rpassword::prompt_password("again: ")?;
    if password != again {
        bail!("the two passwords differ");
    }
    accounts.set(&uuid, user, &password, shadow_only)?;
    let at = save(&providers, in_vm, &uuid, &accounts, &path).await?;
    let watch = if shadow_only { ", watch only" } else { "" };
    println!("{user} set on {vm} ({uuid}){watch}, {at}");
    Ok(())
}

/// Keep the accounts of the VM `uuid`: with the VM if its manager does (`in_vm`), in the file
/// at `path` otherwise. Where they went.
async fn save(
    providers: &Providers,
    in_vm: bool,
    uuid: &str,
    accounts: &Accounts,
    path: &std::path::Path,
) -> Result<String> {
    if in_vm && providers.set_accounts(uuid, accounts).await? {
        Ok("in the VM's definition".to_string())
    } else {
        accounts.write(path)?;
        Ok(format!("in {}", path.display()))
    }
}

async fn accounts(args: &[String]) -> Result<()> {
    let (config, rest) = split_config(args)?;
    if !rest.is_empty() {
        bail!("usage: qemu-dbus-webrtc accounts [--config <file.toml>]");
    }
    let (path, providers) = setup(config)?;
    let accounts = Accounts::read(&path)?;
    if accounts.accounts.is_empty() {
        println!("no account in {}", path.display());
        return Ok(());
    }
    for account in &accounts.accounts {
        let known = providers.find(&account.vm).await?.is_some();
        let watch = if account.shadow_only {
            " (watch only)"
        } else {
            ""
        };
        let gone = if known {
            ""
        } else {
            "   no such VM on this host"
        };
        println!("{}  {}{watch}{gone}", account.vm, account.user);
    }
    Ok(())
}
