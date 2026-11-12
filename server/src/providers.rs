// SPDX-License-Identifier: MPL-2.0

//! Where the server finds the host's VMs: a provider knows how a VM manager runs them, and how
//! to attach to their display. Each one lists the VMs it sees (their way in, and their UUID
//! when it knows it without attaching); the server takes the VM a client asks for by its UUID.
//! A new VM manager is a new provider.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use qdw_qemu::display::{self, Attach, VmProxy};
use tracing::{debug, warn};

/// How long the UUID of a VM may take to read, when a provider does not know it.
const PROBE_TIME: Duration = Duration::from_secs(3);

/// A VM a provider sees.
pub struct Candidate {
    pub attach: Attach,
    /// Its UUID, when the provider knows it without attaching.
    pub uuid: Option<String>,
    /// The names it goes by, when the provider knows them without attaching: what a person
    /// types in an address rather than the UUID.
    pub names: Vec<String>,
}

/// A VM manager.
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    /// The VMs it runs now, with a display the server can attach to.
    fn candidates(&self) -> Result<Vec<Candidate>>;
}

/// libvirt: each domain with `<graphics type='dbus'>` has the bus of its display in `dir`,
/// `<id>-<domain>-dbus.sock`, the id changing at each start.
pub struct Libvirt {
    pub dir: PathBuf,
}

impl Provider for Libvirt {
    fn name(&self) -> &'static str {
        "libvirt"
    }

    fn candidates(&self) -> Result<Vec<Candidate>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // No domain with a D-Bus display runs.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir.display())),
        };
        Ok(entries
            .flatten()
            .filter_map(|e| {
                let file = e.file_name().to_string_lossy().into_owned();
                Some(Candidate {
                    names: libvirt_domain(&file)?.into_iter().collect(),
                    attach: Attach::Bus {
                        address: format!("unix:path={}", e.path().display()),
                    },
                    uuid: None,
                })
            })
            .collect())
    }
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

    fn candidates(&self) -> Result<Vec<Candidate>> {
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
                    attach: Attach::P2p { qmp: e.path() },
                    uuid: proxmox_field(&conf, "smbios1").and_then(|smbios| {
                        smbios
                            .split(',')
                            .find_map(|field| field.trim().strip_prefix("uuid="))
                            .map(str::to_owned)
                    }),
                    names,
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

/// The domain of a libvirt display socket, `<id>-<domain>-dbus.sock`; `None` for another file.
fn libvirt_domain(file: &str) -> Option<Option<String>> {
    let rest = file.strip_suffix("-dbus.sock")?;
    let Some((id, domain)) = rest.split_once('-') else {
        return Some(None);
    };
    let numbered = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    Some((numbered && !domain.is_empty()).then(|| domain.to_owned()))
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

    fn candidates(&self) -> Result<Vec<Candidate>> {
        Ok(self
            .0
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
                    attach,
                    uuid: None,
                    names,
                })
            })
            .collect())
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

    /// How to attach to the VM `uuid`, if a provider runs it.
    pub async fn find(&self, uuid: &str) -> Result<Option<Attach>> {
        for provider in self.all() {
            let candidates = match provider.candidates() {
                Ok(candidates) => candidates,
                Err(e) => {
                    warn!(provider = provider.name(), error = %format!("{e:#}"), "VMs not listed");
                    continue;
                }
            };
            for candidate in candidates {
                let found = match &candidate.uuid {
                    Some(known) => known.eq_ignore_ascii_case(uuid),
                    None => self
                        .probe(&candidate.attach)
                        .await
                        .is_some_and(|probed| probed.uuid.eq_ignore_ascii_case(uuid)),
                };
                if found {
                    debug!(provider = provider.name(), uuid, "VM found");
                    return Ok(Some(candidate.attach));
                }
            }
        }
        Ok(None)
    }

    /// The UUID of the VM named `name`, if a provider runs one by that name; an error when
    /// several VMs go by it.
    pub async fn uuid_of(&self, name: &str) -> Result<Option<String>> {
        let mut found: Vec<(String, &'static str)> = Vec::new();
        for provider in self.all() {
            let Ok(candidates) = provider.candidates() else {
                continue;
            };
            for candidate in candidates {
                let known = candidate.names.iter().any(|n| n == name);
                // A provider that does not know the names: the VM's own.
                let probed = if known && candidate.uuid.is_some() {
                    None
                } else if known || candidate.names.is_empty() {
                    self.probe(&candidate.attach).await
                } else {
                    None
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
        let candidates = Proxmox { run_dir, conf_dir }.candidates().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].uuid.as_deref(),
            Some("7b3a1c2d-0000-4000-8000-123456789abc")
        );
        assert_eq!(candidates[0].names, ["101", "win11"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn libvirt_domains_by_their_socket() {
        assert_eq!(
            libvirt_domain("6-daas-win-dbus-dbus.sock"),
            Some(Some("daas-win-dbus".into()))
        );
        assert_eq!(libvirt_domain("6-daas-win-dbus-dbus.pid"), None);
        assert_eq!(libvirt_domain("x-dbus.sock"), Some(None));
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
        let libvirt = Libvirt { dir: dir.clone() };
        let candidates = libvirt.candidates().unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|c| c.names == ["win-dbus"]));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
