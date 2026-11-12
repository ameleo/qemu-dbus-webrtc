// SPDX-License-Identifier: MPL-2.0

//! A framebuffer QEMU copies in its calls (`Scanout`, `Update`), when it cannot share it: kept
//! in a memory file of our own, which the encoder reads as it reads a shared one.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use anyhow::{Context, Result, bail};

/// Bytes per pixel of the formats QEMU uses for its framebuffers.
const PIXEL: u64 = 4;

pub struct Copied {
    file: File,
    width: u32,
    height: u32,
    stride: u32,
}

impl Copied {
    /// A framebuffer of `width`x`height` pixels, `stride` bytes per row, with `data` as its
    /// content.
    pub fn new(width: u32, height: u32, stride: u32, data: &[u8]) -> Result<Self> {
        let size = u64::from(stride) * u64::from(height);
        if (data.len() as u64) < size || u64::from(stride) < u64::from(width) * PIXEL {
            bail!("a {width}x{height} framebuffer of {} bytes", data.len());
        }
        let fd = rustix::fs::memfd_create(
            "qemu-dbus-webrtc framebuffer",
            rustix::fs::MemfdFlags::CLOEXEC,
        )
        .context("memory file for the framebuffer")?;
        let file = File::from(fd);
        file.set_len(size)?;
        file.write_all_at(&data[..size as usize], 0)?;
        Ok(Self {
            file,
            width,
            height,
            stride,
        })
    }

    pub fn fd(&self) -> Result<OwnedFd> {
        Ok(self.file.try_clone()?.into())
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    /// The rectangle at `x`, `y` of `width`x`height` pixels changed: `data` holds its rows,
    /// `stride` bytes apart.
    pub fn update(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        data: &[u8],
    ) -> Result<()> {
        let (Ok(x), Ok(y), Ok(width), Ok(height)) = (
            u32::try_from(x),
            u32::try_from(y),
            u32::try_from(width),
            u32::try_from(height),
        ) else {
            bail!("an update at {x},{y} of {width}x{height}");
        };
        if x.saturating_add(width) > self.width || y.saturating_add(height) > self.height {
            bail!(
                "an update at {x},{y} of {width}x{height} out of a {}x{} framebuffer",
                self.width,
                self.height
            );
        }
        let row = u64::from(width) * PIXEL;
        for line in 0..u64::from(height) {
            let from = (line * u64::from(stride)) as usize;
            let Some(pixels) = data.get(from..from + row as usize) else {
                bail!(
                    "an update of {} bytes for {width}x{height} pixels",
                    data.len()
                );
            };
            let at = (u64::from(y) + line) * u64::from(self.stride) + u64::from(x) * PIXEL;
            self.file.write_all_at(pixels, at)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_land_in_place() {
        let copied = Copied::new(4, 2, 16, &[0; 32]).unwrap();
        // A 2x1 rectangle at 1,1, from rows 8 bytes apart.
        copied
            .update(1, 1, 2, 1, 8, &[1, 2, 3, 4, 5, 6, 7, 8])
            .unwrap();
        let mut content = [0u8; 32];
        copied.file.read_exact_at(&mut content, 0).unwrap();
        assert_eq!(&content[20..28], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(content[..20].iter().all(|&b| b == 0));
        assert!(copied.update(3, 1, 2, 1, 8, &[0; 8]).is_err());
    }
}
