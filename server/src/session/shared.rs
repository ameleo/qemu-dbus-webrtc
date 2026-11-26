// SPDX-License-Identifier: MPL-2.0

//! What the viewers of a VM share: its screens, the display loop's requests, the user of the
//! session, the viewers' video depths.

use std::collections::HashMap;

use tokio::sync::{mpsc, oneshot, watch};
use tracing::info;

use qdw_video::encode::Codec;

use std::sync::Arc;

use crate::guest::cameras::Cameras;
use crate::guest::clipboard::Clipboard;
use crate::guest::drives::Drives;
use crate::guest::printers::Printers;
use crate::session::audio::Mic;
use crate::session::input::AgentMouse;
use crate::session::layout::Layout;
use crate::session::lock::Outcome;
use crate::session::pointer::Pointer;
use crate::session::transport::{Bitrates, SoundViewers, VideoViewers};

/// A request from a viewer to the display loop.
#[derive(Debug)]
pub enum Request {
    /// The viewer needs a keyframe of this console.
    Keyframe { console: u32 },
    /// The user's connection takes this video bitrate, in kbit/s, for all the screens.
    Bitrate { bitrate: u32 },
    /// Whether every viewer decodes the video in 10 bits.
    TenBits(bool),
}

#[derive(Clone)]
pub struct Shared {
    /// The VM's name, which the offer gives its viewers.
    pub name: String,
    /// Turns true when the VM stops: its viewers' sessions end.
    pub vm_stopped: watch::Receiver<bool>,
    pub codec: Codec,
    /// The video is in 4:4:4.
    pub full_chroma: bool,
    /// The guest's screens, in order: what the viewers get, one video track each.
    pub screens: Vec<Screen>,
    /// The viewers of the guest's sound.
    pub audio_viewers: SoundViewers,
    /// The guest's microphone.
    pub mic: Arc<Mic>,
    pub requests: mpsc::UnboundedSender<Request>,
    /// The keyboard, and the QEMU tablet of the first screen.
    pub input: qdw_qemu::input::Input,
    /// The pointer through the guest agent, on every screen.
    pub agent_mouse: AgentMouse,
    /// The guest pointer on each screen.
    pub pointer: watch::Receiver<Vec<Pointer>>,
    /// Whether the guest takes the pointer by relative moves (see `PointerMode`).
    pub pointer_mode: watch::Receiver<bool>,
    /// The latency of each screen's frames on the server over the last second, by screen:
    /// the mean and the longest, `None` for a screen not encoded then (see `EncodeLatency`).
    pub encode_latency: watch::Receiver<Vec<Option<(std::time::Duration, std::time::Duration)>>>,
    pub clipboard: Arc<Clipboard>,
    pub drives: Arc<Drives>,
    pub printers: Arc<Printers>,
    pub cameras: Arc<Cameras>,
    /// The size and place of the guest's screens.
    pub layout: Arc<Layout>,
    /// The user of the session.
    pub user: Arc<User>,
    /// What locks the guest's session when its user leaves.
    pub locker: crate::session::lock::Locker,
    /// The bounds of the bandwidth estimation.
    pub bitrates: Bitrates,
    /// How clients reach the host from outside.
    pub network: crate::session::transport::Network,
    /// Which viewers decode the video in 10 bits.
    pub depths: Arc<Depths>,
}

/// A guest screen: its QEMU console, its head (the virtio-gpu scanout), and the viewers its
/// encoder feeds.
#[derive(Clone)]
pub struct Screen {
    pub console: u32,
    pub head: u32,
    pub viewers: VideoViewers,
}

/// Which viewers decode the video in 10 bits. The video is shared: it is in 10 bits only when
/// they all do.
#[derive(Default)]
pub struct Depths {
    viewers: std::sync::Mutex<HashMap<u64, bool>>,
    next: std::sync::atomic::AtomicU64,
}

impl Depths {
    /// Every viewer decodes 10 bits, and there is one at least.
    pub(crate) fn all_deep(viewers: &HashMap<u64, bool>) -> bool {
        !viewers.is_empty() && viewers.values().all(|&deep| deep)
    }

    /// A viewer comes: its id, and whether the video may be in 10 bits now.
    pub(crate) fn join(&self, deep: bool) -> (u64, bool) {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut viewers = self.viewers.lock().unwrap();
        viewers.insert(id, deep);
        (id, Self::all_deep(&viewers))
    }

    /// A viewer goes: whether the video may be in 10 bits now.
    pub(crate) fn leave(&self, id: u64) -> bool {
        let mut viewers = self.viewers.lock().unwrap();
        viewers.remove(&id);
        Self::all_deep(&viewers)
    }
}

/// The one client that uses the session; the others are shadow clients, which only watch.
///
/// It also keeps who the guest's session is open to, so that another person who takes the
/// session finds it locked. Changes of hands and locks run one at a time (`turn`): whether to
/// lock, the lock, and who the session is open to then are one step, which no other change of
/// hands nor lock interleaves with.
#[derive(Default)]
pub struct User {
    state: std::sync::Mutex<UserState>,
    /// Held through a change of hands or a lock, the guest's lock included.
    turn: tokio::sync::Mutex<()>,
    /// Counts the users that left (not those taken over): the shadow clients, who watch the
    /// user, go with it.
    left: watch::Sender<u64>,
}

#[derive(Default)]
struct UserState {
    /// The user's id, and how to end its session when another takes it over.
    current: Option<(u64, oneshot::Sender<()>)>,
    /// The next user's id.
    next: u64,
    open_to: OpenTo,
}

/// Who the guest's session is open to, as far as the server knows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum OpenTo {
    /// The server does not know: it has just attached to the VM (after a restart, or a lock
    /// that failed before it detached), or a lock was not confirmed. Whoever comes locks it.
    #[default]
    Unknown,
    /// Nobody: the agent said it locked it.
    Nobody,
    /// The last user, until a lock. An empty identity is nobody in particular: whoever comes
    /// after locks it, the same empty identity included.
    To(String),
}

impl OpenTo {
    /// Whether `identity` must lock the guest's session before it sees it.
    fn locks_for(&self, identity: &str) -> bool {
        match self {
            Self::Unknown => true,
            Self::Nobody => false,
            Self::To(last) => last.is_empty() || last != identity,
        }
    }

    /// After a lock that came to `outcome`; `None` when it failed, and nothing changed.
    fn after(outcome: Outcome) -> Option<Self> {
        match outcome {
            Outcome::Locked => Some(Self::Nobody),
            Outcome::Unconfirmed => Some(Self::Unknown),
            Outcome::Failed => None,
        }
    }
}

impl User {
    /// `identity` takes the session. When the guest's session may be open to another person,
    /// the last user's session ends, and the guest's session is locked, with `lock` (`None`:
    /// the server does not lock); when that fails, the new user is refused: `None`, the
    /// guest's session still open to whom it was (the last user, gone, may come back).
    /// Otherwise the last user's session ends, and the new user's id comes back, with what
    /// tells it its own session was taken over.
    pub(crate) async fn arrive<F, L>(
        &self,
        identity: &str,
        lock: Option<L>,
    ) -> Option<(u64, oneshot::Receiver<()>)>
    where
        L: FnOnce() -> F,
        F: std::future::Future<Output = Outcome>,
    {
        let _turn = self.turn.lock().await;
        let locks = self.state.lock().unwrap().open_to.locks_for(identity);
        if locks && let Some(lock) = lock {
            // The last user lets go of the guest first: its keys no longer reach it.
            self.end_current();
            info!("the session changes hands: the guest's session is locked first");
            let open_to = OpenTo::after(lock().await)?;
            self.state.lock().unwrap().open_to = open_to;
        }
        let mut state = self.state.lock().unwrap();
        let id = state.next;
        state.next += 1;
        let (end, ended) = oneshot::channel();
        if let Some((_, last)) = state.current.replace((id, end)) {
            info!("a new client takes the session over");
            let _ = last.send(());
        }
        state.open_to = OpenTo::To(identity.to_owned());
        Some((id, ended))
    }

    /// End the current user's session, taken over.
    fn end_current(&self) {
        if let Some((_, last)) = self.state.lock().unwrap().current.take() {
            info!("a new client takes the session over");
            let _ = last.send(());
        }
    }

    /// The user `id` left: the guest's session is locked with `lock`, unless a user took the
    /// session since. What came of it; `None` when it was not tried.
    pub(crate) async fn lock_after<F, L>(&self, id: u64, lock: L) -> Option<Outcome>
    where
        L: FnOnce() -> F,
        F: std::future::Future<Output = Outcome>,
    {
        let _turn = self.turn.lock().await;
        if !self.absent_since(id) {
            return None;
        }
        let outcome = lock().await;
        if let Some(open_to) = OpenTo::after(outcome) {
            self.state.lock().unwrap().open_to = open_to;
        }
        Some(outcome)
    }

    /// Whether nobody took the session since the user `id`: it left, and no user came after.
    pub(crate) fn absent_since(&self, id: u64) -> bool {
        let state = self.state.lock().unwrap();
        state.current.is_none() && state.next == id + 1
    }

    /// The user `id` left; nobody uses the session until the next one comes.
    pub(crate) fn leave(&self, id: u64) {
        let mut state = self.state.lock().unwrap();
        if state.current.as_ref().is_some_and(|(user, _)| *user == id) {
            state.current = None;
            self.left.send_modify(|left| *left += 1);
        }
    }

    /// Changes when the user leaves: the shadow sessions end.
    pub(crate) fn left(&self) -> watch::Receiver<u64> {
        self.left.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every sequence of arrivals, departures, locks after a departure, unlocks by the user and
    /// attachments anew (the server's state gone), with two people and one without identity, each
    /// lock confirmed, unconfirmed or failed, a departure of the user possibly during a lock:
    /// nobody ever finds the guest's session open to another person. An unconfirmed lock is
    /// taken to have locked: what the server can do is lock again whoever comes next.
    #[test]
    fn nobody_finds_another_persons_session() {
        every_sequence(4);
    }

    /// The same, a step longer: about a minute (`cargo test -- --ignored`).
    #[test]
    #[ignore]
    fn nobody_finds_another_persons_session_longer() {
        every_sequence(5);
    }

    fn every_sequence(depth: u32) {
        #[derive(Clone, Copy, Debug)]
        enum Op {
            /// A person arrives ("" for nobody in particular); a lock, if asked for, comes to
            /// its outcome, and the user leaves during it or not.
            Arrive(char, Outcome, bool),
            /// The user leaves.
            Leave,
            /// The lock after the last departure.
            LockAfter(Outcome),
            /// The user, at the guest's locked screen, unlocks it.
            Unlock,
            /// No user: the server detaches, and attaches anew without what it knew.
            Attach,
        }
        let outcomes = [Outcome::Locked, Outcome::Unconfirmed, Outcome::Failed];
        let mut ops = Vec::new();
        for who in ['a', 'b', ' '] {
            for outcome in outcomes {
                ops.push(Op::Arrive(who, outcome, false));
            }
            ops.push(Op::Arrive(who, Outcome::Locked, true));
        }
        ops.extend(outcomes.map(Op::LockAfter));
        ops.extend([Op::Leave, Op::Unlock, Op::Attach]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let total = ops.len().pow(depth);
        for n in 0..total {
            let sequence: Vec<Op> = (0..depth)
                .map(|i| ops[n / ops.len().pow(i) % ops.len()])
                .collect();
            runtime.block_on(async {
                let mut user = User::default();
                // Who the guest's desktop is open to, as the guest sees it: `None`, locked. At
                // first, a user's: the server cannot know.
                let guest = std::cell::Cell::new(Some('z'));
                let mut current: Option<(u64, char)> = None;
                let mut departed: Option<u64> = None;
                let identity = |who: char| {
                    if who == ' ' {
                        String::new()
                    } else {
                        who.to_string()
                    }
                };
                for op in &sequence {
                    match *op {
                        Op::Arrive(who, outcome, leave_during) => {
                            let id = std::cell::Cell::new(current.map(|(id, _)| id));
                            let lock = || async {
                                if leave_during && let Some(user_id) = id.get() {
                                    user.leave(user_id);
                                    id.set(None);
                                }
                                if outcome != Outcome::Failed {
                                    guest.set(None);
                                }
                                outcome
                            };
                            let arrived = user.arrive(&identity(who), Some(lock)).await;
                            // Ended by the change of hands, or gone during the lock.
                            let still = user
                                .state
                                .lock()
                                .unwrap()
                                .current
                                .as_ref()
                                .map(|(id, _)| *id);
                            if let Some((last, _)) = current
                                && still != Some(last)
                            {
                                current = None;
                                if id.get().is_none() {
                                    departed = Some(last);
                                }
                            }
                            if let Some((new, _)) = arrived {
                                let open = guest.get();
                                assert!(
                                    open.is_none() || (who != ' ' && open == Some(who)),
                                    "{who:?} finds {open:?}'s session after {sequence:?}"
                                );
                                guest.set(Some(who));
                                current = Some((new, who));
                            }
                        }
                        Op::Leave => {
                            if let Some((id, _)) = current.take() {
                                user.leave(id);
                                departed = Some(id);
                            }
                        }
                        Op::LockAfter(outcome) => {
                            if let Some(id) = departed {
                                let lock = || async {
                                    if outcome != Outcome::Failed {
                                        guest.set(None);
                                    }
                                    outcome
                                };
                                user.lock_after(id, lock).await;
                            }
                        }
                        Op::Unlock => {
                            if let Some((_, who)) = current {
                                guest.set(Some(who));
                            }
                        }
                        Op::Attach => {
                            if current.is_none() {
                                user = User::default();
                                departed = None;
                            }
                        }
                    }
                }
            });
        }
    }

    /// Arrivals at once take turns: the second one waits for the first one's lock, and locks
    /// again if it is another person still.
    #[test]
    fn arrivals_at_once_take_turns() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let user = User::default();
            let locks = std::cell::Cell::new(0);
            let lock = || async {
                locks.set(locks.get() + 1);
                tokio::task::yield_now().await;
                Outcome::Locked
            };
            user.arrive("alice", Some(lock)).await.unwrap();
            assert_eq!(locks.get(), 1, "the first user after an attachment locks");
            let (first, second) = tokio::join!(
                user.arrive("bob", Some(lock)),
                user.arrive("bob", Some(lock))
            );
            assert!(first.is_some() && second.is_some());
            assert_eq!(locks.get(), 2, "the second bob found the session bob's");
            assert!(user.arrive("charlie", Some(lock)).await.is_some());
            assert_eq!(locks.get(), 3);
        });
    }

    /// A lock that fails leaves the session to whom it was open: the new person is refused,
    /// and the last user, back, does not lock it.
    #[test]
    fn a_failed_lock_refuses_the_new_person() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let user = User::default();
            let locked = || async { Outcome::Locked };
            let (alice, _) = user.arrive("alice", Some(locked)).await.unwrap();
            let fail = || async { Outcome::Failed };
            assert!(user.arrive("bob", Some(fail)).await.is_none());
            user.leave(alice);
            let fail = || async { Outcome::Failed };
            assert!(user.arrive("alice", Some(fail)).await.is_some());
        });
    }

    /// A lock after a departure is not done once a user took the session again: it would lock
    /// the screen in front of them and forget who the session is open to.
    #[test]
    fn a_late_lock_spares_who_came_back() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let user = User::default();
            let locked = || async { Outcome::Locked };
            let (alice, _) = user.arrive("alice", Some(locked)).await.unwrap();
            user.leave(alice);
            let (_, _) = user.arrive("alice", Some(locked)).await.unwrap();
            let tried = std::cell::Cell::new(false);
            let lock = || async {
                tried.set(true);
                Outcome::Locked
            };
            assert_eq!(user.lock_after(alice, lock).await, None);
            assert!(!tried.get());
            // Still alice's: bob finds it locked.
            assert!(user.state.lock().unwrap().open_to.locks_for("bob"));
        });
    }

    /// After a lock nothing confirmed, whoever comes locks again, the last user included.
    #[test]
    fn an_unconfirmed_lock_locks_again() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let user = User::default();
            let locked = || async { Outcome::Locked };
            let (alice, _) = user.arrive("alice", Some(locked)).await.unwrap();
            user.leave(alice);
            let unconfirmed = || async { Outcome::Unconfirmed };
            assert_eq!(
                user.lock_after(alice, unconfirmed).await,
                Some(Outcome::Unconfirmed)
            );
            assert!(user.state.lock().unwrap().open_to.locks_for("alice"));
        });
    }

    /// Nobody in particular always locks, even after nobody in particular.
    #[test]
    fn nobody_in_particular_always_locks() {
        assert!(OpenTo::To(String::new()).locks_for(""));
        assert!(OpenTo::To("alice".into()).locks_for(""));
        assert!(OpenTo::Unknown.locks_for("alice"));
        assert!(!OpenTo::Nobody.locks_for(""));
        assert!(!OpenTo::To("alice".into()).locks_for("alice"));
    }

    #[test]
    fn ten_bits_when_every_viewer_decodes_them() {
        let depths = Depths::default();
        let (deep, ten_bits) = depths.join(true);
        assert!(ten_bits);
        let (shallow, ten_bits) = depths.join(false);
        assert!(!ten_bits);
        assert!(depths.leave(shallow));
        assert!(!depths.leave(deep));
    }
}
