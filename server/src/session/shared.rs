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
    /// The connection to the guest agent, while there is one.
    pub agent: watch::Receiver<Option<zbus::Connection>>,
    /// What asks the VM's power actions, if anything can.
    pub power: Option<Arc<crate::vms::power::Control>>,
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
#[derive(Default)]
pub struct User {
    /// Its id, and how to end its session when another takes it over.
    current: std::sync::Mutex<Option<(u64, oneshot::Sender<()>)>>,
    next: std::sync::atomic::AtomicU64,
    /// Counts the users that left (not those taken over): the shadow clients, who watch the
    /// user, go with it.
    left: watch::Sender<u64>,
    /// Held by a user that arrives and by what tells the guest a user left, one at a time:
    /// whether a user came since is settled, and the guest told, before the next user arrives.
    turn: tokio::sync::Mutex<()>,
}

impl User {
    /// A new user takes the session over: the last one's session ends. Returns the new user's
    /// id, and what tells it its own session was taken over. Waits while the guest is being
    /// told a user left (see `after_leaving`).
    pub(crate) async fn take_over(&self) -> (u64, oneshot::Receiver<()>) {
        let _turn = self.turn.lock().await;
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (end, ended) = oneshot::channel();
        if let Some((_, last)) = self.current.lock().unwrap().replace((id, end)) {
            info!("a new client takes the session over");
            let _ = last.send(());
        }
        (id, ended)
    }

    /// The user `id` left; nobody uses the session until the next one comes.
    pub(crate) fn leave(&self, id: u64) {
        let mut current = self.current.lock().unwrap();
        if current.as_ref().is_some_and(|(user, _)| *user == id) {
            *current = None;
            self.left.send_modify(|left| *left += 1);
        }
    }

    /// Changes when the user leaves: the shadow sessions end.
    pub(crate) fn left(&self) -> watch::Receiver<u64> {
        self.left.subscribe()
    }

    /// After the user `id` left, run `tell` (which tells the guest), unless another user came
    /// since: then the session is that user's, whose own departure tells the guest. No user
    /// arrives while `tell` runs. Returns whether it ran.
    pub(crate) async fn after_leaving<F: Future<Output = ()>>(
        &self,
        id: u64,
        tell: impl FnOnce() -> F,
    ) -> bool {
        let _turn = self.turn.lock().await;
        // Ids follow the arrivals, all under the turn: the next one is still `id + 1` while
        // nobody came since.
        if self.next.load(std::sync::atomic::Ordering::Relaxed) != id + 1 {
            return false;
        }
        tell().await;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn the_guest_is_told_unless_a_user_came_since() {
        let user = User::default();
        let (first, _) = user.take_over().await;
        user.leave(first);
        assert!(user.after_leaving(first, || async {}).await);
        // Taken over, then gone: the second user's departure tells the guest, not the first's.
        let (second, _) = user.take_over().await;
        let (third, _) = user.take_over().await;
        user.leave(second);
        assert!(!user.after_leaving(second, || async {}).await);
        user.leave(third);
        assert!(user.after_leaving(third, || async {}).await);
        // Back before the guest was told: it is not.
        let (fourth, _) = user.take_over().await;
        user.leave(fourth);
        let (fifth, _) = user.take_over().await;
        assert!(!user.after_leaving(fourth, || async {}).await);
        user.leave(fifth);
    }

    #[tokio::test]
    async fn no_user_arrives_while_the_guest_is_told() {
        let user = std::sync::Arc::new(User::default());
        let (first, _) = user.take_over().await;
        user.leave(first);
        let (telling, told) = oneshot::channel::<()>();
        let (release, released) = oneshot::channel::<()>();
        let tell = tokio::spawn({
            let user = user.clone();
            async move {
                user.after_leaving(first, || async move {
                    let _ = telling.send(());
                    let _ = released.await;
                })
                .await
            }
        });
        told.await.unwrap();
        let mut arrival = tokio::spawn({
            let user = user.clone();
            async move { user.take_over().await.0 }
        });
        // The arrival waits for the guest to be told.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut arrival)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        assert!(tell.await.unwrap());
        assert_eq!(arrival.await.unwrap(), first + 1);
    }
}
