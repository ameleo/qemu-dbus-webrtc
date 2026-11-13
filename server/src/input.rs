// SPDX-License-Identifier: MPL-2.0

//! A viewer's keyboard and pointer, applied to the guest.
//!
//! The pointer goes through the guest agent when there is one: it places it on each guest
//! screen, where the QEMU tablet knows one surface only, and scrolls by fractions of a notch.
//! Without the agent, the QEMU tablet, on the first screen.
//!
//! What is pressed is tracked, so that a viewer that goes away leaves no key or button stuck.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures_util::StreamExt;
use qdw_protocol::v1::{Button, Input, InputEvent, Key, LockKeys, input_event::Event};
use qdw_qemu::agent::MouseProxy;
use qdw_qemu::input::{self as qemu, CAPS_LOCK, NUM_LOCK, SCROLL_LOCK};
use tokio::sync::watch;
use tracing::{debug, info};

/// A wheel notch, in the units of `Wheel` messages.
const NOTCH: i32 = 120;
/// The largest wheel step taken from a message, in those units: 20 notches.
const MAX_WHEEL: i32 = 20 * NOTCH;

/// The scancodes of the lock keys.
const CAPS_LOCK_KEY: u32 = 0x3A;
const NUM_LOCK_KEY: u32 = 0x45;
const SCROLL_LOCK_KEY: u32 = 0x46;

/// QEMU's number for Pause, which PC keyboards send as E1 1D 45.
const PAUSE: u32 = 0xC6;

/// The largest relative move taken from a message, in guest pixels each way: the widest screen.
const MAX_MOTION: i32 = 8192;

/// The guest agent's pointer, while the agent is connected, and whether it is.
#[derive(Clone)]
pub struct AgentMouse {
    mouse: Arc<Mutex<Option<MouseProxy<'static>>>>,
    present: watch::Sender<bool>,
}

impl Default for AgentMouse {
    fn default() -> Self {
        Self {
            mouse: Arc::default(),
            present: watch::Sender::new(false),
        }
    }
}

impl AgentMouse {
    /// The agent is connected.
    pub async fn attach(&self, connection: &zbus::Connection) -> zbus::Result<()> {
        let mouse = MouseProxy::new(connection).await?;
        *self.mouse.lock().unwrap() = Some(mouse);
        self.present.send_replace(true);
        Ok(())
    }

    /// The agent went away.
    pub fn detach(&self) {
        *self.mouse.lock().unwrap() = None;
        self.present.send_replace(false);
    }

    fn get(&self) -> Option<MouseProxy<'static>> {
        self.mouse.lock().unwrap().clone()
    }

    /// Whether the agent's pointer is there, and its changes.
    pub fn present(&self) -> watch::Receiver<bool> {
        self.present.subscribe()
    }
}

/// Follow how the guest takes the pointer, into `relative`, until the VM's connection ends:
/// relative when neither the guest agent nor QEMU's pointing device (a tablet) places it, only a
/// mouse moving by steps.
pub async fn follow_mode(
    mouse: qemu::MouseProxy<'static>,
    mut agent: watch::Receiver<bool>,
    relative: watch::Sender<bool>,
) {
    let mut changes = mouse.receive_is_absolute_changed().await;
    loop {
        // Cached, and kept up to date by QEMU's signals: an absolute pointer when unknown.
        let absolute = mouse.is_absolute().await.unwrap_or(true);
        let now = !*agent.borrow_and_update() && !absolute;
        if relative.send_if_modified(|r| std::mem::replace(r, now) != now) {
            info!(relative = now, "pointer mode");
        }
        tokio::select! {
            Some(_) = changes.next() => {}
            Ok(()) = agent.changed() => {}
            else => return,
        }
    }
}

/// The input of one viewer.
pub struct ViewerInput {
    qemu: qemu::Input,
    agent: AgentMouse,
    /// The guest's head of each screen, in screen order.
    heads: Vec<u32>,
    keys: HashSet<u32>,
    buttons: HashSet<qemu::Button>,
    /// Wheel rotation not yet sent, less than a notch.
    wheel: (i32, i32),
}

impl ViewerInput {
    pub fn new(qemu: qemu::Input, agent: AgentMouse, heads: Vec<u32>) -> Self {
        Self {
            qemu,
            agent,
            heads,
            keys: HashSet::new(),
            buttons: HashSet::new(),
            wheel: (0, 0),
        }
    }

    pub async fn apply(&mut self, input: Input) -> Result<()> {
        for InputEvent { event } in input.events {
            match event {
                Some(Event::Key(key)) => self.key(&key).await?,
                Some(Event::PointerMove(m)) => {
                    let head = self.heads.get(m.screen as usize).copied();
                    match (self.agent.get(), head) {
                        (Some(mouse), Some(head)) => {
                            mouse.set_abs_position(head, m.x, m.y).await?;
                        }
                        // The QEMU tablet: the first screen only.
                        (None, _) if m.screen == 0 => self.qemu.move_to(m.x, m.y).await?,
                        _ => {}
                    }
                }
                Some(Event::PointerButton(b)) => {
                    let button = match b.button() {
                        Button::Left => qemu::Button::Left,
                        Button::Middle => qemu::Button::Middle,
                        Button::Right => qemu::Button::Right,
                        Button::Back => qemu::Button::Side,
                        Button::Forward => qemu::Button::Extra,
                        Button::Unspecified => continue,
                    };
                    debug!(?button, down = b.down, "button");
                    self.button(button, b.down).await?;
                    if b.down {
                        self.buttons.insert(button);
                    } else {
                        self.buttons.remove(&button);
                    }
                }
                // Only for the QEMU mouse moving by steps: QEMU refuses it from a tablet, and the
                // agent places the pointer.
                Some(Event::PointerMotion(m)) if self.agent.get().is_none() => {
                    let clamp = |d: i32| d.clamp(-MAX_MOTION, MAX_MOTION);
                    self.qemu.move_by(clamp(m.dx), clamp(m.dy)).await?;
                }
                Some(Event::PointerMotion(_)) => {}
                Some(Event::Wheel(w)) => self.wheel(w.dx, w.dy).await?,
                Some(Event::LockKeys(locks)) => self.lock_keys(&locks).await?,
                // An event of a newer client.
                None => {}
            }
        }
        Ok(())
    }

    async fn key(&mut self, key: &Key) -> Result<()> {
        let keycode = match (key.scancode, key.extended, key.extended1) {
            (0x1D, false, true) => PAUSE,
            (code @ 1..=0x7F, extended, false) => code | if extended { 0x80 } else { 0 },
            _ => {
                debug!(?key, "unknown key");
                return Ok(());
            }
        };
        debug!(keycode = %format!("{keycode:#x}"), down = key.down, "key");
        self.qemu.key(keycode, key.down).await?;
        if key.down {
            self.keys.insert(keycode);
        } else {
            self.keys.remove(&keycode);
        }
        Ok(())
    }

    async fn button(&self, button: qemu::Button, down: bool) -> Result<()> {
        match self.agent.get() {
            Some(mouse) if down => mouse.press(button as u32).await?,
            Some(mouse) => mouse.release(button as u32).await?,
            None => self.qemu.button(button, down).await?,
        }
        Ok(())
    }

    /// Through the agent, as it comes, in 1/120 of a notch; to the QEMU tablet, whole notches,
    /// the rest waiting for more.
    async fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        // A step of at most `MAX_WHEEL` notches: a client's value is not trusted to be sane,
        // and a huge one would have the guest scroll for ever.
        let (dx, dy) = (
            dx.clamp(-MAX_WHEEL, MAX_WHEEL),
            dy.clamp(-MAX_WHEEL, MAX_WHEEL),
        );
        if let Some(mouse) = self.agent.get() {
            mouse.scroll(dx, dy).await?;
            return Ok(());
        }
        // The rest of the last steps is less than a notch: no overflow.
        self.wheel.0 += dx;
        self.wheel.1 += dy;
        while self.wheel.1 >= NOTCH {
            self.qemu.click(qemu::Button::WheelUp).await?;
            self.wheel.1 -= NOTCH;
        }
        while self.wheel.1 <= -NOTCH {
            self.qemu.click(qemu::Button::WheelDown).await?;
            self.wheel.1 += NOTCH;
        }
        while self.wheel.0 >= NOTCH {
            self.qemu.click(qemu::Button::WheelRight).await?;
            self.wheel.0 -= NOTCH;
        }
        while self.wheel.0 <= -NOTCH {
            self.qemu.click(qemu::Button::WheelLeft).await?;
            self.wheel.0 += NOTCH;
        }
        Ok(())
    }

    /// Toggle the guest's lock keys that differ from the client's.
    async fn lock_keys(&mut self, locks: &LockKeys) -> Result<()> {
        let guest = self.qemu.keyboard.modifiers().await?;
        debug!(?locks, guest, "lock keys");
        for (wanted, bit, key) in [
            (locks.caps_lock, CAPS_LOCK, CAPS_LOCK_KEY),
            (locks.num_lock, NUM_LOCK, NUM_LOCK_KEY),
            (locks.scroll_lock, SCROLL_LOCK, SCROLL_LOCK_KEY),
        ] {
            if wanted != (guest & bit != 0) {
                self.qemu.key(key, true).await?;
                self.qemu.key(key, false).await?;
            }
        }
        Ok(())
    }

    /// Release whatever the viewer left pressed: everything, a failure on one key not leaving
    /// the next ones pressed; the first failure, if any.
    pub async fn release_all(&mut self) -> Result<()> {
        let mut first = Ok(());
        for keycode in std::mem::take(&mut self.keys) {
            if let Err(e) = self.qemu.key(keycode, false).await
                && first.is_ok()
            {
                first = Err(e);
            }
        }
        for button in std::mem::take(&mut self.buttons) {
            if let Err(e) = self.button(button, false).await
                && first.is_ok()
            {
                first = Err(e);
            }
        }
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QEMU's `org.qemu.Display1.Mouse`, as far as the mode goes.
    struct FakeMouse {
        absolute: bool,
    }

    #[zbus::interface(name = "org.qemu.Display1.Mouse")]
    impl FakeMouse {
        #[zbus(property)]
        fn is_absolute(&self) -> bool {
            self.absolute
        }
    }

    const PATH: &str = "/org/qemu/Display1/Console_0";

    /// The pointer is relative when neither the agent nor QEMU's device places it, and follows
    /// their changes.
    #[tokio::test]
    async fn the_mode_follows_the_guest() {
        let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
        let guid = zbus::Guid::generate();
        let (qemu, connection) = tokio::join!(
            zbus::connection::Builder::unix_stream(theirs)
                .server(guid)
                .unwrap()
                .p2p()
                .serve_at(PATH, FakeMouse { absolute: true })
                .unwrap()
                .build(),
            zbus::connection::Builder::unix_stream(ours).p2p().build(),
        );
        let (qemu, connection) = (qemu.unwrap(), connection.unwrap());
        let mouse = qemu::MouseProxy::builder(&connection)
            .path(PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        let agent = AgentMouse::default();
        let (tx, mut relative) = watch::channel(false);
        let task = tokio::spawn(follow_mode(mouse, agent.present(), tx));
        let settled = |relative: &mut watch::Receiver<bool>, wanted: bool| {
            let mut relative = relative.clone();
            async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    relative.wait_for(|r| *r == wanted),
                )
                .await
                .expect("the mode did not follow")
                .unwrap();
            }
        };
        settled(&mut relative, false).await;
        // The guest's tablet goes: QEMU's mouse moves by steps.
        let iface = qemu
            .object_server()
            .interface::<_, FakeMouse>(PATH)
            .await
            .unwrap();
        iface.get_mut().await.absolute = false;
        iface
            .get()
            .await
            .is_absolute_changed(iface.signal_emitter())
            .await
            .unwrap();
        settled(&mut relative, true).await;
        // The agent comes: it places the pointer.
        agent.present.send_replace(true);
        settled(&mut relative, false).await;
        agent.present.send_replace(false);
        settled(&mut relative, true).await;
        task.abort();
    }
}
