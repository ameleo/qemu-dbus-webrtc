// SPDX-License-Identifier: MPL-2.0

//! From RGB to YUV on the CPU, for the encoder that runs there: BT.601, limited range (Rec.
//! ITU-R BT.601-7, 2.5.3, in 8 bits), as the GPU converts for the others. In 4:2:0, the
//! chroma of 2x2 pixels is averaged; past an odd size, the last column and row repeat.

use crate::{Frame, PixelFormat};

/// One plane to fill: its rows of `stride` bytes.
pub(crate) struct Plane<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
}

/// The red, green and blue of a pixel at `x`, `y`, clamped to the frame.
#[inline]
fn rgb(frame: &Frame<'_>, x: u32, y: u32) -> (i32, i32, i32) {
    let (x, y) = (x.min(frame.width - 1), y.min(frame.height - 1));
    let i = y as usize * frame.stride + x as usize * 4;
    let p = &frame.pixels[i..i + 4];
    match frame.format {
        PixelFormat::Bgrx => (i32::from(p[2]), i32::from(p[1]), i32::from(p[0])),
        PixelFormat::Rgbx => (i32::from(p[0]), i32::from(p[1]), i32::from(p[2])),
    }
}

/// Y, Cb and Cr in 8 bits from 8-bit R, G, B, in fixed point with 16 bits of fraction:
/// BT.601's coefficients times 219/255 (luma) and 224/255 (chroma), times 65536, rounded;
/// each set of chroma coefficients adds up to 0, the luma's to 219 * 65536 / 255.
#[inline]
fn luma((r, g, b): (i32, i32, i32)) -> u8 {
    (((16829 * r + 33039 * g + 6416 * b + 32768) >> 16) + 16) as u8
}

#[inline]
fn chroma((r, g, b): (i32, i32, i32)) -> (u8, u8) {
    let cb = ((-9714 * r - 19070 * g + 28784 * b + 32768) >> 16) + 128;
    let cr = ((28784 * r - 24103 * g - 4681 * b + 32768) >> 16) + 128;
    (cb as u8, cr as u8)
}

/// Convert `frame` into the three planes, its chroma at full resolution when `full_chroma`,
/// on several threads, a band of rows each.
pub(crate) fn convert(frame: &Frame<'_>, full_chroma: bool, planes: [Plane<'_>; 3]) {
    let [y_plane, u_plane, v_plane] = planes;
    let (y_stride, c_stride) = (y_plane.stride, u_plane.stride);
    debug_assert_eq!(u_plane.stride, v_plane.stride);
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(8);
    let step = if full_chroma { 1 } else { 2 };
    let chroma_width = frame.width.div_ceil(step) as usize;
    let chroma_rows = frame.height.div_ceil(step) as usize;
    // Bands of whole chroma rows, so that no two threads write the same row.
    let band = chroma_rows.div_ceil(threads).max(1);
    let luma_band = band * step as usize;
    std::thread::scope(|scope| {
        let y_rows = y_plane.data.chunks_mut(y_stride * luma_band);
        let u_rows = u_plane.data.chunks_mut(c_stride * band);
        let v_rows = v_plane.data.chunks_mut(c_stride * band);
        for (index, ((y, u), v)) in y_rows.zip(u_rows).zip(v_rows).enumerate() {
            let first = index * band;
            scope.spawn(move || {
                for cy in first..(first + band).min(chroma_rows) {
                    let row = (cy - first) * c_stride;
                    for cx in 0..chroma_width {
                        let (mut r, mut g, mut b) = (0, 0, 0);
                        for dy in 0..step {
                            let py = cy as u32 * step + dy;
                            for dx in 0..step {
                                let px = cx as u32 * step + dx;
                                let pixel = rgb(frame, px, py);
                                if px < frame.width && py < frame.height {
                                    let at = (py as usize - first * step as usize) * y_stride
                                        + px as usize;
                                    y[at] = luma(pixel);
                                }
                                r += pixel.0;
                                g += pixel.1;
                                b += pixel.2;
                            }
                        }
                        let count = (step * step) as i32;
                        let half = count / 2;
                        let (cb, cr) =
                            chroma(((r + half) / count, (g + half) / count, (b + half) / count));
                        u[row + cx] = cb;
                        v[row + cx] = cr;
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values of BT.601's limited range, from its coefficients rounded: black, white and
    /// the primaries (red 16 + 65.481, green 16 + 128.553, blue 16 + 24.966).
    #[test]
    fn bt601_limited_range() {
        assert_eq!(luma((0, 0, 0)), 16);
        assert_eq!(luma((255, 255, 255)), 235);
        assert_eq!(chroma((0, 0, 0)), (128, 128));
        assert_eq!(chroma((255, 255, 255)), (128, 128));
        // Blue: Cb at its top (240); red: Cr at its top.
        assert_eq!(chroma((0, 0, 255)).0, 240);
        assert_eq!(chroma((255, 0, 0)).1, 240);
        assert_eq!(luma((255, 0, 0)), 81);
        assert_eq!(luma((0, 255, 0)), 145);
        assert_eq!(luma((0, 0, 255)), 41);
    }

    /// An odd frame in 4:2:0: every luma sample written, the chroma of the last column and
    /// row from the pixels there are.
    #[test]
    fn odd_frames() {
        let (width, height) = (3u32, 3u32);
        let mut pixels = vec![0u8; 12 * 3];
        // The right column red, the rest white.
        for y in 0..3 {
            for x in 0..3 {
                let i = y * 12 + x * 4;
                pixels[i..i + 4].copy_from_slice(if x == 2 { &[0, 0, 255, 0] } else { &[255; 4] });
            }
        }
        let frame = Frame {
            pixels: &pixels,
            format: PixelFormat::Bgrx,
            width,
            height,
            stride: 12,
        };
        let (mut y, mut u, mut v) = (vec![0; 3 * 3], vec![0; 2 * 2], vec![0; 2 * 2]);
        convert(
            &frame,
            false,
            [
                Plane {
                    data: &mut y,
                    stride: 3,
                },
                Plane {
                    data: &mut u,
                    stride: 2,
                },
                Plane {
                    data: &mut v,
                    stride: 2,
                },
            ],
        );
        assert_eq!(y, [235, 235, 81, 235, 235, 81, 235, 235, 81]);
        // The left pairs are white; the right ones red, the last column repeated.
        assert_eq!(u[0], 128);
        assert_eq!((u[1], v[1]), chroma((255, 0, 0)));
        assert_eq!((u[3], v[3]), chroma((255, 0, 0)));
    }
}
