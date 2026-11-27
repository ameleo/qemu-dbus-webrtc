// SPDX-License-Identifier: MPL-2.0

//! The power actions a user may ask of their VM, through what manages it: libvirt, when
//! libvirt-dbus knows the domain; QEMU's QMP socket otherwise, when the server attaches through
//! it. Neither for a VM on a D-Bus bus that libvirt does not manage. A clean reboot or shutdown
//! only through the QEMU guest agent, which libvirt reaches: the ACPI power button alone, which
//! the guest's settings may have it ignore, would answer that it was done when nothing is.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use qdw_protocol::v1::PowerAction;
use qdw_qemu::display::Attach;
use qdw_qemu::qmp::Qmp;

use crate::vms::libvirt;
use crate::vms::providers::Providers;

/// How long a power action may take to be taken: libvirt answers once it asked the guest (a
/// clean reboot or shutdown), or once QEMU reset or stopped.
const POWER_TIME: Duration = Duration::from_secs(30);

/// What asks a VM's power actions.
pub enum Control {
    /// libvirt, for the domain of this UUID, with a channel to the QEMU guest agent or not.
    Libvirt {
        system: zbus::Connection,
        uuid: String,
        guest_agent: bool,
    },
    /// QEMU's QMP socket.
    Qmp(PathBuf),
}

impl Control {
    /// What asks the power actions of the VM `uuid`, which the server attaches to by `attach`.
    pub async fn of(providers: &Providers, uuid: &str, attach: &Attach) -> Option<Self> {
        if let Some(libvirt) = &providers.libvirt
            && let Some((system, guest_agent)) = libvirt.knows(uuid).await
        {
            return Some(Self::Libvirt {
                system,
                uuid: uuid.to_owned(),
                guest_agent,
            });
        }
        match attach {
            Attach::P2p { qmp } => Some(Self::Qmp(qmp.clone())),
            Attach::Bus { .. } => None,
        }
    }

    /// The actions it takes: a clean reboot or shutdown only through libvirt and the QEMU guest
    /// agent.
    pub fn actions(&self) -> &'static [PowerAction] {
        match self {
            Self::Libvirt {
                guest_agent: true, ..
            } => &[
                PowerAction::Reboot,
                PowerAction::Shutdown,
                PowerAction::Reset,
                PowerAction::PowerOff,
            ],
            Self::Libvirt { .. } | Self::Qmp(_) => &[PowerAction::Reset, PowerAction::PowerOff],
        }
    }

    /// Take the action `action`, one of `actions`.
    pub async fn run(&self, action: PowerAction) -> Result<()> {
        if !self.actions().contains(&action) {
            bail!("{} cannot be asked of this VM", action.as_str_name());
        }
        let run = async {
            match self {
                Self::Libvirt { system, uuid, .. } => libvirt::power(system, uuid, action).await,
                Self::Qmp(qmp) => {
                    let command = match action {
                        PowerAction::Reset => "system_reset",
                        PowerAction::PowerOff => "quit",
                        PowerAction::Reboot | PowerAction::Shutdown | PowerAction::Unspecified => {
                            bail!("no QMP command for {}", action.as_str_name())
                        }
                    };
                    let qmp = qmp.clone();
                    // QMP is a short blocking exchange; keep it off the async threads.
                    tokio::task::spawn_blocking(move || Qmp::connect(&qmp)?.command(command))
                        .await?
                }
            }
        };
        tokio::time::timeout(POWER_TIME, run)
            .await
            .context("the VM's manager did not answer in time")?
    }
}
