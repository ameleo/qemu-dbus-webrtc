// SPDX-License-Identifier: MPL-2.0

//! The consent of the user signed in to the guest, asked through its agent on their own desktop,
//! before someone else views the session or takes control of it (`fr.ameleo.Agent.Consent1`).
//! The server never decides for the user: without an agent that asks, nobody watches.
//!
//! All the state is under one lock, so that each transition (a request made, answered,
//! withdrawn; a consent granted, stopped; the agent coming or going) is whole: an answer and the
//! agent going cannot interleave so that a consent outlives its agent. Each agent connection has
//! its generation: what an agent gone says is not heard.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use qdw_qemu::agent::{self, AgentProxy, ConsentProxy, consent, iface};
use tokio::sync::{oneshot, watch};
use tracing::{debug, info};

/// How long the user has to answer, with a margin for the agent's own timeout and the calls.
const ANSWER_TIME: Duration = Duration::from_secs(consent::TIMEOUT_SECS as u64 + 5);
/// How long a request waits for an agent that is connecting (the VM just attached, the agent
/// restarting), before it is refused as having none.
const AGENT_TIME: Duration = Duration::from_secs(15);

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

/// Where the agent stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Not connected yet, or connecting again: it may well come.
    Connecting,
    /// Connected, and asks its user.
    Ready,
    /// No agent device, or an agent that does not ask: none will.
    Absent,
}

/// A consent granted: dropped (which tells its holder) when it ends.
struct Live {
    // Held to be dropped: that ends the consent for its holder.
    _stop: oneshot::Sender<()>,
    over: Arc<AtomicBool>,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.over.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct State {
    /// The agent connection's generation: answers and stops of another are not heard.
    generation: u64,
    /// The agent's consent interface, while an agent that asks is connected.
    agent: Option<ConsentProxy<'static>>,
    next: u32,
    /// The requests not answered yet: what to tell the asker.
    pending: HashMap<u32, oneshot::Sender<Option<Answer>>>,
    /// The consents granted.
    granted: HashMap<u32, Live>,
}

/// A yes, as the asker gets it.
struct Answer {
    stopped: oneshot::Receiver<()>,
    over: Arc<AtomicBool>,
}

/// The consents of one VM's guest user.
pub struct Consents {
    state: Mutex<State>,
    phase: watch::Sender<Phase>,
}

/// A consent granted, until the user stops it or the holder drops it (which tells the agent).
pub struct Granted {
    id: u32,
    generation: u64,
    consents: Arc<Consents>,
    stopped: oneshot::Receiver<()>,
    over: Arc<AtomicBool>,
}

impl Granted {
    /// Resolves when the user stopped it, or it no longer holds (the agent went).
    pub async fn stopped(&mut self) {
        let _ = (&mut self.stopped).await;
    }

    /// Whether it ended already: checked before acting on it.
    pub fn is_over(&self) -> bool {
        self.over.load(Ordering::Acquire)
    }
}

impl Drop for Granted {
    fn drop(&mut self) {
        self.consents.withdraw(self.id, self.generation);
    }
}

/// A request waiting for the user's answer: withdrawn when dropped before it.
struct Asking {
    id: u32,
    generation: u64,
    consents: Arc<Consents>,
    answered: bool,
}

impl Drop for Asking {
    fn drop(&mut self) {
        if !self.answered {
            self.consents.withdraw(self.id, self.generation);
        }
    }
}

impl Consents {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            phase: watch::Sender::new(Phase::Connecting),
        })
    }

    /// Serve the host side, where the agent answers, for the connection about to be made.
    pub fn serve(
        self: &Arc<Self>,
        builder: zbus::connection::Builder<'static>,
    ) -> zbus::connection::Builder<'static> {
        let generation = self.state.lock().unwrap().generation;
        builder.serve_at(
            agent::HOST_PATH,
            HostConsent {
                consents: self.clone(),
                generation,
            },
        )
    }

    /// The agent is connected: ask through it, if it asks its user.
    pub async fn attach(&self, connection: &zbus::Connection) -> Result<()> {
        let channels = AgentProxy::new(connection).await?.channels().await?;
        if !channels.iter().any(|c| c == iface::CONSENT) {
            self.phase.send_replace(Phase::Absent);
            bail!("the agent does not ask its user's consent: nobody may watch");
        }
        let proxy = ConsentProxy::new(connection).await?;
        self.state.lock().unwrap().agent = Some(proxy);
        self.phase.send_replace(Phase::Ready);
        Ok(())
    }

    /// The agent is gone, and its consents with it: the requests are refused, what was granted
    /// ends, and what it says from now on is not heard. Another agent may come.
    pub fn detach(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.generation += 1;
            state.agent = None;
            state.pending.clear();
            state.granted.clear();
        }
        self.phase.send_replace(Phase::Connecting);
    }

    /// The VM has no agent device: none will come.
    pub fn absent(&self) {
        self.phase.send_replace(Phase::Absent);
    }

    /// Ask the user signed in to the guest whether `who` may do `kind`, and wait for the answer.
    pub async fn ask(self: &Arc<Self>, kind: Kind, who: &str) -> Result<Granted, Refused> {
        // An agent connecting is waited for; one that does not ask, or none, refuses.
        let mut phase = self.phase.subscribe();
        let settled = phase.wait_for(|phase| *phase != Phase::Connecting);
        match tokio::time::timeout(AGENT_TIME, settled).await {
            Ok(Ok(phase)) if *phase == Phase::Ready => {}
            _ => return Err(Refused::NoAgent),
        }
        let (tell, told) = oneshot::channel();
        let (agent, id, generation) = {
            let mut state = self.state.lock().unwrap();
            let Some(agent) = state.agent.clone() else {
                return Err(Refused::NoAgent);
            };
            let id = state.next;
            state.next = state.next.wrapping_add(1);
            state.pending.insert(id, tell);
            (agent, id, state.generation)
        };
        let mut asking = Asking {
            id,
            generation,
            consents: self.clone(),
            answered: false,
        };
        let who: String = who.chars().take(consent::WHO_MAX).collect();
        info!(id, ?kind, who, "asking the guest's user");
        if let Err(e) = agent.ask(id, kind.code(), &who).await {
            debug!(id, error = %e, "consent not asked");
            return Err(Refused::NoAgent);
        }
        match tokio::time::timeout(ANSWER_TIME, told).await {
            Ok(Ok(Some(answer))) => {
                asking.answered = true;
                info!(id, ?kind, who, "the guest's user agreed");
                Ok(Granted {
                    id,
                    generation,
                    consents: self.clone(),
                    stopped: answer.stopped,
                    over: answer.over,
                })
            }
            // No, the agent gone, or no answer in time: `asking` withdraws the request.
            _ => {
                info!(id, ?kind, who, "the guest's user did not agree");
                Err(Refused::No)
            }
        }
    }

    /// The request or consent `id` is over on this side: forget it, and tell the agent, if it is
    /// the one it was made with.
    fn withdraw(&self, id: u32, generation: u64) {
        let agent = {
            let mut state = self.state.lock().unwrap();
            if state.generation != generation {
                return;
            }
            state.pending.remove(&id);
            state.granted.remove(&id);
            state.agent.clone()
        };
        let Some(agent) = agent else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = agent.end(id).await {
                debug!(id, error = %e, "consent end not told");
            }
        });
    }

    /// The user's answer to `id`: a consent granted is kept from now on, in the same step, so
    /// that the agent going or a stop that follows is not lost.
    fn answered(&self, generation: u64, id: u32, granted: bool) {
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return;
        }
        let Some(tell) = state.pending.remove(&id) else {
            return;
        };
        let answer = granted.then(|| {
            let (stop, stopped) = oneshot::channel();
            let over = Arc::new(AtomicBool::new(false));
            state.granted.insert(
                id,
                Live {
                    _stop: stop,
                    over: over.clone(),
                },
            );
            Answer { stopped, over }
        });
        // The asker gone meanwhile: what was granted ends with its answer dropped.
        if let Err(Some(_)) = tell.send(answer) {
            state.granted.remove(&id);
        }
    }

    /// The user stopped `id`, or it no longer holds.
    fn stopped(&self, generation: u64, id: u32) {
        let mut state = self.state.lock().unwrap();
        if state.generation == generation && state.granted.remove(&id).is_some() {
            info!(id, "the guest's user stopped what they agreed to");
        }
    }
}

/// `fr.ameleo.Host.Consent1`: the agent's answers and stops, for one agent connection.
struct HostConsent {
    consents: Arc<Consents>,
    generation: u64,
}

#[zbus::interface(name = "fr.ameleo.Host.Consent1")]
impl HostConsent {
    fn answered(&self, id: u32, granted: bool) {
        self.consents.answered(self.generation, id, granted);
    }

    fn stopped(&self, id: u32) {
        self.consents.stopped(self.generation, id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request as `ask` leaves it, without an agent: its id, what tells the asker.
    fn pending(consents: &Consents) -> (u32, oneshot::Receiver<Option<Answer>>) {
        let mut state = consents.state.lock().unwrap();
        let id = state.next;
        state.next += 1;
        let (tell, told) = oneshot::channel();
        state.pending.insert(id, tell);
        (id, told)
    }

    fn closed(answer: &mut Answer) -> bool {
        matches!(
            answer.stopped.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ) && answer.over.load(Ordering::Acquire)
    }

    #[tokio::test]
    async fn a_no_grants_nothing() {
        let consents = Consents::new();
        let (id, told) = pending(&consents);
        consents.answered(0, id, false);
        assert!(told.await.unwrap().is_none());
        assert!(consents.state.lock().unwrap().granted.is_empty());
    }

    #[tokio::test]
    async fn a_stop_right_after_the_yes_is_kept() {
        let consents = Consents::new();
        let (id, told) = pending(&consents);
        consents.answered(0, id, true);
        consents.stopped(0, id);
        // The asker hears the yes after the stop: its consent is over already.
        let mut answer = told.await.unwrap().unwrap();
        assert!(closed(&mut answer));
    }

    #[tokio::test]
    async fn answers_count_once_and_for_known_requests() {
        let consents = Consents::new();
        let (id, told) = pending(&consents);
        consents.answered(0, id + 7, true);
        assert!(consents.state.lock().unwrap().granted.is_empty());
        consents.answered(0, id, false);
        consents.answered(0, id, true);
        assert!(told.await.unwrap().is_none());
        assert!(consents.state.lock().unwrap().granted.is_empty());
    }

    #[tokio::test]
    async fn the_agent_going_ends_everything() {
        let consents = Consents::new();
        let (_, told) = pending(&consents);
        let (id, granted) = pending(&consents);
        consents.answered(0, id, true);
        let mut answer = granted.await.unwrap().unwrap();
        consents.detach();
        // The request is refused, the consent ends.
        assert!(told.await.is_err());
        assert!(closed(&mut answer));
    }

    #[tokio::test]
    async fn a_gone_agent_is_not_heard() {
        let consents = Consents::new();
        let (id, told) = pending(&consents);
        // The connection the request went through goes; its late yes grants nothing, even to a
        // request of the same id made through the next one.
        consents.detach();
        assert!(told.await.is_err());
        let (again, told) = pending(&consents);
        assert_eq!(again, id + 1);
        consents.answered(0, again, true);
        assert!(consents.state.lock().unwrap().granted.is_empty());
        consents.answered(1, again, false);
        assert!(told.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_yes_nobody_waits_for_grants_nothing() {
        let consents = Consents::new();
        let (id, told) = pending(&consents);
        drop(told);
        consents.answered(0, id, true);
        assert!(consents.state.lock().unwrap().granted.is_empty());
    }

    #[tokio::test]
    async fn no_agent_no_watching() {
        let consents = Consents::new();
        consents.absent();
        assert_eq!(
            consents.ask(Kind::View, "Alice").await.err(),
            Some(Refused::NoAgent)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_connecting_is_waited_for() {
        let consents = Consents::new();
        // Still connecting past the wait: refused as having none.
        let started = tokio::time::Instant::now();
        assert_eq!(
            consents.ask(Kind::View, "Alice").await.err(),
            Some(Refused::NoAgent)
        );
        assert!(started.elapsed() >= AGENT_TIME);
        // Settling during the wait ends it.
        let asking = tokio::spawn({
            let consents = consents.clone();
            async move { consents.ask(Kind::View, "Alice").await.err() }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        consents.absent();
        assert_eq!(asking.await.unwrap(), Some(Refused::NoAgent));
    }
}
