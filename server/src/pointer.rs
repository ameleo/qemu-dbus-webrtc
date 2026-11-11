// SPDX-License-Identifier: MPL-2.0

//! The guest pointer, as the viewers draw it.

use std::sync::Arc;

use qdw_protocol::Message;
use qdw_protocol::v1::{PointerShape, PointerUpdate, pointer_update::Update};

/// The guest pointer of a console: what every viewer must show.
#[derive(Debug, Clone, Default)]
pub struct Pointer {
    pub shape: Option<Arc<PointerShape>>,
    pub visible: bool,
}

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

/// What a viewer has been sent, to send only what changed.
#[derive(Default)]
pub struct Sent {
    shape: Option<Arc<PointerShape>>,
    visible: Option<bool>,
}

impl Sent {
    /// The messages that bring a viewer from what it was sent to `pointer`.
    pub fn update(&mut self, pointer: &Pointer) -> Vec<Vec<u8>> {
        let mut messages = Vec::new();
        if let Some(shape) = &pointer.shape
            && !self
                .shape
                .as_ref()
                .is_some_and(|sent| Arc::ptr_eq(sent, shape))
        {
            messages.push(encode(Update::Shape((**shape).clone())));
            self.shape = Some(shape.clone());
        }
        if self.visible != Some(pointer.visible) {
            messages.push(encode(Update::Visible(pointer.visible)));
            self.visible = Some(pointer.visible);
        }
        messages
    }
}

fn encode(update: Update) -> Vec<u8> {
    PointerUpdate {
        update: Some(update),
    }
    .encode_to_vec()
}
