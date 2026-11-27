// SPDX-License-Identifier: MPL-2.0

//! The power actions a user may ask of their VM, through what manages it: libvirt, when
//! libvirt-dbus knows the domain; QEMU's QMP socket otherwise, when the server attaches through
//! it. Neither for a VM on a D-Bus bus that libvirt does not manage. Only a reset: a guest that
//! answers restarts or shuts down from its own menu.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use qdw_protocol::v1::PowerAction;
use qdw_qemu::display::Attach;
use qdw_qemu::qmp::Qmp;

use crate::vms::libvirt;
use crate::vms::providers::Providers;

/// How long a power action may take: libvirt and QEMU answer once the VM was reset.
const POWER_TIME: Duration = Duration::from_secs(30);

/// What asks a VM's power actions.
pub enum Control {
    /// libvirt, for the domain of this UUID.
    Libvirt {
        system: zbus::Connection,
        uuid: String,
    },
    /// QEMU's QMP socket.
    Qmp(PathBuf),
}

impl Control {
    /// What asks the power actions of the VM `uuid`, which the server attaches to by `attach`.
    pub async fn of(providers: &Providers, uuid: &str, attach: &Attach) -> Option<Self> {
        if let Some(libvirt) = &providers.libvirt
            && let Some(system) = libvirt.knows(uuid).await
        {
            return Some(Self::Libvirt {
                system,
                uuid: uuid.to_owned(),
            });
        }
        match attach {
            Attach::P2p { qmp } => Some(Self::Qmp(qmp.clone())),
            Attach::Bus { .. } => None,
        }
    }

    /// The actions it takes.
    pub fn actions(&self) -> &'static [PowerAction] {
        &[PowerAction::Reset]
    }

    /// Take the action `action`, one of `actions`.
    pub async fn run(&self, action: PowerAction) -> Result<()> {
        if action != PowerAction::Reset {
            bail!("{} cannot be asked of this VM", action.as_str_name());
        }
        let run = async {
            match self {
                Self::Libvirt { system, uuid } => libvirt::reset(system, uuid).await,
                Self::Qmp(qmp) => {
                    let qmp = qmp.clone();
                    // QMP is a short blocking exchange; keep it off the async threads.
                    tokio::task::spawn_blocking(move || Qmp::connect(&qmp)?.command("system_reset"))
                        .await?
                }
            }
        };
        tokio::time::timeout(POWER_TIME, run)
            .await
            .context("the VM's manager did not answer in time")?
    }
}
