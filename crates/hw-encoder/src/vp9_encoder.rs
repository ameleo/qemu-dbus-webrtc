// SPDX-License-Identifier: MPL-2.0

//! VP9 on a GPU's VA-API encoder. Unlike AV1 and HEVC, the driver writes the whole frame,
//! headers included (the GPUs of Intel take no header from the application): this module
//! only says what the frame is.
//!
//! Every frame is predicted from the one before it, and none is held back or hidden. A key
//! frame comes first, and then only when asked. The bitrate changes with the next frame,
//! whatever it is. Profile 0 is 4:2:0, profile 1 4:4:4, both in 8 bits.

use std::sync::Arc;

use crate::common::{self, ENTRYPOINT, INTRA_PERIOD, RateControl};
use crate::error::{Error, Result};
use crate::va::{self, Buffer, Config, Context, Display, Surface, sys};
use crate::{Packet, Settings};

/// A tile is at most 64 superblocks of 64x64 samples wide (VP9 bitstream specification,
/// MAX_TILE_WIDTH_B64).
const MAX_TILE_WIDTH_B64: u32 = 64;

/// The quantizer and loop filter a frame starts with: the rate control sets its own.
const QINDEX: u8 = 128;
const FILTER_LEVEL: u8 = 10;

/// The profile and the surfaces of a chroma format: 4:4:4 is profile 1.
fn format(full_chroma: bool) -> (sys::VAProfile, u32, u32) {
    if full_chroma {
        (
            sys::VAProfileVP9Profile1,
            sys::VA_RT_FORMAT_YUV444,
            va::fourcc(b"AYUV"),
        )
    } else {
        (
            sys::VAProfileVP9Profile0,
            sys::VA_RT_FORMAT_YUV420,
            va::fourcc(b"NV12"),
        )
    }
}

/// Whether the GPU encodes VP9 with this chroma and depth: 8 bits only (10 bits is profiles
/// 2 and 3, which few clients decode).
pub(crate) fn supported(display: &Display, full_chroma: bool, ten_bits: bool) -> bool {
    let (profile, rt_format, _) = format(full_chroma);
    !ten_bits
        && display.has(profile, ENTRYPOINT)
        && display
            .attribute(profile, ENTRYPOINT, sys::VAConfigAttribRTFormat)
            .is_some_and(|formats| formats & rt_format != 0)
}

/// A VP9 encoder for frames of one size.
pub(crate) struct Vp9 {
    context: Context,
    /// Kept as long as its context.
    _config: Config,
    width: u32,
    height: u32,
    /// The frame to encode: the converter draws it.
    input: Surface,
    /// The frames as a decoder will see them: the one being coded and the one before it,
    /// which it is predicted from.
    reconstructed: [Surface; 2],
    /// Which of the two the next frame is reconstructed in.
    current: usize,
    coded: Buffer,
    /// The next frame may be predicted from the one before it.
    predicted: bool,
    rate: RateControl,
}

impl Vp9 {
    pub fn new(
        display: &Arc<Display>,
        settings: &Settings,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        if settings.ten_bits {
            return Err(Error::Unsupported(
                "VP9 in 10 bits (profiles 2 and 3)".into(),
            ));
        }
        let (profile, rt_format, fourcc) = format(settings.full_chroma);
        let chroma = if settings.full_chroma {
            "4:4:4"
        } else {
            "4:2:0"
        };
        // The driver writes the headers: none from here.
        common::check(display, profile, rt_format, 0, &format!("VP9 {chroma}"))?;
        let config = Config::new(
            display,
            profile,
            ENTRYPOINT,
            &[
                (sys::VAConfigAttribRTFormat, rt_format),
                (sys::VAConfigAttribRateControl, sys::VA_RC_CBR),
            ],
        )?;
        // Whole blocks of 8x8 samples: the converter fills them.
        let (coded_width, coded_height) = (width.next_multiple_of(8), height.next_multiple_of(8));
        let context = Context::new(&config, coded_width, coded_height)?;
        let surface = || Surface::new(display, rt_format, fourcc, coded_width, coded_height);
        let input = surface()?;
        let reconstructed = [surface()?, surface()?];
        let coded = Buffer::coded(&context, common::coded_size(coded_width, coded_height))?;
        Ok(Self {
            context,
            _config: config,
            width,
            height,
            input,
            reconstructed,
            current: 0,
            coded,
            predicted: false,
            rate: RateControl::new(display, profile, settings.framerate, settings.bitrate),
        })
    }

    /// The size of the input surface.
    pub fn coded_size(&self) -> (u32, u32) {
        (
            self.width.next_multiple_of(8),
            self.height.next_multiple_of(8),
        )
    }

    /// The surface to draw the next frame in.
    pub fn input(&self) -> &Surface {
        &self.input
    }

    /// Change the bitrate, in bit/s, from the next frame on.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.rate.set_bitrate(bitrate);
    }

    /// Encode the frame drawn in the input surface; as a key frame when asked.
    pub fn encode(&mut self, keyframe: bool) -> Result<Packet> {
        self.rate.frame();
        let key = keyframe || !self.predicted;
        // Whatever fails below, the next frame has nothing to be predicted from.
        self.predicted = false;
        let context = &self.context;
        let mut buffers = Vec::new();
        if key {
            let sequence = sys::VAEncSequenceParameterBufferVP9 {
                max_frame_width: self.width,
                max_frame_height: self.height,
                // Key frames only when asked.
                kf_auto: 0,
                kf_min_dist: 1,
                kf_max_dist: INTRA_PERIOD,
                intra_period: INTRA_PERIOD,
                bits_per_second: self.rate.bitrate(),
                ..Default::default()
            };
            // SAFETY: the structure of this buffer type.
            buffers.push(unsafe {
                Buffer::parameter(context, sys::VAEncSequenceParameterBufferType, &sequence)
            }?);
        }
        buffers.extend(self.rate.buffers(context, key)?);
        // SAFETY: the structure of this buffer type.
        buffers.push(unsafe {
            Buffer::parameter(
                context,
                sys::VAEncPictureParameterBufferType,
                &self.picture_parameters(key),
            )
        }?);
        self.context.render(&self.input, &buffers)?;
        self.input.sync()?;
        let data = self.coded.read_coded()?;
        self.rate.told(key);
        self.current ^= 1;
        self.predicted = true;
        Ok(Packet {
            data,
            keyframe: key,
        })
    }

    /// The frame, as the encoder is told it.
    fn picture_parameters(&self, key: bool) -> sys::VAEncPictureParameterBufferVP9 {
        // The fewest tile columns a frame this wide may have.
        let superblocks = self.width.div_ceil(64);
        let mut log2_tile_columns = 0;
        while (MAX_TILE_WIDTH_B64 << log2_tile_columns) < superblocks {
            log2_tile_columns += 1;
        }
        let mut parameters = sys::VAEncPictureParameterBufferVP9 {
            frame_width_src: self.width,
            frame_height_src: self.height,
            frame_width_dst: self.width,
            frame_height_dst: self.height,
            reconstructed_frame: self.reconstructed[self.current].id(),
            reference_frames: [sys::VA_INVALID_SURFACE; 8],
            coded_buf: self.coded.id(),
            // Every slot takes this frame: the next one finds it in any.
            refresh_frame_flags: 0xff,
            luma_ac_qindex: QINDEX,
            filter_level: FILTER_LEVEL,
            log2_tile_columns,
            ..Default::default()
        };
        if !key {
            parameters.reference_frames = [self.reconstructed[self.current ^ 1].id(); 8];
        }
        // SAFETY: both views of these unions are plain integers.
        unsafe {
            let flags = &mut parameters.pic_flags.bits;
            // 0: KEY_FRAME, 1: NON_KEY_FRAME.
            flags.set_frame_type((!key).into());
            flags.set_show_frame(1);
            flags.set_allow_high_precision_mv(1);
            if !key {
                // The last frame only (bit 0 of the references to search: LAST, GOLDEN,
                // ALTREF), from slot 0.
                let references = &mut parameters.ref_flags.bits;
                references.set_ref_frame_ctrl_l0(1);
                references.set_ref_last_idx(0);
                references.set_ref_gf_idx(0);
                references.set_ref_arf_idx(0);
            }
        }
        parameters
    }
}
