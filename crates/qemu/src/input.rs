// SPDX-License-Identifier: MPL-2.0

//! Keyboard and pointer of a console (`org.qemu.Display1.Keyboard`, `org.qemu.Display1.Mouse`).
//!
//! The calls expect no reply: input goes out in order without waiting for QEMU, as fast as it
//! comes.

use anyhow::Result;

#[zbus::proxy(interface = "org.qemu.Display1.Keyboard", default_service = "org.qemu")]
pub trait Keyboard {
    /// `keycode`: a QEMU key number, the PC scancode (set 1) with 0x80 for the E0 prefix.
    #[zbus(no_reply)]
    fn press(&self, keycode: u32) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn release(&self, keycode: u32) -> zbus::Result<()>;
    /// The guest's lock keys: [`SCROLL_LOCK`], [`NUM_LOCK`], [`CAPS_LOCK`].
    #[zbus(property)]
    fn modifiers(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(interface = "org.qemu.Display1.Mouse", default_service = "org.qemu")]
pub trait Mouse {
    #[zbus(no_reply)]
    fn press(&self, button: u32) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn release(&self, button: u32) -> zbus::Result<()>;
    #[zbus(no_reply)]
    fn set_abs_position(&self, x: u32, y: u32) -> zbus::Result<()>;
}

/// Bits of [`KeyboardProxy::modifiers`].
pub const SCROLL_LOCK: u32 = 1 << 0;
pub const NUM_LOCK: u32 = 1 << 1;
pub const CAPS_LOCK: u32 = 1 << 2;

/// QEMU's buttons (`InputButton` in QEMU's `qapi/ui.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Button {
    Left = 0,
    Middle = 1,
    Right = 2,
    WheelUp = 3,
    WheelDown = 4,
    /// The back button.
    Side = 5,
    /// The forward button.
    Extra = 6,
    WheelLeft = 7,
    WheelRight = 8,
}

/// The keyboard and pointer of one console.
#[derive(Clone)]
pub struct Input {
    pub keyboard: KeyboardProxy<'static>,
    pub mouse: MouseProxy<'static>,
}

impl Input {
    pub async fn new(vm: &zbus::Connection, console: u32) -> Result<Self> {
        let path = format!("/org/qemu/Display1/Console_{console}");
        Ok(Self {
            keyboard: KeyboardProxy::builder(vm)
                .path(path.clone())?
                // The lock keys change in the guest: always ask.
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await?,
            mouse: MouseProxy::builder(vm).path(path)?.build().await?,
        })
    }

    pub async fn key(&self, keycode: u32, down: bool) -> Result<()> {
        if down {
            self.keyboard.press(keycode).await?;
        } else {
            self.keyboard.release(keycode).await?;
        }
        Ok(())
    }

    pub async fn button(&self, button: Button, down: bool) -> Result<()> {
        if down {
            self.mouse.press(button as u32).await?;
        } else {
            self.mouse.release(button as u32).await?;
        }
        Ok(())
    }

    /// One click of a wheel button: what QEMU makes of a wheel notch.
    pub async fn click(&self, button: Button) -> Result<()> {
        self.mouse.press(button as u32).await?;
        self.mouse.release(button as u32).await?;
        Ok(())
    }

    pub async fn move_to(&self, x: u32, y: u32) -> Result<()> {
        self.mouse.set_abs_position(x, y).await?;
        Ok(())
    }
}
