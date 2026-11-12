// SPDX-License-Identifier: MPL-2.0

//! The guest's screens at the size and place of the user's monitors.
//!
//! The user's client says the monitors it shows the guest on (`MonitorLayout`): their sizes, and
//! where they are among each other, the first at 0, 0. Once the layout has settled (a window
//! being resized sends many), the guest is asked for it, one guest screen per monitor in order
//! (`SetUIInfo`), and its other screens are turned off: the guest arranges its desktop as the
//! client's, and the pointer goes from one screen to the next as on the client. A request can
//! be lost: sent while the guest is still booting, before its display driver can apply a mode,
//! it is ignored, and the driver then restores the mode Windows remembered. So the layout stays
//! the goal: when a guest screen is still at another size a while after a request, the request
//! goes again, a few times at most in case the guest refuses the mode.

use std::time::Duration;

use qdw_protocol::v1::Monitor;
use qdw_qemu::display::ConsoleProxy;
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{info, warn};

/// How long a layout must stay the same before the guest is asked for it.
const SETTLE: Duration = Duration::from_millis(300);
/// How long the guest has to apply a layout before it counts as missed.
const APPLY_TIME: Duration = Duration::from_secs(3);
/// Requests sent again for one layout, when the guest misses it.
const RETRIES: u8 = 3;
/// The sizes a guest screen may have (those of RDP's display control, MS-RDPEDISP).
const MIN_SIZE: u32 = 200;
const MAX_SIZE: u32 = 8192;

/// A guest screen's size and place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub x: i32,
    pub y: i32,
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
            x: monitor.x,
            y: monitor.y,
            width: bounded(monitor.width),
            height: bounded(monitor.height),
            width_mm: mm(monitor.width_mm),
            height_mm: mm(monitor.height_mm),
        })
    }
}

/// The layout wanted of the guest's screens, and the sizes they show at.
pub struct Layout {
    wanted: watch::Sender<Option<Vec<Size>>>,
    shown: watch::Sender<Vec<(u32, u32)>>,
}

impl Layout {
    /// Follow the wanted layout of the screens of `consoles` (in screen order) until the
    /// program ends.
    pub fn start(consoles: Vec<ConsoleProxy<'static>>) -> std::sync::Arc<Self> {
        let (wanted, wanted_rx) = watch::channel(None);
        let (shown, shown_rx) = watch::channel(vec![(0, 0); consoles.len()]);
        tokio::spawn(run(consoles, wanted_rx, shown_rx));
        std::sync::Arc::new(Self { wanted, shown })
    }

    /// The user's client wants these screens, in order; the other screens off.
    pub fn want(&self, screens: Vec<Size>) {
        self.wanted.send_replace(Some(screens));
    }

    /// Guest screen `screen` shows at this size now.
    pub fn shown(&self, screen: usize, width: u32, height: u32) {
        self.shown.send_modify(|shown| {
            if let Some(s) = shown.get_mut(screen) {
                *s = (width, height);
            }
        });
    }
}

/// Ask the guest for `screens`, and turn its other screens off, in one go.
async fn apply(consoles: &[ConsoleProxy<'static>], screens: &[Size]) {
    for (i, console) in consoles.iter().enumerate() {
        let result = match screens.get(i) {
            Some(s) => {
                console
                    .set_ui_info(s.width_mm, s.height_mm, s.x, s.y, s.width, s.height)
                    .await
            }
            None => console.set_ui_info(0, 0, 0, 0, 0, 0).await,
        };
        if let Err(e) = result {
            warn!(screen = i, error = %e, "guest screen size not requested");
        }
    }
}

fn describe(screens: &[Size]) -> String {
    screens
        .iter()
        .map(|s| format!("{}x{}+{}+{}", s.width, s.height, s.x, s.y))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether the guest shows `screens` at their sizes.
fn matches(screens: &[Size], shown: &[(u32, u32)]) -> bool {
    screens
        .iter()
        .zip(shown)
        .all(|(s, &shown)| shown == (s.width, s.height))
}

async fn run(
    consoles: Vec<ConsoleProxy<'static>>,
    mut wanted: watch::Receiver<Option<Vec<Size>>>,
    mut shown: watch::Receiver<Vec<(u32, u32)>>,
) {
    // The layout last asked of the guest, when, and the requests left for it.
    let mut goal: Option<(Vec<Size>, Instant, u8)> = None;
    loop {
        // When to look whether the guest missed the goal.
        let check = goal
            .as_ref()
            .filter(|(_, _, left)| *left > 0)
            .map(|(_, sent, _)| *sent + APPLY_TIME);
        tokio::select! {
            changed = wanted.changed() => {
                if changed.is_err() {
                    return;
                }
                // Wait for the layout to settle.
                while let Ok(Ok(())) = tokio::time::timeout(SETTLE, wanted.changed()).await {}
                let Some(screens) = wanted.borrow_and_update().clone() else { continue };
                if goal.as_ref().is_some_and(|(g, _, _)| *g == screens) && matches(&screens, &shown.borrow()) {
                    continue;
                }
                info!(screens = %describe(&screens), "guest screens requested");
                apply(&consoles, &screens).await;
                goal = Some((screens, Instant::now(), RETRIES));
            }
            changed = shown.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = async { tokio::time::sleep_until(check.unwrap()).await }, if check.is_some() => {}
        }
        // A miss: a guest screen shows at another size, past the time it has to apply the goal.
        if let Some((screens, sent, left)) = &mut goal
            && *left > 0
            && sent.elapsed() >= APPLY_TIME
            && !matches(screens, &shown.borrow_and_update())
        {
            *left -= 1;
            info!(
                screens = %describe(screens),
                shown = ?*shown.borrow(),
                left = *left,
                "the guest screens are not at the requested sizes: requested again"
            );
            apply(&consoles, screens).await;
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
            x: 1920,
            ..Default::default()
        };
        let size = Size::of(&monitor(3441, 1439)).unwrap();
        assert_eq!(
            (size.x, size.width, size.height, size.width_mm),
            (1920, 3440, 1438, 600)
        );
        let size = Size::of(&monitor(100, 9000)).unwrap();
        assert_eq!((size.width, size.height), (200, 8192));
        assert_eq!(Size::of(&monitor(0, 1080)), None);
    }

    #[test]
    fn a_layout_matches_the_screens_it_names() {
        let size = |width, height| Size {
            x: 0,
            y: 0,
            width,
            height,
            width_mm: 0,
            height_mm: 0,
        };
        assert!(matches(&[size(1920, 1080)], &[(1920, 1080), (0, 0)]));
        assert!(!matches(
            &[size(1920, 1080), size(1280, 720)],
            &[(1920, 1080), (0, 0)]
        ));
    }
}
