// SPDX-License-Identifier: MPL-2.0

//! VA-API, the video API of Intel and AMD GPUs on Linux: the bindings to libva, and the few
//! objects the encoders need over them, each released when dropped.
//!
//! Every object keeps its display alive, so they can be dropped in any order.

use std::ffi::CStr;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;

use crate::error::{Error, Result};

/// The bindings bindgen generates from the installed headers.
#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::all
)]
pub mod sys {
    include!(concat!(env!("OUT_DIR"), "/va.rs"));
}

/// A pixel format's code, as `VA_FOURCC` makes it (a macro, which bindgen leaves out).
pub const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

/// An error out of a libva status.
fn check(call: &'static str, status: sys::VAStatus) -> Result<()> {
    if status == sys::VA_STATUS_SUCCESS as sys::VAStatus {
        return Ok(());
    }
    // SAFETY: vaErrorStr returns a static string for any status.
    let message = unsafe { CStr::from_ptr(sys::vaErrorStr(status)) }
        .to_string_lossy()
        .into_owned();
    Err(Error::Va {
        call,
        status,
        message,
    })
}

/// A GPU, through its DRM render node.
pub struct Display {
    raw: sys::VADisplay,
    /// The render node, which the display uses until it is terminated.
    _node: File,
}

// SAFETY: libva allows calls on one display from several threads (its drivers lock their own
// state), and the display is terminated once, when the last object using it is dropped.
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

impl Display {
    /// Open the GPU of a render node: `/dev/dri/renderD128`.
    pub fn open(node: &Path) -> Result<Arc<Self>> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(node)
            .map_err(Error::Device)?;
        // SAFETY: the descriptor is open, and kept open as long as the display.
        let raw = unsafe { sys::vaGetDisplayDRM(file.as_raw_fd()) };
        if raw.is_null() {
            return Err(Error::Unsupported(format!(
                "{} is not a VA-API device",
                node.display()
            )));
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: a display just obtained, and two integers to write the version to.
        let status = unsafe { sys::vaInitialize(raw, &mut major, &mut minor) };
        if let Err(error) = check("vaInitialize", status) {
            // SAFETY: the display is ours and used by nothing else.
            unsafe { sys::vaTerminate(raw) };
            return Err(error);
        }
        Ok(Arc::new(Self { raw, _node: file }))
    }

    /// The render nodes of this host, in order.
    pub fn nodes() -> Vec<PathBuf> {
        let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        nodes
    }

    /// The driver's name and version.
    pub fn vendor(&self) -> String {
        // SAFETY: an initialized display; the string, when there is one, lives as long as it.
        let vendor = unsafe { sys::vaQueryVendorString(self.raw) };
        if vendor.is_null() {
            return String::new();
        }
        // SAFETY: a NUL-terminated string of the driver.
        unsafe { CStr::from_ptr(vendor) }
            .to_string_lossy()
            .into_owned()
    }

    /// Whether the GPU has `entrypoint` for `profile`.
    pub fn has(&self, profile: sys::VAProfile, entrypoint: sys::VAEntrypoint) -> bool {
        // SAFETY: an initialized display.
        let max = unsafe { sys::vaMaxNumEntrypoints(self.raw) }.max(0) as usize;
        let mut entrypoints = vec![0; max];
        let mut count = 0;
        // SAFETY: the list holds as many entrypoints as the display may return.
        let status = unsafe {
            sys::vaQueryConfigEntrypoints(self.raw, profile, entrypoints.as_mut_ptr(), &mut count)
        };
        status == sys::VA_STATUS_SUCCESS as sys::VAStatus
            && entrypoints[..(count.max(0) as usize).min(max)].contains(&entrypoint)
    }

    /// The value of a configuration attribute, when the driver has it.
    pub fn attribute(
        &self,
        profile: sys::VAProfile,
        entrypoint: sys::VAEntrypoint,
        attribute: sys::VAConfigAttribType,
    ) -> Option<u32> {
        let mut attrib = sys::VAConfigAttrib {
            type_: attribute,
            value: 0,
        };
        // SAFETY: one attribute to fill.
        let status =
            unsafe { sys::vaGetConfigAttributes(self.raw, profile, entrypoint, &mut attrib, 1) };
        (status == sys::VA_STATUS_SUCCESS as sys::VAStatus
            && attrib.value != sys::VA_ATTRIB_NOT_SUPPORTED)
            .then_some(attrib.value)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: the last use of the display: every object made from it holds it.
        unsafe { sys::vaTerminate(self.raw) };
    }
}

/// What a context does: a profile, an entrypoint and their attributes.
pub struct Config {
    display: Arc<Display>,
    id: sys::VAConfigID,
}

impl Config {
    pub fn new(
        display: &Arc<Display>,
        profile: sys::VAProfile,
        entrypoint: sys::VAEntrypoint,
        attributes: &[(sys::VAConfigAttribType, u32)],
    ) -> Result<Self> {
        let mut attributes: Vec<sys::VAConfigAttrib> = attributes
            .iter()
            .map(|&(type_, value)| sys::VAConfigAttrib { type_, value })
            .collect();
        let mut id = 0;
        // SAFETY: the list and its length; libva only reads it.
        let status = unsafe {
            sys::vaCreateConfig(
                display.raw,
                profile,
                entrypoint,
                attributes.as_mut_ptr(),
                attributes.len() as i32,
                &mut id,
            )
        };
        check("vaCreateConfig", status)?;
        Ok(Self {
            display: display.clone(),
            id,
        })
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: our configuration, destroyed once.
        unsafe { sys::vaDestroyConfig(self.display.raw, self.id) };
    }
}

/// A picture in GPU memory.
pub struct Surface {
    display: Arc<Display>,
    id: sys::VASurfaceID,
}

/// A surface's pixels, mapped: where its first plane is in `data`.
pub struct Mapped<'a> {
    pub data: &'a mut [u8],
    pub fourcc: u32,
    pub offset: usize,
    pub pitch: usize,
}

impl Surface {
    /// A surface of `width`x`height` pixels in the pixel format `fourcc`, of the family
    /// `rt_format` (`VA_RT_FORMAT_*`).
    pub fn new(
        display: &Arc<Display>,
        rt_format: u32,
        fourcc: u32,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let mut attribute = sys::VASurfaceAttrib {
            type_: sys::VASurfaceAttribPixelFormat,
            flags: sys::VA_SURFACE_ATTRIB_SETTABLE,
            value: sys::VAGenericValue {
                type_: sys::VAGenericValueTypeInteger,
                value: sys::_VAGenericValue__bindgen_ty_1 { i: fourcc as i32 },
            },
        };
        let mut id = 0;
        // SAFETY: one surface to create, one attribute.
        let status = unsafe {
            sys::vaCreateSurfaces(
                display.raw,
                rt_format,
                width,
                height,
                &mut id,
                1,
                &mut attribute,
                1,
            )
        };
        check("vaCreateSurfaces", status)?;
        Ok(Self {
            display: display.clone(),
            id,
        })
    }

    pub fn id(&self) -> sys::VASurfaceID {
        self.id
    }

    /// Wait until what was rendered to the surface is done.
    pub fn sync(&self) -> Result<()> {
        // SAFETY: our surface.
        check("vaSyncSurface", unsafe {
            sys::vaSyncSurface(self.display.raw, self.id)
        })
    }

    /// Give `write` the surface's own memory, to fill it.
    pub fn write<R>(&self, write: impl FnOnce(Mapped<'_>) -> R) -> Result<R> {
        let mut image = sys::VAImage::default();
        // SAFETY: our surface, and an image to describe it.
        check("vaDeriveImage", unsafe {
            sys::vaDeriveImage(self.display.raw, self.id, &mut image)
        })?;
        let mut memory = ptr::null_mut();
        // SAFETY: the image's buffer, mapped until vaUnmapBuffer below.
        let mapped = check("vaMapBuffer", unsafe {
            sys::vaMapBuffer(self.display.raw, image.buf, &mut memory)
        });
        let result = mapped.map(|()| {
            // SAFETY: the mapping holds `data_size` bytes, which nothing else uses meanwhile.
            let data = unsafe {
                std::slice::from_raw_parts_mut(memory.cast::<u8>(), image.data_size as usize)
            };
            let written = write(Mapped {
                data,
                fourcc: image.format.fourcc,
                offset: image.offsets[0] as usize,
                pitch: image.pitches[0] as usize,
            });
            // SAFETY: mapped above; the slice is gone.
            unsafe { sys::vaUnmapBuffer(self.display.raw, image.buf) };
            written
        });
        // SAFETY: the image derived above.
        unsafe { sys::vaDestroyImage(self.display.raw, image.image_id) };
        result
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: our surface, destroyed once.
        unsafe { sys::vaDestroySurfaces(self.display.raw, &mut self.id, 1) };
    }
}

/// A processing or encoding session.
pub struct Context {
    display: Arc<Display>,
    id: sys::VAContextID,
}

impl Context {
    /// A context for pictures of `width`x`height` pixels.
    pub fn new(config: &Config, width: u32, height: u32) -> Result<Self> {
        let mut id = 0;
        // SAFETY: a configuration of this display; no list of render targets.
        let status = unsafe {
            sys::vaCreateContext(
                config.display.raw,
                config.id,
                width as i32,
                height as i32,
                sys::VA_PROGRESSIVE as i32,
                ptr::null_mut(),
                0,
                &mut id,
            )
        };
        check("vaCreateContext", status)?;
        Ok(Self {
            display: config.display.clone(),
            id,
        })
    }

    /// Submit one picture: `buffers`, in order, applied to `target`.
    pub fn render(&self, target: &Surface, buffers: &[Buffer]) -> Result<()> {
        let mut ids: Vec<sys::VABufferID> = buffers.iter().map(|buffer| buffer.id).collect();
        // SAFETY: our context and a surface of the same display.
        check("vaBeginPicture", unsafe {
            sys::vaBeginPicture(self.display.raw, self.id, target.id)
        })?;
        // SAFETY: the list of buffers and its length.
        let rendered = check("vaRenderPicture", unsafe {
            sys::vaRenderPicture(
                self.display.raw,
                self.id,
                ids.as_mut_ptr(),
                ids.len() as i32,
            )
        });
        // Even after a failure: the picture begun must be ended.
        // SAFETY: the picture begun above.
        let ended = check("vaEndPicture", unsafe {
            sys::vaEndPicture(self.display.raw, self.id)
        });
        rendered.and(ended)
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: our context, destroyed once.
        unsafe { sys::vaDestroyContext(self.display.raw, self.id) };
    }
}

/// A buffer of parameters or of data, in a context.
pub struct Buffer {
    display: Arc<Display>,
    id: sys::VABufferID,
}

impl Buffer {
    fn create(
        context: &Context,
        type_: sys::VABufferType,
        size: usize,
        data: *mut std::ffi::c_void,
    ) -> Result<Self> {
        let size = u32::try_from(size).map_err(|_| Error::Frame("buffer too large".into()))?;
        let mut id = 0;
        // SAFETY: `data` is null or holds `size` bytes, which libva copies.
        let status = unsafe {
            sys::vaCreateBuffer(
                context.display.raw,
                context.id,
                type_,
                size,
                1,
                data,
                &mut id,
            )
        };
        check("vaCreateBuffer", status)?;
        Ok(Self {
            display: context.display.clone(),
            id,
        })
    }

    /// A parameter buffer holding a copy of `value`.
    ///
    /// # Safety
    ///
    /// `T` must be the C structure libva defines for `type_`: the driver reads the buffer as
    /// that structure.
    pub unsafe fn parameter<T: Copy>(
        context: &Context,
        type_: sys::VABufferType,
        value: &T,
    ) -> Result<Self> {
        Self::create(
            context,
            type_,
            size_of::<T>(),
            ptr::from_ref(value).cast_mut().cast(),
        )
    }

    /// A miscellaneous encoding parameter: its type, then a copy of `value`.
    ///
    /// # Safety
    ///
    /// `T` must be the C structure libva defines for `type_`.
    pub unsafe fn miscellaneous<T: Copy>(
        context: &Context,
        type_: sys::VAEncMiscParameterType,
        value: &T,
    ) -> Result<Self> {
        let header = size_of::<sys::VAEncMiscParameterBuffer>();
        let buffer = Self::create(
            context,
            sys::VAEncMiscParameterBufferType,
            header + size_of::<T>(),
            ptr::null_mut(),
        )?;
        let mut memory = ptr::null_mut();
        // SAFETY: our buffer, mapped until vaUnmapBuffer below.
        check("vaMapBuffer", unsafe {
            sys::vaMapBuffer(buffer.display.raw, buffer.id, &mut memory)
        })?;
        // SAFETY: the buffer holds the header and a `T` after it, as allocated above; the
        // writes assume no alignment.
        unsafe {
            memory
                .cast::<sys::VAEncMiscParameterType>()
                .write_unaligned(type_);
            memory
                .cast::<u8>()
                .add(header)
                .cast::<T>()
                .write_unaligned(*value);
            sys::vaUnmapBuffer(buffer.display.raw, buffer.id);
        }
        Ok(buffer)
    }

    /// A buffer holding a copy of `data`.
    pub fn bytes(context: &Context, type_: sys::VABufferType, data: &[u8]) -> Result<Self> {
        Self::create(context, type_, data.len(), data.as_ptr().cast_mut().cast())
    }

    /// A buffer for an encoded frame of up to `size` bytes.
    pub fn coded(context: &Context, size: usize) -> Result<Self> {
        Self::create(context, sys::VAEncCodedBufferType, size, ptr::null_mut())
    }

    pub fn id(&self) -> sys::VABufferID {
        self.id
    }

    /// The encoded frame of a coded buffer, once its picture is done.
    pub fn read_coded(&self) -> Result<Vec<u8>> {
        let mut memory = ptr::null_mut();
        // SAFETY: our buffer, mapped until vaUnmapBuffer below.
        check("vaMapBuffer", unsafe {
            sys::vaMapBuffer(self.display.raw, self.id, &mut memory)
        })?;
        let mut frame = Vec::new();
        let mut overflow = false;
        let mut segment = memory.cast::<sys::VACodedBufferSegment>();
        while !segment.is_null() {
            // SAFETY: a coded buffer maps to a list of segments, each with `size` bytes at
            // `buf`, ended by a null `next`.
            unsafe {
                let s = &*segment;
                overflow |= s.status & sys::VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0;
                if !s.buf.is_null() {
                    frame.extend_from_slice(std::slice::from_raw_parts(
                        s.buf.cast::<u8>(),
                        s.size as usize,
                    ));
                }
                segment = s.next.cast();
            }
        }
        // SAFETY: mapped above.
        unsafe { sys::vaUnmapBuffer(self.display.raw, self.id) };
        if overflow {
            return Err(Error::Overflow);
        }
        Ok(frame)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: our buffer, destroyed once.
        unsafe { sys::vaDestroyBuffer(self.display.raw, self.id) };
    }
}
