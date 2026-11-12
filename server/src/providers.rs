// SPDX-License-Identifier: MPL-2.0

//! Where the server finds the host's VMs: a provider knows how a VM manager runs them, and how
//! to attach to their display. Each one lists the VMs it sees (their way in while they run,
//! and their UUID and names when it knows them without attaching), and may start one that is
//! off; the server takes the VM a client asks for by its UUID or name. A new VM manager is a
//! new provider: libvirt is in `libvirt.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use qdw_qemu::display::{self, Attach, VmProxy};
use tracing::{debug, warn};

use crate::access::Accounts;
use crate::libvirt::Libvirt;

/// How long the UUID of a VM may take to read, when a provider does not know it.
const PROBE_TIME: Duration = Duration::from_secs(3);

/// A VM a provider sees.
pub struct Candidate {
    /// How to attach to its display; `None` while it is off.
    pub attach: Option<Attach>,
    /// Its UUID, when the provider knows it without attaching.
    pub uuid: Option<String>,
    /// The names it goes by, when the provider knows them without attaching: what a person
    /// types in an address rather than the UUID.
    pub names: Vec<String>,
    /// Off, the provider starts it when a client asks for it.
    pub startable: bool,
}

/// A VM manager.
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    /// The VMs it sees now, with a display the server can attach to.
    fn candidates(&self) -> BoxFuture<'_, Result<Vec<Candidate>>>;
    /// Start the VM `uuid`, which is off.
    fn start<'a>(&'a self, uuid: &'a str) -> BoxFuture<'a, Result<()>> {
        let _ = uuid;
        async move { bail!("{} does not start VMs", self.name()) }.boxed()
    }
    /// The accounts of the VM `uuid`, if this VM manager keeps them with the VM: `None` when it
    /// does not, and the server's accounts file holds them.
    fn accounts<'a>(&'a self, uuid: &'a str) -> BoxFuture<'a, Result<Option<Accounts>>> {
        let _ = uuid;
        async move { Ok(None) }.boxed()
    }
    /// Keep the accounts of the VM `uuid` with it.
    fn set_accounts<'a>(
        &'a self,
        uuid: &'a str,
        accounts: &'a Accounts,
    ) -> BoxFuture<'a, Result<()>> {
        let _ = (uuid, accounts);
        async move { bail!("{} does not keep accounts", self.name()) }.boxed()
    }
}

/// What the providers say of a VM.
pub enum Found<'a> {
    /// It runs: attach this way.
    Running(Attach),
    /// It is off; this provider starts it, if it can.
    Off {
        provider: &'a dyn Provider,
        startable: bool,
    },
}

/// Proxmox VE: each VM's QMP socket is `<vmid>.qmp` in `run_dir`, and its UUID is in its
/// configuration, `<vmid>.conf` in `conf_dir` (`smbios1: uuid=...`). The VM must run with
/// `-display dbus,p2p=on` (`args:` in its configuration). Not tried yet.
pub struct Proxmox {
    pub run_dir: PathBuf,
    pub conf_dir: PathBuf,
}

impl Provider for Proxmox {
    fn name(&self) -> &'static str {
        "proxmox"
    }

    fn candidates(&self) -> BoxFuture<'_, Result<Vec<Candidate>>> {
        async move { self.running() }.boxed()
    }
}

impl Proxmox {
    fn running(&self) -> Result<Vec<Candidate>> {
        let entries = match std::fs::read_dir(&self.run_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.run_dir.display())),
        };
        Ok(entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let vmid = name.strip_suffix(".qmp")?;
                let conf = std::fs::read_to_string(self.conf_dir.join(format!("{vmid}.conf")))
                    .unwrap_or_default();
                // Its VM id, and its name.
                let mut names = vec![vmid.to_owned()];
                names.extend(proxmox_field(&conf, "name").map(str::to_owned));
                Some(Candidate {
                    attach: Some(Attach::P2p { qmp: e.path() }),
                    uuid: proxmox_field(&conf, "smbios1").and_then(|smbios| {
                        smbios
                            .split(',')
                            .find_map(|field| field.trim().strip_prefix("uuid="))
                            .map(str::to_owned)
                    }),
                    names,
                    startable: false,
                })
            })
            .collect())
    }
}

/// A field of a Proxmox VE VM configuration, `<field>: <value>`, in its main section (before
/// the snapshots').
fn proxmox_field<'a>(conf: &'a str, field: &str) -> Option<&'a str> {
    conf.lines()
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .map(str::trim)
}

/// A VM given by hand: in the configuration (`[[vm]]`) or on the command line.
#[derive(Debug, Clone)]
pub enum Source {
    /// Its QMP socket: QEMU hands the display over a peer-to-peer connection.
    Qmp(PathBuf),
    /// The D-Bus bus on which QEMU owns `org.qemu`.
    Dbus(String),
    /// A libvirt domain, by its name.
    Domain { dir: PathBuf, domain: String },
}

#[derive(Default)]
pub struct Statics(pub Vec<Source>);

impl Provider for Statics {
    fn name(&self) -> &'static str {
        "configuration"
    }

    fn candidates(&self) -> BoxFuture<'_, Result<Vec<Candidate>>> {
        async move { Ok(self.running()) }.boxed()
    }
}

impl Statics {
    fn running(&self) -> Vec<Candidate> {
        self.0
            .iter()
            .filter_map(|source| {
                let attach = match source {
                    Source::Qmp(qmp) => Attach::P2p { qmp: qmp.clone() },
                    Source::Dbus(address) => Attach::Bus {
                        address: address.clone(),
                    },
                    // A domain that does not run now has no bus.
                    Source::Domain { dir, domain } => Attach::Bus {
                        address: libvirt_bus(dir, domain).ok()?,
                    },
                };
                let names = match source {
                    Source::Domain { domain, .. } => vec![domain.clone()],
                    _ => Vec::new(),
                };
                Some(Candidate {
                    attach: Some(attach),
                    uuid: None,
                    names,
                    startable: false,
                })
            })
            .collect()
    }
}

/// The D-Bus address of a libvirt domain's display: its socket in `dir`, whatever id the
/// domain has now.
pub fn libvirt_bus(dir: &Path, domain: &str) -> Result<String> {
    let suffix = format!("-{domain}-dbus.sock");
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("looking for {domain} in {}", dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name.strip_suffix(&suffix) else {
            continue;
        };
        if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(format!("unix:path={}", entry.path().display()));
        }
    }
    bail!(
        "no D-Bus display for the libvirt domain {domain}: is it running, with <graphics type='dbus'>?"
    )
}

/// What a VM says of itself.
#[derive(Debug, Clone)]
struct Probed {
    uuid: String,
    name: String,
}

/// The VM managers the server asks, in order.
#[derive(Default)]
pub struct Providers {
    pub libvirt: Option<Libvirt>,
    pub proxmox: Option<Proxmox>,
    pub statics: Statics,
    /// The UUIDs and names read from the VMs on a bus, by bus address: a bus is a VM's for as
    /// long as it runs, and the next start of the VM has another.
    probed: Mutex<HashMap<String, Probed>>,
}

impl Providers {
    pub fn new(libvirt: Option<Libvirt>, proxmox: Option<Proxmox>, statics: Vec<Source>) -> Self {
        Self {
            libvirt,
            proxmox,
            statics: Statics(statics),
            probed: Mutex::default(),
        }
    }

    fn all(&self) -> Vec<&dyn Provider> {
        let mut all: Vec<&dyn Provider> = Vec::new();
        if let Some(libvirt) = &self.libvirt {
            all.push(libvirt);
        }
        if let Some(proxmox) = &self.proxmox {
            all.push(proxmox);
        }
        all.push(&self.statics);
        all
    }

    /// The names of the providers asked.
    pub fn names(&self) -> Vec<&'static str> {
        self.all().iter().map(|p| p.name()).collect()
    }

    /// What the providers say of the VM `uuid`: `None` when none knows it.
    pub async fn find(&self, uuid: &str) -> Result<Option<Found<'_>>> {
        for provider in self.all() {
            let candidates = match provider.candidates().await {
                Ok(candidates) => candidates,
                Err(e) => {
                    warn!(provider = provider.name(), error = %format!("{e:#}"), "VMs not listed");
                    continue;
                }
            };
            for candidate in candidates {
                let found = match (&candidate.uuid, &candidate.attach) {
                    (Some(known), _) => known.eq_ignore_ascii_case(uuid),
                    (None, Some(attach)) => self
                        .probe(attach)
                        .await
                        .is_some_and(|probed| probed.uuid.eq_ignore_ascii_case(uuid)),
                    (None, None) => false,
                };
                if found {
                    debug!(provider = provider.name(), uuid, "VM found");
                    return Ok(Some(match candidate.attach {
                        Some(attach) => Found::Running(attach),
                        None => Found::Off {
                            provider,
                            startable: candidate.startable,
                        },
                    }));
                }
            }
        }
        Ok(None)
    }

    /// The provider of the VM `uuid`, when it knows the VM's UUID without attaching.
    async fn provider_of(&self, uuid: &str) -> Result<Option<&dyn Provider>> {
        for provider in self.all() {
            let candidates = provider.candidates().await?;
            if candidates.iter().any(|c| {
                c.uuid
                    .as_deref()
                    .is_some_and(|u| u.eq_ignore_ascii_case(uuid))
            }) {
                return Ok(Some(provider));
            }
        }
        Ok(None)
    }

    /// The accounts of the VM `uuid`, if its manager keeps them with the VM; `None` when the
    /// server's accounts file holds them.
    pub async fn accounts(&self, uuid: &str) -> Result<Option<Accounts>> {
        match self.provider_of(uuid).await? {
            Some(provider) => provider.accounts(uuid).await,
            None => Ok(None),
        }
    }

    /// Keep the accounts of the VM `uuid` with it, if its manager does: whether it did.
    pub async fn set_accounts(&self, uuid: &str, accounts: &Accounts) -> Result<bool> {
        match self.provider_of(uuid).await? {
            Some(provider) if provider.accounts(uuid).await?.is_some() => {
                provider.set_accounts(uuid, accounts).await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// The UUID of the VM named `name`, if a provider runs one by that name; an error when
    /// several VMs go by it.
    pub async fn uuid_of(&self, name: &str) -> Result<Option<String>> {
        let mut found: Vec<(String, &'static str)> = Vec::new();
        for provider in self.all() {
            let candidates = match provider.candidates().await {
                Ok(candidates) => candidates,
                Err(e) => {
                    warn!(provider = provider.name(), error = %format!("{e:#}"), "VMs not listed");
                    continue;
                }
            };
            for candidate in candidates {
                let known = candidate.names.iter().any(|n| n == name);
                // A provider that does not know the names: the VM's own.
                let probed = match &candidate.attach {
                    Some(attach)
                        if (known && candidate.uuid.is_none()) || candidate.names.is_empty() =>
                    {
                        self.probe(attach).await
                    }
                    _ => None,
                };
                let uuid = match (known, candidate.uuid, probed) {
                    (true, Some(uuid), _) => uuid,
                    (true, None, Some(probed)) => probed.uuid,
                    (false, _, Some(probed)) if probed.name == name => probed.uuid,
                    _ => continue,
                };
                let uuid = uuid.to_ascii_lowercase();
                if !found.iter().any(|(u, _)| *u == uuid) {
                    found.push((uuid, provider.name()));
                }
            }
        }
        match found.len() {
            0 | 1 => Ok(found.pop().map(|(uuid, _)| uuid)),
            _ => bail!(
                "several VMs are named {name} ({}): ask for one by its UUID",
                found
                    .iter()
                    .map(|(uuid, provider)| format!("{uuid} in {provider}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// The UUID and name of the VM behind `attach`, read from the VM itself; remembered for a
    /// bus.
    async fn probe(&self, attach: &Attach) -> Option<Probed> {
        let bus = match attach {
            Attach::Bus { address } => Some(address.clone()),
            Attach::P2p { .. } => None,
        };
        if let Some(bus) = &bus
            && let Some(probed) = self.probed.lock().unwrap().get(bus)
        {
            return Some(probed.clone());
        }
        let read = async {
            let connection = display::attach(attach).await?;
            let vm = VmProxy::new(&connection).await?;
            anyhow::Ok(Probed {
                uuid: vm.uuid().await?,
                name: vm.name().await?,
            })
        };
        match tokio::time::timeout(PROBE_TIME, read).await {
            Ok(Ok(probed)) => {
                if let Some(bus) = bus {
                    self.probed.lock().unwrap().insert(bus, probed.clone());
                }
                Some(probed)
            }
            Ok(Err(e)) => {
                debug!(?attach, error = %format!("{e:#}"), "VM not read");
                None
            }
            Err(_) => {
                debug!(?attach, "VM not read in time");
                None
            }
        }
    }

    /// Forget what was read of `attach`: its VM is not the one it was.
    pub fn forget(&self, attach: &Attach) {
        if let Attach::Bus { address } = attach {
            self.probed.lock().unwrap().remove(address);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proxmox_vm() {
        let dir = std::env::temp_dir().join(format!("qdw-proxmox-{}", std::process::id()));
        let (run_dir, conf_dir) = (dir.join("run"), dir.join("conf"));
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(run_dir.join("101.qmp"), "").unwrap();
        std::fs::write(run_dir.join("101.pid"), "").unwrap();
        std::fs::write(
            conf_dir.join("101.conf"),
            "boot: order=scsi0\nname: win11\n\
             smbios1: uuid=7b3a1c2d-0000-4000-8000-123456789abc,manufacturer=QQ\n\
             [snap]\nname: old\nsmbios1: uuid=00000000-0000-0000-0000-000000000000\n",
        )
        .unwrap();
        let candidates = Proxmox { run_dir, conf_dir }.running().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].uuid.as_deref(),
            Some("7b3a1c2d-0000-4000-8000-123456789abc")
        );
        assert_eq!(candidates[0].names, ["101", "win11"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_domain_by_its_name() {
        let dir = std::env::temp_dir().join(format!("qdw-libvirt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "6-win-dbus-dbus.sock",
            "6-win-dbus-dbus.pid",
            "7-other-win-dbus-dbus.sock",
        ] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        let address = libvirt_bus(&dir, "win-dbus").unwrap();
        assert!(address.ends_with("/6-win-dbus-dbus.sock"), "{address}");
        assert!(libvirt_bus(&dir, "missing").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
