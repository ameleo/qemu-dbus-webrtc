// SPDX-License-Identifier: MPL-2.0

//! VP9 on the CPU, with libvpx, for hosts without a GPU that encodes: the same frames in, the
//! same packets out as the GPU's encoders.
//!
//! Real time (deadline `VPX_DL_REALTIME`, `cpu-used` 6: measured at 3440x1440, 69 frames a
//! second on about 7 cores, 48.8 dB at 5 Mbit/s; 8 is faster but blurs the picture), rows
//! and tiles on several threads, no frame held back (`g_lag_in_frames` 0, so no hidden
//! frame), keyframes only when asked, a constant bitrate with half a second of buffer. libvpx
//! takes the frames' timestamps: its rate control follows the real frame rate.

use std::ffi::{CStr, c_int, c_uint};
use std::time::Instant;

use crate::error::{Error, Result};
use crate::yuv::{self, Plane};
use crate::{Frame, Packet, Settings};

/// The bindings bindgen generates from the installed headers.
#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unused_imports,
    clippy::all
)]
mod sys {
    include!(concat!(env!("OUT_DIR"), "/vpx.rs"));
}

/// `VPX_ENCODER_ABI_VERSION` (vpx_encoder.h), which the macro `vpx_codec_enc_init` passes.
const ENCODER_ABI_VERSION: c_int = sys::VPX_ENCODER_ABI_VERSION as c_int;

/// `cpu-used`: see the module.
const CPU_USED: c_int = 6;

/// Timestamps in microseconds.
const TIMEBASE: c_int = 1_000_000;

/// The rate control buffer, its starting and optimal fullness, in milliseconds.
const BUFFER: c_uint = 500;
const BUFFER_INITIAL: c_uint = 250;
const BUFFER_OPTIMAL: c_uint = 400;

/// The message of a libvpx error.
fn check(call: &'static str, status: sys::vpx_codec_err_t) -> Result<()> {
    if status == sys::VPX_CODEC_OK {
        return Ok(());
    }
    // SAFETY: vpx_codec_err_to_string returns a static string for any status.
    let message = unsafe { CStr::from_ptr(sys::vpx_codec_err_to_string(status)) }
        .to_string_lossy()
        .into_owned();
    Err(Error::Va {
        call,
        status: status as i32,
        message,
    })
}

/// A VP9 encoder on the CPU, for frames of one size.
pub(crate) struct Vpx {
    context: sys::vpx_codec_ctx_t,
    config: sys::vpx_codec_enc_cfg_t,
    image: *mut sys::vpx_image_t,
    full_chroma: bool,
    width: u32,
    height: u32,
    /// When the encoder started: the origin of the timestamps.
    start: Instant,
    /// The timestamp of the last frame, which the next one must follow.
    last: Option<i64>,
}

// SAFETY: the context and the image are only used through `&mut self`, by one thread at a
// time; libvpx keeps no thread-local state for them.
unsafe impl Send for Vpx {}

impl Vpx {
    pub fn new(settings: &Settings, width: u32, height: u32) -> Result<Self> {
        if settings.ten_bits {
            return Err(Error::Unsupported(
                "VP9 in 10 bits (profiles 2 and 3)".into(),
            ));
        }
        // SAFETY: the VP9 encoder's interface, static in libvpx.
        let interface = unsafe { sys::vpx_codec_vp9_cx() };
        let mut config = sys::vpx_codec_enc_cfg_t::default();
        // SAFETY: a configuration to fill with the encoder's defaults.
        check("vpx_codec_enc_config_default", unsafe {
            sys::vpx_codec_enc_config_default(interface, &mut config, 0)
        })?;
        config.g_w = width;
        config.g_h = height;
        // Profile 1 is 4:4:4 in 8 bits.
        config.g_profile = settings.full_chroma.into();
        config.g_timebase = sys::vpx_rational {
            num: 1,
            den: TIMEBASE,
        };
        config.g_threads =
            std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as u32;
        config.g_lag_in_frames = 0;
        config.g_error_resilient = 0;
        config.g_pass = sys::VPX_RC_ONE_PASS;
        config.rc_end_usage = sys::VPX_CBR;
        config.rc_target_bitrate = (settings.bitrate / 1000).max(1);
        config.rc_buf_sz = BUFFER;
        config.rc_buf_initial_sz = BUFFER_INITIAL;
        config.rc_buf_optimal_sz = BUFFER_OPTIMAL;
        config.kf_mode = sys::VPX_KF_DISABLED;
        let mut context = sys::vpx_codec_ctx_t::default();
        // SAFETY: an encoder to initialize with this configuration, which libvpx copies.
        check("vpx_codec_enc_init", unsafe {
            sys::vpx_codec_enc_init_ver(&mut context, interface, &config, 0, ENCODER_ABI_VERSION)
        })?;
        let mut encoder = Self {
            context,
            config,
            image: std::ptr::null_mut(),
            full_chroma: settings.full_chroma,
            width,
            height,
            start: Instant::now(),
            last: None,
        };
        // VP9E_SET_TILE_COLUMNS takes the log2 of the columns, which the frame's width may
        // lower: four columns where it allows.
        for (control, value) in [
            (sys::VP8E_SET_CPUUSED, CPU_USED),
            (sys::VP9E_SET_ROW_MT, 1),
            (sys::VP9E_SET_TILE_COLUMNS, 2),
            // What the decoders are told of the colours: BT.601, limited range.
            (sys::VP9E_SET_COLOR_SPACE, sys::VPX_CS_BT_601 as c_int),
            (sys::VP9E_SET_COLOR_RANGE, sys::VPX_CR_STUDIO_RANGE as c_int),
        ] {
            // SAFETY: an initialized encoder; every one of these controls takes an int.
            check("vpx_codec_control", unsafe {
                sys::vpx_codec_control_(&mut encoder.context, control as c_int, value)
            })?;
        }
        let format = if settings.full_chroma {
            sys::VPX_IMG_FMT_I444
        } else {
            sys::VPX_IMG_FMT_I420
        };
        // SAFETY: a new image that libvpx allocates, freed when the encoder is dropped.
        encoder.image =
            unsafe { sys::vpx_img_alloc(std::ptr::null_mut(), format, width, height, 32) };
        if encoder.image.is_null() {
            return Err(Error::Unsupported("an image for libvpx".into()));
        }
        Ok(encoder)
    }

    /// Whether this encoder takes frames of this size.
    pub fn takes(&self, width: u32, height: u32) -> bool {
        (width, height) == (self.width, self.height)
    }

    /// Change the bitrate, in bit/s, from the next frame on, without a keyframe.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        self.config.rc_target_bitrate = (bitrate / 1000).max(1);
        // SAFETY: an initialized encoder; libvpx copies the configuration.
        check("vpx_codec_enc_config_set", unsafe {
            sys::vpx_codec_enc_config_set(&mut self.context, &self.config)
        })
    }

    /// Encode `frame`, which must be of the encoder's size; as a keyframe when asked.
    pub fn encode(&mut self, frame: &Frame<'_>, keyframe: bool) -> Result<Packet> {
        if (frame.width, frame.height) != (self.width, self.height) {
            return Err(Error::Frame("not the encoder's size".into()));
        }
        // SAFETY: the image allocated in `new`, of the encoder's size and format.
        let image = unsafe { &*self.image };
        let step = if self.full_chroma { 1 } else { 2 };
        let chroma_rows = self.height.div_ceil(step) as usize;
        let stride = |plane: usize| image.stride[plane] as usize;
        // SAFETY: the planes of the image libvpx allocated, each of its stride by its rows.
        let planes = unsafe {
            [
                Plane {
                    data: std::slice::from_raw_parts_mut(
                        image.planes[0],
                        stride(0) * self.height as usize,
                    ),
                    stride: stride(0),
                },
                Plane {
                    data: std::slice::from_raw_parts_mut(image.planes[1], stride(1) * chroma_rows),
                    stride: stride(1),
                },
                Plane {
                    data: std::slice::from_raw_parts_mut(image.planes[2], stride(2) * chroma_rows),
                    stride: stride(2),
                },
            ]
        };
        yuv::convert(frame, self.full_chroma, planes);

        // Its time, after the last frame's.
        let now = self.start.elapsed().as_micros() as i64;
        let pts = self.last.map_or(now, |last| now.max(last + 1));
        let duration = self.last.map_or(1, |last| (pts - last).max(1)) as u64;
        self.last = Some(pts);
        let flags = if keyframe {
            sys::VPX_EFLAG_FORCE_KF as sys::vpx_enc_frame_flags_t
        } else {
            0
        };
        // SAFETY: an initialized encoder and the image filled above.
        check("vpx_codec_encode", unsafe {
            sys::vpx_codec_encode(
                &mut self.context,
                self.image,
                pts,
                duration,
                flags,
                sys::VPX_DL_REALTIME.into(),
            )
        })?;
        let mut data = Vec::new();
        let mut key = false;
        let mut iterator = std::ptr::null();
        loop {
            // SAFETY: the encoder's output, valid until the next call on it.
            let packet = unsafe { sys::vpx_codec_get_cx_data(&mut self.context, &mut iterator) };
            if packet.is_null() {
                break;
            }
            // SAFETY: a packet libvpx returned; a frame packet's union member is its frame.
            unsafe {
                if (*packet).kind == sys::VPX_CODEC_CX_FRAME_PKT {
                    let frame = (*packet).data.frame;
                    data.extend_from_slice(std::slice::from_raw_parts(
                        frame.buf.cast::<u8>(),
                        frame.sz,
                    ));
                    key |= frame.flags & sys::VPX_FRAME_IS_KEY != 0;
                }
            }
        }
        Ok(Packet {
            data,
            keyframe: key,
        })
    }
}

impl Drop for Vpx {
    fn drop(&mut self) {
        // SAFETY: the image and the encoder made in `new`, freed once.
        unsafe {
            if !self.image.is_null() {
                sys::vpx_img_free(self.image);
            }
            sys::vpx_codec_destroy(&mut self.context);
        }
    }
}
