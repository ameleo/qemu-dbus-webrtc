// SPDX-License-Identifier: MPL-2.0

//! The consent of the user signed in to the guest, asked through its agent on their own desktop,
//! before someone else views the session or takes control of it (`fr.ameleo.Agent.Consent1`).
//! The server never decides for the user: without an agent that asks, nobody watches.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use qdw_qemu::agent::{self, AgentProxy, ConsentProxy, consent, iface};
use tokio::sync::oneshot;
use tracing::{debug, info};

/// How long the user has to answer, with a margin for the agent's own timeout and the calls.
const ANSWER_TIME: Duration = Duration::from_secs(consent::TIMEOUT_SECS as u64 + 5);

/// What a consent lets the person do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// See the guest's screens and hear its sound.
    View,
    /// Act in the guest with their keyboard and pointer, besides the user.
    Control,
}

impl Kind {
    fn code(self) -> u32 {
        match self {
            Kind::View => consent::VIEW,
            Kind::Control => consent::CONTROL,
        }
    }
}

/// Why nobody was let in.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// No agent, or one that does not ask its user.
    NoAgent,
    /// The user said no, did not answer, or nobody could answer (no user signed in, the session
    /// locked, another request shown): the agent does not say which.
    No,
}

/// The consents of one VM's guest user.
#[derive(Default)]
pub struct Consents {
    /// The agent's consent interface, while an agent that implements it is connected.
    agent: Mutex<Option<ConsentProxy<'static>>>,
    next: AtomicU32,
    /// The requests the user has not answered yet: what to tell the asker.
    pending: Mutex<HashMap<u32, oneshot::Sender<Option<oneshot::Receiver<()>>>>>,
    /// The consents granted: dropped (or sent to) when they end, which their holder sees.
    granted: Mutex<HashMap<u32, oneshot::Sender<()>>>,
}

/// A consent granted, until the user stops it or the holder drops it (which tells the agent).
pub struct Granted {
    id: u32,
    consents: Arc<Consents>,
    stopped: oneshot::Receiver<()>,
}

impl Granted {
    /// Resolves when the user stopped it, or it no longer holds (the agent went).
    pub async fn stopped(&mut self) {
        let _ = (&mut self.stopped).await;
    }
}

impl Drop for Granted {
    fn drop(&mut self) {
        self.consents.granted.lock().unwrap().remove(&self.id);
        self.consents.end(self.id);
    }
}

/// A request waiting for the user's answer: withdrawn when dropped before it.
struct Asking {
    id: u32,
    consents: Arc<Consents>,
    answered: bool,
}

impl Drop for Asking {
    fn drop(&mut self) {
        if !self.answered {
            self.consents.pending.lock().unwrap().remove(&self.id);
            // Granted meanwhile, nobody holds it: it ends.
            self.consents.granted.lock().unwrap().remove(&self.id);
            self.consents.end(self.id);
        }
    }
}

impl Consents {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Serve the host side, where the agent answers.
    pub fn serve(
        self: &Arc<Self>,
        builder: zbus::connection::Builder<'static>,
    ) -> zbus::connection::Builder<'static> {
        builder.serve_at(
            agent::HOST_PATH,
            HostConsent {
                consents: self.clone(),
            },
        )
    }

    /// The agent is connected: ask through it, if it asks its user.
    pub async fn attach(&self, connection: &zbus::Connection) -> Result<()> {
        let channels = AgentProxy::new(connection).await?.channels().await?;
        if !channels.iter().any(|c| c == iface::CONSENT) {
            bail!("the agent does not ask its user's consent: nobody may watch");
        }
        *self.agent.lock().unwrap() = Some(ConsentProxy::new(connection).await?);
        Ok(())
    }

    /// The agent is gone, and its consents with it: the requests are refused, and what was
    /// granted ends.
    pub fn detach(&self) {
        *self.agent.lock().unwrap() = None;
        self.pending.lock().unwrap().clear();
        self.granted.lock().unwrap().clear();
    }

    /// Ask the user signed in to the guest whether `who` may do `kind`, and wait for the answer.
    pub async fn ask(self: &Arc<Self>, kind: Kind, who: &str) -> Result<Granted, Refused> {
        let Some(agent) = self.agent.lock().unwrap().clone() else {
            return Err(Refused::NoAgent);
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tell, told) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tell);
        let mut asking = Asking {
            id,
            consents: self.clone(),
            answered: false,
        };
        let who: String = who.chars().take(consent::WHO_MAX).collect();
        info!(id, ?kind, who, "asking the guest's user");
        if let Err(e) = agent.ask(id, kind.code(), &who).await {
            debug!(id, error = %e, "consent not asked");
            return Err(Refused::NoAgent);
        }
        let answer = tokio::time::timeout(ANSWER_TIME, told).await;
        match answer {
            Ok(Ok(Some(stopped))) => {
                asking.answered = true;
                info!(id, ?kind, who, "the guest's user agreed");
                Ok(Granted {
                    id,
                    consents: self.clone(),
                    stopped,
                })
            }
            // No, the agent gone, or no answer in time: `asking` withdraws the request.
            _ => {
                info!(id, ?kind, who, "the guest's user did not agree");
                Err(Refused::No)
            }
        }
    }

    /// Tell the agent that `id` is over, if it is still there.
    fn end(&self, id: u32) {
        let Some(agent) = self.agent.lock().unwrap().clone() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = agent.end(id).await {
                debug!(id, error = %e, "consent end not told");
            }
        });
    }

    /// The user's answer to `id`: a consent granted is kept from now on, so that a stop that
    /// follows at once is not lost.
    fn answered(&self, id: u32, granted: bool) {
        let Some(tell) = self.pending.lock().unwrap().remove(&id) else {
            return;
        };
        let stopped = granted.then(|| {
            let (stop, stopped) = oneshot::channel();
            self.granted.lock().unwrap().insert(id, stop);
            stopped
        });
        let _ = tell.send(stopped);
    }

    /// The user stopped `id`, or it no longer holds.
    fn stopped(&self, id: u32) {
        info!(id, "the guest's user stopped what they agreed to");
        self.granted.lock().unwrap().remove(&id);
    }
}

/// `fr.ameleo.Host.Consent1`: the agent's answers and stops.
struct HostConsent {
    consents: Arc<Consents>,
}

#[zbus::interface(name = "fr.ameleo.Host.Consent1")]
impl HostConsent {
    fn answered(&self, id: u32, granted: bool) {
        self.consents.answered(id, granted);
    }

    fn stopped(&self, id: u32) {
        self.consents.stopped(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request as `ask` leaves it, without an agent: its id, what tells the asker.
    fn pending(
        consents: &Arc<Consents>,
    ) -> (u32, oneshot::Receiver<Option<oneshot::Receiver<()>>>) {
        let id = consents.next.fetch_add(1, Ordering::Relaxed);
        let (tell, told) = oneshot::channel();
        consents.pending.lock().unwrap().insert(id, tell);
        (id, told)
    }

    #[tokio::test]
    async fn a_no_grants_nothing() {
        let consents = Consents::new();
        let (id, told) = pending(&consents);
        consents.answered(id, false);
        assert!(told.await.unwrap().is_none());
        assert!(consents.granted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_stop_right_after_the_yes_is_kept() {
        let consents = Consents::new();
        let (id, told) = pending(&consents);
        consents.answered(id, true);
        consents.stopped(id);
        // The asker hears the yes after the stop: its consent is over already.
        let mut stopped = told.await.unwrap().unwrap();
        assert!(stopped.try_recv().is_err());
        assert!(matches!(
            stopped.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn answers_count_once_and_for_known_requests() {
        let consents = Consents::new();
        let (id, told) = pending(&consents);
        consents.answered(id + 7, true);
        assert!(consents.granted.lock().unwrap().is_empty());
        consents.answered(id, false);
        consents.answered(id, true);
        assert!(told.await.unwrap().is_none());
        assert!(consents.granted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_agent_going_ends_everything() {
        let consents = Consents::new();
        let (asked, told) = pending(&consents);
        let (id, granted) = pending(&consents);
        consents.answered(id, true);
        let mut stopped = granted.await.unwrap().unwrap();
        consents.detach();
        // The request is refused, the consent ends.
        assert!(told.await.is_err());
        assert!(matches!(
            stopped.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(consents.pending.lock().unwrap().get(&asked).is_none());
    }

    #[tokio::test]
    async fn no_agent_no_watching() {
        let consents = Consents::new();
        assert_eq!(
            consents.ask(Kind::View, "Alice").await.err(),
            Some(Refused::NoAgent)
        );
    }
}
