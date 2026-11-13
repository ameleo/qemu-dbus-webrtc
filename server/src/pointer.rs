// SPDX-License-Identifier: MPL-2.0

//! The guest pointer, as the viewers draw it, on each guest screen.

use std::sync::Arc;

use qdw_protocol::Message;
use qdw_protocol::v1::{PointerPosition, PointerShape, PointerUpdate, pointer_update::Update};

/// The guest pointer on one screen: what every viewer must show there.
#[derive(Debug, Clone, Default)]
pub struct Pointer {
    pub shape: Option<Arc<PointerShape>>,
    pub visible: bool,
    /// Where the guest put it, once it said: the top left corner of its shape, in guest pixels,
    /// as QEMU gives it (its SPICE display adds the hot spot to it), so negative near the top
    /// and the left of the screen.
    pub position: Option<(i32, i32)>,
}

impl Pointer {
    /// Where its hot spot is on the screen, as the clients take it: the corner of its shape
    /// plus the shape's hot spot.
    fn hot_spot(&self) -> Option<(u32, u32)> {
        let (x, y) = self.position?;
        let (hot_x, hot_y) = self
            .shape
            .as_ref()
            .map_or((0, 0), |shape| (shape.hot_x, shape.hot_y));
        let at =
            |corner: i32, hot: u32| u32::try_from(i64::from(corner) + i64::from(hot)).unwrap_or(0);
        Some((at(x, hot_x), at(y, hot_y)))
    }
}

/// The largest pointer taken, in pixels each way.
const MAX_POINTER: u32 = 512;

/// A pointer shape from QEMU: 32-bit pixels holding 0xAARRGGBB in host order, so B, G, R, A in
/// memory on the little-endian hosts we run on.
pub fn shape(
    width: i32,
    height: i32,
    hot_x: i32,
    hot_y: i32,
    pixels: &[u8],
) -> Option<PointerShape> {
    let (width, height) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    // A pointer is small: a larger shape is a broken one, and the page draws it on a canvas.
    if width > MAX_POINTER || height > MAX_POINTER {
        return None;
    }
    if pixels.len() != width as usize * height as usize * 4 {
        return None;
    }
    let rgba = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[b, g, r, a]| [r, g, b, a])
        .collect();
    Some(PointerShape {
        width,
        height,
        hot_x: u32::try_from(hot_x).unwrap_or(0),
        hot_y: u32::try_from(hot_y).unwrap_or(0),
        rgba,
    })
}

/// What a viewer has been sent of one screen's pointer.
#[derive(Default)]
struct ScreenSent {
    shape: Option<Arc<PointerShape>>,
    visible: Option<bool>,
    position: Option<(u32, u32)>,
}

/// What a viewer has been sent, to send only what changed.
#[derive(Default)]
pub struct Sent {
    screens: Vec<ScreenSent>,
}

impl Sent {
    /// The messages that bring a viewer from what it was sent to `pointers`, one per screen;
    /// their positions only while the pointer is `relative`.
    pub fn update(&mut self, pointers: &[Pointer], relative: bool) -> Vec<Vec<u8>> {
        self.screens
            .resize_with(pointers.len(), ScreenSent::default);
        let mut messages = Vec::new();
        for (screen, (pointer, sent)) in pointers.iter().zip(&mut self.screens).enumerate() {
            let screen = screen as u32;
            if let Some(shape) = &pointer.shape
                && !sent
                    .shape
                    .as_ref()
                    .is_some_and(|sent| Arc::ptr_eq(sent, shape))
            {
                messages.push(encode(screen, Update::Shape((**shape).clone())));
                sent.shape = Some(shape.clone());
            }
            if sent.visible != Some(pointer.visible) {
                messages.push(encode(screen, Update::Visible(pointer.visible)));
                sent.visible = Some(pointer.visible);
            }
            if !relative {
                // Sent again when the pointer becomes relative.
                sent.position = None;
            } else if let Some(position) = pointer.hot_spot()
                && sent.position != Some(position)
            {
                let (x, y) = position;
                messages.push(encode(screen, Update::Position(PointerPosition { x, y })));
                sent.position = Some(position);
            }
        }
        messages
    }
}

fn encode(screen: u32, update: Update) -> Vec<u8> {
    PointerUpdate {
        update: Some(update),
        screen,
    }
    .encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_are_checked() {
        let shape = shape(2, 1, 0, 0, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(shape.rgba, [3, 2, 1, 4, 7, 6, 5, 8]);
        assert!(super::shape(2, 2, 0, 0, &[0; 8]).is_none());
        assert!(super::shape(-1, 1, 0, 0, &[]).is_none());
        assert!(super::shape(513, 1, 0, 0, &[0; 513 * 4]).is_none());
        assert!(super::shape(i32::MAX, i32::MAX, 0, 0, &[]).is_none());
    }

    /// Positions go only while the pointer is relative, and again each time it becomes so.
    #[test]
    fn positions_while_relative() {
        // A shape whose hot spot is at 4, 6: the guest gives the corner, the clients the spot.
        let shape = Arc::new(PointerShape {
            width: 8,
            height: 8,
            hot_x: 4,
            hot_y: 6,
            rgba: vec![0; 8 * 8 * 4],
        });
        let pointers = |x: i32| {
            vec![Pointer {
                shape: Some(shape.clone()),
                visible: true,
                position: Some((x - 4, -1)),
            }]
        };
        let mut sent = Sent::default();
        // The shape and the visibility first, then nothing new while absolute.
        assert_eq!(sent.update(&pointers(1), false).len(), 2);
        assert!(sent.update(&pointers(2), false).is_empty());
        // Relative: the position, once.
        let messages = sent.update(&pointers(2), true);
        assert_eq!(messages.len(), 1);
        let update = PointerUpdate::decode(messages[0].as_slice()).unwrap();
        assert_eq!(
            update.update,
            Some(Update::Position(PointerPosition { x: 2, y: 5 }))
        );
        assert!(sent.update(&pointers(2), true).is_empty());
        assert_eq!(sent.update(&pointers(3), true).len(), 1);
        // Absolute, then relative again: sent again, though it did not move.
        assert!(sent.update(&pointers(3), false).is_empty());
        assert_eq!(sent.update(&pointers(3), true).len(), 1);
    }
}
