// SPDX-License-Identifier: MPL-2.0

//! The host's VMs, by UUID: the server attaches to a VM when its first client comes, and
//! detaches a while after the last one left, so that a VM nobody watches costs nothing, to the
//! server or to QEMU, and a client that reconnects finds its VM still attached.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tracing::{info, warn};

use crate::config::VmConfig;
use crate::providers::{Found, Providers};
use crate::vm::{self, Vm};
use crate::web;

/// How long a VM stays attached after its last client left.
const DETACH_AFTER: Duration = Duration::from_secs(60);
/// How often the VMs without clients are looked at.
const REAP_INTERVAL: Duration = Duration::from_secs(10);
/// How long a VM just started has for its display to come up.
const START_TIME: Duration = Duration::from_secs(15);

/// A VM attached, and who uses it.
struct Attached {
    vm: Vm,
    clients: AtomicUsize,
    /// When its last client left; `None` while it has some.
    idle_since: std::sync::Mutex<Option<Instant>>,
}

/// A client's hold on a VM: the VM stays attached while it lasts.
pub struct Client {
    attached: Arc<Attached>,
}

impl Client {
    /// What the VM's clients share.
    pub fn shared(&self) -> &web::Shared {
        &self.attached.vm.shared
    }

    /// Where what is logged of the VM goes.
    pub fn span(&self) -> &tracing::Span {
        &self.attached.vm.span
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.attached.clients.fetch_sub(1, Ordering::AcqRel) == 1 {
            *self.attached.idle_since.lock().unwrap() = Some(Instant::now());
        }
    }
}

pub struct Vms {
    providers: Providers,
    config: VmConfig,
    /// By UUID, in lower case. One attachment at a time: two clients of the same VM coming
    /// together attach once.
    attached: tokio::sync::Mutex<HashMap<String, Arc<Attached>>>,
}

impl Vms {
    pub fn new(providers: Providers, config: VmConfig) -> Arc<Self> {
        let vms = Arc::new(Self {
            providers,
            config,
            attached: Default::default(),
        });
        tokio::spawn(reap(Arc::downgrade(&vms)));
        vms
    }

    /// The names of the VM managers asked.
    pub fn providers(&self) -> Vec<&'static str> {
        self.providers.names()
    }

    /// The UUID of the VM `id`, a UUID or a VM's name: `None` when no provider runs a VM by
    /// that name; an error when several do.
    pub async fn uuid_of(&self, id: &str) -> Result<Option<String>> {
        match normalize(id) {
            Some(uuid) => Ok(Some(uuid)),
            None => self.providers.uuid_of(id).await,
        }
    }

    /// The VM `id` (a UUID or a VM's name), attached if it was not: `None` when no provider
    /// runs it.
    pub async fn client(&self, id: &str) -> Result<Option<Client>> {
        let Some(uuid) = self.uuid_of(id).await? else {
            return Ok(None);
        };
        let mut attached = self.attached.lock().await;
        if attached.get(&uuid).is_some_and(|a| a.vm.ended()) {
            info!(uuid, "VM gone: attaching again");
            attached.remove(&uuid);
        }
        let entry = match attached.get(&uuid) {
            Some(entry) => entry.clone(),
            None => {
                let Some(how) = self.attach_to(&uuid).await? else {
                    return Ok(None);
                };
                let vm = match vm::start(&how, &uuid, &self.config).await {
                    Ok(vm) => vm,
                    Err(e) => {
                        // What the provider remembered of it may be stale.
                        self.providers.forget(&how);
                        return Err(e);
                    }
                };
                let entry = Arc::new(Attached {
                    vm,
                    clients: AtomicUsize::new(0),
                    idle_since: Default::default(),
                });
                attached.insert(uuid, entry.clone());
                entry
            }
        };
        entry.clients.fetch_add(1, Ordering::AcqRel);
        *entry.idle_since.lock().unwrap() = None;
        Ok(Some(Client { attached: entry }))
    }
}

impl Vms {
    /// How to attach to the VM `uuid`, started first if it is off and its provider starts it:
    /// `None` when no provider knows it.
    async fn attach_to(&self, uuid: &str) -> Result<Option<qdw_qemu::display::Attach>> {
        let (provider, startable) = match self.providers.find(uuid).await? {
            None => return Ok(None),
            Some(Found::Running(attach)) => return Ok(Some(attach)),
            Some(Found::Off {
                provider,
                startable,
            }) => (provider, startable),
        };
        if !startable {
            bail!("the VM is off");
        }
        provider.start(uuid).await?;
        // Its display comes up with QEMU, at once or nearly.
        let started = Instant::now();
        loop {
            if let Some(Found::Running(attach)) = self.providers.find(uuid).await? {
                return Ok(Some(attach));
            }
            if started.elapsed() >= START_TIME {
                bail!("the VM started, but its display did not come up");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// A UUID as the server keys VMs by: lower case, and only a UUID (it comes from a URL).
pub fn normalize(uuid: &str) -> Option<String> {
    let well_formed = uuid.len() == 36
        && uuid.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    well_formed.then(|| uuid.to_ascii_lowercase())
}

/// Detach from the VMs without clients for `DETACH_AFTER`, and from the VMs gone, until the
/// server ends.
async fn reap(vms: std::sync::Weak<Vms>) {
    let mut interval = tokio::time::interval(REAP_INTERVAL);
    loop {
        interval.tick().await;
        let Some(vms) = vms.upgrade() else { return };
        let mut attached = vms.attached.lock().await;
        attached.retain(|uuid, entry| {
            if entry.clients.load(Ordering::Acquire) > 0 {
                return true;
            }
            let idle = entry
                .idle_since
                .lock()
                .unwrap()
                .is_none_or(|since| since.elapsed() >= DETACH_AFTER);
            if entry.vm.ended() {
                warn!(uuid, name = %entry.vm.name, "VM gone");
                false
            } else if idle {
                info!(uuid, name = %entry.vm.name, "no client for a while: detached");
                false
            } else {
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_from_urls() {
        assert_eq!(
            normalize("B8FF54EE-77f5-43dd-aa19-de8eaed5eb75").as_deref(),
            Some("b8ff54ee-77f5-43dd-aa19-de8eaed5eb75")
        );
        assert_eq!(normalize("../../etc/passwd"), None);
        assert_eq!(normalize("b8ff54ee77f543ddaa19de8eaed5eb75"), None);
        assert_eq!(normalize("b8ff54ee-77f5-43dd-aa19-de8eaed5eb7g"), None);
    }
}
