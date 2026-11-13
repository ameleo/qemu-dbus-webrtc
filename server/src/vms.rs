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

/// How long a VM stays attached after its last client was cut off (no clean close): it may
/// well come back.
const DETACH_AFTER: Duration = Duration::from_secs(60);
/// How long after its last client left with a clean close: it meant to, but may be reloading
/// its page.
const DETACH_AFTER_LEAVING: Duration = Duration::from_secs(5);
/// How often the VMs without clients are looked at.
const REAP_INTERVAL: Duration = Duration::from_secs(10);
/// How long a VM just started has for its display to come up.
const START_TIME: Duration = Duration::from_secs(15);

/// A VM attached, and who uses it.
struct Attached {
    vm: Vm,
    /// Wakes the reaper: the VM stopped, or its last client left one that did.
    reap: Arc<tokio::sync::Notify>,
    clients: AtomicUsize,
    /// When to detach from it, its last client gone; `None` while it has some.
    detach_at: std::sync::Mutex<Option<Instant>>,
}

/// A client's hold on a VM: the VM stays attached while it lasts.
pub struct Client {
    attached: Arc<Attached>,
    /// It left with a clean close.
    left: bool,
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

    /// The client left with a clean close: if it was the last, the VM need not wait for it.
    pub fn leaves(&mut self) {
        self.left = true;
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.attached.clients.fetch_sub(1, Ordering::AcqRel) == 1 {
            let wait = if self.left {
                DETACH_AFTER_LEAVING
            } else {
                DETACH_AFTER
            };
            *self.attached.detach_at.lock().unwrap() = Some(Instant::now() + wait);
            let reap = self.attached.reap.clone();
            if self.attached.vm.ended() {
                // A VM that stopped goes as soon as its clients have.
                reap.notify_one();
            } else if self.left {
                tokio::spawn(async move {
                    tokio::time::sleep(DETACH_AFTER_LEAVING).await;
                    reap.notify_one();
                });
            }
        }
    }
}

pub struct Vms {
    providers: Providers,
    config: VmConfig,
    /// By UUID, in lower case. Held briefly: never across an attachment.
    attached: tokio::sync::Mutex<HashMap<String, Arc<Attached>>>,
    /// One attachment at a time per VM, by UUID: two clients of the same VM coming together
    /// attach once, while the clients of other VMs come and go.
    attaching: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Turns true when the server stops: every session ends.
    stopping: tokio::sync::watch::Sender<bool>,
    /// Wakes the reaper before its time.
    reap: Arc<tokio::sync::Notify>,
}

impl Vms {
    pub fn new(providers: Providers, config: VmConfig) -> Arc<Self> {
        let vms = Arc::new(Self {
            providers,
            config,
            attached: Default::default(),
            attaching: Default::default(),
            stopping: tokio::sync::watch::Sender::new(false),
            reap: Default::default(),
        });
        tokio::spawn(reap(Arc::downgrade(&vms), vms.reap.clone()));
        vms
    }

    /// The server stops: every session ends, telling its client why.
    pub fn stop(&self) {
        self.stopping.send_replace(true);
    }

    /// Detach from every VM, and wait until their encoders are finished: the server stops.
    pub async fn detach_all(&self) {
        let attached: Vec<_> = self.attached.lock().await.drain().collect();
        let shut_down = attached.iter().map(|(_, entry)| entry.vm.shut_down());
        futures_util::future::join_all(shut_down).await;
    }

    /// Turns true when the server stops.
    pub fn stopping(&self) -> tokio::sync::watch::Receiver<bool> {
        self.stopping.subscribe()
    }

    /// The names of the VM managers asked.
    pub fn providers(&self) -> Vec<&'static str> {
        self.providers.names()
    }

    /// How the VMs are encoded: what a client must decode.
    pub fn config(&self) -> &VmConfig {
        &self.config
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
        // This VM's turn: a client of the same VM coming meanwhile waits, and then finds it
        // attached; the other VMs' clients do not.
        // UNTESTED: one attachment per VM at a time, none across VMs; the table's lock never
        // held across an attachment (a VM starting takes seconds). Check both when changing the
        // locking: no test attaches VMs.
        let gate = self
            .attaching
            .lock()
            .unwrap()
            .entry(uuid.clone())
            .or_default()
            .clone();
        let turn = gate.lock().await;
        let result = self.client_in_turn(&uuid).await;
        drop(turn);
        // The gate goes with its last user: this one, and none waiting.
        let mut attaching = self.attaching.lock().unwrap();
        if Arc::strong_count(&gate) == 2 {
            attaching.remove(&uuid);
        }
        result
    }

    /// The VM `uuid`, attached if it was not, this VM's attachments being this caller's.
    async fn client_in_turn(&self, uuid: &str) -> Result<Option<Client>> {
        loop {
            let known = {
                let mut attached = self.attached.lock().await;
                if attached.get(uuid).is_some_and(|a| a.vm.ended()) {
                    info!(uuid, "VM gone: attaching again");
                    attached.remove(uuid);
                }
                attached.get(uuid).cloned()
            };
            let entry = match known {
                Some(entry) => entry,
                None => {
                    let Some(how) = self.attach_to(uuid).await? else {
                        return Ok(None);
                    };
                    let vm = match vm::start(&how, uuid, &self.config, self.reap.clone()).await {
                        Ok(vm) => vm,
                        Err(e) => {
                            // What the provider remembered of it may be stale.
                            self.providers.forget(&how);
                            return Err(e);
                        }
                    };
                    // In the table with its client already counted, under one lock: the reaper
                    // never sees it without one (it would detach a VM just attached), and a
                    // server that stops, which empties the table under this lock, either has
                    // it to shut down or finds it not there yet, and it is shut down here.
                    // UNTESTED: both need a VM to attach, and the reaper or a stop in between.
                    let entry = Arc::new(Attached {
                        vm,
                        reap: self.reap.clone(),
                        clients: AtomicUsize::new(1),
                        detach_at: Default::default(),
                    });
                    let mut attached = self.attached.lock().await;
                    if *self.stopping.borrow() {
                        drop(attached);
                        entry.vm.shut_down().await;
                        anyhow::bail!("the server stops");
                    }
                    attached.insert(uuid.to_owned(), entry.clone());
                    drop(attached);
                    return Ok(Some(Client {
                        attached: entry,
                        left: false,
                    }));
                }
            };
            // A VM already attached: its client counted under the table's lock, as the reaper
            // looks: a VM it sees without clients is one no client is joining. The reaper may
            // have detached it since it was looked up: then attach again.
            let attached = self.attached.lock().await;
            if !attached.get(uuid).is_some_and(|e| Arc::ptr_eq(e, &entry)) {
                continue;
            }
            entry.clients.fetch_add(1, Ordering::AcqRel);
            *entry.detach_at.lock().unwrap() = None;
            drop(attached);
            return Ok(Some(Client {
                attached: entry,
                left: false,
            }));
        }
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

/// Detach from the VMs without clients since their time (`DETACH_AFTER`, or
/// `DETACH_AFTER_LEAVING` after a clean close), and from the VMs gone, until the
/// server ends.
async fn reap(vms: std::sync::Weak<Vms>, woken: Arc<tokio::sync::Notify>) {
    let mut interval = tokio::time::interval(REAP_INTERVAL);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            () = woken.notified() => {}
        }
        let Some(vms) = vms.upgrade() else { return };
        let mut attached = vms.attached.lock().await;
        attached.retain(|uuid, entry| {
            if entry.clients.load(Ordering::Acquire) > 0 {
                return true;
            }
            let idle = entry
                .detach_at
                .lock()
                .unwrap()
                .is_none_or(|at| Instant::now() >= at);
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
