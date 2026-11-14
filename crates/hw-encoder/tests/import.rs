// SPDX-License-Identifier: MPL-2.0

//! A frame in a DMA-BUF, read by the GPU where it is: a picture in a udmabuf (memory a
//! DMA-BUF exports, as QEMU gives a guest's framebuffer with `blob=on`), encoded in HEVC and
//! AV1 and decoded. Skipped without udmabuf or a GPU that encodes.

mod common;

use std::ffi::c_void;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use common::{check_quality, decoded, picture};
use qdw_hw_encoder::{Codec, DmaBuf, Settings};
use rustix::ioctl::{Ioctl, IoctlOutput, Opcode, opcode};

/// `struct udmabuf_create` (linux/udmabuf.h).
#[repr(C)]
struct UdmabufCreate {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

/// `UDMABUF_CREATE`, which returns the DMA-BUF.
struct Create(UdmabufCreate);

// SAFETY: the opcode is the one of linux/udmabuf.h, its argument a `struct udmabuf_create`
// read by the kernel, and its result a new descriptor.
unsafe impl Ioctl for Create {
    type Output = OwnedFd;
    const IS_MUTATING: bool = false;

    fn opcode(&self) -> Opcode {
        opcode::write::<UdmabufCreate>(b'u', 0x42)
    }

    fn as_ptr(&mut self) -> *mut c_void {
        std::ptr::from_mut(&mut self.0).cast()
    }

    unsafe fn output_from_ptr(out: IoctlOutput, _: *mut c_void) -> rustix::io::Result<OwnedFd> {
        // SAFETY: the descriptor the kernel just made, ours alone.
        Ok(unsafe { OwnedFd::from_raw_fd(out) })
    }
}

/// A DMA-BUF holding `pixels`, padded to whole pages; `None` without udmabuf.
fn udmabuf(pixels: &[u8]) -> Option<(OwnedFd, u32)> {
    use rustix::fs::{MemfdFlags, SealFlags};
    let size = pixels.len().next_multiple_of(4096);
    let memfd =
        rustix::fs::memfd_create("frame", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).unwrap();
    rustix::fs::ftruncate(&memfd, size as u64).unwrap();
    rustix::io::write(&memfd, pixels).unwrap();
    // udmabuf takes only memory that cannot shrink.
    rustix::fs::fcntl_add_seals(&memfd, SealFlags::SHRINK).unwrap();
    let device = rustix::fs::open(
        "/dev/udmabuf",
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .ok()?;
    use std::os::fd::AsRawFd;
    let create = Create(UdmabufCreate {
        memfd: memfd.as_raw_fd() as u32,
        flags: 0,
        offset: 0,
        size: size as u64,
    });
    // SAFETY: the ioctl and its argument match, see `Create`.
    let buffer = unsafe { rustix::ioctl::ioctl(&device, create) }.ok()?;
    Some((buffer, size as u32))
}

fn encode(codec: Codec, width: u32, height: u32) {
    let Some(mut encoder) = common::encoder(Settings::new(codec, 4_000_000)) else {
        return;
    };
    // The bytes of the picture are B, G, R, X: DRM's XRGB8888, "XR24".
    let pixels = picture(width, height, 0);
    let Some((fd, size)) = udmabuf(&pixels) else {
        eprintln!("skipped: no udmabuf");
        return;
    };
    let frame = encoder
        .import(&DmaBuf {
            fd: fd.as_fd(),
            size,
            fourcc: u32::from_le_bytes(*b"XR24"),
            modifier: 0,
            offset: 0,
            stride: width * 4,
            width,
            height,
        })
        .unwrap();
    // The same buffer twice: a key frame, then a frame predicted from it.
    let mut stream = Vec::new();
    for keyframe in [true, false] {
        stream.extend(encoder.encode_imported(&frame, keyframe).unwrap().data);
    }
    let (format, decoder, shown) = match codec {
        Codec::Av1 => ("obu", "libdav1d", (width, height)),
        Codec::Hevc => (
            "hevc",
            "hevc",
            (width.next_multiple_of(2), height.next_multiple_of(2)),
        ),
    };
    let Some(pictures) = decoded(&stream, format, decoder, shown, false, false) else {
        eprintln!("no ffmpeg: the stream is not decoded");
        return;
    };
    check_quality(
        &[pixels.clone(), pixels],
        &pictures,
        (width, height),
        false,
        40.0,
    );
}

#[test]
fn hevc() {
    encode(Codec::Hevc, 320, 240);
    encode(Codec::Hevc, 1920, 1080);
}

#[test]
fn av1() {
    encode(Codec::Av1, 320, 240);
    encode(Codec::Av1, 1920, 1080);
}

/// A format the encoder does not take is refused when imported.
#[test]
fn unknown_formats_are_refused() {
    let Some(encoder) = common::encoder(Settings::new(Codec::Av1, 4_000_000)) else {
        return;
    };
    let pixels = picture(64, 64, 0);
    let Some((fd, size)) = udmabuf(&pixels) else {
        return;
    };
    let buffer = DmaBuf {
        fd: fd.as_fd(),
        size,
        fourcc: u32::from_le_bytes(*b"NV12"),
        modifier: 0,
        offset: 0,
        stride: 64,
        width: 64,
        height: 64,
    };
    assert!(encoder.import(&buffer).is_err());
}
