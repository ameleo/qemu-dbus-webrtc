// SPDX-License-Identifier: MPL-2.0

//! Locking the guest's session when its user leaves, so that whoever opens the console next
//! finds the guest's logon screen rather than the last user's desktop, as with RDP.
//!
//! The guest agent locks it (`fr.ameleo.Agent1.Reset`: `LockWorkStation` on Windows, logind on
//! Linux), and only when a user is logged on and the session is not locked already; the server
//! gives the guest a moment to show it, until the agent says the session is locked
//! (`SessionLocked`). Only that confirms the lock: an agent that wrongly takes the session for
//! locked answers without locking it (the Windows agent does not always update it).
//! Without an agent, or when it does not answer, the server types Win+L, which locks Windows and the common Linux
//! desktops (GNOME, KDE Plasma), unless a policy turned the shortcut off or an application took
//! the keys: nothing tells the server whether it worked.

use std::time::Duration;

use anyhow::{Result, bail};
use qdw_qemu::agent::AgentProxy;
use qdw_qemu::input::KeyboardProxy;
use tokio::sync::watch;
use tracing::{info, warn};

/// The longest a lock may take, through the agent or the keyboard.
const LOCK_TIME: Duration = Duration::from_secs(5);

/// How long the guest has to show the lock after the agent locked it, unless the agent says
/// the session is locked before; and how often the server asks it.
const LOCKED_TIME: Duration = Duration::from_secs(1);
const LOCKED_POLL: Duration = Duration::from_millis(100);

/// How long a lock waits for the agent when it is not connected: just after the server attached
/// to the VM, it connects within moments.
const AGENT_TIME: Duration = Duration::from_secs(1);

/// How long the guest has to lock after Win+L, before anyone sees it: nothing tells when it has.
const WIN_L_SETTLE: Duration = Duration::from_secs(1);

/// The default delay after a lost connection, in seconds: as long as the server keeps a VM
/// attached for its client to come back.
pub const DEFAULT_DELAY: u64 = 60;

/// The longest delay the configuration takes, in seconds.
pub const MAX_DELAY: u64 = 3600;

/// QEMU key numbers (PC scancodes, set 1, 0x80 for the E0 prefix): the left Windows key, L.
const LEFT_META: u32 = 0x80 | 0x5b;
const KEY_L: u32 = 0x26;

/// The modifiers, released before Win+L so that a key a client left down does not make it
/// another shortcut: both Shifts, Ctrls, Alts and Windows keys.
const MODIFIERS: [u32; 8] = [
    0x2a,
    0x36,
    0x1d,
    0x80 | 0x1d,
    0x38,
    0x80 | 0x38,
    LEFT_META,
    0x80 | 0x5c,
];

/// What came of a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The agent says the guest's session is locked.
    Locked,
    /// The agent answered, or Win+L went to the guest: it should be locked, but nothing says it
    /// is.
    Unconfirmed,
    /// Nothing locked it.
    Failed,
}

/// When to lock the guest's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// At once when the user leaves on purpose (`Leave`), after the delay when the connection
    /// is lost and the client does not come back.
    #[default]
    Leave,
    /// At once when the user's session ends, however.
    Immediate,
    /// Never: the guest's session stays as the user left it.
    Never,
}

impl Mode {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "leave" => Self::Leave,
            "immediate" => Self::Immediate,
            "never" => Self::Never,
            _ => bail!("{text}: \"leave\", \"immediate\" or \"never\""),
        })
    }
}

/// When the server locks the guest's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub mode: Mode,
    /// After a lost connection, in `Leave` mode.
    pub delay: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: Mode::Leave,
            delay: Duration::from_secs(DEFAULT_DELAY),
        }
    }
}

/// How the user's session ended, as far as the lock goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Departure {
    /// The client said it leaves (`Leave`).
    Left,
    /// The connection ended without a `Leave`: lost, or a client that does not send it.
    Lost,
    /// Another client took the session over: it is the user now.
    TakenOver,
    /// The server stops: no client reaches the guest until it is back.
    ServerStopping,
    /// The VM stopped: there is nothing to lock.
    VmStopped,
}

impl Policy {
    /// How long after `departure` to lock the guest's session; `None` for not at all.
    pub fn after(&self, departure: Departure) -> Option<Duration> {
        match (self.mode, departure) {
            (Mode::Never, _) | (_, Departure::TakenOver | Departure::VmStopped) => None,
            (_, Departure::Left | Departure::ServerStopping)
            | (Mode::Immediate, Departure::Lost) => Some(Duration::ZERO),
            (Mode::Leave, Departure::Lost) => Some(self.delay),
        }
    }
}

/// What locks the guest's session: its agent, or its keyboard.
#[derive(Clone)]
pub struct Locker {
    /// When to lock.
    pub policy: Policy,
    /// The connection to the guest agent, while there is one.
    pub agent: watch::Receiver<Option<zbus::Connection>>,
    pub keyboard: KeyboardProxy<'static>,
}

impl Locker {
    /// Lock the guest's session now: through its agent; or by typing Win+L without one, or when
    /// it does not answer.
    pub async fn lock(&self) -> Outcome {
        let mut agent_link = self.agent.clone();
        let agent = tokio::time::timeout(AGENT_TIME, agent_link.wait_for(Option::is_some))
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|agent| agent.clone());
        if let Some(agent) = agent {
            match tokio::time::timeout(LOCK_TIME, reset(&agent)).await {
                Ok(Ok(())) => {
                    if locked(&agent).await {
                        info!("guest session locked by its agent");
                        return Outcome::Locked;
                    }
                    info!("guest session locked by its agent, which did not confirm it");
                    return Outcome::Unconfirmed;
                }
                Ok(Err(e)) => {
                    warn!(error = %format!("{e:#}"), "the guest agent did not lock: Win+L instead")
                }
                Err(_) => warn!("the guest agent did not answer: Win+L instead"),
            }
        }
        if self.win_l().await {
            // Nothing says when the guest has locked: a moment, before anyone sees it.
            tokio::time::sleep(WIN_L_SETTLE).await;
            Outcome::Unconfirmed
        } else {
            Outcome::Failed
        }
    }

    /// Type Win+L, the modifiers released first: whether QEMU took the presses. The keys are
    /// released whatever became of the presses, so that none stays down in the guest, each
    /// release bounded in time.
    async fn win_l(&self) -> bool {
        for key in MODIFIERS {
            self.release(key).await;
        }
        let pressed = tokio::time::timeout(LOCK_TIME, async {
            self.keyboard.press(LEFT_META).await?;
            self.keyboard.press(KEY_L).await
        })
        .await;
        for key in [KEY_L, LEFT_META] {
            self.release(key).await;
        }
        match pressed {
            Ok(Ok(())) => {
                info!("Win+L typed to lock the guest session");
                true
            }
            Ok(Err(e)) => {
                warn!(error = %e, "guest session not locked");
                false
            }
            Err(_) => {
                warn!("guest session not locked: QEMU did not take the keys");
                false
            }
        }
    }

    async fn release(&self, key: u32) {
        match tokio::time::timeout(LOCK_TIME, self.keyboard.release(key)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(key, error = %e, "key not released"),
            Err(_) => warn!(key, "key not released: QEMU did not take it"),
        }
    }
}

/// Whether the agent says the guest's session is locked, within `LOCKED_TIME`: it locks it
/// after it answered `Reset`, and the guest shows it moments later.
async fn locked(agent: &zbus::Connection) -> bool {
    let wait = async {
        let proxy = AgentProxy::builder(agent)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        loop {
            if proxy.session_locked().await? {
                return anyhow::Ok(true);
            }
            tokio::time::sleep(LOCKED_POLL).await;
        }
    };
    match tokio::time::timeout(LOCKED_TIME, wait).await {
        Ok(Ok(locked)) => locked,
        Ok(Err(e)) => {
            warn!(error = %format!("{e:#}"), "the guest agent's SessionLocked not read");
            false
        }
        Err(_) => false,
    }
}

/// The agent's `Reset`: what the user's client brought to the guest let go, and the session
/// locked.
async fn reset(agent: &zbus::Connection) -> Result<()> {
    AgentProxy::new(agent).await?.reset().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(Mode::parse("leave").unwrap(), Mode::Leave);
        assert_eq!(Mode::parse("immediate").unwrap(), Mode::Immediate);
        assert_eq!(Mode::parse("never").unwrap(), Mode::Never);
        assert!(Mode::parse("Leave").is_err());
        assert!(Mode::parse("").is_err());
    }

    #[test]
    fn when_to_lock() {
        let delay = Duration::from_secs(30);
        let policy = |mode| Policy { mode, delay };
        let now = Some(Duration::ZERO);
        let leave = policy(Mode::Leave);
        assert_eq!(leave.after(Departure::Left), now);
        assert_eq!(leave.after(Departure::Lost), Some(delay));
        assert_eq!(leave.after(Departure::ServerStopping), now);
        assert_eq!(leave.after(Departure::TakenOver), None);
        assert_eq!(leave.after(Departure::VmStopped), None);
        let immediate = policy(Mode::Immediate);
        assert_eq!(immediate.after(Departure::Left), now);
        assert_eq!(immediate.after(Departure::Lost), now);
        assert_eq!(immediate.after(Departure::TakenOver), None);
        let never = policy(Mode::Never);
        for departure in [
            Departure::Left,
            Departure::Lost,
            Departure::TakenOver,
            Departure::ServerStopping,
            Departure::VmStopped,
        ] {
            assert_eq!(never.after(departure), None);
        }
    }

    #[test]
    fn win_l_is_left_meta_and_l() {
        // E0 5B, the left Windows key; 26, L (set 1).
        assert_eq!(LEFT_META, 0xdb);
        assert_eq!(KEY_L, 0x26);
    }
}
