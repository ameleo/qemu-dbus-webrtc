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
use qdw_protocol::v1::{Button, Input, InputEvent, Key, LockKeys, input_event::Event};
use qdw_qemu::agent::MouseProxy;
use qdw_qemu::input::{self as qemu, CAPS_LOCK, NUM_LOCK, SCROLL_LOCK};
use tracing::debug;

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

/// The guest agent's pointer, while the agent is connected.
#[derive(Clone, Default)]
pub struct AgentMouse(Arc<Mutex<Option<MouseProxy<'static>>>>);

impl AgentMouse {
    /// The agent is connected.
    pub async fn attach(&self, connection: &zbus::Connection) -> zbus::Result<()> {
        let mouse = MouseProxy::new(connection).await?;
        *self.0.lock().unwrap() = Some(mouse);
        Ok(())
    }

    /// The agent went away.
    pub fn detach(&self) {
        *self.0.lock().unwrap() = None;
    }

    fn get(&self) -> Option<MouseProxy<'static>> {
        self.0.lock().unwrap().clone()
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
