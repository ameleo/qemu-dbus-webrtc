// SPDX-License-Identifier: MPL-2.0

//! The guest's screen at the size of the user's window.
//!
//! The user's client says the size it wants (`MonitorLayout`); once it has settled (a window
//! being resized sends many), the guest is asked for it (`SetUIInfo`). A request can be lost:
//! sent while the guest is still booting, before its display driver can apply a mode, it is
//! ignored, and the driver then restores the mode Windows remembered. So the size stays the
//! goal: when the guest's screen is still at another size a while after a request, the request
//! goes again, a few times at most in case the guest refuses the mode.

use std::time::Duration;

use qdw_protocol::v1::Monitor;
use qdw_qemu::display::ConsoleProxy;
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{info, warn};

/// How long a size must stay the same before the guest is asked for it.
const SETTLE: Duration = Duration::from_millis(300);
/// How long the guest has to apply a size before it counts as missed.
const APPLY_TIME: Duration = Duration::from_secs(3);
/// Requests sent again for one size, when the guest misses it.
const RETRIES: u8 = 3;
/// The sizes a guest screen may have (those of RDP's display control, MS-RDPEDISP).
const MIN_SIZE: u32 = 200;
const MAX_SIZE: u32 = 8192;

/// A size the guest is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
    pub width_mm: u16,
    pub height_mm: u16,
}

impl Size {
    /// The size of `monitor` that a guest takes: even (for the video's chroma) and within
    /// bounds. `None` for a monitor without a size.
    pub fn of(monitor: &Monitor) -> Option<Self> {
        if monitor.width == 0 || monitor.height == 0 {
            return None;
        }
        let bounded = |v: u32| v.clamp(MIN_SIZE, MAX_SIZE) & !1;
        let mm = |v: u32| u16::try_from(v).unwrap_or(0);
        Some(Self {
            width: bounded(monitor.width),
            height: bounded(monitor.height),
            width_mm: mm(monitor.width_mm),
            height_mm: mm(monitor.height_mm),
        })
    }
}

/// The size wanted of the guest's screen, and the size it shows at.
pub struct Layout {
    wanted: watch::Sender<Option<Size>>,
    shown: watch::Sender<(u32, u32)>,
}

impl Layout {
    /// Follow the wanted size of `console`'s screen until the program ends.
    pub fn start(console: ConsoleProxy<'static>) -> std::sync::Arc<Self> {
        let (wanted, wanted_rx) = watch::channel(None);
        let (shown, shown_rx) = watch::channel((0, 0));
        tokio::spawn(run(console, wanted_rx, shown_rx));
        std::sync::Arc::new(Self { wanted, shown })
    }

    /// The user's client wants this size.
    pub fn want(&self, size: Size) {
        self.wanted.send_replace(Some(size));
    }

    /// The guest's screen shows at this size now.
    pub fn shown(&self, width: u32, height: u32) {
        self.shown.send_replace((width, height));
    }
}

async fn apply(console: &ConsoleProxy<'static>, size: Size) {
    if let Err(e) = console
        .set_ui_info(size.width_mm, size.height_mm, 0, 0, size.width, size.height)
        .await
    {
        warn!(error = %e, "guest screen size not requested");
    }
}

async fn run(
    console: ConsoleProxy<'static>,
    mut wanted: watch::Receiver<Option<Size>>,
    mut shown: watch::Receiver<(u32, u32)>,
) {
    // The size last asked of the guest, when, and the requests left for it.
    let mut goal: Option<(Size, Instant, u8)> = None;
    loop {
        // When to look whether the guest missed the goal.
        let check = goal
            .filter(|&(_, _, left)| left > 0)
            .map(|(_, sent, _)| sent + APPLY_TIME);
        tokio::select! {
            changed = wanted.changed() => {
                if changed.is_err() {
                    return;
                }
                // Wait for the size to settle.
                while let Ok(Ok(())) = tokio::time::timeout(SETTLE, wanted.changed()).await {}
                let Some(size) = *wanted.borrow_and_update() else { continue };
                if goal.is_some_and(|(g, _, _)| g == size) && *shown.borrow() == (size.width, size.height) {
                    continue;
                }
                info!(size = %format!("{}x{}", size.width, size.height), "guest screen size requested");
                apply(&console, size).await;
                goal = Some((size, Instant::now(), RETRIES));
            }
            changed = shown.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = async { tokio::time::sleep_until(check.unwrap()).await }, if check.is_some() => {}
        }
        // A miss: the guest shows at another size, past the time it has to apply the goal.
        if let Some((size, sent, left)) = &mut goal
            && *left > 0
            && sent.elapsed() >= APPLY_TIME
            && *shown.borrow_and_update() != (size.width, size.height)
        {
            *left -= 1;
            info!(
                size = %format!("{}x{}", size.width, size.height),
                shown = ?*shown.borrow(),
                left = *left,
                "the guest screen is not at the requested size: requested again"
            );
            apply(&console, *size).await;
            *sent = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_the_guest_takes() {
        let monitor = |width, height| Monitor {
            width,
            height,
            width_mm: 600,
            ..Default::default()
        };
        let size = Size::of(&monitor(3441, 1439)).unwrap();
        assert_eq!((size.width, size.height, size.width_mm), (3440, 1438, 600));
        let size = Size::of(&monitor(100, 9000)).unwrap();
        assert_eq!((size.width, size.height), (200, 8192));
        assert_eq!(Size::of(&monitor(0, 1080)), None);
    }
}
